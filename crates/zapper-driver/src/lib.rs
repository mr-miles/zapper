//! Driving an agent: the harnesses' conversation code (Claude Code, pi) and
//! the process driver that runs one over a command stream: the office's
//! foreman drives its workers with it, and the OpenShell client pumps
//! commands with it.

pub mod harness;
pub mod process;

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
