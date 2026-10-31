//! AWS Signature Version 4, signed on the host.
//!
//! An `aws` secret's guest holds a fake access key id (`aws_key_id`) and
//! signs with it as any SDK does. When an inspected request carries such a
//! signature, in its `Authorization` header or as a presigned URL's query,
//! and a rule allowing it lists the secret, the proxy signs it again with the
//! real keys: the same region, service and signed headers the guest chose,
//! its own clock, and the session token when the keys are temporary. An S3
//! upload in signed chunks (`STREAMING-AWS4-HMAC-SHA256-PAYLOAD`, with or
//! without a signed trailer) has every chunk's signature redone as it
//! streams. The keys never reach the guest, and a signature made with them
//! covers exactly the request the policy allowed.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};
use hyper::header::{HeaderName, HeaderValue};
use hyper::HeaderMap;
use ring::{digest, hmac};

use super::Body;

/// Real keys, from the secret's credential_process JSON.
#[derive(Clone)]
pub struct Creds {
    pub key_id: String,
    pub secret: String,
    pub token: Option<String>,
}

impl std::fmt::Debug for Creds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Creds({})", self.key_id)
    }
}

impl Creds {
    pub fn parse(value: &str) -> Result<Creds, String> {
        let v: serde_json::Value = serde_json::from_str(value)
            .map_err(|_| "an aws secret's value is credential_process JSON".to_string())?;
        let field = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
        Ok(Creds {
            key_id: field("AccessKeyId").ok_or("the aws secret has no AccessKeyId")?,
            secret: field("SecretAccessKey").ok_or("the aws secret has no SecretAccessKey")?,
            token: field("SessionToken"),
        })
    }
}

/// What a guest's signature says: who, when for, and over what.
#[derive(Debug, Clone, PartialEq)]
pub struct Signed {
    pub key_id: String,
    pub region: String,
    pub service: String,
    /// Lowercase, as the guest listed them.
    pub headers: Vec<String>,
    /// From the query (a presigned URL), not the `Authorization` header.
    pub presigned: bool,
}

const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
pub const STREAMING: &str = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD";
pub const STREAMING_TRAILER: &str = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER";

fn token_ok(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `KEYID/DATE/REGION/SERVICE/aws4_request`.
fn parse_credential(c: &str) -> Option<(String, String, String)> {
    let mut it = c.split('/');
    let (id, _date, region, service, term) =
        (it.next()?, it.next()?, it.next()?, it.next()?, it.next()?);
    (term == "aws4_request" && it.next().is_none() && token_ok(region) && token_ok(service))
        .then(|| (id.to_string(), region.to_string(), service.to_string()))
}

/// The guest's signature, from its `Authorization` header or its query.
pub fn signed(headers: &HeaderMap, query: &str) -> Option<Signed> {
    if let Some(auth) = headers
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        let rest = auth.strip_prefix(ALGORITHM)?.trim_start();
        let mut credential = None;
        let mut list = None;
        for part in rest.split(',') {
            let (k, v) = part.trim().split_once('=')?;
            match k {
                "Credential" => credential = Some(v),
                "SignedHeaders" => list = Some(v),
                _ => {}
            }
        }
        let (key_id, region, service) = parse_credential(credential?)?;
        return Some(Signed {
            key_id,
            region,
            service,
            headers: list?.split(';').map(|h| h.to_ascii_lowercase()).collect(),
            presigned: false,
        });
    }
    let params = query_pairs(query);
    let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    if get("X-Amz-Algorithm")? != ALGORITHM {
        return None;
    }
    let (key_id, region, service) = parse_credential(get("X-Amz-Credential")?)?;
    Some(Signed {
        key_id,
        region,
        service,
        headers: get("X-Amz-SignedHeaders")
            .unwrap_or("host")
            .split(';')
            .map(|h| h.to_ascii_lowercase())
            .collect(),
        presigned: true,
    })
}

/// AWS's URI encoding: every byte but the unreserved ones, and `/` when
/// `slash` says so, as `%XX` in capitals.
fn encode(s: &str, slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if super::sanitize::unreserved(b) || (slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A query's pairs, decoded.
fn query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (
                super::sanitize::percent_decode(k, false),
                super::sanitize::percent_decode(v, false),
            )
        })
        .collect()
}

/// The canonical query: every pair (but the signature) encoded and sorted.
fn canonical_query(query: &str) -> String {
    let mut pairs: Vec<(String, String)> = query_pairs(query)
        .into_iter()
        .filter(|(k, _)| k != "X-Amz-Signature")
        .map(|(k, v)| (encode(&k, false), encode(&v, false)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// S3 signs the path as sent; every other service signs it encoded again.
fn canonical_uri(path: &str, service: &str) -> String {
    if path.is_empty() {
        return "/".into();
    }
    if service == "s3" || service.starts_with("s3-") {
        path.to_string()
    } else {
        encode(path, true)
    }
}

fn canonical_headers(headers: &HeaderMap, names: &[String]) -> String {
    let mut out = String::new();
    for n in names {
        let values: Vec<String> = headers
            .get_all(n.as_str())
            .iter()
            .map(|v| {
                String::from_utf8_lossy(v.as_bytes())
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        out.push_str(&format!("{n}:{}\n", values.join(",")));
    }
    out
}

fn sha256_hex(data: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, data).as_ref())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hmac_sha256(key: &[u8], data: &str) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data.as_bytes())
        .as_ref()
        .to_vec()
}

/// The key a day's signatures in one region and service are made with.
fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k = hmac_sha256(format!("AWS4{secret}").as_bytes(), date);
    let k = hmac_sha256(&k, region);
    let k = hmac_sha256(&k, service);
    hmac_sha256(&k, "aws4_request")
}

/// `20130524T000000Z`.
pub fn amz_date(t: time::OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// What a request was signed with: for its chunks to follow.
#[derive(Clone)]
pub struct Signer {
    key: Vec<u8>,
    date: String,
    scope: String,
}

impl Signer {
    fn sign(&self, string_to_sign: &str) -> String {
        hex(&hmac_sha256(&self.key, string_to_sign))
    }
}

/// Signs a request over `path_and_query`, its `headers` (which carry `host`)
/// and the payload hash. For a request signed in its header, `headers` gains
/// `x-amz-date`, the token and `Authorization`; for a presigned one, the
/// returned path carries them. Returns the path to forward, the signature
/// and what chunks are signed with.
#[allow(clippy::too_many_arguments)]
pub fn sign(
    method: &str,
    path_and_query: &str,
    headers: &mut HeaderMap,
    signed: &Signed,
    creds: &Creds,
    payload: &str,
    now: time::OffsetDateTime,
) -> Result<(String, String, Signer), String> {
    let stamp = amz_date(now);
    let date = &stamp[..8];
    let scope = format!("{date}/{}/{}/aws4_request", signed.region, signed.service);
    let (path, query) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    // The headers the guest signed that are still there, and what the
    // signature needs besides.
    let mut names: Vec<String> = signed
        .headers
        .iter()
        .filter(|n| headers.contains_key(n.as_str()))
        .filter(|n| {
            !matches!(
                n.as_str(),
                "authorization" | "x-amz-date" | "x-amz-security-token"
            )
        })
        .cloned()
        .collect();
    if !names.iter().any(|n| n == "host") {
        names.push("host".into());
    }
    headers.remove(hyper::header::AUTHORIZATION);
    headers.remove("x-amz-security-token");
    let mut query = query.to_string();
    if signed.presigned {
        let credential = format!("{}/{scope}", creds.key_id);
        let mut pairs: Vec<(String, String)> = query_pairs(&query)
            .into_iter()
            .filter(|(k, _)| {
                !matches!(
                    k.as_str(),
                    "X-Amz-Signature"
                        | "X-Amz-Credential"
                        | "X-Amz-Date"
                        | "X-Amz-Security-Token"
                        | "X-Amz-SignedHeaders"
                )
            })
            .collect();
        names.sort();
        names.dedup();
        pairs.push(("X-Amz-Credential".into(), credential));
        pairs.push(("X-Amz-Date".into(), stamp.clone()));
        pairs.push(("X-Amz-SignedHeaders".into(), names.join(";")));
        if let Some(t) = &creds.token {
            pairs.push(("X-Amz-Security-Token".into(), t.clone()));
        }
        query = pairs
            .iter()
            .map(|(k, v)| format!("{}={}", encode(k, false), encode(v, false)))
            .collect::<Vec<_>>()
            .join("&");
    } else {
        headers.insert(
            HeaderName::from_static("x-amz-date"),
            HeaderValue::from_str(&stamp).expect("a date is a header value"),
        );
        names.push("x-amz-date".into());
        if let Some(t) = &creds.token {
            headers.insert(
                HeaderName::from_static("x-amz-security-token"),
                HeaderValue::from_str(t).map_err(|_| "the session token isn't a header value")?,
            );
            names.push("x-amz-security-token".into());
        }
        names.sort();
        names.dedup();
    }
    let signed_headers = names.join(";");
    let canonical = format!(
        "{method}\n{}\n{}\n{}\n{signed_headers}\n{payload}",
        canonical_uri(path, &signed.service),
        canonical_query(&query),
        canonical_headers(headers, &names),
    );
    let signer = Signer {
        key: signing_key(&creds.secret, date, &signed.region, &signed.service),
        date: stamp.clone(),
        scope: scope.clone(),
    };
    let signature = signer.sign(&format!(
        "{ALGORITHM}\n{stamp}\n{scope}\n{}",
        sha256_hex(canonical.as_bytes())
    ));
    let out_path = if signed.presigned {
        format!("{path}?{query}&X-Amz-Signature={signature}")
    } else {
        headers.insert(
            hyper::header::AUTHORIZATION,
            HeaderValue::from_str(&format!(
                "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
                creds.key_id
            ))
            .map_err(|_| "the key id isn't a header value")?,
        );
        if query.is_empty() {
            path.to_string()
        } else {
            format!("{path}?{query}")
        }
    };
    Ok((out_path, signature, signer))
}

/// A body in aws-chunked encoding, its chunk signatures (and the trailer's)
/// redone as it streams, each chained from the one before and the request's
/// own. Lengths don't change, so `Content-Length` stays right.
pub struct Chunks {
    inner: Body,
    signer: Arc<Signer>,
    prev: String,
    buf: Vec<u8>,
    state: State,
    trailer: bool,
    ended: bool,
    failed: bool,
}

#[derive(Debug, PartialEq)]
enum State {
    /// Waiting for a chunk's `size;chunk-signature=...\r\n` line.
    Header,
    /// A chunk of this size, then `\r\n`, after the header line that was
    /// read (kept to rewrite).
    Data(usize, Vec<u8>),
    /// After the last chunk: trailer lines (with a trailer) up to the blank
    /// line that ends the body.
    Trailer(Vec<Vec<u8>>),
    Done,
}

/// No chunk is buffered past this: S3's are 64 KiB to a few MiB.
const MAX_CHUNK: usize = 64 << 20;

impl Chunks {
    pub fn wrap(inner: Body, signer: Signer, seed: String, trailer: bool) -> Body {
        use http_body_util::BodyExt as _;
        Chunks {
            inner,
            signer: Arc::new(signer),
            prev: seed,
            buf: Vec::new(),
            state: State::Header,
            trailer,
            ended: false,
            failed: false,
        }
        .boxed()
    }

    /// Whatever complete pieces `buf` holds, rewritten; `None` when more is
    /// needed. An error is a body that isn't aws-chunked.
    fn step(&mut self) -> Result<Option<Vec<u8>>, String> {
        let line_end = |b: &[u8]| b.windows(2).position(|w| w == b"\r\n");
        loop {
            match &mut self.state {
                State::Header => {
                    let Some(end) = line_end(&self.buf) else {
                        if self.buf.len() > 4096 {
                            return Err("an aws-chunked header line is too long".into());
                        }
                        return Ok(None);
                    };
                    let line: Vec<u8> = self.buf.drain(..end + 2).collect();
                    let text = String::from_utf8_lossy(&line[..end]).into_owned();
                    let size = text.split(';').next().unwrap_or("");
                    let size = usize::from_str_radix(size, 16)
                        .map_err(|_| format!("an aws-chunked chunk has a bad size {size:?}"))?;
                    if size > MAX_CHUNK {
                        return Err("an aws-chunked chunk is too large".into());
                    }
                    self.state = State::Data(size, line);
                }
                State::Data(size, line) => {
                    let size = *size;
                    // The last chunk of a body with a trailer has no CRLF
                    // of its own: the trailer's lines follow.
                    let last_before_trailer = size == 0 && self.trailer;
                    let need = if last_before_trailer { 0 } else { size + 2 };
                    if self.buf.len() < need {
                        return Ok(None);
                    }
                    let data: Vec<u8> = self.buf.drain(..need).collect();
                    if !last_before_trailer && &data[size..] != b"\r\n" {
                        return Err("an aws-chunked chunk isn't followed by CRLF".into());
                    }
                    let payload = if last_before_trailer {
                        &[][..]
                    } else {
                        &data[..size]
                    };
                    let sig = self.signer.sign(&format!(
                        "AWS4-HMAC-SHA256-PAYLOAD\n{}\n{}\n{}\n{EMPTY_SHA256}\n{}",
                        self.signer.date,
                        self.signer.scope,
                        self.prev,
                        sha256_hex(payload)
                    ));
                    let mut out = rewrite_signature(line, b"chunk-signature=", &sig)?;
                    out.extend_from_slice(&data);
                    self.prev = sig;
                    self.state = match (size, last_before_trailer) {
                        (_, true) => State::Trailer(Vec::new()),
                        (0, false) => State::Done,
                        _ => State::Header,
                    };
                    return Ok(Some(out));
                }
                State::Trailer(lines) => {
                    let Some(end) = line_end(&self.buf) else {
                        return Ok(None);
                    };
                    let line: Vec<u8> = self.buf.drain(..end + 2).collect();
                    if end == 0 && !lines.is_empty() {
                        // The blank line: sign what came before it.
                        let mut canonical = String::new();
                        let mut out = Vec::new();
                        let mut sig_line = None;
                        for l in lines.iter() {
                            let text = String::from_utf8_lossy(&l[..l.len() - 2]).into_owned();
                            if text.starts_with("x-amz-trailer-signature:") {
                                sig_line = Some(l.clone());
                            } else if !text.is_empty() {
                                canonical.push_str(&text);
                                canonical.push('\n');
                                out.extend_from_slice(l);
                            } else {
                                out.extend_from_slice(l);
                            }
                        }
                        let sig = self.signer.sign(&format!(
                            "AWS4-HMAC-SHA256-TRAILER\n{}\n{}\n{}\n{}",
                            self.signer.date,
                            self.signer.scope,
                            self.prev,
                            sha256_hex(canonical.as_bytes())
                        ));
                        let sig_line = sig_line.ok_or("the signed trailer has no signature")?;
                        out.extend(rewrite_signature(
                            &sig_line,
                            b"x-amz-trailer-signature:",
                            &sig,
                        )?);
                        out.extend_from_slice(&line);
                        self.state = State::Done;
                        return Ok(Some(out));
                    }
                    lines.push(line);
                    if lines.len() > 64 {
                        return Err("too many trailer lines".into());
                    }
                }
                State::Done => {
                    if !self.buf.is_empty() {
                        return Err("bytes after the end of an aws-chunked body".into());
                    }
                    return Ok(None);
                }
            }
        }
    }
}

/// `line` with the 64 hex digits after `marker` replaced by `sig`.
fn rewrite_signature(line: &[u8], marker: &[u8], sig: &str) -> Result<Vec<u8>, String> {
    let at = line
        .windows(marker.len())
        .position(|w| w == marker)
        .ok_or("an aws-chunked line has no signature")?
        + marker.len();
    if line.len() < at + 64 {
        return Err("an aws-chunked signature is short".into());
    }
    let mut out = line.to_vec();
    out[at..at + 64].copy_from_slice(sig.as_bytes());
    Ok(out)
}

impl HttpBody for Chunks {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let this = self.get_mut();
        loop {
            if this.failed {
                return Poll::Ready(None);
            }
            match this.step() {
                Ok(Some(out)) => return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out))))),
                Ok(None) => {}
                Err(e) => {
                    // A body that isn't what it claimed: stop sending it, so
                    // the upstream sees a short body and refuses it.
                    tracing::warn!("aws-chunked body: {e}");
                    this.failed = true;
                    return Poll::Ready(None);
                }
            }
            if this.ended {
                if this.state != State::Done || !this.buf.is_empty() {
                    tracing::warn!("aws-chunked body ended early");
                }
                return Poll::Ready(None);
            }
            match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        this.buf.extend_from_slice(&data);
                    }
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => this.ended = true,
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.failed || (self.ended && self.state == State::Done && self.buf.is_empty())
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// What became of a request: left as it was (not signed with a fake key),
/// or signed again.
pub(super) enum Outcome {
    Unsigned(Body),
    Resigned(Resigned),
}

/// A request signed again with an `aws` secret's keys.
pub struct Resigned {
    pub body: Body,
    pub path: String,
    /// The secret's name, for the record.
    pub secret: String,
    /// What to mask in the response.
    pub mask: Vec<String>,
}

/// A body is hashed in memory, up to this, when the guest didn't say its
/// hash (`x-amz-content-sha256`): AWS's JSON and query APIs.
const MAX_HASHED: usize = 32 << 20;

/// Signs an inspected request again if the guest signed it with one of
/// zapper's fake keys: the secret it names must be listed by the rule that
/// allowed the request, and may be sent to `host`. A request not signed
/// with a fake key is left as it is. An error refuses the request.
#[allow(clippy::too_many_arguments)]
pub(super) async fn resign(
    d: &super::Daemon,
    s: &super::Session,
    host: &str,
    plaintext: bool,
    listed: &[String],
    parts: &mut hyper::http::request::Parts,
    body: Body,
    path: &str,
) -> Result<Outcome, String> {
    use http_body_util::BodyExt as _;
    let query = path.split_once('?').map_or("", |(_, q)| q);
    let Some(signed) = signed(&parts.headers, query) else {
        return Ok(Outcome::Unsigned(body));
    };
    if !zapper_policy::is_aws_fake_key_id(&signed.key_id) {
        return Ok(Outcome::Unsigned(body));
    }
    let own = s.secrets.read().unwrap().clone();
    let found = {
        let config = d.config.read().unwrap();
        listed.iter().find_map(|name| {
            let secret = crate::daemon::secrets::find(&config, &own, name)?;
            (secret.mode == zapper_policy::SecretMode::Aws
                && zapper_policy::aws_key_id(name) == signed.key_id)
                .then(|| (name.clone(), secret.clone()))
        })
    };
    let Some((name, secret)) = found else {
        return Err(format!(
            "signed with zapper's AWS key {}, but no rule allowing this request lists its secret",
            signed.key_id
        ));
    };
    crate::daemon::secrets::check(
        &name,
        &secret,
        crate::daemon::secrets::Use::Sign,
        host,
        !plaintext,
    )?;
    let creds = match crate::mint::secret_value(&secret.source).await {
        Ok(v) => match Creds::parse(&v) {
            Ok(c) => c,
            Err(e) => return Err(format!("secret {name}: {e}")),
        },
        Err(e) => return Err(format!("secret {name}: {e:#}")),
    };
    let claimed = parts
        .headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (payload, body, chunked) = match claimed.as_deref() {
        _ if signed.presigned => ("UNSIGNED-PAYLOAD".to_string(), body, None),
        Some(h) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => {
            (h.to_string(), body, None)
        }
        Some(h @ ("UNSIGNED-PAYLOAD" | "STREAMING-UNSIGNED-PAYLOAD-TRAILER")) => {
            (h.to_string(), body, None)
        }
        Some(h @ STREAMING) => (h.to_string(), body, Some(false)),
        Some(h @ STREAMING_TRAILER) => (h.to_string(), body, Some(true)),
        Some(h) => {
            return Err(format!(
                "the payload signing {h:?} isn't one the proxy can sign"
            ))
        }
        None => {
            let limited = http_body_util::Limited::new(body, MAX_HASHED);
            let bytes = match limited.collect().await {
                Ok(c) => c.to_bytes(),
                Err(_) => {
                    return Err(format!(
                        "a body over {} MiB without x-amz-content-sha256 can't be signed",
                        MAX_HASHED >> 20
                    ))
                }
            };
            let hash = sha256_hex(&bytes);
            let full = http_body_util::Full::new(bytes)
                .map_err(|e| match e {})
                .boxed();
            (hash, full, None)
        }
    };
    // The response may carry the token back: it must be readable to mask.
    parts.headers.insert(
        hyper::header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    let now = time::OffsetDateTime::now_utc();
    let (out_path, seed, signer) = match sign(
        parts.method.as_str(),
        path,
        &mut parts.headers,
        &signed,
        &creds,
        &payload,
        now,
    ) {
        Ok(x) => x,
        Err(e) => return Err(e),
    };
    let body = match chunked {
        Some(trailer) => Chunks::wrap(body, signer, seed, trailer),
        None => body,
    };
    let mut mask = vec![creds.secret.clone()];
    mask.extend(creds.token.clone());
    Ok(Outcome::Resigned(Resigned {
        body,
        path: out_path,
        secret: name,
        mask,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The examples' keys (AWS's documentation, and botocore for the rest).
    fn creds(token: Option<&str>) -> Creds {
        Creds {
            key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            token: token.map(str::to_string),
        }
    }

    fn at() -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(1_369_353_600).unwrap() // 2013-05-24
    }

    fn headers(h: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in h {
            m.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        m
    }

    /// The guest's signature over `list`, as from a fake key.
    fn guest(region: &str, service: &str, list: &str) -> Signed {
        Signed {
            key_id: zapper_policy::aws_key_id("aws"),
            region: region.into(),
            service: service.into(),
            headers: list.split(';').map(str::to_string).collect(),
            presigned: false,
        }
    }

    fn signature(h: &HeaderMap) -> String {
        let a = h["authorization"].to_str().unwrap();
        a.rsplit("Signature=").next().unwrap().to_string()
    }

    #[test]
    fn the_s3_get_example_signs_as_aws_documents_it() {
        let mut h = headers(&[
            ("host", "examplebucket.s3.amazonaws.com"),
            ("range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("authorization", "AWS4-HMAC-SHA256 Credential=ZAPPER/20991231/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, Signature=00"),
            ("x-amz-date", "20991231T000000Z"),
        ]);
        let s = signed(&h, "").unwrap();
        assert_eq!((s.region.as_str(), s.service.as_str()), ("us-east-1", "s3"));
        let (path, sig, _) = sign(
            "GET",
            "/test.txt",
            &mut h,
            &s,
            &creds(None),
            EMPTY_SHA256,
            at(),
        )
        .unwrap();
        assert_eq!(path, "/test.txt");
        assert_eq!(
            sig,
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        assert_eq!(signature(&h), sig);
        assert_eq!(h["x-amz-date"], "20130524T000000Z", "the host's clock");
        assert!(h["authorization"]
            .to_str()
            .unwrap()
            .contains("Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request"));
    }

    #[test]
    fn s3_with_a_token_and_an_awkward_key_and_query_matches_botocore() {
        let mut h = headers(&[
            ("host", "bucket.s3.eu-west-2.amazonaws.com"),
            ("content-type", "text/plain"),
            ("x-amz-meta-note", "  two   spaces  "),
            (
                "x-amz-content-sha256",
                "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            ),
            ("x-amz-security-token", "the guest's own, dropped"),
        ]);
        let s = guest(
            "eu-west-2",
            "s3",
            "content-type;host;x-amz-content-sha256;x-amz-date;x-amz-meta-note",
        );
        let (_, sig, _) = sign(
            "PUT",
            "/dir/my%20key%2B%281%29.txt?tagging=&x-id=PutObject&prefix=a%20b",
            &mut h,
            &s,
            &creds(Some("the//session+token=")),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            at(),
        )
        .unwrap();
        assert_eq!(
            sig,
            "10ccaf8b6c071de62fa6c544376d8f83381bd9a0be2fcfff170331778c27c24f"
        );
        assert_eq!(h["x-amz-security-token"], "the//session+token=");
    }

    #[test]
    fn a_json_api_with_its_body_hashed_matches_botocore() {
        let body = br#"{"Limit": 5}"#;
        let mut h = headers(&[
            ("host", "dynamodb.us-west-2.amazonaws.com"),
            ("content-type", "application/x-amz-json-1.0"),
            ("x-amz-target", "DynamoDB_20120810.ListTables"),
        ]);
        let s = guest(
            "us-west-2",
            "dynamodb",
            "content-type;host;x-amz-date;x-amz-target",
        );
        let (_, sig, _) = sign(
            "POST",
            "/",
            &mut h,
            &s,
            &creds(Some("the//session+token=")),
            &sha256_hex(body),
            at(),
        )
        .unwrap();
        assert_eq!(
            sig,
            "7b3421e5817a241a25a092037988de55303399f84bf4d4699c91495a1c97c416"
        );
    }

    #[test]
    fn a_path_outside_s3_is_encoded_twice_as_botocore_does() {
        let mut h = headers(&[("host", "abc.execute-api.us-east-1.amazonaws.com")]);
        let s = guest("us-east-1", "execute-api", "host;x-amz-date");
        let (path, sig, _) = sign(
            "GET",
            "/prod/items/a%20b/caf%C3%A9?b=2&a=1&a=0",
            &mut h,
            &s,
            &creds(None),
            EMPTY_SHA256,
            at(),
        )
        .unwrap();
        assert_eq!(
            sig,
            "75f7d2b9f6b2c803e5dd2dee35044dee6f3fedf63b003653b69ede1432999b3b"
        );
        assert_eq!(
            path, "/prod/items/a%20b/caf%C3%A9?b=2&a=1&a=0",
            "forwarded as sent"
        );
    }

    #[test]
    fn a_presigned_url_is_signed_again_with_the_token_in_its_query() {
        let fake = zapper_policy::aws_key_id("aws");
        let query = format!("X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={fake}%2F20991231%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20991231T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host&X-Amz-Signature=00");
        let mut h = headers(&[("host", "examplebucket.s3.amazonaws.com")]);
        let s = signed(&h, &query).unwrap();
        assert!(s.presigned && s.key_id == fake);
        let (path, sig, _) = sign(
            "GET",
            &format!("/test.txt?{query}"),
            &mut h,
            &s,
            &creds(Some("the//session+token=")),
            "UNSIGNED-PAYLOAD",
            at(),
        )
        .unwrap();
        assert_eq!(
            sig,
            "1301a1ee79cd9cf73ba446e4df255a9a9e0d3cf372bdf1ccd597e4954e51cf85"
        );
        assert!(
            path.contains("X-Amz-Security-Token=the%2F%2Fsession%2Btoken%3D"),
            "{path}"
        );
        assert!(path.ends_with(&format!("&X-Amz-Signature={sig}")), "{path}");
        assert!(!path.contains(&fake), "{path}");
        assert!(h.get("authorization").is_none());
    }

    /// AWS's documented chunked upload: the seed signature, then each
    /// chunk's, chained.
    #[tokio::test]
    async fn chunks_are_signed_as_aws_documents_them() {
        use http_body_util::{BodyExt, StreamBody};
        let mut h = headers(&[
            ("host", "s3.amazonaws.com"),
            ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
            ("x-amz-content-sha256", STREAMING),
            ("content-encoding", "aws-chunked"),
            ("x-amz-decoded-content-length", "66560"),
            ("content-length", "66824"),
        ]);
        let s = guest(
            "us-east-1",
            "s3",
            "content-encoding;content-length;host;x-amz-content-sha256;x-amz-date;x-amz-decoded-content-length;x-amz-storage-class",
        );
        let (_, seed, signer) = sign(
            "PUT",
            "/examplebucket/chunkObject.txt",
            &mut h,
            &s,
            &creds(None),
            STREAMING,
            at(),
        )
        .unwrap();
        assert_eq!(
            seed,
            "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9"
        );
        // The guest's body, signed with its fake key; split awkwardly.
        let zeros = "0".repeat(64);
        let mut body = Vec::new();
        for n in [65536usize, 1024, 0] {
            body.extend_from_slice(format!("{n:x};chunk-signature={zeros}\r\n").as_bytes());
            body.extend(std::iter::repeat_n(b'a', n));
            body.extend_from_slice(b"\r\n");
        }
        assert_eq!(body.len(), 66824);
        let frames: Vec<Result<Frame<Bytes>, hyper::Error>> = body
            .chunks(7777)
            .map(|c| Ok(Frame::data(Bytes::copy_from_slice(c))))
            .collect();
        let inner: Body = StreamBody::new(futures_util::stream::iter(frames)).boxed();
        let out = Chunks::wrap(inner, signer, seed, false)
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(out.len(), 66824, "the same length");
        let text = String::from_utf8_lossy(&out);
        for sig in [
            "ad80c730a21e5b8d04586a2213dd63b9a0e99e0e2307b0ade35a65485a288648",
            "0055627c9e194cb4542bae2aa5492e3c1575bbb81b612b7d234b86a503ef5497",
            "b6c6ea8a5354eaf15b3cb7646744f4275b71ea724fed81ceb9323e279d449df9",
        ] {
            assert!(
                text.contains(&format!(";chunk-signature={sig}\r\n")),
                "{sig}"
            );
        }
        assert!(out.ends_with(b"\r\n\r\n"));
    }

    #[test]
    fn creds_are_credential_process_json() {
        let c =
            Creds::parse(r#"{"AccessKeyId":"ASIA1","SecretAccessKey":"sk","SessionToken":"t"}"#)
                .unwrap();
        assert_eq!(
            (c.key_id.as_str(), c.token.as_deref()),
            ("ASIA1", Some("t"))
        );
        assert!(Creds::parse("plain").is_err());
        assert!(Creds::parse(r#"{"AccessKeyId":"A"}"#).is_err());
        assert!(!format!("{c:?}").contains("sk"));
    }
}
