//! The `zapper` binary against `zapper serve` on a config and socket of its
//! own (no VMs, so no sessions), and `init` and `check`, which need no
//! daemon.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;

const CONFIG: &str = "
policy:
  net:
    - { id: global-deny, action: deny, host: '*.bad.example' }
profiles:
  web:
    policy:
      net:
        - { id: web-allow, action: allow, host: example.com }
  bot:
    agent: { kind: claude-code, driven: true }
";

/// A directory with a config whose daemon section points into it:
/// placeholder agent and Firecracker files, no UI.
struct Dir {
    dir: PathBuf,
}

impl Dir {
    fn new(daemon: &str, yaml: &str) -> Dir {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "zapper-cli-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["agent", "firecracker"] {
            std::fs::write(dir.join(f), f).unwrap();
        }
        let mut extra = String::new();
        for l in daemon.lines().filter(|l| !l.trim().is_empty()) {
            extra.push_str(&format!("  {}\n", l.trim()));
        }
        let d = dir.display();
        let config = format!(
            "daemon:\n  state: {d}/state\n  agent: {d}/agent\n  firecracker: {d}/firecracker\n  images: {d}/images\n  kernels: {d}/kernels\n  ui: \"\"\n{extra}{yaml}"
        );
        std::fs::write(dir.join("zapper.yaml"), config).unwrap();
        Dir { dir }
    }

    fn startup(&self) -> PathBuf {
        self.dir.join("zapper.yaml")
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `zapper serve` on the directory's config, its API on a socket there.
struct Daemon {
    t: Dir,
    socket: PathBuf,
    serve: tokio::process::Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.serve.start_kill();
    }
}

async fn daemon(t: Dir) -> Daemon {
    let socket = t.dir.join("run").join("api.sock");
    let serve = tokio::process::Command::new(env!("CARGO_BIN_EXE_zapper"))
        .arg("--config")
        .arg(t.startup())
        .arg("serve")
        .env("ZAPPER_SOCKET", &socket)
        .env("HOME", &t.dir)
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(&socket).await.is_ok() {
            return Daemon { t, socket, serve };
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("zapper serve did not listen on {}", socket.display());
}

/// Runs `zapper` with the daemon's config and socket.
async fn zapper(d: &Daemon, args: &[&str]) -> Output {
    run(&d.t.startup(), Some(&d.socket), args).await
}

async fn run(config: &Path, socket: Option<&Path>, args: &[&str]) -> Output {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_zapper"));
    cmd.arg("--config").arg(config).args(args);
    cmd.env_remove("ZAPPER_CONFIG")
        .env_remove("XDG_RUNTIME_DIR");
    cmd.env("HOME", config.parent().unwrap());
    // With no daemon, a socket where nothing listens.
    let none = config.parent().unwrap().join("no-daemon.sock");
    cmd.env("ZAPPER_SOCKET", socket.unwrap_or(&none));
    cmd.output().await.unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Succeeded, with its output.
fn ok(o: Output) -> String {
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
    out(&o)
}

/// Failed with exit code 1, with its error.
fn failed(o: Output) -> String {
    assert_eq!(o.status.code(), Some(1), "{}{}", out(&o), err(&o));
    err(&o)
}

#[tokio::test(flavor = "multi_thread")]
async fn ps_lists_sessions() {
    let d = daemon(Dir::new("", CONFIG)).await;
    let text = ok(zapper(&d, &["ps"]).await);
    assert_eq!(text.lines().count(), 1, "the header only: {text}");
    assert!(text.starts_with("ID"), "{text}");
    let v: Value = serde_json::from_str(&ok(zapper(&d, &["--json", "ps"]).await)).unwrap();
    assert_eq!(v, serde_json::json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn tell_needs_a_session() {
    let d = daemon(Dir::new("", CONFIG)).await;
    let e = failed(zapper(&d, &["tell", "nosuch", "hi"]).await);
    assert!(e.contains("nosuch"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn rules_changed_diffed_and_saved() {
    let d = daemon(Dir::new("", CONFIG)).await;
    let text = ok(zapper(&d, &["rules"]).await);
    assert!(text.lines().next().unwrap().starts_with("SCOPE"), "{text}");
    for want in ["global-deny", "web-allow", "*.bad.example", "example.com"] {
        assert!(text.contains(want), "{want}: {text}");
    }
    let text = ok(zapper(&d, &["rules", "--scope", "profile:web"]).await);
    assert!(
        text.contains("web-allow") && !text.contains("global-deny"),
        "{text}"
    );

    assert_eq!(
        ok(zapper(&d, &["config", "diff"]).await),
        "no unsaved changes\n"
    );
    let rule = "{kind: net, action: allow, host: api.example.com}";
    let text = ok(zapper(&d, &["rule", "add", "profile:web", rule]).await);
    let id = text
        .strip_prefix("added ")
        .and_then(|t| t.split_once(' '))
        .map(|(id, _)| id.to_string())
        .unwrap_or_else(|| panic!("{text}"));
    assert!(
        text.contains("to profile:web; `zapper config save` to keep it"),
        "{text}"
    );
    assert!(ok(zapper(&d, &["config", "diff"]).await).contains("api.example.com"));

    let text = ok(zapper(&d, &["config", "save"]).await);
    assert!(text.starts_with("saved to "), "{text}");
    assert!(std::fs::read_to_string(d.t.startup())
        .unwrap()
        .contains("api.example.com"));
    assert_eq!(
        ok(zapper(&d, &["config", "diff"]).await),
        "no unsaved changes\n"
    );

    let text = ok(zapper(&d, &["rule", "rm", &id]).await);
    assert_eq!(text, format!("removed {id} from profile:web\n"));
    let e = failed(zapper(&d, &["rule", "rm", &id]).await);
    assert!(e.contains("no rule"), "{e}");
    let e = failed(zapper(&d, &["rule", "add", "profile:nosuch", rule]).await);
    assert!(e.contains("nosuch"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn answering_an_approval_that_isnt_there() {
    let d = daemon(Dir::new("", CONFIG)).await;
    let v: Value = serde_json::from_str(&ok(zapper(&d, &["--json", "approvals"]).await)).unwrap();
    assert_eq!(v, serde_json::json!([]));
    for verb in ["approve", "deny"] {
        let e = failed(zapper(&d, &[verb, "nosuch"]).await);
        assert!(e.starts_with("zapper: "), "{e}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_daemon_the_error_says_how_to_start_one() {
    let t = Dir::new("", CONFIG);
    let e = failed(run(&t.startup(), None, &["ps"]).await);
    assert!(e.contains("is `zapper serve` running?"), "{e}");
}

// `init` and `check`: no daemon needed.

#[tokio::test]
async fn init_writes_the_config_and_skills_once() {
    let t = Dir::new("", "");
    let config = t.dir.join("fresh").join("zapper.yaml");
    let text = ok(run(&config, None, &["init"]).await);
    assert!(
        text.starts_with(&format!("wrote {}", config.display())),
        "{text}"
    );
    let written = std::fs::read_to_string(&config).unwrap();
    let example = include_str!("../../../config/zapper.yaml");
    assert!(written.contains("profiles:"), "{written}");
    let skills = config.parent().unwrap().join("skills");
    if cfg!(feature = "office") || !example.contains("\nfirms:") {
        assert_eq!(written, example);
    } else {
        // Without the office: no firms, nor the comment above them.
        assert!(example.starts_with(&written));
        assert!(!written.contains("\nfirms:"), "{written}");
        let last = written.trim_end().lines().last().unwrap();
        assert!(!last.starts_with('#'), "{last}");
    }
    if cfg!(feature = "office") {
        assert!(
            text.contains(&format!("wrote skills to {}", skills.display())),
            "{text}"
        );
        assert!(skills.join("ceo/SKILL.md").is_file());
    } else {
        assert!(!skills.exists());
        assert_eq!(text.lines().count(), 1, "{text}");
    }
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&config).unwrap().permissions().mode();
    assert_eq!(mode & 0o077, 0, "the config is private");

    std::fs::write(&config, "# mine\n").unwrap();
    let text = ok(run(&config, None, &["init"]).await);
    assert!(text.contains("exists; not touching it"), "{text}");
    if cfg!(feature = "office") {
        assert!(text.contains("the default skills are all there"), "{text}");
    }
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "# mine\n");
}

#[cfg(feature = "office")]
/// `check`'s lines starting `prefix`.
fn lines_of(text: &str, prefix: &str) -> Vec<String> {
    text.lines()
        .filter(|l| l.starts_with(prefix))
        .map(str::to_string)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn check_reports_the_driver_and_the_daemon() {
    let d = daemon(Dir::new("", CONFIG)).await;
    let text = out(&zapper(&d, &["check"]).await);
    assert!(
        text.contains("note   the driver is on: driven profiles: bot\n"),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "ok     daemon listening on {}\n",
            d.socket.display()
        )),
        "{text}"
    );
    assert!(text.contains("ok     config "), "{text}");
    assert!(
        text.contains("note   the record is written by this user"),
        "{text}"
    );

    let t = Dir::new("driver: false", "profiles:\n  web: {}\n");
    let text = out(&run(&t.startup(), None, &["check"]).await);
    assert!(
        text.contains("note   the driver is disabled (daemon.driver: false)"),
        "{text}"
    );
    assert!(text.contains("note   the daemon isn't running"), "{text}");
    let t = Dir::new("", "profiles:\n  web: {}\n");
    let text = out(&run(&t.startup(), None, &["check"]).await);
    assert!(
        text.contains("no profile is driven (agent.driven)"),
        "{text}"
    );

    // A config that doesn't load.
    let bad = t.dir.join("bad.yaml");
    std::fs::write(&bad, "profiles: [").unwrap();
    let o = run(&bad, None, &["check"]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        out(&o).starts_with(&format!("ERROR  config {}", bad.display())),
        "{}",
        out(&o)
    );
}

#[cfg(feature = "office")]
#[tokio::test]
async fn check_reports_a_firms_missing_skills_until_init() {
    let t = Dir::new(
        "",
        "profiles:\n  w: { agent: { kind: claude-code } }\nfirms:\n  acme:\n    roles:\n      ceo: { profile: w, skills: { work: ceo } }\n    workers:\n      - { name: ceo, role: ceo }\n",
    );
    let skills = t.dir.join("skills");
    let text = out(&run(&t.startup(), None, &["check"]).await);
    let firm = lines_of(&text, "ERROR  firm acme");
    assert_eq!(
        firm,
        [format!("ERROR  firm acme: skills in {}", skills.display())],
        "{text}"
    );
    assert!(
        text.contains(
            "fix: zapper init (writes the default skills), or add <name>/SKILL.md there for: ceo"
        ),
        "{text}"
    );

    ok(run(&t.startup(), None, &["init"]).await);
    let text = out(&run(&t.startup(), None, &["check"]).await);
    assert_eq!(
        lines_of(&text, "ok     firm acme"),
        [format!("ok     firm acme: skills in {}", skills.display())],
        "{text}"
    );
}
