//! The `zapper` binary against a daemon in the test: the API served on a
//! socket of its own over a `TestCore` (no VMs), and `init` and `check`,
//! which need none.

use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::Value;
use zapper_core::testing::TestCore;
use zapper_core::NewSession;

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

struct Daemon {
    t: TestCore,
    socket: PathBuf,
}

/// A daemon whose config is `yaml`, its API on a socket in its directory.
async fn daemon(t: TestCore) -> Daemon {
    let socket = t.dir.join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let app = zapper_api::api(t.core.clone(), &zapper_api::Mount::default());
    tokio::spawn(async move { axum::serve(listener, app).await });
    Daemon { t, socket }
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

fn session(d: &Daemon, profile: &str) -> String {
    let new = NewSession::plain(profile.into(), d.t.workdir(), Default::default());
    d.t.core.test_session(new, true).unwrap().id.clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn ps_lists_sessions() {
    let d = daemon(TestCore::new(CONFIG)).await;
    let text = ok(zapper(&d, &["ps"]).await);
    assert_eq!(text.lines().count(), 1, "the header only: {text}");
    assert!(text.starts_with("ID"), "{text}");

    let id = session(&d, "web");
    let text = ok(zapper(&d, &["ps"]).await);
    let row = text.lines().nth(1).unwrap();
    assert!(row.starts_with(&format!("{id}  web")), "{text}");
    assert!(row.contains("running"), "{text}");

    let v: Value = serde_json::from_str(&ok(zapper(&d, &["--json", "ps"]).await)).unwrap();
    assert_eq!(v[0]["id"], id.as_str());
}

#[tokio::test(flavor = "multi_thread")]
async fn tell_gives_a_driven_session_a_task() {
    let d = daemon(TestCore::new(CONFIG)).await;
    let bot = session(&d, "bot");
    let text = ok(zapper(&d, &["tell", &bot, "fix", "the", "build"]).await);
    assert_eq!(text, format!("queued for {bot} (the agent is idle)\n"));
    let text = ok(zapper(&d, &["tell", &bot, "and the tests"]).await);
    assert_eq!(
        text,
        format!("queued for {bot}: 2 waiting (the agent is idle)\n")
    );

    let web = session(&d, "web");
    let e = failed(zapper(&d, &["tell", &web, "hi"]).await);
    assert!(e.contains("is not driven"), "{e}");
    let e = failed(zapper(&d, &["tell", "nosuch", "hi"]).await);
    assert!(e.contains("nosuch"), "{e}");

    let mut worker = NewSession::plain("bot".into(), d.t.workdir(), Default::default());
    worker.labels = [("firm", "acme"), ("member", "pat")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let worker = d.t.core.test_session(worker, true).unwrap();
    let e = failed(zapper(&d, &["tell", &worker.id, "hi"]).await);
    assert!(
        e.contains("pat works for firm acme: mail them instead"),
        "{e}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tell_with_the_driver_off() {
    // No profile can be driven then: that config doesn't load.
    let d = daemon(TestCore::with_daemon(
        "driver: false",
        "profiles:\n  web: {}\n",
    ))
    .await;
    let web = session(&d, "web");
    let e = failed(zapper(&d, &["tell", &web, "hi"]).await);
    assert!(e.contains("the driver is disabled"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn rules_changed_diffed_and_saved() {
    let d = daemon(TestCore::new(CONFIG)).await;
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
    let d = daemon(TestCore::new(CONFIG)).await;
    let v: Value = serde_json::from_str(&ok(zapper(&d, &["--json", "approvals"]).await)).unwrap();
    assert_eq!(v, serde_json::json!([]));
    for verb in ["approve", "deny"] {
        let e = failed(zapper(&d, &[verb, "nosuch"]).await);
        assert!(e.starts_with("zapper: "), "{e}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_daemon_the_error_says_how_to_start_one() {
    let t = TestCore::new(CONFIG);
    let e = failed(run(&t.startup(), None, &["ps"]).await);
    assert!(e.contains("is `zapper serve` running?"), "{e}");
}

/// `zapper serve` itself: the API on the socket it is given, which the CLI
/// answers through.
#[tokio::test(flavor = "multi_thread")]
async fn serve_listens_on_its_socket() {
    let t = TestCore::new(CONFIG);
    let socket = t.dir.join("run").join("api.sock");
    let mut serve = tokio::process::Command::new(env!("CARGO_BIN_EXE_zapper"))
        .arg("--config")
        .arg(t.startup())
        .arg("serve")
        .env("ZAPPER_SOCKET", &socket)
        .env("HOME", &t.dir)
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // Up within a minute, however loaded the machine (an instrumented
    // build under coverage starts slowly); a serve that exits says so.
    let mut up = false;
    for _ in 0..1200 {
        if tokio::net::UnixStream::connect(&socket).await.is_ok() {
            up = true;
            break;
        }
        if let Some(status) = serve.try_wait().unwrap() {
            panic!("zapper serve exited: {status}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        up,
        "zapper serve didn't listen on {} within a minute",
        socket.display()
    );
    let text = ok(run(&t.startup(), Some(&socket), &["rules"]).await);
    assert!(text.contains("global-deny"), "{text}");
    let text = ok(run(&t.startup(), Some(&socket), &["ps"]).await);
    assert_eq!(text.lines().count(), 1, "the header only: {text}");
    // With the office, its routes are mounted.
    #[cfg(feature = "office")]
    ok(run(&t.startup(), Some(&socket), &["firm", "ls"]).await);
    serve.start_kill().unwrap();
}

/// The board approves a background check or objects: one of the two, and
/// `--clear`, the old name, still approves.
#[cfg(feature = "office")]
#[tokio::test(flavor = "multi_thread")]
async fn screen_needs_approve_or_object() {
    let d = daemon(TestCore::new(CONFIG)).await;
    let e = failed(zapper(&d, &["firm", "screen", "acme", "c-1"]).await);
    assert!(e.contains("give --approve or --object"), "{e}");
    let e = err(&zapper(
        &d,
        &["firm", "screen", "acme", "c-1", "--approve", "--object"],
    )
    .await);
    assert!(e.contains("cannot be used with"), "{e}");
    // Past parsing to the daemon, which has no such firm.
    for flag in ["--approve", "--clear"] {
        let e = failed(zapper(&d, &["firm", "screen", "acme", "c-1", flag]).await);
        assert!(e.starts_with("zapper: "), "{e}");
    }
}

/// Without the office, `zapper serve` refuses a config with firms.
#[cfg(not(feature = "office"))]
#[tokio::test]
async fn serve_without_the_office_refuses_firms() {
    let t = TestCore::new("");
    std::fs::write(
        t.startup(),
        std::fs::read_to_string(t.startup()).unwrap()
            + "profiles:\n  w: { agent: { kind: claude-code } }\nfirms:\n  acme:\n    roles: { ceo: { profile: w, skills: { work: ceo } } }\n    workers: [{ name: ceo, role: ceo }]\n",
    )
    .unwrap();
    let o = tokio::process::Command::new(env!("CARGO_BIN_EXE_zapper"))
        .arg("--config")
        .arg(t.startup())
        .arg("serve")
        .env("ZAPPER_SOCKET", t.dir.join("api.sock"))
        .env("HOME", &t.dir)
        .output()
        .await
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    let e = err(&o);
    assert!(e.contains("built without the office"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn services_added_listed_and_removed() {
    let d = daemon(TestCore::new(CONFIG)).await;
    let id = session(&d, "web");
    let text = ok(zapper(
        &d,
        &[
            "service",
            "add",
            "db.internal:5432",
            "--to-session",
            &format!("{id}:5432"),
            "--for",
            "team=a",
        ],
    )
    .await);
    assert_eq!(
        text,
        format!("service 1: db.internal:5432 to session {id} port 5432; sessions also need a net rule for it\n")
    );
    // Another client's, which the operator can't remove.
    let firms =
        d.t.core
            .add_service(zapper_core::ServiceSpec {
                name: "git.acme.internal".into(),
                port: 443,
                sessions: Default::default(),
                target: zapper_core::ServiceTarget::Addr("10.0.0.6:443".parse().unwrap()),
                owner: "firm acme".into(),
            })
            .unwrap();
    let text = ok(zapper(&d, &["service", "ls"]).await);
    let rows: Vec<&str> = text.lines().collect();
    assert!(rows[0].starts_with("ID"), "{text}");
    assert!(
        rows[1].contains("db.internal") && rows[1].contains("team=a"),
        "{text}"
    );
    assert!(
        rows[2].contains("all") && rows[2].contains("firm acme"),
        "{text}"
    );

    let e = failed(zapper(&d, &["service", "rm", &firms.to_string()]).await);
    assert!(e.contains("is firm acme's"), "{e}");
    let text = ok(zapper(&d, &["service", "rm", "1"]).await);
    assert_eq!(text, "removed service 1 (db.internal:5432)\n");
    let e = failed(zapper(&d, &["service", "add", "x.internal", "--to", "10.0.0.5:80"]).await);
    assert!(e.contains("name:port"), "{e}");
    let o = zapper(&d, &["service", "add", "x.internal:80"]).await;
    assert_eq!(
        o.status.code(),
        Some(2),
        "--to or --to-session: {}",
        err(&o)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn run_labels_are_checked_before_anything_starts() {
    let d = daemon(TestCore::new(CONFIG)).await;
    let dir = d.t.workdir();
    let dir = dir.to_str().unwrap();
    let e = failed(
        zapper(
            &d,
            &[
                "run",
                "-p",
                "web",
                "-C",
                dir,
                "-l",
                "firm=acme",
                "--",
                "true",
            ],
        )
        .await,
    );
    assert!(e.contains("label firm is the office's"), "{e}");
    let e = failed(
        zapper(
            &d,
            &["run", "-p", "web", "-C", dir, "-l", "team", "--", "true"],
        )
        .await,
    );
    assert!(e.contains("key=value"), "{e}");
}

// `init` and `check`: no daemon needed.

#[tokio::test]
async fn init_writes_the_config_and_skills_once() {
    let t = TestCore::new("");
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
    let d = daemon(TestCore::new(CONFIG)).await;
    let text = out(&zapper(&d, &["check"]).await);
    assert!(
        text.contains("note   the driver is on, for driven profiles: bot\n"),
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

    let t = TestCore::with_daemon("driver: false", "profiles:\n  web: {}\n");
    let text = out(&run(&t.startup(), None, &["check"]).await);
    assert!(
        text.contains("note   the driver is disabled (daemon.driver: false)"),
        "{text}"
    );
    assert!(text.contains("note   the daemon isn't running"), "{text}");
    let t = TestCore::new("profiles:\n  web: {}\n");
    let text = out(&run(&t.startup(), None, &["check"]).await);
    assert!(
        text.contains("note   the driver is on, unused: no firm, and no profile sets agent.driven"),
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

/// `zapper check` notes a firm config that is valid but likely not meant.
#[cfg(feature = "office")]
#[tokio::test]
async fn check_notes_officers_without_a_budget() {
    let t = TestCore::new(
        "profiles:\n  w: { agent: { kind: claude-code } }\nfirms:\n  acme:\n    budget: { seed: 1000, security_share: 0 }\n    security: { officers: [sec] }\n    roles:\n      ceo: { profile: w, skills: { work: ceo } }\n      sec: { profile: w, skills: { work: security-review } }\n    workers:\n      - { name: ceo, role: ceo }\n      - { name: sec, role: sec, reports_to: ceo }\n",
    );
    let text = out(&run(&t.startup(), None, &["check"]).await);
    let notes = lines_of(&text, "note   firm acme");
    assert_eq!(notes.len(), 1, "{text}");
    assert!(notes[0].contains("budget.security_share is 0"), "{text}");
    assert!(
        text.contains("note   the driver is on, for the workers of firm acme\n"),
        "{text}"
    );
}

#[cfg(feature = "office")]
#[tokio::test]
async fn check_reports_a_firms_missing_skills_until_init() {
    let t = TestCore::new(
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
