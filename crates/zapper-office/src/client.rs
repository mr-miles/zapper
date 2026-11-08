//! Commands run over the office's client; the client itself
//! (`OfficeClient`) is zapper-openshell's.

use zapper_policy::proto::ShellRequest;

use anyhow::Result;

pub use zapper_openshell::client::*;

/// Runs one command in session `id` over the office's client and returns its
/// exit code, stdout (capped at `limit`) and stderr. Built on `attach`: the
/// office drives the command over the sandbox's command stream (the firm's
/// cluster is a sandbox, not an agent). A shell script may bundle many steps
/// into one call.
pub(crate) async fn run_command(
    client: &dyn OfficeClient,
    id: &str,
    argv: Vec<String>,
    stdin: Option<Vec<u8>>,
    limit: u64,
) -> Result<(i32, Vec<u8>, String)> {
    let stream = client.attach(id, ShellRequest::system(argv)).await?;
    let input = stdin.map(std::io::Cursor::new);
    let mut out = Vec::new();
    let (code, err) = zapper_driver::process::pump_command(stream, input, &mut out, limit).await?;
    Ok((code, out, err))
}
