//! Minimal TDS (MS SQL Server) bits for the SQL Server auth-relay (proposal
//! 07), SQL authentication only (no Kerberos/NTLM/integrated security).
//!
//! The guest connects with no password. The proxy answers its PRELOGIN with
//! `ENCRYPT_NOT_SUP` (steering it to a plaintext login leg), reads and ignores
//! its LOGIN7, and separately logs in to the real server with the host-held
//! credential — performing the TLS-in-PRELOGIN handshake when the server wants
//! encryption — then forwards the server's login response to the guest and the
//! relay splices.
//!
//! TDS packet headers are big-endian; LOGIN7's own fields are little-endian.
//! The TLS-in-TDS handshake path is built to the MS-TDS spec but, unlike the
//! plaintext path, is not yet exercised by a test against a live server.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

pub const PKT_LOGIN7: u8 = 0x10;
pub const PKT_RESPONSE: u8 = 0x04;
pub const PKT_PRELOGIN: u8 = 0x12;
const STATUS_EOM: u8 = 0x01;

pub const ENCRYPT_OFF: u8 = 0x00;
pub const ENCRYPT_ON: u8 = 0x01;
pub const ENCRYPT_NOT_SUP: u8 = 0x02;
pub const ENCRYPT_REQ: u8 = 0x03;

const HEADER: usize = 8;
const MAX_PACKET: usize = 0xffff;

/// Frames a payload as one (final) TDS packet of `typ`.
pub fn frame(typ: u8, payload: &[u8]) -> Vec<u8> {
    let len = (HEADER + payload.len()) as u16;
    let mut out = vec![typ, STATUS_EOM];
    out.extend_from_slice(&len.to_be_bytes()); // length (big-endian)
    out.extend_from_slice(&[0, 0, 1, 0]); // SPID, PacketID, Window
    out.extend_from_slice(payload);
    out
}

/// Reads one TDS packet, returning `(type, status, payload)`.
pub async fn read_packet<S: AsyncRead + Unpin>(
    s: &mut S,
    buf: &mut Vec<u8>,
) -> Result<(u8, u8, Vec<u8>), String> {
    let mut chunk = [0u8; 4096];
    loop {
        if buf.len() >= HEADER {
            let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
            if !(HEADER..=MAX_PACKET).contains(&len) {
                return Err("implausible TDS packet length".into());
            }
            if buf.len() >= len {
                let (typ, status) = (buf[0], buf[1]);
                let payload = buf[HEADER..len].to_vec();
                buf.drain(..len);
                return Ok((typ, status, payload));
            }
        }
        let n = s
            .read(&mut chunk)
            .await
            .map_err(|e| format!("reading TDS packet: {e}"))?;
        if n == 0 {
            return Err("connection closed during the TDS handshake".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Reads a whole TDS message (concatenating packets until the EOM status bit),
/// returning `(type, payload)`.
pub async fn read_message<S: AsyncRead + Unpin>(
    s: &mut S,
    buf: &mut Vec<u8>,
) -> Result<(u8, Vec<u8>), String> {
    let mut out = Vec::new();
    loop {
        let (typ, status, payload) = read_packet(s, buf).await?;
        out.extend_from_slice(&payload);
        if status & STATUS_EOM != 0 {
            return Ok((typ, out));
        }
        if out.len() > 4 * 1024 * 1024 {
            return Err("TDS message too large".into());
        }
    }
}

/// A PRELOGIN payload advertising just the VERSION and ENCRYPTION options.
pub fn prelogin(encryption: u8) -> Vec<u8> {
    // Option table: entries (token, offset u16 BE, length u16 BE), then 0xFF,
    // then the option data.
    const VERSION: u8 = 0x00;
    const ENCRYPTION: u8 = 0x01;
    let n_entries = 2;
    let table = n_entries * 5 + 1; // +1 for the terminator
    let ver = [0u8, 0, 0, 0, 0, 0]; // 6-byte version (UL_VERSION + US_SUBBUILD)
    let mut out = Vec::new();
    // VERSION entry
    out.push(VERSION);
    out.extend_from_slice(&(table as u16).to_be_bytes());
    out.extend_from_slice(&(ver.len() as u16).to_be_bytes());
    // ENCRYPTION entry
    out.push(ENCRYPTION);
    out.extend_from_slice(&((table + ver.len()) as u16).to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.push(0xFF); // terminator
    out.extend_from_slice(&ver);
    out.push(encryption);
    out
}

/// Reads the ENCRYPTION option out of a PRELOGIN payload.
pub fn prelogin_encryption(payload: &[u8]) -> Option<u8> {
    let mut i = 0;
    loop {
        let token = *payload.get(i)?;
        if token == 0xFF {
            return None;
        }
        let off = u16::from_be_bytes(payload.get(i + 1..i + 3)?.try_into().ok()?) as usize;
        let len = u16::from_be_bytes(payload.get(i + 3..i + 5)?.try_into().ok()?) as usize;
        if token == 0x01 {
            return payload.get(off).copied().filter(|_| len >= 1);
        }
        i += 5;
    }
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

fn utf16_decode(b: &[u8]) -> String {
    let units: Vec<u16> = b
        .chunks(2)
        .filter(|c| c.len() == 2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// TDS password obfuscation: UTF-16LE, each byte nibble-swapped then XOR 0xA5.
fn obfuscate(password: &str) -> Vec<u8> {
    utf16(password)
        .into_iter()
        .map(|b| b.rotate_left(4) ^ 0xA5)
        .collect()
}

/// Builds a LOGIN7 payload for SQL authentication.
pub fn login7(user: &str, password: &str, database: Option<&str>, server: &str) -> Vec<u8> {
    const FIXED: usize = 36;
    const OFFSETS: usize = 58;
    let data_start = FIXED + OFFSETS; // 94

    let host = utf16("zapper");
    let user_u = utf16(user);
    let pass = obfuscate(password);
    let app = utf16("zapper");
    let srv = utf16(server);
    let clt = utf16("zapper");
    let db = utf16(database.unwrap_or(""));

    let mut data = Vec::new();
    let place = |data: &mut Vec<u8>, bytes: &[u8]| -> (u16, u16) {
        let off = (data_start + data.len()) as u16;
        let cch = (bytes.len() / 2) as u16;
        data.extend_from_slice(bytes);
        (off, cch)
    };
    let host = place(&mut data, &host);
    let user = place(&mut data, &user_u);
    // Password offset uses the obfuscated bytes but its length is in characters.
    let pass = {
        let off = (data_start + data.len()) as u16;
        let cch = (pass.len() / 2) as u16;
        data.extend_from_slice(&pass);
        (off, cch)
    };
    let app = place(&mut data, &app);
    let srv = place(&mut data, &srv);
    let clt = place(&mut data, &clt);
    let lang = ((data_start + data.len()) as u16, 0u16);
    let db = place(&mut data, &db);

    let total = (data_start + data.len()) as u32;
    let mut p = Vec::new();
    p.extend_from_slice(&total.to_le_bytes());
    p.extend_from_slice(&0x7400_0004u32.to_le_bytes()); // TDS 7.4
    p.extend_from_slice(&4096u32.to_le_bytes()); // packet size
    p.extend_from_slice(&0u32.to_le_bytes()); // client prog version
    p.extend_from_slice(&0u32.to_le_bytes()); // client PID
    p.extend_from_slice(&0u32.to_le_bytes()); // connection id
    p.push(0x00); // OptionFlags1 (little-endian byte order, defaults)
    p.push(0x00); // OptionFlags2 (fIntSecurity=0 → SQL authentication)
    p.push(0x00); // TypeFlags
    p.push(0x00); // OptionFlags3
    p.extend_from_slice(&0i32.to_le_bytes()); // client time zone
    p.extend_from_slice(&0u32.to_le_bytes()); // client LCID
    let pair = |p: &mut Vec<u8>, (off, cch): (u16, u16)| {
        p.extend_from_slice(&off.to_le_bytes());
        p.extend_from_slice(&cch.to_le_bytes());
    };
    pair(&mut p, host);
    pair(&mut p, user);
    pair(&mut p, pass);
    pair(&mut p, app);
    pair(&mut p, srv);
    pair(&mut p, (0, 0)); // Extension (unused)
    pair(&mut p, clt);
    pair(&mut p, lang);
    pair(&mut p, db);
    p.extend_from_slice(&[0u8; 6]); // ClientID
    pair(&mut p, (0, 0)); // SSPI
    pair(&mut p, (0, 0)); // AtchDBFile
    pair(&mut p, (0, 0)); // ChangePassword
    p.extend_from_slice(&0u32.to_le_bytes()); // cbSSPILong
    p.extend_from_slice(&data);
    debug_assert_eq!(p.len(), total as usize);
    p
}

/// Scans a login response for an ERROR token (`0xAA`); `Err` with its message
/// if the login failed, `Ok` otherwise (a LOGINACK response).
pub fn login_result(payload: &[u8]) -> Result<(), String> {
    let mut i = 0;
    while i < payload.len() {
        let token = payload[i];
        i += 1;
        match token {
            // Variable-length tokens: Int16 length then body.
            0xAA | 0xAB | 0xAD | 0xE3 => {
                let Some(len) = payload
                    .get(i..i + 2)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
                else {
                    break;
                };
                i += 2;
                let body = payload.get(i..(i + len).min(payload.len())).unwrap_or(&[]);
                if token == 0xAA {
                    return Err(error_token(body));
                }
                i += len;
            }
            0xFD..=0xFF => i += 12, // DONE / DONEPROC / DONEINPROC
            0x79 => i += 4,         // RETURNSTATUS
            _ => break,             // unknown token: stop scanning
        }
    }
    Ok(())
}

/// The message out of an ERROR token body (`Int32` number, state, class,
/// `Int16` message length in characters, then the UTF-16 message).
fn error_token(b: &[u8]) -> String {
    let Some(mlen) = b
        .get(6..8)
        .map(|m| u16::from_le_bytes([m[0], m[1]]) as usize * 2)
    else {
        return "login failed".into();
    };
    match b.get(8..8 + mlen) {
        Some(m) => format!("server rejected login: {}", utf16_decode(m)),
        None => "login failed".into(),
    }
}

/// Wraps a stream so the TLS handshake is tunnelled in TDS PRELOGIN packets, as
/// MS-TDS requires. While `handshaking` is true each write is framed as a
/// PRELOGIN packet and each read is de-framed; once the caller flips the flag
/// (after the TLS handshake completes), reads and writes pass through, because
/// the TLS record layer then protects the TDS PDUs directly.
pub struct TdsTls<S> {
    inner: S,
    handshaking: Arc<AtomicBool>,
    wbuf: Vec<u8>,
    wpos: usize,
    wclaim: usize,
    racc: Vec<u8>,
    rout: Vec<u8>,
    rpos: usize,
}

impl<S> TdsTls<S> {
    pub fn new(inner: S, handshaking: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            handshaking,
            wbuf: Vec::new(),
            wpos: 0,
            wclaim: 0,
            racc: Vec::new(),
            rout: Vec::new(),
            rpos: 0,
        }
    }
    fn framing(&self) -> bool {
        self.handshaking.load(Ordering::Relaxed)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for TdsTls<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        // Finish flushing any framed packet still in flight.
        while self.wpos < self.wbuf.len() {
            let me = &mut *self;
            match Pin::new(&mut me.inner).poll_write(cx, &me.wbuf[me.wpos..])? {
                Poll::Ready(n) => me.wpos += n,
                Poll::Pending => return Poll::Pending,
            }
        }
        if !self.wbuf.is_empty() {
            let claim = self.wclaim;
            self.wbuf.clear();
            self.wpos = 0;
            return Poll::Ready(Ok(claim));
        }
        if !self.framing() {
            return Pin::new(&mut self.inner).poll_write(cx, buf);
        }
        // Start a new PRELOGIN-framed packet carrying this write.
        self.wbuf = frame(PKT_PRELOGIN, buf);
        self.wpos = 0;
        self.wclaim = buf.len();
        while self.wpos < self.wbuf.len() {
            let me = &mut *self;
            match Pin::new(&mut me.inner).poll_write(cx, &me.wbuf[me.wpos..])? {
                Poll::Ready(n) => me.wpos += n,
                Poll::Pending => return Poll::Pending,
            }
        }
        let claim = self.wclaim;
        self.wbuf.clear();
        self.wpos = 0;
        Poll::Ready(Ok(claim))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for TdsTls<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // Serve any de-framed payload left from a previous packet.
        if self.rpos < self.rout.len() {
            let me = &mut *self;
            let n = buf.remaining().min(me.rout.len() - me.rpos);
            buf.put_slice(&me.rout[me.rpos..me.rpos + n]);
            me.rpos += n;
            return Poll::Ready(Ok(()));
        }
        if !self.framing() {
            return Pin::new(&mut self.inner).poll_read(cx, buf);
        }
        loop {
            // Extract a whole TDS packet's payload from the accumulator.
            if self.racc.len() >= HEADER {
                let len = u16::from_be_bytes([self.racc[2], self.racc[3]]) as usize;
                if len >= HEADER && self.racc.len() >= len {
                    let me = &mut *self;
                    me.rout = me.racc[HEADER..len].to_vec();
                    me.rpos = 0;
                    me.racc.drain(..len);
                    let n = buf.remaining().min(me.rout.len());
                    buf.put_slice(&me.rout[..n]);
                    me.rpos = n;
                    return Poll::Ready(Ok(()));
                }
            }
            let mut tmp = [0u8; 4096];
            let mut rb = ReadBuf::new(&mut tmp);
            let me = &mut *self;
            match Pin::new(&mut me.inner).poll_read(cx, &mut rb)? {
                Poll::Ready(()) => {
                    let filled = rb.filled();
                    if filled.is_empty() {
                        return Poll::Ready(Ok(())); // EOF
                    }
                    me.racc.extend_from_slice(filled);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn packet_round_trips_and_reads_a_message() {
        let bytes = frame(PKT_LOGIN7, b"body");
        let mut buf = Vec::new();
        let (typ, status, payload) = read_packet(&mut &bytes[..], &mut buf).await.unwrap();
        assert_eq!(
            (typ, status, payload),
            (PKT_LOGIN7, STATUS_EOM, b"body".to_vec())
        );
    }

    #[test]
    fn prelogin_encryption_round_trips() {
        for enc in [ENCRYPT_OFF, ENCRYPT_ON, ENCRYPT_NOT_SUP, ENCRYPT_REQ] {
            assert_eq!(prelogin_encryption(&prelogin(enc)), Some(enc));
        }
    }

    #[test]
    fn obfuscation_and_login7_shape() {
        // Obfuscation is reversible: nibble-swap then XOR, applied twice, is id.
        let o = obfuscate("pw");
        let back: Vec<u8> = o.iter().map(|b| (b ^ 0xA5).rotate_left(4)).collect();
        assert_eq!(back, utf16("pw"));
        // The LOGIN7 total-length prefix matches the payload length.
        let p = login7("sa", "pw", Some("master"), "db.example");
        assert_eq!(
            u32::from_le_bytes(p[0..4].try_into().unwrap()) as usize,
            p.len()
        );
    }

    #[test]
    fn login_result_flags_an_error_token() {
        // ERROR token: 0xAA, Int16 len, Int32 number, state, class, Int16 mlen, msg.
        let msg = utf16("Login failed");
        let mut body = 0u32.to_le_bytes().to_vec(); // number
        body.push(1); // state
        body.push(14); // class
        body.extend_from_slice(&((msg.len() / 2) as u16).to_le_bytes());
        body.extend_from_slice(&msg);
        let mut tok = vec![0xAA];
        tok.extend_from_slice(&(body.len() as u16).to_le_bytes());
        tok.extend_from_slice(&body);
        assert!(login_result(&tok).unwrap_err().contains("Login failed"));
        // A lone DONE token is success.
        assert!(login_result(&[0xFD, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_ok());
    }

    #[tokio::test]
    async fn tds_tls_wrapper_frames_and_deframes_during_handshake() {
        // Writes through the wrapper come out as PRELOGIN packets on the wire;
        // PRELOGIN packets fed in come back out de-framed.
        let (a, mut b) = tokio::io::duplex(4096);
        let flag = Arc::new(AtomicBool::new(true));
        let mut w = TdsTls::new(a, flag.clone());
        w.write_all(b"clienthello").await.unwrap();
        let (typ, _s, payload) = read_packet(&mut b, &mut Vec::new()).await.unwrap();
        assert_eq!((typ, payload), (PKT_PRELOGIN, b"clienthello".to_vec()));

        b.write_all(&frame(PKT_PRELOGIN, b"serverhello"))
            .await
            .unwrap();
        let mut got = [0u8; 11];
        w.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"serverhello");
    }
}
