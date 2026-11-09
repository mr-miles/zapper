//! The warden on the in-memory gateway: what it makes, from what, for whom,
//! within which caps, and what an agent's stream carries.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use hyper::{Method, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zapper_openshell::edge::Backend;
use zapper_openshell::Labels;

use crate::audit::Audit;
use crate::config::Config;
use crate::fake::Fake;
use crate::gateway::Gateway;
use crate::names;
use crate::profiles::Profiles;
use crate::warden::{route, Refusal, Route, Warden, AGENT_GREETING};

const ZAPPER: &str = include_str!("../../../config/zapper.yaml");
const EDGE: &str = "http://host.openshell.internal:8970";
const FAKE: &str = "zapperclaw-fake";

fn profiles(config: &Config) -> Profiles {
    let zapper = zapper_policy::Config::from_yaml(ZAPPER).expect("the checkout's zapper.yaml");
    Profiles::prepare(config, &zapper, EDGE).expect("the profiles")
}

fn config() -> Config {
    let text = format!(
        "office: {{ profile: zapperclaw-office }}\nmax_workers: 3\nprofiles:\n  {FAKE}: {{ max: 2 }}\n  claude: {{ proposals: false }}\n"
    );
    Config::from_yaml(&text, Path::new("/tmp")).unwrap()
}

/// A warden on a fresh fake, its office made.
async fn warden() -> (Arc<Fake>, Warden) {
    let fake = Arc::new(Fake::default());
    let gw: Arc<dyn Gateway> = fake.clone();
    let config = config();
    let profiles = profiles(&config);
    let office = Warden::ensure_office(gw.as_ref(), &profiles, &Audit::none())
        .await
        .unwrap();
    let w = Warden::new(gw, config, profiles, office, Audit::none());
    (fake, w)
}

fn refusal(e: &anyhow::Error) -> &Refusal {
    e.downcast_ref::<Refusal>()
        .unwrap_or_else(|| panic!("not a refusal: {e:#}"))
}

#[tokio::test]
async fn the_office_is_made_once_from_its_template_with_its_providers_only() {
    let (fake, w) = warden().await;
    assert_eq!(
        fake.calls(),
        ["create zc-office zc-office warden", "setup zc-office"]
    );
    let again = Warden::ensure_office(&*fake, &profiles(&config()), &Audit::none())
        .await
        .unwrap();
    assert_eq!(again, w.office_id(), "found, not made again");
    assert_eq!(fake.calls().len(), 2);
}

#[tokio::test]
async fn a_sandbox_named_as_the_office_that_the_warden_didnt_make_is_refused() {
    let fake = Fake::default();
    fake.add(names::OFFICE, Some("wb-dev"), Labels::new());
    let e = Warden::ensure_office(&fake, &profiles(&config()), &Audit::none())
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("didn't make"), "{e:#}");
    assert!(fake.calls().is_empty(), "nothing made or deleted");
}

#[tokio::test]
async fn a_worker_is_made_from_its_profiles_template_and_only_once() {
    let (fake, w) = warden().await;
    let (ada, made) = w.put_worker("ada", FAKE).await.unwrap();
    assert!(made);
    assert_eq!(ada.sandbox, "zcw-ada");
    assert_eq!(ada.profile, FAKE);
    let calls = fake.calls();
    assert_eq!(
        calls[2..],
        [
            format!("create zcp-{FAKE} zcw-ada anthropic,office-edge"),
            "proposals zcw-ada".to_string(),
            "setup zcw-ada".to_string(),
        ]
    );
    let s = fake.sandboxes.lock().unwrap()[1].clone();
    assert_eq!(s.labels, names::worker_labels("ada", FAKE));
    let (again, made) = w.put_worker("ada", FAKE).await.unwrap();
    assert!(!made, "there already");
    assert_eq!(again, ada);
    assert_eq!(fake.calls().len(), calls.len(), "nothing made again");
    let e = w.put_worker("ada", "claude").await.unwrap_err();
    assert!(matches!(refusal(&e), Refusal::Conflict(_)), "{e:#}");
    // A profile without proposals isn't given them.
    w.put_worker("bob", "claude").await.unwrap();
    assert!(!fake.calls().contains(&"proposals zcw-bob".to_string()));
}

#[tokio::test]
async fn only_offered_profiles_and_good_names_are_made() {
    let (fake, w) = warden().await;
    for (name, profile) in [
        ("ada", "k3s"),
        ("ada", "zapperclaw-office"),
        ("ada", "../x"),
        ("Ada", FAKE),
        ("a--b", FAKE),
        ("", FAKE),
    ] {
        let e = w.put_worker(name, profile).await.unwrap_err();
        assert!(
            matches!(refusal(&e), Refusal::Bad(_)),
            "{name} {profile}: {e:#}"
        );
    }
    assert_eq!(fake.calls().len(), 2, "only the office was made");
}

#[tokio::test]
async fn the_caps_hold_for_a_profile_and_for_all() {
    let (_fake, w) = warden().await;
    w.put_worker("a", FAKE).await.unwrap();
    w.put_worker("b", FAKE).await.unwrap();
    let e = w.put_worker("c", FAKE).await.unwrap_err();
    assert!(
        matches!(refusal(&e), Refusal::Full(_)),
        "the profile's max: {e:#}"
    );
    w.put_worker("c", "claude").await.unwrap();
    let e = w.put_worker("d", "claude").await.unwrap_err();
    assert!(
        matches!(refusal(&e), Refusal::Full(_)),
        "max_workers: {e:#}"
    );
    w.delete_worker("a").await.unwrap();
    w.put_worker("d", "claude").await.unwrap();
}

#[tokio::test]
async fn a_worker_whose_setup_fails_is_deleted() {
    let (fake, w) = warden().await;
    fake.fail_setup.store(true, Ordering::SeqCst);
    assert!(w.put_worker("ada", FAKE).await.is_err());
    assert!(fake.calls().contains(&"delete zcw-ada".to_string()));
    assert!(w.workers().await.unwrap().is_empty());
}

#[tokio::test]
async fn sandboxes_it_didnt_make_are_neither_listed_nor_touched() {
    let (fake, w) = warden().await;
    // Labelled as a worker, but not from the profile's template.
    fake.add("zcw-eve", Some("wb-dev"), names::worker_labels("eve", FAKE));
    // From the template, but under another name.
    let template = names::profile_template(FAKE);
    fake.add(
        "eve",
        Some(template.as_str()),
        names::worker_labels("eve", FAKE),
    );
    fake.add("other", None, Labels::new());
    assert!(w.workers().await.unwrap().is_empty());
    let e = w.put_worker("eve", FAKE).await.unwrap_err();
    assert!(matches!(refusal(&e), Refusal::Conflict(_)), "{e:#}");
    let e = w.delete_worker("eve").await.unwrap_err();
    assert!(matches!(refusal(&e), Refusal::NotFound(_)), "{e:#}");
    assert!(!fake.calls().iter().any(|c| c.starts_with("delete")));
    assert_eq!(fake.sandboxes.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn an_agent_is_its_profiles_harness_and_reaches_the_office_with_its_ticket() {
    let (fake, w) = warden().await;
    w.put_worker("ada", FAKE).await.unwrap();
    let ticket = "0123456789abcdef".repeat(2);
    let resume = "3f2a9c4e-1b2d-4c5e-8f90-123456789abc";
    w.start_agent("ada", &ticket, Some(resume)).await.unwrap();

    let exec = fake.execs.lock().unwrap()[0].clone();
    assert_eq!(exec.sandbox, "zcw-ada");
    assert_eq!(exec.argv[..2], ["claude", "-p"], "the harness's command");
    assert_eq!(exec.argv[exec.argv.len() - 2..], ["--resume", resume]);
    assert_eq!(exec.env["ZAPPER_SESSION"], "zcw-ada");
    assert_eq!(exec.env["ZAPPER_OFFICE_URL"], format!("{EDGE}/mcp"));
    assert_eq!(exec.env["ZAPPERCLAW_PROFILE"], FAKE);
    assert!(!exec.workdir.is_empty());
    assert_eq!(
        fake.forwards.lock().unwrap()[0],
        (names::OFFICE.to_string(), 8973)
    );

    let mut agent = fake.exec_ends.lock().unwrap().pop().unwrap();
    let mut office = fake.forward_ends.lock().unwrap().pop().unwrap();
    let greeting = format!("{AGENT_GREETING} {ticket}\n");
    let mut got = vec![0u8; greeting.len()];
    office.read_exact(&mut got).await.unwrap();
    assert_eq!(String::from_utf8(got).unwrap(), greeting);
    agent.write_all(b"from the agent").await.unwrap();
    let mut got = [0u8; 14];
    office.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"from the agent");
    office.write_all(b"to the agent").await.unwrap();
    let mut got = [0u8; 12];
    agent.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"to the agent");
}

#[tokio::test]
async fn an_agent_starts_only_as_asked_in_plain_terms() {
    let (fake, w) = warden().await;
    w.put_worker("ada", FAKE).await.unwrap();
    let ticket = "ab".repeat(16);
    for (name, t, resume) in [
        ("ada", "short", None),
        (
            "ada",
            ticket.as_str(),
            Some("--dangerously-skip-permissions"),
        ),
        ("ada", ticket.as_str(), Some("a b")),
        ("Ada", ticket.as_str(), None),
    ] {
        let e = w.start_agent(name, t, resume).await.unwrap_err();
        assert!(matches!(refusal(&e), Refusal::Bad(_)), "{e:#}");
    }
    let e = w.start_agent("bob", &ticket, None).await.unwrap_err();
    assert!(matches!(refusal(&e), Refusal::NotFound(_)), "{e:#}");
    assert!(fake.execs.lock().unwrap().is_empty(), "nothing ran");
    assert!(fake.forwards.lock().unwrap().is_empty());
    w.start_agent("ada", &ticket, None).await.unwrap();
    let exec = fake.execs.lock().unwrap()[0].clone();
    assert!(!exec.argv.contains(&"--resume".to_string()));
}

#[tokio::test]
async fn a_worker_has_one_agent_at_a_time() {
    let (fake, w) = warden().await;
    w.put_worker("ada", FAKE).await.unwrap();
    w.put_worker("bob", FAKE).await.unwrap();
    let ticket = "cd".repeat(16);
    w.start_agent("ada", &ticket, None).await.unwrap();
    let e = w.start_agent("ada", &ticket, None).await.unwrap_err();
    assert!(matches!(refusal(&e), Refusal::Conflict(_)), "{e:#}");
    w.start_agent("bob", &ticket, None).await.unwrap();
    assert_eq!(
        fake.execs.lock().unwrap().len(),
        2,
        "ada's second never ran"
    );
    // Both ends of ada's stream close: it ends, and ada may start again.
    let agent = fake.exec_ends.lock().unwrap().remove(0);
    let office = fake.forward_ends.lock().unwrap().remove(0);
    drop((agent, office));
    let mut again = None;
    for _ in 0..100 {
        tokio::task::yield_now().await;
        if let Ok(()) = w.start_agent("ada", &ticket, None).await {
            again = Some(());
            break;
        }
    }
    assert!(
        again.is_some(),
        "ada's agent starts again once its stream ended"
    );
}

#[tokio::test]
async fn only_the_office_sandbox_is_a_caller() {
    let (fake, w) = warden().await;
    let office = w.office_id().to_string();
    assert_eq!(w.caller(&office).unwrap().session, names::OFFICE);
    let (ada, _) = w.put_worker("ada", FAKE).await.unwrap();
    assert!(w.caller(&ada.id).is_none(), "a worker is no caller");
    assert!(w.caller("").is_none());
    drop(fake);
}

#[tokio::test]
async fn the_jwks_is_the_drivers() {
    let (_fake, w) = warden().await;
    assert_eq!(w.jwks().await.unwrap()["keys"][0]["kid"], "k1");
}

#[test]
fn routes_are_these_five_and_nothing_else() {
    let r = |m: Method, p: &str| route(&m, p);
    assert_eq!(r(Method::GET, "/workers"), Some(Route::List));
    assert_eq!(r(Method::GET, "/workers/"), Some(Route::List));
    assert_eq!(
        r(Method::PUT, "/workers/ada"),
        Some(Route::Put("ada".into()))
    );
    assert_eq!(
        r(Method::DELETE, "/workers/ada"),
        Some(Route::Delete("ada".into()))
    );
    assert_eq!(
        r(Method::POST, "/workers/ada/agent"),
        Some(Route::Agent("ada".into()))
    );
    assert_eq!(r(Method::GET, "/jwks"), Some(Route::Jwks));
    assert_eq!(r(Method::POST, "/workers"), Some(Route::Method("GET")));
    assert_eq!(
        r(Method::GET, "/workers/ada"),
        Some(Route::Method("PUT, DELETE"))
    );
    assert_eq!(
        r(Method::GET, "/workers/ada/agent"),
        Some(Route::Method("POST"))
    );
    for p in [
        "/",
        "/mcp",
        "/workers/ada/exec",
        "/workers/ada/agent/x",
        "/oauth",
    ] {
        assert_eq!(r(Method::GET, p), None, "{p}");
    }
}

#[test]
fn refusals_have_the_status_that_says_what_to_do() {
    for (e, status) in [
        (Refusal::Bad("x".into()), StatusCode::BAD_REQUEST),
        (Refusal::NotFound("x".into()), StatusCode::NOT_FOUND),
        (Refusal::Conflict("x".into()), StatusCode::CONFLICT),
        (Refusal::Full("x".into()), StatusCode::TOO_MANY_REQUESTS),
    ] {
        assert_eq!(crate::warden::status_of(&anyhow::Error::from(e)), status);
    }
    assert_eq!(
        crate::warden::status_of(&anyhow::anyhow!("the gateway is down")),
        StatusCode::BAD_GATEWAY
    );
}

#[test]
fn a_worker_profile_is_wired_to_the_office_and_the_office_to_the_warden() {
    let config = config();
    let p = profiles(&config);
    let w = &p.workers[FAKE];
    assert_eq!(w.template, format!("zcp-{FAKE}"));
    assert_eq!(w.setup.env["ZAPPER_OFFICE_URL"], format!("{EDGE}/mcp"));
    assert_eq!(
        w.profile.agent.turn_end.as_deref(),
        Some("/usr/local/bin/zapper-agent office stop-hook")
    );
    assert_eq!(
        w.profile.agent.mcp_servers.as_ref().unwrap()["office"]["url"],
        format!("{EDGE}/mcp"),
        "Claude Code reaches the office's tools over HTTP"
    );
    let settings = w
        .setup
        .files
        .iter()
        .find(|f| f.path.ends_with("/.claude/settings.json"))
        .expect("Claude Code's settings");
    let settings: serde_json::Value = serde_json::from_str(&settings.content).unwrap();
    assert_eq!(
        settings["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "/usr/local/bin/zapper-agent office instructions",
        "its session starts with its brief, from the edge"
    );
    assert_eq!(
        settings["hooks"]["Stop"][0]["hooks"][0]["command"],
        "/usr/local/bin/zapper-agent office stop-hook"
    );
    assert_eq!(p.office.template, names::OFFICE);
    assert_eq!(p.office.providers, ["warden"]);
    assert_eq!(
        p.office.setup.env["ZAPPERCLAW_WARDEN_URL"],
        "http://host.openshell.internal:8972"
    );
    assert_eq!(p.office.setup.env["ZAPPERCLAW_AGENT_PORT"], "8973");
    assert!(!p.office.setup.env.contains_key("ZAPPER_OFFICE_URL"));
}

#[test]
fn a_worker_profile_must_be_drivable() {
    let text = "office: { profile: zapperclaw-office }\nprofiles:\n  k3s: {}\n";
    let config = Config::from_yaml(text, Path::new("/tmp")).unwrap();
    let zapper = zapper_policy::Config::from_yaml(ZAPPER).unwrap();
    let e = Profiles::prepare(&config, &zapper, EDGE).unwrap_err();
    assert!(format!("{e:#}").contains("headless"), "{e:#}");
}

#[tokio::test(start_paused = true)]
async fn the_office_runs_its_command_with_the_boards_token_and_again_when_it_stops() {
    let fake = Arc::new(Fake::default());
    let gw: Arc<dyn Gateway> = fake.clone();
    let config = config();
    let profiles = profiles(&config);
    let command = config.office.command.clone();
    assert_eq!(command, ["/usr/local/bin/zapperclaw", "serve"]);
    let run = tokio::spawn(crate::office::keep_running(
        gw,
        profiles.office.clone(),
        command.clone(),
        "tok".into(),
        Arc::new(Audit::none()),
    ));
    let execs = |fake: &Fake| fake.execs.lock().unwrap().len();
    while execs(&fake) == 0 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    {
        let e = &fake.execs.lock().unwrap()[0];
        assert_eq!(e.sandbox, names::OFFICE);
        assert_eq!(e.argv, command);
        assert_eq!(e.env["ZAPPERCLAW_BOARD_TOKEN"], "tok");
        assert_eq!(e.env["ZAPPERCLAW_AGENT_PORT"], "8973");
        assert_eq!(e.env["HOME"], "/home/agent");
    }
    // It exits: the warden starts it again, after a pause.
    let mut end = fake.exec_ends.lock().unwrap().remove(0);
    end.write_all(&zapper_policy::proto::Frame::Exit(1).encode())
        .await
        .unwrap();
    while execs(&fake) < 2 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    run.abort();
}

#[tokio::test]
async fn an_empty_office_command_runs_nothing() {
    let fake = Arc::new(Fake::default());
    let gw: Arc<dyn Gateway> = fake.clone();
    let config = config();
    let run = tokio::spawn(crate::office::keep_running(
        gw,
        profiles(&config).office,
        vec![],
        "tok".into(),
        Arc::new(Audit::none()),
    ));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(fake.execs.lock().unwrap().is_empty());
    run.abort();
}

#[test]
fn the_checkouts_config_runs_zapperclaw_with_its_config_and_offers_its_roster() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
    let config = Config::load(&dir.join("warden.yaml")).unwrap();
    assert_eq!(
        config.office.command,
        ["/usr/local/bin/zapperclaw", "serve"]
    );
    let p = profiles(&config);
    let mount = p.office.profile.mounts.last().expect("zapperclaw's config");
    assert!(mount.host.ends_with("config/zapperclaw"), "{}", mount.host);
    assert_eq!(mount.guest, "/home/agent/.config/zapperclaw");
    assert_eq!(mount.mode, zapper_policy::MountMode::Ro);
    let roster = std::fs::read_to_string(dir.join("zapperclaw/zapperclaw.yaml")).unwrap();
    let roster: serde_yaml::Value = serde_yaml::from_str(&roster).unwrap();
    for (_, m) in roster["workers"].as_mapping().unwrap() {
        let profile = m["profile"].as_str().unwrap();
        assert!(
            p.workers.contains_key(profile),
            "the warden offers {profile}"
        );
    }
}
