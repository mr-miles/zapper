//! `zapper github setup`: make sure an account has a GitHub App for zapper and
//! that it is installed there.
//!
//! The App may already exist (its credentials file is there, or `--credentials`
//! points at one made elsewhere): then nothing is created and an existing file
//! is never overwritten, because GitHub shows an App's private key once.
//! Otherwise the App is created through GitHub's manifest flow: a page on
//! localhost posts the manifest to GitHub, the owner confirms there, and
//! GitHub redirects back with a one-time code that is exchanged for the key.

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use hyper::Method;
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::app::{owner_file, valid_login, App};
use crate::github::{GitHub, Installation};
use crate::http;

/// Permissions the App asks for by default; `with` raises more to write.
const DEFAULT_PERMISSIONS: [(&str, &str); 8] = [
    ("contents", "write"),
    ("pull_requests", "write"),
    ("issues", "write"),
    ("metadata", "read"),
    ("checks", "read"),
    ("statuses", "read"),
    ("actions", "read"),
    ("artifact_metadata", "read"),
];

/// Permissions `with` may add (as write). `workflows` lets an agent change CI,
/// so it is never on by default.
pub const EXTRA_PERMISSIONS: [&str; 4] = ["checks", "statuses", "actions", "workflows"];

/// Opens a URL for the user.
pub type OpenUrl = Box<dyn Fn(&str) + Send + Sync>;

pub struct Options {
    /// The account the App lives in.
    pub owner: String,
    /// `owner` is a user rather than an organisation.
    pub user: bool,
    /// The App's name on GitHub, unique there.
    pub name: String,
    /// Raised from read to write, from [`EXTRA_PERMISSIONS`].
    pub with: Vec<String>,
    /// Installable on any account, not only the owner's.
    pub public: bool,
    /// Local port for GitHub's redirect; 0 picks one.
    pub port: u16,
    /// An existing App's credentials file to adopt instead of creating one.
    pub credentials: Option<PathBuf>,
    /// The credentials file to write; default: the owner's.
    pub file: Option<PathBuf>,
    /// API base, for tests.
    pub api: Option<String>,
    /// Opens a URL in a browser; `None` tries `xdg-open`/`open`.
    pub open: Option<OpenUrl>,
    /// How often to look for the installation, and how long to wait for it.
    pub poll: Duration,
    pub wait: Duration,
}

impl Options {
    pub fn new(owner: &str, user: bool) -> Options {
        Options {
            owner: owner.into(),
            user,
            name: format!("zapper-{}", owner.to_ascii_lowercase()),
            with: Vec::new(),
            public: false,
            port: 8765,
            credentials: None,
            file: None,
            api: None,
            open: None,
            poll: Duration::from_secs(3),
            wait: Duration::from_secs(15 * 60),
        }
    }
}

/// What setup found or made.
pub struct Outcome {
    pub file: PathBuf,
    pub created: bool,
    pub installation: Installation,
}

/// Creates (or adopts) the App, then waits until it is installed on the
/// owner. `paste` carries lines the user typed, for when the browser is on
/// another machine and its redirect cannot reach this one.
pub async fn run(
    o: Options,
    out: &mut (dyn std::io::Write + Send),
    mut paste: mpsc::Receiver<String>,
) -> Result<Outcome> {
    if !valid_login(&o.owner) {
        bail!("{:?} is not a GitHub account name", o.owner);
    }
    for p in &o.with {
        if !EXTRA_PERMISSIONS.contains(&p.as_str()) {
            bail!(
                "unknown --with permission: {p} (allowed: {})",
                EXTRA_PERMISSIONS.join(", ")
            );
        }
    }
    let file = match &o.file {
        Some(f) => f.clone(),
        None => owner_file(&o.owner)?,
    };
    let gh = GitHub::new(o.api.clone());

    let created = if file.exists() {
        writeln!(out, "Using the App in {}", file.display())?;
        false
    } else if let Some(src) = &o.credentials {
        adopt(src, &file, out)?;
        false
    } else {
        create(&o, &gh, &file, out, &mut paste).await?;
        true
    };

    let mut warn = |w: String| {
        let _ = writeln!(std::io::stderr(), "{w}");
    };
    let app = App::load(&file, &mut warn)?;
    let installation = ensure_installed(&o, &gh, &app, out).await?;
    Ok(Outcome {
        file,
        created,
        installation,
    })
}

/// Copies an App made elsewhere (agentctl's file has the same layout) into
/// place, after checking that it loads.
fn adopt(src: &Path, file: &Path, out: &mut (dyn std::io::Write + Send)) -> Result<()> {
    App::load(src, &mut |w| {
        let _ = writeln!(std::io::stderr(), "{w}");
    })?;
    let text =
        std::fs::read_to_string(src).with_context(|| format!("reading {}", src.display()))?;
    write_new(file, text.as_bytes())?;
    writeln!(out, "✔ Copied the App's credentials to {}", file.display())?;
    Ok(())
}

async fn create(
    o: &Options,
    gh: &GitHub,
    file: &Path,
    out: &mut (dyn std::io::Write + Send),
    paste: &mut mpsc::Receiver<String>,
) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", o.port))
        .await
        .with_context(|| {
            format!(
                "listening on 127.0.0.1:{} for GitHub's redirect (use --port)",
                o.port
            )
        })?;
    let port = listener.local_addr()?.port();
    let state = random_hex(16)?;
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let page = form_page(&action(&o.owner, o.user, &state), &manifest(o, &redirect)?);

    writeln!(
        out,
        "Creating GitHub App {:?} under {}.\n\n1. Your browser opens a local page that sends GitHub the App's manifest.\n2. Review it on GitHub and click \"Create GitHub App\".\n3. GitHub redirects to {redirect}, which this command is listening on.\n\nIf the browser is on another machine, that redirect won't load: paste its address here instead.",
        o.name, o.owner
    )?;
    let start = format!("http://127.0.0.1:{port}/");
    match &o.open {
        Some(f) => f(&start),
        None => open_browser(&start),
    }

    let (tx, mut rx) = mpsc::channel::<String>(2);
    let server = tokio::spawn(serve(listener, page, state.clone(), port, tx));
    let code = tokio::select! {
        Some(code) = rx.recv() => code,
        Some(line) = paste.recv() => parse_pasted(&line, &state)?,
        _ = tokio::time::sleep(o.wait) => bail!("gave up waiting for GitHub's redirect"),
    };
    server.abort();
    writeln!(out, "Got the code from GitHub.")?;

    let made = convert(gh, &code).await?;
    let saved = save_created(&made, &o.owner, file)?;
    writeln!(
        out,
        "✔ Created GitHub App {:?} (id {}): {}",
        made.slug, made.id, made.html_url
    )?;
    writeln!(out, "✔ Wrote the App's credentials to {}", saved.display())?;
    if !made.owner_login.eq_ignore_ascii_case(&o.owner) {
        bail!(
            "the App was created under {:?}, not {:?} (signed in as someone else?); its credentials are in {}, which is for {:?}'s agents, not {:?}'s",
            made.owner_login, o.owner, saved.display(), made.owner_login, o.owner
        );
    }
    Ok(())
}

/// GitHub's "new App" URL: under the organisation, or under the signed-in user.
fn action(owner: &str, user: bool, state: &str) -> String {
    if user {
        format!("https://github.com/settings/apps/new?state={state}")
    } else {
        format!("https://github.com/organizations/{owner}/settings/apps/new?state={state}")
    }
}

pub fn manifest(o: &Options, redirect: &str) -> Result<String> {
    let mut perms: std::collections::BTreeMap<&str, &str> =
        DEFAULT_PERMISSIONS.into_iter().collect();
    for p in &o.with {
        perms.insert(p, "write");
    }
    Ok(json!({
        "name": o.name,
        // A homepage is required; the App serves nothing there.
        "url": format!("https://github.com/{}", o.owner),
        "redirect_url": redirect,
        "public": o.public,
        "default_permissions": perms,
        "default_events": [],
    })
    .to_string())
}

fn form_page(action: &str, manifest: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Create GitHub App</title></head>\n<body style=\"font-family:system-ui;padding:2rem\">\n<form id=\"f\" method=\"post\" action=\"{}\">\n<input type=\"hidden\" name=\"manifest\" value=\"{}\">\n<p>Redirecting to GitHub… <button type=\"submit\">Continue</button></p>\n</form>\n<script>document.getElementById('f').submit()</script>\n</body></html>\n",
        escape(action),
        escape(manifest)
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const DONE_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>GitHub App created</title></head>\n<body style=\"font-family:system-ui;padding:2rem\"><p>GitHub App created. You can close this tab and return to the terminal.</p></body></html>\n";

/// Serves the form at `/` and takes GitHub's redirect at `/callback`. Only a
/// local `Host` is answered (a page on another site cannot be rebound to it).
async fn serve(
    listener: TcpListener,
    page: String,
    state: String,
    port: u16,
    tx: mpsc::Sender<String>,
) {
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let (page, state, tx) = (page.clone(), state.clone(), tx.clone());
        tokio::spawn(async move {
            let Ok(head) = read_head(&mut sock).await else {
                return;
            };
            let (status, body) = match route(&head, &page, &state, port) {
                Route::Page(p) => ("200 OK", p),
                Route::Done(code) => {
                    let _ = tx.send(code).await;
                    ("200 OK", DONE_PAGE.to_string())
                }
                // A stale tab or another local program must not end the setup:
                // only a valid code does.
                Route::Bad(why) => ("400 Bad Request", why),
                Route::Ignore => ("404 Not Found", "not found".to_string()),
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
    }
}

async fn read_head(sock: &mut TcpStream) -> Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut chunk)).await??;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 8192 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

enum Route {
    Page(String),
    Done(String),
    Bad(String),
    Ignore,
}

fn route(head: &str, page: &str, state: &str, port: u16) -> Route {
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, target) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    let host = lines
        .find_map(|l| l.strip_prefix("Host:").or_else(|| l.strip_prefix("host:")))
        .map(str::trim)
        .unwrap_or("");
    let local = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    if method != "GET" || !local.iter().any(|h| h == host) {
        return Route::Ignore;
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match path {
        "/" => Route::Page(page.to_string()),
        "/callback" => {
            let get = |k: &str| {
                query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix(k).and_then(|v| v.strip_prefix('=')))
                    .unwrap_or("")
            };
            if get("state") != state {
                Route::Bad(
                    "The state in GitHub's redirect doesn't match this setup. Start again.".into(),
                )
            } else if !valid_code(get("code")) {
                Route::Bad("GitHub's redirect has no valid code.".into())
            } else {
                Route::Done(get("code").to_string())
            }
        }
        _ => Route::Ignore,
    }
}

fn valid_code(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 128
        && c.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A redirect URL, or the bare code, typed at the prompt.
pub fn parse_pasted(line: &str, state: &str) -> Result<String> {
    let line = line.trim();
    let Some((_, query)) = line.split_once('?') else {
        return if valid_code(line) {
            Ok(line.into())
        } else {
            Err(anyhow!("that is neither GitHub's redirect URL nor a code"))
        };
    };
    let get = |k: &str| {
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(k).and_then(|v| v.strip_prefix('=')))
            .unwrap_or("")
    };
    if get("state") != state {
        bail!("the state in that URL doesn't match this setup");
    }
    if !valid_code(get("code")) {
        bail!("that URL has no valid code");
    }
    Ok(get("code").into())
}

struct Created {
    id: u64,
    slug: String,
    client_id: String,
    pem: String,
    html_url: String,
    owner_login: String,
}

/// Exchanges the manifest's one-time code for the App's credentials.
async fn convert(gh: &GitHub, code: &str) -> Result<Created> {
    #[derive(Deserialize)]
    struct Owner {
        login: String,
    }
    #[derive(Deserialize)]
    struct R {
        id: u64,
        slug: String,
        client_id: String,
        pem: String,
        html_url: String,
        owner: Owner,
    }
    let url = format!("{}/app-manifests/{code}/conversions", gh.api());
    let r = http::send(Method::POST, &url, None, None)
        .await
        .context("converting the manifest")?;
    if r.status != 201 {
        let msg = serde_json::from_slice::<serde_json::Value>(&r.body)
            .ok()
            .and_then(|v| v["message"].as_str().map(str::to_string))
            .unwrap_or_default();
        bail!("converting the manifest: HTTP {}: {msg}", r.status);
    }
    let r: R = serde_json::from_slice(&r.body).context("decoding GitHub's response")?;
    if r.pem.is_empty() || r.client_id.is_empty() {
        bail!("GitHub's response has no private key or client ID");
    }
    Ok(Created {
        id: r.id,
        slug: r.slug,
        client_id: r.client_id,
        pem: r.pem,
        html_url: r.html_url,
        owner_login: r.owner.login,
    })
}

/// Writes the new App's credentials. They go to the owner's file, unless
/// GitHub made the App under another account: then to that account's file, or
/// a recovery file, so they never serve the wrong owner's agents. The private
/// key is shown once, so this must not lose it.
fn save_created(made: &Created, expected: &str, file: &Path) -> Result<PathBuf> {
    let text = credentials_yaml(made);
    let dir = file.parent().unwrap_or(Path::new("."));
    let mut candidates = vec![file.to_path_buf()];
    if !made.owner_login.eq_ignore_ascii_case(expected) {
        candidates = vec![
            dir.join(format!("{}.yaml", made.owner_login.to_ascii_lowercase())),
            dir.join(format!("recovered-{}.yaml", made.slug)),
        ];
    }
    for c in &candidates {
        if write_new(c, text.as_bytes()).is_ok() {
            return Ok(c.clone());
        }
    }
    bail!(
        "the App was created ({}) but its credentials could not be saved; the private key is shown once and is now lost: delete the App on GitHub and start again",
        made.html_url
    )
}

fn credentials_yaml(made: &Created) -> String {
    let mut s = String::from(
        "# GitHub App credentials for zapper. Generated by `zapper github setup`.\n# Keep this file mode 0600; it holds the App's private key.\ngithub:\n",
    );
    s.push_str(&format!(
        "  appId: \"{}\"\n  appSlug: {}\n  clientId: {}\n  privateKey: |\n",
        made.id,
        json_str(&made.slug),
        json_str(&made.client_id)
    ));
    for l in made.pem.trim_end().lines() {
        s.push_str(&format!("    {l}\n"));
    }
    s
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// Writes a 0600 file that must not exist yet, atomically: a half-written
/// key is never seen, and an existing file is never replaced.
pub fn write_new(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no directory", path.display()))?;
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), random_hex(4)?));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    linked.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            anyhow!("{} already exists; it is not overwritten, since GitHub shows an App's private key only once", path.display())
        } else {
            anyhow!("writing {}: {e}", path.display())
        }
    })
}

async fn ensure_installed(
    o: &Options,
    gh: &GitHub,
    app: &App,
    out: &mut (dyn std::io::Write + Send),
) -> Result<Installation> {
    if let Some(i) = gh.installation_for_owner(app, &o.owner).await? {
        writeln!(
            out,
            "✔ The App is installed on {} (installation {}).",
            o.owner, i.id
        )?;
        warn_selection(&i, out)?;
        return Ok(i);
    }
    let slug = app.slug.as_deref().ok_or_else(|| {
        anyhow!("the App is not installed on {} and its credentials file has no appSlug to build the install link from; install it from the App's settings page", o.owner)
    })?;
    let link = format!("https://github.com/apps/{slug}/installations/new");
    writeln!(out, "\nThe App is not installed on {} yet. Install it here, choosing \"Only select repositories\" so its reach is deliberate:\n  {link}\nWaiting for the installation…", o.owner)?;
    match &o.open {
        Some(f) => f(&link),
        None => open_browser(&link),
    }
    let deadline = tokio::time::Instant::now() + o.wait;
    loop {
        if let Some(i) = gh.installation_for_owner(app, &o.owner).await? {
            writeln!(out, "✔ Installed on {} (installation {}).", o.owner, i.id)?;
            warn_selection(&i, out)?;
            return Ok(i);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "the App was not installed on {} within {:?}",
                o.owner,
                o.wait
            );
        }
        tokio::time::sleep(o.poll).await;
    }
}

fn warn_selection(i: &Installation, out: &mut (dyn std::io::Write + Send)) -> Result<()> {
    if i.repository_selection == "all" {
        writeln!(out, "warning: the App is installed on all of the account's repositories. Limit it to the ones agents should reach (\"Only select repositories\" in the installation's settings).")?;
    }
    Ok(())
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn random_hex(n: usize) -> Result<String> {
    let mut b = vec![0u8; n];
    SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| anyhow!("no randomness"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_redirects_are_checked() {
        assert_eq!(parse_pasted("abc_123-X", "s").unwrap(), "abc_123-X");
        assert_eq!(
            parse_pasted(" http://127.0.0.1:8765/callback?code=abc&state=s \n", "s").unwrap(),
            "abc"
        );
        assert!(parse_pasted("http://127.0.0.1:8765/callback?code=abc&state=other", "s").is_err());
        assert!(parse_pasted("http://x/callback?code=a/b&state=s", "s").is_err());
        assert!(parse_pasted("not a code!", "s").is_err());
    }

    #[test]
    fn the_callback_needs_the_state_and_a_local_host() {
        let req =
            |host: &str, target: &str| format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n\r\n");
        let ok = route(
            &req("127.0.0.1:9", "/callback?code=abc&state=s"),
            "page",
            "s",
            9,
        );
        assert!(matches!(ok, Route::Done(c) if c == "abc"));
        assert!(matches!(
            route(
                &req("127.0.0.1:9", "/callback?code=abc&state=x"),
                "p",
                "s",
                9
            ),
            Route::Bad(_)
        ));
        assert!(matches!(
            route(&req("127.0.0.1:9", "/callback?state=s"), "p", "s", 9),
            Route::Bad(_)
        ));
        assert!(matches!(
            route(
                &req("evil.example:9", "/callback?code=abc&state=s"),
                "p",
                "s",
                9
            ),
            Route::Ignore
        ));
        assert!(
            matches!(route(&req("127.0.0.1:9", "/"), "page", "s", 9), Route::Page(p) if p == "page")
        );
        assert!(matches!(
            route(
                "POST /callback HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n",
                "p",
                "s",
                9
            ),
            Route::Ignore
        ));
    }

    #[test]
    fn the_manifest_asks_for_no_workflows_unless_told_to() {
        let mut o = Options::new("acme", false);
        let m: serde_json::Value =
            serde_json::from_str(&manifest(&o, "http://127.0.0.1:1/callback").unwrap()).unwrap();
        assert_eq!(m["default_permissions"]["contents"], "write");
        assert!(m["default_permissions"].get("workflows").is_none());
        assert_eq!(m["default_events"], json!([]));
        o.with = vec!["workflows".into(), "checks".into()];
        let m: serde_json::Value = serde_json::from_str(&manifest(&o, "x").unwrap()).unwrap();
        assert_eq!(m["default_permissions"]["workflows"], "write");
        assert_eq!(m["default_permissions"]["checks"], "write");
    }

    #[test]
    fn the_form_escapes_what_it_embeds() {
        let p = form_page("https://x/?a=1&b=2", r#"{"name":"a\"><script>"}"#);
        assert!(!p.contains("<script>alert") && !p.contains("\"><script>"));
        assert!(p.contains("&quot;") && p.contains("&amp;b=2"));
    }

    #[test]
    fn credentials_are_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("zapper-setup-test-{}", std::process::id()));
        let f = dir.join("sub").join("acme.yaml");
        write_new(&f, b"one").unwrap();
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(f.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let e = write_new(&f, b"two").unwrap_err().to_string();
        assert!(e.contains("not overwritten"), "{e}");
        assert_eq!(std::fs::read(&f).unwrap(), b"one");
        assert_eq!(
            std::fs::read_dir(f.parent().unwrap()).unwrap().count(),
            1,
            "no temp file is left"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
