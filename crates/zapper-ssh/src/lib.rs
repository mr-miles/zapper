//! The SSH gateway: the server's side of SSH, spoken on one stream (the
//! stdio of `zapper ssh proxy`, which the host's sshd runs as a key's forced
//! command), its channels carried to a sandbox's terminals. There is no sshd
//! in the guest: a shell channel becomes a terminal running the profile's
//! command, `exec` a command under `/bin/sh -c`, both opened through
//! [`Sandbox`] as the workload user under exec supervision, recorded as any
//! terminal is. Everything that would open another way in or out (port
//! forwarding either way, agent and X11 forwarding, subsystems such as SFTP)
//! is refused.
//!
//! Who may connect is decided before the gateway runs (sshd's key check,
//! or who may run it at all), so it accepts any client authentication.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use russh::keys::PrivateKey;
use russh::server::{Auth, ChannelOpenHandle, Handle, Msg, Session};
use russh::{Channel, ChannelId, ChannelOpenFailure, Pty, Sig};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use zapper_proto::proto::{Frame, MAX_FRAME};

/// Channels open on one SSH connection at once, at most.
pub const MAX_CHANNELS: usize = 16;

/// A stream to a terminal in the sandbox, carrying [`Frame`]s.
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Stream for T {}

/// A terminal to open: a program, with a terminal of a size or without one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Terminal {
    /// Empty: the profile's command (the agent).
    pub argv: Vec<String>,
    /// Rows and columns, for a pseudo-terminal.
    pub tty: Option<(u16, u16)>,
    /// `TERM`, `LANG` and `LC_*` from the client; nothing else is passed.
    pub env: BTreeMap<String, String>,
}

/// The sandbox an SSH connection is to.
pub trait Sandbox: Send + Sync + 'static {
    /// Opens a terminal in it. An error (SSH turned off for it, say) is told
    /// to the client.
    fn terminal(&self, t: Terminal) -> impl Future<Output = Result<Box<dyn Stream>>> + Send;
}

/// The gateway's host key at `path`, made on first use (Ed25519, mode 0600).
pub fn host_key(path: &Path) -> Result<PrivateKey> {
    if let Ok(text) = std::fs::read_to_string(path) {
        return PrivateKey::from_openssh(&text)
            .with_context(|| format!("the SSH host key {}", path.display()));
    }
    let mut seed = [0u8; 32];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut seed)?;
    let key = PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(
        &seed,
    ));
    let text = key.to_openssh(russh::keys::ssh_key::LineEnding::LF)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing the SSH host key {}", path.display()))?
        .write_all(text.as_bytes())?;
    Ok(key)
}

/// The sandbox an SSH target names: `ID` or `ID.zapper` (as `%h` or the
/// forced command's `SSH_ORIGINAL_COMMAND` has it), the ID a session's.
pub fn sandbox_id(target: &str) -> Result<String> {
    let id = target.trim();
    let id = id.strip_suffix(".zapper").unwrap_or(id);
    if id.is_empty() || id.len() > 32 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        anyhow::bail!("{target:?} doesn't name a sandbox: `ssh ID.zapper`");
    }
    Ok(id.to_string())
}

/// A public key as `authorized_keys` takes it, from what a user pasted:
/// its algorithm and its key, checked by parsing, without options or a
/// comment (which a line of the workbench's own gives).
pub fn public_key(line: &str) -> Result<String> {
    let line = line.trim();
    if line.contains('\n') || line.contains('\r') {
        anyhow::bail!("one key, on one line");
    }
    let key = russh::keys::PublicKey::from_openssh(line)
        .map_err(|e| anyhow::anyhow!("not an OpenSSH public key ({e}): `ssh-ed25519 AAAA…`"))?;
    let mut key = key;
    key.set_comment("");
    Ok(key.to_openssh()?.trim().to_string())
}

/// The `known_hosts` line for `key` under the alias `zapper`, which every
/// sandbox's name matches through `HostKeyAlias`.
pub fn known_hosts_line(key: &PrivateKey) -> Result<String> {
    Ok(format!("zapper {}", key.public_key().to_openssh()?))
}

/// Serves one SSH connection on `stream` until the client goes, its
/// channels carried to `sandbox`.
pub async fn serve<S, IO>(sandbox: Arc<S>, host_key: PrivateKey, stream: IO) -> Result<()>
where
    S: Sandbox,
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let config = Arc::new(russh::server::Config {
        keys: vec![host_key],
        // Who connects was decided before this ran: nothing to wait out.
        auth_rejection_time: std::time::Duration::ZERO,
        auth_rejection_time_initial: Some(std::time::Duration::ZERO),
        inactivity_timeout: None,
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        maximum_packet_size: 32768,
        ..Default::default()
    });
    let handler = Connection {
        sandbox,
        channels: HashMap::new(),
    };
    let running = russh::server::run_stream(config, stream, handler)
        .await
        .map_err(|e| anyhow::anyhow!("SSH: {e}"))?;
    running.await.map_err(|e| anyhow::anyhow!("SSH: {e}"))
}

/// One SSH connection's state: its open session channels.
struct Connection<S> {
    sandbox: Arc<S>,
    channels: HashMap<ChannelId, Chan>,
}

/// A session channel: what the client asked for before its shell or exec,
/// and once that runs, the way to its terminal.
#[derive(Default)]
struct Chan {
    pty: Option<(u16, u16)>,
    env: BTreeMap<String, String>,
    to_guest: Option<mpsc::Sender<Frame>>,
}

/// The environment a client may set: the terminal's type and the locale.
fn passed(name: &str) -> bool {
    name == "TERM" || name == "LANG" || name.starts_with("LC_")
}

/// What `exec CMD` runs, or why it is refused.
fn exec_argv(cmd: &[u8]) -> std::result::Result<Vec<String>, &'static str> {
    let cmd = std::str::from_utf8(cmd).map_err(|_| "the command isn't UTF-8")?;
    let first = cmd.split_whitespace().next().unwrap_or_default();
    let second = cmd.split_whitespace().nth(1).unwrap_or_default();
    if first == "scp" && (second.starts_with("-t") || second.starts_with("-f")) {
        return Err("scp into or out of a sandbox isn't supported; `zapper cp` copies out");
    }
    Ok(vec!["/bin/sh".into(), "-c".into(), cmd.to_string()])
}

/// A size from SSH's columns and rows, within what a terminal takes.
fn size(cols: u32, rows: u32) -> (u16, u16) {
    let fit = |v: u32, default: u16| match v {
        0 => default,
        v => v.min(u16::MAX as u32) as u16,
    };
    (fit(rows, 24), fit(cols, 80))
}

impl<S: Sandbox> Connection<S> {
    /// Opens the channel's terminal running `argv`, and carries it: the
    /// guest's output to the client, the client's input (through
    /// `to_guest`) to the guest.
    async fn run(&mut self, channel: ChannelId, argv: Vec<String>, session: &mut Session) {
        let Some(chan) = self.channels.get_mut(&channel) else {
            let _ = session.channel_failure(channel);
            return;
        };
        if chan.to_guest.is_some() {
            // One program per channel.
            let _ = session.channel_failure(channel);
            return;
        }
        let t = Terminal {
            argv,
            tty: chan.pty,
            env: chan.env.clone(),
        };
        let tty = t.tty.is_some();
        let handle = session.handle();
        let _ = session.channel_success(channel);
        match self.sandbox.terminal(t).await {
            Ok(io) => {
                let (tx, rx) = mpsc::channel(64);
                chan.to_guest = Some(tx);
                tokio::spawn(carry(io, rx, handle, channel));
            }
            Err(e) => {
                let eol = if tty { "\r\n" } else { "\n" };
                tokio::spawn(async move {
                    let msg = format!("zapper: {e:#}{eol}");
                    let _ = handle.extended_data(channel, 1, msg.into_bytes()).await;
                    let _ = handle.exit_status_request(channel, 255).await;
                    let _ = handle.eof(channel).await;
                    let _ = handle.close(channel).await;
                });
            }
        }
    }

    /// Sends `frame` to the channel's terminal, if it has one running.
    async fn send_to_guest(&mut self, channel: ChannelId, frame: Frame) {
        let tx = self.channels.get(&channel).and_then(|c| c.to_guest.clone());
        if let Some(tx) = tx {
            let _ = tx.send(frame).await;
        }
    }
}

/// Carries a terminal: frames from `rx` to the guest, and the guest's frames
/// to the client's channel, until the program exits or either side goes.
async fn carry(
    io: Box<dyn Stream>,
    mut rx: mpsc::Receiver<Frame>,
    handle: Handle,
    channel: ChannelId,
) {
    let (mut gr, mut gw) = tokio::io::split(io);
    let input = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if gw.write_all(&frame.encode()).await.is_err() {
                break;
            }
        }
        let _ = gw.shutdown().await;
    });
    let mut status = None;
    while let Ok(Some(frame)) = read_frame(&mut gr).await {
        let sent = match frame {
            Frame::Data(d) => handle.data(channel, d).await.is_ok(),
            Frame::Stderr(d) => handle.extended_data(channel, 1, d).await.is_ok(),
            Frame::Exit(code) => {
                status = Some(code);
                break;
            }
            _ => true,
        };
        if !sent {
            break;
        }
    }
    input.abort();
    if let Some(code) = status {
        let _ = handle
            .exit_status_request(channel, code.clamp(0, 255) as u32)
            .await;
    }
    let _ = handle.eof(channel).await;
    let _ = handle.close(channel).await;
}

/// The next frame from a terminal's stream; `None` at its end.
async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Option<Frame>> {
    let mut header = [0u8; 5];
    match r.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let (kind, len) = Frame::header(&header)?;
    debug_assert!(len <= MAX_FRAME);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Frame::decode(kind, body).map(Some)
}

impl<S: Sandbox> russh::server::Handler for Connection<S> {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn auth_publickey(
        &mut self,
        _user: &str,
        _key: russh::server::VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.channels.len() >= MAX_CHANNELS {
            reply.reject(ChannelOpenFailure::ResourceShortage).await;
            return Ok(());
        }
        self.channels.insert(channel.id(), Chan::default());
        reply.accept().await;
        Ok(())
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        _channel: Channel<Msg>,
        _host_to_connect: &str,
        _port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply
            .reject(ChannelOpenFailure::AdministrativelyProhibited)
            .await;
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Dropping the sender closes the terminal's input, and the guest
        // hangs the program up.
        self.channels.remove(&channel);
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.send_to_guest(channel, Frame::Eof).await;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        for chunk in data.chunks(MAX_FRAME) {
            self.send_to_guest(channel, Frame::Data(chunk.to_vec()))
                .await;
        }
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match self.channels.get_mut(&channel) {
            Some(c) if c.to_guest.is_none() => {
                c.pty = Some(size(col_width, row_height));
                if !term.is_empty() && term.len() <= 64 {
                    c.env.insert("TERM".into(), term.into());
                }
                session.channel_success(channel)?;
            }
            _ => session.channel_failure(channel)?,
        }
        Ok(())
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match self.channels.get_mut(&channel) {
            Some(c) if passed(name) && value.len() <= 256 && c.env.len() < 32 => {
                c.env.insert(name.into(), value.into());
                session.channel_success(channel)?;
            }
            _ => session.channel_failure(channel)?,
        }
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.run(channel, Vec::new(), session).await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match exec_argv(data) {
            Ok(argv) => self.run(channel, argv, session).await,
            Err(why) => {
                session.channel_success(channel)?;
                session.extended_data(channel, 1, format!("zapper: {why}\n").into_bytes())?;
                session.exit_status_request(channel, 1)?;
                session.eof(channel)?;
                session.close(channel)?;
            }
        }
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let (rows, cols) = size(col_width, row_height);
        self.send_to_guest(channel, Frame::Resize(rows, cols)).await;
        Ok(())
    }

    async fn signal(
        &mut self,
        _channel: ChannelId,
        _signal: Sig,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::client;
    use russh::ChannelMsg;
    use std::sync::Mutex;

    /// A sandbox whose terminals echo their input, and exit 3 at its end;
    /// or that refuses them all with `refuse`.
    #[derive(Default)]
    struct Fake {
        asked: Mutex<Vec<Terminal>>,
        refuse: Option<String>,
    }

    impl Sandbox for Fake {
        async fn terminal(&self, t: Terminal) -> Result<Box<dyn Stream>> {
            self.asked.lock().unwrap().push(t);
            if let Some(why) = &self.refuse {
                anyhow::bail!("{why}");
            }
            let (ours, mut guest) = tokio::io::duplex(64 * 1024);
            tokio::spawn(async move {
                while let Ok(Some(frame)) = read_frame(&mut guest).await {
                    match frame {
                        Frame::Data(d) => {
                            let _ = guest.write_all(&Frame::Data(d).encode()).await;
                        }
                        Frame::Eof => {
                            let _ = guest.write_all(&Frame::Exit(3).encode()).await;
                            break;
                        }
                        _ => {}
                    }
                }
            });
            Ok(Box::new(ours))
        }
    }

    struct Client;

    impl client::Handler for Client {
        type Error = russh::Error;
        async fn check_server_key(
            &mut self,
            _: &russh::keys::PublicKeyOrCertificate,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }
    }

    fn key() -> PrivateKey {
        PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(
            &[7; 32],
        ))
    }

    async fn connect(fake: Arc<Fake>) -> client::Handle<Client> {
        let (server, client_io) = tokio::io::duplex(256 * 1024);
        tokio::spawn(serve(fake, key(), server));
        let mut h = client::connect_stream(Arc::new(client::Config::default()), client_io, Client)
            .await
            .unwrap();
        assert!(h.authenticate_none("user").await.unwrap().success());
        h
    }

    /// What a channel said until it closed.
    #[derive(Debug, Default)]
    struct Said {
        out: Vec<u8>,
        err: Vec<u8>,
        exit: Option<u32>,
        failures: usize,
    }

    async fn until_closed(ch: &mut Channel<client::Msg>) -> Said {
        let mut said = Said::default();
        while let Some(msg) = ch.wait().await {
            match msg {
                ChannelMsg::Data { data } => said.out.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, .. } => said.err.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => said.exit = Some(exit_status),
                ChannelMsg::Failure => said.failures += 1,
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        said
    }

    #[tokio::test]
    async fn exec_runs_under_sh_without_a_terminal_and_carries_its_bytes_and_status() {
        let fake = Arc::new(Fake::default());
        let h = connect(fake.clone()).await;
        let mut ch = h.channel_open_session().await.unwrap();
        ch.set_env(false, "LANG", "C.UTF-8").await.unwrap();
        ch.set_env(false, "SECRET", "x").await.unwrap();
        ch.exec(true, "cat").await.unwrap();
        ch.data_bytes(&b"\x00binary\xff"[..]).await.unwrap();
        ch.eof().await.unwrap();
        let said = until_closed(&mut ch).await;
        assert_eq!(said.out, b"\x00binary\xff");
        assert_eq!(said.exit, Some(3));
        let asked = fake.asked.lock().unwrap();
        assert_eq!(
            asked[..],
            [Terminal {
                argv: vec!["/bin/sh".into(), "-c".into(), "cat".into()],
                tty: None,
                env: [("LANG".to_string(), "C.UTF-8".to_string())].into(),
            }]
        );
    }

    #[tokio::test]
    async fn a_shell_with_a_pty_is_the_profiles_command_at_the_clients_size() {
        let fake = Arc::new(Fake::default());
        let h = connect(fake.clone()).await;
        let ch = h.channel_open_session().await.unwrap();
        ch.request_pty(true, "xterm-256color", 120, 40, 0, 0, &[])
            .await
            .unwrap();
        ch.request_shell(true).await.unwrap();
        ch.data_bytes(&b"hi"[..]).await.unwrap();
        let mut ch = ch;
        let mut got = Vec::new();
        while got.len() < 2 {
            if let Some(ChannelMsg::Data { data }) = ch.wait().await {
                got.extend_from_slice(&data);
            }
        }
        assert_eq!(got, b"hi");
        let asked = fake.asked.lock().unwrap();
        assert_eq!(asked[0].argv, Vec::<String>::new());
        assert_eq!(asked[0].tty, Some((40, 120)));
        assert_eq!(asked[0].env["TERM"], "xterm-256color");
    }

    #[tokio::test]
    async fn a_refused_terminal_tells_the_client_why() {
        let fake = Arc::new(Fake {
            refuse: Some("SSH is off for session abc234".into()),
            ..Default::default()
        });
        let h = connect(fake).await;
        let mut ch = h.channel_open_session().await.unwrap();
        ch.exec(true, "true").await.unwrap();
        let said = until_closed(&mut ch).await;
        assert_eq!(said.exit, Some(255));
        assert!(
            String::from_utf8_lossy(&said.err).contains("SSH is off"),
            "{said:?}"
        );
    }

    #[tokio::test]
    async fn legacy_scp_sftp_and_forwards_are_refused() {
        let fake = Arc::new(Fake::default());
        let h = connect(fake.clone()).await;

        let mut ch = h.channel_open_session().await.unwrap();
        ch.exec(true, "scp -t /tmp").await.unwrap();
        let said = until_closed(&mut ch).await;
        assert_eq!(said.exit, Some(1));
        assert!(String::from_utf8_lossy(&said.err).contains("scp"));

        let mut ch = h.channel_open_session().await.unwrap();
        ch.request_subsystem(true, "sftp").await.unwrap();
        ch.eof().await.unwrap();
        loop {
            match ch.wait().await {
                Some(ChannelMsg::Failure) => break,
                Some(_) => {}
                None => panic!("no answer to the subsystem request"),
            }
        }

        assert!(h
            .channel_open_direct_tcpip("127.0.0.1", 22, "127.0.0.1", 5000)
            .await
            .is_err());
        assert!(
            fake.asked.lock().unwrap().is_empty(),
            "nothing reached the sandbox"
        );
    }

    #[test]
    fn only_the_terminals_type_and_the_locale_are_passed() {
        for name in ["TERM", "LANG", "LC_ALL", "LC_CTYPE"] {
            assert!(passed(name), "{name}");
        }
        for name in ["PATH", "LD_PRELOAD", "HOME", "lang", "SSH_AUTH_SOCK"] {
            assert!(!passed(name), "{name}");
        }
    }

    #[test]
    fn a_size_of_nothing_is_the_default_and_a_huge_one_fits() {
        assert_eq!(size(0, 0), (24, 80));
        assert_eq!(size(200, 50), (50, 200));
        assert_eq!(size(u32::MAX, u32::MAX), (u16::MAX, u16::MAX));
    }

    #[test]
    fn a_target_names_a_sandbox_with_or_without_its_suffix() {
        assert_eq!(sandbox_id("abc234.zapper").unwrap(), "abc234");
        assert_eq!(sandbox_id("abc234").unwrap(), "abc234");
        assert_eq!(sandbox_id(" abc234.zapper\n").unwrap(), "abc234");
        for bad in [
            "",
            ".zapper",
            "abc;rm -rf /",
            "../x",
            "a b",
            "abc234.example",
        ] {
            assert!(sandbox_id(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_public_key_is_parsed_and_kept_without_options_or_comment() {
        let k = key().public_key().to_openssh().unwrap();
        let pasted = format!("  {k} laptop@home \n");
        let kept = public_key(&pasted).unwrap();
        assert_eq!(kept, k.split(' ').take(2).collect::<Vec<_>>().join(" "));
        assert!(!kept.contains("laptop"));
        for bad in [
            "".to_string(),
            "ssh-ed25519 not-base64".to_string(),
            format!("command=\"sh\" {k}"),
            format!("{k}\n{k}"),
        ] {
            assert!(public_key(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_host_key_is_made_once_and_kept_private() {
        let dir = std::env::temp_dir().join(format!("zapper-ssh-{}", std::process::id()));
        let path = dir.join("ssh_host_ed25519");
        let made = host_key(&path).unwrap();
        let again = host_key(&path).unwrap();
        assert_eq!(made.public_key(), again.public_key());
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(known_hosts_line(&made)
            .unwrap()
            .starts_with("zapper ssh-ed25519 "));
        let _ = std::fs::remove_dir_all(dir);
    }
}
