//! Driving an agent: the harnesses' conversation code (Claude Code, pi) and
//! the process driver that runs one on a terminal. A crate of its own so
//! both the daemon (for driven sessions) and the office (over a worker's
//! terminal) use it without the office linking the core.

pub mod harness;
pub mod process;

/// A short random id in the same alphabet the daemon uses, for a worker whose
/// name a harness must invent.
pub(crate) fn random_id(len: usize) -> String {
    use rand::Rng;
    const ID_CHARS: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| ID_CHARS[rng.gen_range(0..ID_CHARS.len())] as char)
        .collect()
}

/// Reads one length-prefixed protocol frame from `r`, or `None` at a clean
/// end of stream.
pub async fn read_frame(
    r: &mut (impl tokio::io::AsyncRead + Unpin),
) -> std::io::Result<Option<zapper_policy::proto::Frame>> {
    use tokio::io::AsyncReadExt as _;
    use zapper_policy::proto::Frame;
    let mut header = [0u8; 5];
    match r.read_exact(&mut header).await {
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        other => other?,
    };
    let (kind, len) = Frame::header(&header)?;
    let mut body = vec![0; len];
    r.read_exact(&mut body).await?;
    Frame::decode(kind, body).map(Some)
}
