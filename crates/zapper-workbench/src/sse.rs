//! zapper's server-sent events, read a block at a time: to filter what a
//! client may see, and to hand facilities the events.

use bytes::Bytes;
use futures_util::stream::{self, Stream};
use http_body_util::BodyExt;
use serde_json::Value;

/// One block of the stream, up to its blank line.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    /// As it came, without the blank line.
    pub raw: String,
    /// Its `id:`, `EPOCH:SEQ` for an event.
    pub id: Option<String>,
    /// Its `data:` lines, joined.
    pub data: Option<String>,
}

impl Block {
    fn parse(raw: &str) -> Block {
        let (mut id, mut data): (Option<String>, Option<String>) = (None, None);
        for line in raw.lines() {
            if let Some(v) = line.strip_prefix("id:") {
                id = Some(v.trim_start().to_string());
            } else if let Some(v) = line.strip_prefix("data:") {
                let v = v.strip_prefix(' ').unwrap_or(v);
                data = Some(match data {
                    Some(d) => format!("{d}\n{v}"),
                    None => v.to_string(),
                });
            }
        }
        Block {
            raw: raw.to_string(),
            id,
            data,
        }
    }

    /// Its data as JSON: an event, or a `gap`.
    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(self.data.as_deref()?).ok()
    }

    /// As it goes on the wire again.
    pub fn bytes(&self) -> Bytes {
        Bytes::from(format!("{}\n\n", self.raw))
    }
}

/// Longest block kept: past it the stream is taken to be broken, and ends.
const MAX_BLOCK: usize = 1 << 20;

/// The blocks of `body`, in order, until it ends.
pub struct Blocks;

impl Blocks {
    #[allow(clippy::new_ret_no_self)]
    pub fn new<B>(body: B) -> impl Stream<Item = Block> + Send + Unpin
    where
        B: hyper::body::Body<Data = Bytes> + Send + Unpin + 'static,
        B::Error: Send,
    {
        Box::pin(stream::unfold(
            (body, String::new()),
            |(mut body, mut buf)| async move {
                loop {
                    if let Some(at) = buf.find("\n\n") {
                        let raw: String = buf.drain(..at + 2).collect();
                        let raw = raw.trim_end_matches('\n').to_string();
                        return Some((Block::parse(&raw), (body, buf)));
                    }
                    if buf.len() > MAX_BLOCK {
                        return None;
                    }
                    match body.frame().await {
                        Some(Ok(frame)) => {
                            if let Ok(data) = frame.into_data() {
                                buf.push_str(&String::from_utf8_lossy(&data).replace("\r\n", "\n"));
                            }
                        }
                        _ => return None,
                    }
                }
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use http_body_util::StreamBody;

    fn body(
        chunks: &[&'static str],
    ) -> impl hyper::body::Body<Data = Bytes, Error = std::convert::Infallible> + Unpin {
        StreamBody::new(stream::iter(
            chunks
                .iter()
                .map(|c| Ok(hyper::body::Frame::data(Bytes::from_static(c.as_bytes()))))
                .collect::<Vec<_>>(),
        ))
    }

    #[tokio::test]
    async fn blocks_are_read_across_chunks_with_their_id_and_data() {
        let b: Vec<Block> = Blocks::new(body(&[
            "id: e1:3\ndata: {\"seq\":3,",
            "\"session\":\"abc\"}\n\n: keep-alive\n\n",
            "data: {\"type\":\"gap\"}\n\nid: e1:4\ndata: {\"seq\"",
        ]))
        .collect()
        .await;
        assert_eq!(b.len(), 3, "the unfinished last one is dropped: {b:?}");
        assert_eq!(b[0].id.as_deref(), Some("e1:3"));
        assert_eq!(b[0].json().unwrap()["session"], "abc");
        assert_eq!((b[1].id.as_ref(), b[1].data.as_ref()), (None, None));
        assert_eq!(b[1].raw, ": keep-alive");
        assert_eq!(b[2].json().unwrap()["type"], "gap");
        assert_eq!(
            b[0].bytes(),
            "id: e1:3\ndata: {\"seq\":3,\"session\":\"abc\"}\n\n"
        );
    }
}
