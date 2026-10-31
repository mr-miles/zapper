//! What the proxy makes of an inspected request before policy sees it, and of
//! the response before the guest does:
//!
//! - the path a rule judges is the one the upstream will act on: percent-
//!   encoded unreserved characters decoded, dot segments removed, and that
//!   path is what is forwarded. A path whose meaning depends on how the
//!   upstream decodes it (a dot segment behind an encoded slash, a
//!   backslash, a NUL) is refused;
//! - a request asking the upstream to run another method than its request
//!   line (`X-HTTP-Method-Override: DELETE` on a GET, `?_method=DELETE`) is
//!   refused, since rules judge the request line;
//! - a secret the proxy added never comes back in the response: its value is
//!   masked, byte for byte, in the response's headers and body; a body the
//!   upstream compressed though asked not to is decoded first.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};
use hyper::HeaderMap;

use super::Body;

/// RFC 3986's unreserved characters: decoding them never changes a URL's
/// meaning.
pub(super) fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// The path and query to judge and forward, or why the path is refused. The
/// query is kept as sent; it is the path that rules match.
pub fn normalize_path(path_and_query: &str) -> Result<String, String> {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };
    if path == "*" {
        return Ok(path_and_query.to_string());
    }
    if !path.starts_with('/') {
        return Err("the path must start with /".into());
    }
    // Decode what is safe to decode; write every other escape in capitals.
    let bytes = path.as_bytes();
    let mut decoded = String::with_capacity(path.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' || b < 0x20 || b == 0x7f {
            return Err("the path has a backslash or a control character".into());
        }
        if b != b'%' {
            decoded.push(b as char);
            i += 1;
            continue;
        }
        let (Some(h), Some(l)) = (
            bytes.get(i + 1).copied().and_then(hex),
            bytes.get(i + 2).copied().and_then(hex),
        ) else {
            return Err("the path has a bad percent-escape".into());
        };
        let c = h * 16 + l;
        match c {
            0 => return Err("the path has an encoded NUL".into()),
            c if unreserved(c) => decoded.push(c as char),
            c => decoded.push_str(&format!("%{c:02X}")),
        }
        i += 3;
    }
    // An upstream that decodes an encoded slash or backslash would see dot
    // segments the proxy can't: refuse rather than guess.
    for segment in decoded.split('/') {
        let pieces = segment.split("%2F").flat_map(|p| p.split("%5C"));
        if segment.contains('%')
            && pieces.clone().count() > 1
            && pieces.into_iter().any(|p| p == "." || p == "..")
        {
            return Err("the path has a dot segment behind an encoded slash".into());
        }
    }
    let mut out = remove_dot_segments(&decoded);
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    Ok(out)
}

/// RFC 3986 section 5.2.4, for an absolute path: `.` segments dropped, `..`
/// taking the one before it (never above the root).
fn remove_dot_segments(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let segments: Vec<&str> = path[1..].split('/').collect();
    let last = segments.len() - 1;
    for (i, seg) in segments.iter().enumerate() {
        match *seg {
            "." => {
                if i == last {
                    out.push("");
                }
            }
            ".." => {
                out.pop();
                if i == last {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

/// Headers that ask an upstream to run another method.
const OVERRIDE_HEADERS: [&str; 4] = [
    "x-http-method-override",
    "x-http-method",
    "x-method-override",
    "x-goog-http-method-override",
];
/// Query parameters that do (Rails and Laravel `_method`, Salesforce
/// `_HttpMethod`, OData `$httpmethod`), compared without case.
const OVERRIDE_PARAMS: [&str; 3] = ["_method", "_httpmethod", "$httpmethod"];

/// Percent-decoded, `+` as a space: as a framework reads a query.
fn query_decode(s: &str) -> String {
    percent_decode(s, true)
}

/// `s` percent-decoded, a `%` not followed by two hex digits kept as it
/// is; `+` as a space too when `plus_is_space` (a form's encoding).
pub(super) fn percent_decode(s: &str, plus_is_space: bool) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' if plus_is_space => out.push(b' '),
            b'%' => match (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            ) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Why a request is refused for naming another method than its own, if it
/// does: in an override header (each comma-separated value) or an override
/// query parameter. Naming its own method is harmless.
pub fn method_override(method: &str, path_and_query: &str, headers: &HeaderMap) -> Option<String> {
    let other = |named: &str| !named.trim().eq_ignore_ascii_case(method);
    for name in OVERRIDE_HEADERS {
        for v in headers.get_all(name) {
            let v = String::from_utf8_lossy(v.as_bytes());
            if let Some(named) = v.split(',').find(|n| other(n)) {
                return Some(format!(
                    "{name}: {} asks the upstream to run another method than {method}; rules judge the request's own method, so send it as that method",
                    named.trim()
                ));
            }
        }
    }
    let (_, query) = path_and_query.split_once('?')?;
    query.split(['&', ';']).find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let k = query_decode(k);
        if !OVERRIDE_PARAMS.iter().any(|p| p.eq_ignore_ascii_case(k.trim())) {
            return None;
        }
        let v = query_decode(v);
        other(&v).then(|| {
            format!(
                "the query parameter {}={} asks the upstream to run another method than {method}; rules judge the request's own method, so send it as that method",
                k.trim(),
                v.trim()
            )
        })
    })
}

/// Secret values shorter than this aren't masked in responses: they would
/// mangle ordinary text, and are too short to be credentials worth hiding.
const MASK_MIN: usize = 8;

/// The injected values worth masking, longest first.
pub fn mask_values(values: &[String]) -> Arc<Vec<Vec<u8>>> {
    let mut v: Vec<Vec<u8>> = values
        .iter()
        .filter(|v| v.len() >= MASK_MIN)
        .map(|v| v.as_bytes().to_vec())
        .collect();
    v.sort_by_key(|v| std::cmp::Reverse(v.len()));
    v.dedup();
    Arc::new(v)
}

/// The response's content coding, when it has one other than `identity`
/// and some of `values` would be masked in it: a body that can't be
/// searched for them until it is decoded ([`decode_response`]).
pub fn unmaskable(headers: &HeaderMap, values: &[String]) -> Option<String> {
    if !values.iter().any(|v| v.len() >= MASK_MIN) {
        return None;
    }
    let enc = headers
        .get_all(hyper::header::CONTENT_ENCODING)
        .iter()
        .map(|v| {
            String::from_utf8_lossy(v.as_bytes())
                .trim()
                .to_ascii_lowercase()
        })
        .filter(|e| !e.is_empty() && e != "identity")
        .collect::<Vec<_>>();
    (!enc.is_empty()).then(|| enc.join(", "))
}

/// Input fed to a decoder at a time, so what one frame expands to comes
/// out in pieces: a small, highly compressed frame can't fill memory.
const DECODE_STEP: usize = 1024;

/// A push decoder for one content coding, its output collected as it
/// comes.
enum Decoder {
    Gzip(flate2::write::MultiGzDecoder<Vec<u8>>),
    Deflate(flate2::write::ZlibDecoder<Vec<u8>>),
    Br(Box<brotli::DecompressorWriter<Vec<u8>>>),
}

impl Decoder {
    fn new(coding: &str) -> Option<Self> {
        Some(match coding {
            "gzip" | "x-gzip" => Decoder::Gzip(flate2::write::MultiGzDecoder::new(Vec::new())),
            "deflate" => Decoder::Deflate(flate2::write::ZlibDecoder::new(Vec::new())),
            "br" => Decoder::Br(Box::new(brotli::DecompressorWriter::new(Vec::new(), 4096))),
            _ => return None,
        })
    }

    fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        use std::io::Write as _;
        match self {
            Decoder::Gzip(d) => d.write_all(data),
            Decoder::Deflate(d) => d.write_all(data),
            Decoder::Br(d) => d.write_all(data),
        }
    }

    fn finish(&mut self) -> std::io::Result<()> {
        match self {
            Decoder::Gzip(d) => d.try_finish(),
            Decoder::Deflate(d) => d.try_finish(),
            Decoder::Br(d) => d.close().map_err(std::io::Error::other),
        }
    }

    /// What has been decoded since last asked.
    fn take(&mut self) -> Vec<u8> {
        std::mem::take(match self {
            Decoder::Gzip(d) => d.get_mut(),
            Decoder::Deflate(d) => d.get_mut(),
            Decoder::Br(d) => d.get_mut(),
        })
    }
}

/// The response decoded from its content coding `coding` (what
/// [`unmaskable`] found), so the values in it can be masked: sent on
/// uncompressed, without `Content-Encoding` or `Content-Length`. An error
/// for a coding that can't be decoded here (several, or one unknown).
/// Malformed compressed data ends the body where it stops decoding.
pub fn decode_response(
    resp: hyper::Response<hyper::body::Incoming>,
    coding: &str,
) -> Result<hyper::Response<Body>, String> {
    use http_body_util::BodyExt as _;
    let decoder = Decoder::new(coding)
        .ok_or_else(|| format!("a {coding}-encoded response, which can't be decoded here"))?;
    let (mut parts, body) = resp.into_parts();
    parts.headers.remove(hyper::header::CONTENT_ENCODING);
    parts.headers.remove(hyper::header::CONTENT_LENGTH);
    let body = Decoded {
        inner: body.boxed(),
        decoder,
        input: Bytes::new(),
        trailers: None,
        ended: false,
        done: false,
    }
    .boxed();
    Ok(hyper::Response::from_parts(parts, body))
}

/// A body decoded as it streams ([`decode_response`]).
struct Decoded {
    inner: Body,
    decoder: Decoder,
    /// Compressed bytes not yet fed to the decoder.
    input: Bytes,
    /// A trailers frame, sent once the decoded bytes have gone.
    trailers: Option<Frame<Bytes>>,
    /// The upstream's body has ended (or a trailers frame came).
    ended: bool,
    /// Everything decoded has been sent.
    done: bool,
}

impl HttpBody for Decoded {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let this = self.get_mut();
        loop {
            if this.done {
                return Poll::Ready(this.trailers.take().map(Ok));
            }
            if !this.input.is_empty() {
                let step = this.input.split_to(this.input.len().min(DECODE_STEP));
                if this.decoder.write(&step).is_err() {
                    // Not what it says it is: what decoded has gone.
                    this.done = true;
                    continue;
                }
                let out = this.decoder.take();
                if !out.is_empty() {
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out)))));
                }
                continue;
            }
            if this.ended {
                this.done = true;
                let _ = this.decoder.finish();
                let out = this.decoder.take();
                if !out.is_empty() {
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out)))));
                }
                continue;
            }
            match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) => this.input = data,
                    Err(trailers) => {
                        this.trailers = Some(trailers);
                        this.ended = true;
                    }
                },
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => this.ended = true,
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done && self.trailers.is_none()
    }
}

/// Overwrites each occurrence of a value in `buf` with `*`, keeping its
/// length.
fn mask(buf: &mut [u8], values: &[Vec<u8>]) {
    for v in values {
        let mut i = 0;
        while i + v.len() <= buf.len() {
            if buf[i..i + v.len()] == v[..] {
                buf[i..i + v.len()].fill(b'*');
                i += v.len();
            } else {
                i += 1;
            }
        }
    }
}

/// Masks the values in every response header.
pub fn mask_headers(headers: &mut HeaderMap, values: &[Vec<u8>]) {
    if values.is_empty() {
        return;
    }
    for v in headers.values_mut() {
        let mut b = v.as_bytes().to_vec();
        mask(&mut b, values);
        if b != v.as_bytes() {
            if let Ok(masked) = hyper::header::HeaderValue::from_bytes(&b) {
                *v = masked;
            }
        }
    }
}

/// A response body with the values masked as it streams. The last
/// `longest - 1` bytes are held back until more arrive, so a value split
/// across two frames is still caught. Lengths don't change, so the body's
/// size (and `Content-Length`) stays right.
pub struct Masked {
    inner: Body,
    values: Arc<Vec<Vec<u8>>>,
    held: Vec<u8>,
    keep: usize,
    /// A trailers frame that waits for the held bytes to go first.
    trailers: Option<Frame<Bytes>>,
    ended: bool,
}

impl Masked {
    pub fn wrap(inner: Body, values: Arc<Vec<Vec<u8>>>) -> Body {
        use http_body_util::BodyExt as _;
        if values.is_empty() {
            return inner;
        }
        let keep = values.iter().map(Vec::len).max().unwrap_or(1) - 1;
        Masked {
            inner,
            values,
            held: Vec::new(),
            keep,
            trailers: None,
            ended: false,
        }
        .boxed()
    }
}

impl HttpBody for Masked {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let this = self.get_mut();
        loop {
            if this.ended || this.trailers.is_some() {
                if !this.held.is_empty() {
                    let rest = std::mem::take(&mut this.held);
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from(rest)))));
                }
                if let Some(t) = this.trailers.take() {
                    return Poll::Ready(Some(Ok(t)));
                }
                if this.ended {
                    return Poll::Ready(None);
                }
            }
            match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) => {
                        this.held.extend_from_slice(&data);
                        mask(&mut this.held, &this.values);
                        let out = this.held.len().saturating_sub(this.keep);
                        if out == 0 {
                            continue;
                        }
                        let rest = this.held.split_off(out);
                        let ready = std::mem::replace(&mut this.held, rest);
                        return Poll::Ready(Some(Ok(Frame::data(Bytes::from(ready)))));
                    }
                    Err(trailers) => this.trailers = Some(trailers),
                },
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => this.ended = true,
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.ended && self.held.is_empty() && self.trailers.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One decoding for a query (`+` a space) and for AWS's (`+` itself);
    /// a `%` without two hex digits stays.
    #[test]
    fn percent_decoding_with_and_without_plus() {
        assert_eq!(percent_decode("a%20b+c%2Fd", true), "a b c/d");
        assert_eq!(percent_decode("a%20b+c%2Fd", false), "a b+c/d");
        assert_eq!(percent_decode("100%", false), "100%");
        assert_eq!(percent_decode("%zz%4", true), "%zz%4");
    }

    #[test]
    fn paths_are_judged_as_the_upstream_reads_them() {
        for (sent, judged) in [
            ("/", "/"),
            ("/a/b", "/a/b"),
            ("/a/./b", "/a/b"),
            ("/a/../b", "/b"),
            ("/../../a", "/a"),
            ("/a/b/..", "/a/"),
            ("/a/b/.", "/a/b/"),
            ("/a//b", "/a//b"),
            ("/%61%62", "/ab"),
            ("/a/%2e%2e/b", "/b"),
            ("/a/%2E/b", "/a/b"),
            ("/%7euser", "/~user"),
            ("/a%2fb", "/a%2Fb"),
            ("/a%20b", "/a%20b"),
            ("/a?x=/../y&%2e", "/a?x=/../y&%2e"),
            ("*", "*"),
        ] {
            assert_eq!(normalize_path(sent).as_deref(), Ok(judged), "{sent}");
        }
        for bad in [
            "a/b",
            "/a\\b",
            "/a%zz",
            "/a%2",
            "/a%00b",
            "/a/..%2fb",
            "/a/%2e%2e%2Fb",
            "/a/.%5Cb",
            "/a/b%2F..",
        ] {
            assert!(normalize_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn masking_keeps_lengths_and_catches_every_copy() {
        let values = mask_values(&["secret-123".into(), "short".into()]);
        assert_eq!(values.len(), 1, "too short to mask");
        let mut b = b"a secret-123 b secret-123secret-123 short".to_vec();
        mask(&mut b, &values);
        assert_eq!(&b[..], b"a ********** b ******************** short");
    }

    #[tokio::test]
    async fn a_value_split_across_frames_is_masked() {
        use http_body_util::{BodyExt, StreamBody};
        let chunks: Vec<Result<Frame<Bytes>, hyper::Error>> =
            ["xx secr", "et-1", "23 yy secret-12", "3"]
                .iter()
                .map(|c| Ok(Frame::data(Bytes::from_static(c.as_bytes()))))
                .collect();
        let inner: Body = StreamBody::new(futures_util::stream::iter(chunks)).boxed();
        let body = Masked::wrap(inner, mask_values(&["secret-123".into()]));
        let all = body.collect().await.unwrap().to_bytes();
        assert_eq!(&all[..], b"xx ********** yy **********");
    }
}
