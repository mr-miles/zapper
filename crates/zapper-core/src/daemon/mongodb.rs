//! Minimal MongoDB wire bits for the MongoDB auth-relay (proposal 07): the
//! proxy authenticates the server leg with SCRAM-SHA-256 carried over `OP_MSG`
//! commands (`saslStart`/`saslContinue`), reusing [`super::pgwire::scram`], then
//! the relay splices. The guest holds no password and never authenticates; its
//! `hello` and commands run on the pre-authenticated server connection.
//!
//! Only the handful of BSON types the SASL commands use are modelled, and all
//! integers are little-endian (MongoDB wire + BSON).

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use super::pgwire::scram;

const OP_MSG: i32 = 2013;
const MAX_MSG: usize = 48 * 1024 * 1024; // MongoDB's default max message size.

fn i32le(b: &[u8]) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(0..4)?.try_into().ok()?))
}

/// A BSON document being built element by element.
struct Doc(Vec<u8>);

impl Doc {
    fn new() -> Self {
        Doc(Vec::new())
    }
    fn name(&mut self, ty: u8, name: &str) {
        self.0.push(ty);
        self.0.extend_from_slice(name.as_bytes());
        self.0.push(0);
    }
    fn int32(mut self, name: &str, v: i32) -> Self {
        self.name(0x10, name);
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn string(mut self, name: &str, v: &str) -> Self {
        self.name(0x02, name);
        self.0
            .extend_from_slice(&(v.len() as i32 + 1).to_le_bytes());
        self.0.extend_from_slice(v.as_bytes());
        self.0.push(0);
        self
    }
    fn binary(mut self, name: &str, v: &[u8]) -> Self {
        self.name(0x05, name);
        self.0.extend_from_slice(&(v.len() as i32).to_le_bytes());
        self.0.push(0x00); // generic binary subtype
        self.0.extend_from_slice(v);
        self
    }
    /// Closes the document: `Int32` total length, the elements, a `0x00` end.
    fn finish(self) -> Vec<u8> {
        let total = 4 + self.0.len() + 1;
        let mut out = (total as i32).to_le_bytes().to_vec();
        out.extend_from_slice(&self.0);
        out.push(0);
        out
    }
}

/// A non-negative BSON length prefix (`Int32`) at the front of `b`. A negative
/// value (which `as usize` would turn into a huge number) is rejected.
fn blen(b: &[u8]) -> Option<usize> {
    usize::try_from(i32le(b)?).ok()
}

/// The byte length of a BSON value of `ty` at the front of `b`, for skipping.
/// Length arithmetic is checked, so a hostile prefix yields `None`, never an
/// overflow.
fn value_len(ty: u8, b: &[u8]) -> Option<usize> {
    Some(match ty {
        0x01 | 0x09 | 0x11 | 0x12 => 8, // double / datetime / timestamp / int64
        0x02 | 0x0D | 0x0E => blen(b)?.checked_add(4)?, // string / code / symbol
        0x03 | 0x04 => blen(b)?,        // embedded document / array (self-sized)
        0x05 => blen(b)?.checked_add(5)?, // binary: len + subtype + bytes
        0x07 => 12,                     // ObjectId
        0x08 => 1,                      // bool
        0x0A => 0,                      // null
        0x10 => 4,                      // int32
        _ => return None,
    })
}

/// Walks a BSON document into `(type, name, value)` triples. `None` if it is
/// malformed or uses a type we do not size.
pub(crate) fn walk(doc: &[u8]) -> Option<Vec<(u8, String, Vec<u8>)>> {
    let total = i32le(doc)? as usize;
    if total > doc.len() || total < 5 {
        return None;
    }
    let mut i = 4;
    let mut out = Vec::new();
    loop {
        let ty = *doc.get(i)?;
        if ty == 0 {
            return Some(out);
        }
        i += 1;
        let nend = i + doc.get(i..)?.iter().position(|&x| x == 0)?;
        let name = std::str::from_utf8(doc.get(i..nend)?).ok()?.to_string();
        i = nend + 1;
        let vlen = value_len(ty, doc.get(i..)?)?;
        out.push((ty, name, doc.get(i..i + vlen)?.to_vec()));
        i += vlen;
    }
}

pub(crate) struct Reply {
    fields: Vec<(u8, String, Vec<u8>)>,
}

impl Reply {
    fn binary(&self, name: &str) -> Option<Vec<u8>> {
        let (_, _, v) = self
            .fields
            .iter()
            .find(|(t, n, _)| *t == 0x05 && n == name)?;
        let end = blen(v)?.checked_add(5)?;
        v.get(5..end).map(<[u8]>::to_vec)
    }
    fn int32(&self, name: &str) -> Option<i32> {
        let (_, _, v) = self
            .fields
            .iter()
            .find(|(t, n, _)| *t == 0x10 && n == name)?;
        i32le(v)
    }
    fn bool(&self, name: &str) -> Option<bool> {
        let (_, _, v) = self
            .fields
            .iter()
            .find(|(t, n, _)| *t == 0x08 && n == name)?;
        Some(v.first()? != &0)
    }
    /// Whether the command reported `ok: 1` (a double `1.0` or an int `1`).
    fn is_ok(&self) -> bool {
        for (t, n, v) in &self.fields {
            if n != "ok" {
                continue;
            }
            return match t {
                0x01 => {
                    v.get(0..8)
                        .map(|b| f64::from_le_bytes(b.try_into().unwrap()))
                        == Some(1.0)
                }
                0x10 => i32le(v) == Some(1),
                _ => false,
            };
        }
        false
    }
    fn errmsg(&self) -> String {
        for (t, n, v) in &self.fields {
            if *t == 0x02 && n == "errmsg" {
                // String value: Int32 length (incl. trailing NUL), then bytes.
                if let Some(end) = blen(v).map(|len| len.saturating_sub(1) + 4) {
                    if let Some(s) = v.get(4..end) {
                        return String::from_utf8_lossy(s).into_owned();
                    }
                }
            }
        }
        "authentication failed".into()
    }
}

/// Builds an `OP_MSG` carrying one command document (section kind 0).
fn op_msg(request_id: i32, doc: &[u8]) -> Vec<u8> {
    let total = 16 + 4 + 1 + doc.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as i32).to_le_bytes()); // messageLength
    out.extend_from_slice(&request_id.to_le_bytes()); // requestID
    out.extend_from_slice(&0i32.to_le_bytes()); // responseTo
    out.extend_from_slice(&OP_MSG.to_le_bytes()); // opCode
    out.extend_from_slice(&0u32.to_le_bytes()); // flagBits
    out.push(0x00); // section kind 0: body
    out.extend_from_slice(doc);
    out
}

/// Reads one `OP_MSG` and returns its body document bytes.
pub(crate) async fn read_reply<S: AsyncRead + Unpin>(
    server: &mut S,
    buf: &mut Vec<u8>,
) -> Result<Reply, String> {
    super::dbwire::read_framed(server, buf, MAX_MSG, |b| {
        if b.len() < 4 {
            return Ok(None);
        }
        let total = i32le(b).unwrap() as usize;
        if !(16..=MAX_MSG).contains(&total) {
            return Err("implausible MongoDB message length".into());
        }
        if b.len() < total {
            return Ok(None);
        }
        if i32le(&b[12..16]) != Some(OP_MSG) {
            return Err("server sent a non-OP_MSG reply".into());
        }
        // flagBits at [16..20], section kind at [20], body doc at [21..].
        let doc = b.get(21..total).ok_or("truncated OP_MSG body")?;
        let fields = walk(doc).ok_or("malformed BSON in reply")?;
        Ok(Some((Reply { fields }, total)))
    })
    .await
}

/// Authenticates the server leg over SCRAM-SHA-256 against `auth_db`, with a
/// fresh client `nonce`. Fails closed on a non-ok reply, a SCRAM mismatch, or a
/// server that drags the empty-exchange out.
pub async fn authenticate<S: AsyncRead + AsyncWrite + Unpin>(
    server: &mut S,
    user: &str,
    password: &str,
    auth_db: &str,
    nonce: &str,
) -> Result<(), String> {
    let client = scram::Client::new(user, password, nonce);
    let start = Doc::new()
        .int32("saslStart", 1)
        .string("mechanism", "SCRAM-SHA-256")
        .binary("payload", client.client_first().as_bytes())
        .string("$db", auth_db)
        .finish();
    server
        .write_all(&op_msg(1, &start))
        .await
        .map_err(|e| format!("sending saslStart: {e}"))?;

    let mut buf = Vec::new();
    let reply = read_reply(server, &mut buf).await?;
    if !reply.is_ok() {
        return Err(format!("saslStart rejected: {}", reply.errmsg()));
    }
    let conversation = reply
        .int32("conversationId")
        .ok_or("saslStart reply has no conversationId")?;
    let server_first = reply
        .binary("payload")
        .ok_or("saslStart reply has no payload")?;
    let fin = client.client_final(&server_first)?;

    let cont = Doc::new()
        .int32("saslContinue", 1)
        .int32("conversationId", conversation)
        .binary("payload", fin.message.as_bytes())
        .string("$db", auth_db)
        .finish();
    server
        .write_all(&op_msg(2, &cont))
        .await
        .map_err(|e| format!("sending saslContinue: {e}"))?;
    let reply = read_reply(server, &mut buf).await?;
    if !reply.is_ok() {
        return Err(format!("saslContinue rejected: {}", reply.errmsg()));
    }
    let server_final = reply
        .binary("payload")
        .ok_or("saslContinue reply has no payload")?;
    scram::Client::verify_server_final(&fin, &server_final)?;

    // Some servers require an extra empty saslContinue before `done: true`.
    let mut done = reply.bool("done").unwrap_or(false);
    let mut request_id = 3;
    while !done {
        if request_id > 6 {
            return Err("server did not finish the SASL exchange".into());
        }
        let empty = Doc::new()
            .int32("saslContinue", 1)
            .int32("conversationId", conversation)
            .binary("payload", b"")
            .string("$db", auth_db)
            .finish();
        server
            .write_all(&op_msg(request_id, &empty))
            .await
            .map_err(|e| format!("sending saslContinue: {e}"))?;
        request_id += 1;
        let reply = read_reply(server, &mut buf).await?;
        if !reply.is_ok() {
            return Err(format!("saslContinue rejected: {}", reply.errmsg()));
        }
        done = reply.bool("done").unwrap_or(true);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bson_builds_and_walks() {
        let doc = Doc::new()
            .int32("saslStart", 1)
            .string("mechanism", "SCRAM-SHA-256")
            .binary("payload", b"n,,n=u,r=abc")
            .finish();
        // Total length prefix matches.
        assert_eq!(i32le(&doc).unwrap() as usize, doc.len());
        let fields = walk(&doc).unwrap();
        let reply = Reply { fields };
        assert_eq!(reply.int32("saslStart"), Some(1));
        assert_eq!(
            reply.binary("payload").as_deref(),
            Some(&b"n,,n=u,r=abc"[..])
        );
    }

    #[test]
    fn walk_rejects_malformed_lengths_without_panicking() {
        // A string element whose length prefix is negative must not overflow.
        let mut doc_body = vec![0x02]; // string element
        doc_body.extend_from_slice(b"k\0");
        doc_body.extend_from_slice(&(-1i32).to_le_bytes()); // bogus length
        let total = 4 + doc_body.len() + 1;
        let mut doc = (total as i32).to_le_bytes().to_vec();
        doc.extend_from_slice(&doc_body);
        doc.push(0);
        assert_eq!(walk(&doc), None);
        // A binary element with a huge length is rejected, not sliced OOB.
        let mut doc_body = vec![0x05];
        doc_body.extend_from_slice(b"k\0");
        doc_body.extend_from_slice(&i32::MAX.to_le_bytes());
        doc_body.push(0); // subtype
        let total = 4 + doc_body.len() + 1;
        let mut doc = (total as i32).to_le_bytes().to_vec();
        doc.extend_from_slice(&doc_body);
        doc.push(0);
        assert_eq!(walk(&doc), None);
    }

    #[tokio::test]
    async fn op_msg_round_trips() {
        let doc = Doc::new().int32("ok", 1).finish();
        let bytes = op_msg(7, &doc);
        let mut buf = Vec::new();
        let reply = read_reply(&mut &bytes[..], &mut buf).await.unwrap();
        assert!(reply.is_ok());
    }

    /// A fake Mongo server that completes SCRAM-SHA-256 for `password` over two
    /// OP_MSG round trips, setting `done: true`.
    async fn fake_mongo<S: AsyncRead + AsyncWrite + Unpin>(mut c: S, password: &str) {
        use super::super::dbwire::scram_test;
        let mut buf = Vec::new();
        // saslStart → read client-first from the payload.
        let start = read_reply(&mut c, &mut buf).await.unwrap();
        let client_first = String::from_utf8(start.binary("payload").unwrap()).unwrap();
        let server = scram_test::Server::new(password, &client_first);

        let reply1 = Doc::new()
            .int32("conversationId", 1)
            .binary("payload", server.server_first().as_bytes())
            .int32("ok", 1)
            .finish();
        c.write_all(&op_msg(100, &reply1)).await.unwrap();

        // saslContinue → verify the client proof.
        let cont = read_reply(&mut c, &mut buf).await.unwrap();
        let client_final = String::from_utf8(cont.binary("payload").unwrap()).unwrap();
        let server_final = server
            .server_final(&client_final)
            .expect("client proof verifies");

        // done:true via a real BSON bool (0x08).
        let mut body = Vec::new();
        body.push(0x10); // int32 conversationId
        body.extend_from_slice(b"conversationId\0");
        body.extend_from_slice(&1i32.to_le_bytes());
        body.push(0x05); // binary payload
        body.extend_from_slice(b"payload\0");
        body.extend_from_slice(&(server_final.len() as i32).to_le_bytes());
        body.push(0);
        body.extend_from_slice(server_final.as_bytes());
        body.push(0x08); // bool done = true
        body.extend_from_slice(b"done\0");
        body.push(1);
        body.push(0x10); // int32 ok = 1
        body.extend_from_slice(b"ok\0");
        body.extend_from_slice(&1i32.to_le_bytes());
        let total = 4 + body.len() + 1;
        let mut doc = (total as i32).to_le_bytes().to_vec();
        doc.extend_from_slice(&body);
        doc.push(0);
        c.write_all(&op_msg(101, &doc)).await.unwrap();
    }

    #[tokio::test]
    async fn authenticate_completes_scram_over_op_msg() {
        let (mut client, server) = tokio::io::duplex(8192);
        let srv = tokio::spawn(async move { fake_mongo(server, "pencil").await });
        super::authenticate(&mut client, "app", "pencil", "admin", "mongoclientnonce")
            .await
            .unwrap();
        srv.await.unwrap();
    }
}
