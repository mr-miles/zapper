//! The client against a fake GitHub that checks what a real one would: the
//! App's JWT (signature, issuer, lifetime), the token request's body, and the
//! token on later calls.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use ring::signature::{UnparsedPublicKey, RSA_PKCS1_2048_8192_SHA256};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use zapper_github::{App, GitHub, Scope, Token};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;
const ISSUER: &str = "Iv23liTEST";
const TOKEN: &str = "ghs_faketoken";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[derive(Default)]
struct Seen {
    token_requests: Vec<Value>,
    revoked: Vec<String>,
    /// Lookups of the installation on the account `late`, which appears on the third.
    late_lookups: u32,
}

/// Checks a JWT the way GitHub does.
fn check_jwt(jwt: &str) -> Result<(), String> {
    let parts: Vec<&str> = jwt.split('.').collect();
    let [h, c, s] = parts[..] else {
        return Err("not three parts".into());
    };
    let public = std::fs::read(fixture("test-key-public.der")).unwrap();
    let sig = B64.decode(s).map_err(|e| e.to_string())?;
    UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public)
        .verify(format!("{h}.{c}").as_bytes(), &sig)
        .map_err(|_| "bad signature".to_string())?;
    let claims: Value = serde_json::from_slice(&B64.decode(c).map_err(|e| e.to_string())?).unwrap();
    if claims["iss"] != ISSUER {
        return Err(format!("issuer {}", claims["iss"]));
    }
    let (iat, exp) = (
        claims["iat"].as_i64().unwrap(),
        claims["exp"].as_i64().unwrap(),
    );
    if exp - iat > 10 * 60 {
        return Err("lives longer than ten minutes".into());
    }
    Ok(())
}

fn reply(status: u16, v: Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap())
        .body(Full::new(if v.is_null() {
            Bytes::new()
        } else {
            Bytes::from(v.to_string())
        }))
        .unwrap()
}

async fn handle(
    seen: Arc<Mutex<Seen>>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let (parts, body) = req.into_parts();
    let body: Value =
        serde_json::from_slice(&body.collect().await.unwrap().to_bytes()).unwrap_or(Value::Null);
    let bearer = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    let path = parts.uri.path().to_string();
    let r = match (parts.method.as_str(), path.as_str()) {
        ("GET", "/installation/repositories") if bearer == TOKEN => reply(
            200,
            json!({ "repositories": [{ "full_name": "acme/api" }, { "full_name": "acme/web" }] }),
        ),
        ("DELETE", "/installation/token") if bearer == TOKEN => {
            seen.lock().unwrap().revoked.push(bearer);
            reply(204, Value::Null)
        }
        ("POST", p) if p.starts_with("/app-manifests/") && p.ends_with("/conversions") => {
            let code = p
                .trim_start_matches("/app-manifests/")
                .trim_end_matches("/conversions");
            let owner = if code == "wrongowner" {
                "someone-else"
            } else {
                "late"
            };
            reply(
                201,
                json!({
                    "id": 5, "slug": "late-agents", "client_id": ISSUER, "html_url": "https://github.com/apps/late-agents",
                    "pem": std::fs::read_to_string(fixture("test-key-pkcs1.pem")).unwrap(),
                    "owner": { "login": owner },
                }),
            )
        }
        (_, "/installation/repositories" | "/installation/token") => {
            reply(401, json!({ "message": "Bad credentials" }))
        }
        _ => match check_jwt(&bearer) {
            Err(why) => reply(
                401,
                json!({ "message": format!("A JSON web token could not be decoded: {why}") }),
            ),
            Ok(()) => match (parts.method.as_str(), path.as_str()) {
                ("GET", "/repos/acme/api/installation") => reply(200, json!({ "id": 42 })),
                ("GET", "/repos/acme/missing/installation") => {
                    reply(404, json!({ "message": "Not Found" }))
                }
                ("POST", "/app/installations/42/access_tokens") => {
                    seen.lock().unwrap().token_requests.push(body);
                    reply(
                        201,
                        json!({ "token": TOKEN, "expires_at": "2030-01-01T00:00:00Z" }),
                    )
                }
                ("GET", "/app/installations/42") => reply(
                    200,
                    json!({ "id": 42, "repository_selection": "selected", "permissions": { "contents": "write", "metadata": "read" } }),
                ),
                ("GET", "/orgs/acme/installation") => reply(
                    200,
                    json!({ "id": 42, "repository_selection": "selected", "permissions": { "contents": "write" } }),
                ),
                ("GET", "/orgs/late/installation") => {
                    let mut s = seen.lock().unwrap();
                    s.late_lookups += 1;
                    if s.late_lookups < 3 {
                        reply(404, json!({ "message": "Not Found" }))
                    } else {
                        reply(
                            200,
                            json!({ "id": 99, "repository_selection": "all", "permissions": {} }),
                        )
                    }
                }
                ("GET", "/users/me/installation") => reply(200, json!({ "id": 7 })),
                _ => reply(404, json!({ "message": "Not Found" })),
            },
        },
    };
    Ok(r)
}

async fn fake() -> (GitHub, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Seen::default()));
    let s = seen.clone();
    tokio::spawn(async move {
        loop {
            let (sock, _) = listener.accept().await.unwrap();
            let s = s.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req| handle(s.clone(), req));
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(sock), svc)
                    .await;
            });
        }
    });
    (GitHub::new(Some(url)), seen)
}

fn app_from(pem_file: &str) -> App {
    let pem = std::fs::read_to_string(fixture(pem_file)).unwrap();
    App::from_pem(ISSUER, Some("acme-agents"), &pem).unwrap()
}

fn perms(p: &[(&str, &str)]) -> BTreeMap<String, String> {
    p.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[tokio::test]
async fn mints_a_token_limited_to_one_repository_and_its_permissions() {
    let (gh, seen) = fake().await;
    let scope = Scope::new(
        "acme/api",
        perms(&[("contents", "write"), ("pull_requests", "write")]),
    )
    .unwrap();
    let t = gh
        .mint(&app_from("test-key-pkcs1.pem"), &scope)
        .await
        .unwrap();
    assert_eq!(t.value, TOKEN);
    assert_eq!(t.expires_at, "2030-01-01T00:00:00Z");
    let sent = seen.lock().unwrap().token_requests.clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["repositories"], json!(["api"]));
    assert_eq!(
        sent[0]["permissions"],
        json!({ "contents": "write", "pull_requests": "write", "metadata": "read" })
    );
}

#[tokio::test]
async fn a_pkcs8_key_signs_too() {
    let (gh, _) = fake().await;
    let scope = Scope::new("acme/api", perms(&[("contents", "read")])).unwrap();
    gh.mint(&app_from("test-key-pkcs8.pem"), &scope)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_repository_the_app_is_not_installed_on_says_so() {
    let (gh, seen) = fake().await;
    let scope = Scope::new("acme/missing", perms(&[("contents", "read")])).unwrap();
    let e = format!(
        "{:#}",
        gh.mint(&app_from("test-key-pkcs1.pem"), &scope)
            .await
            .unwrap_err()
    );
    assert!(
        e.contains("is the App installed there?") && e.contains("HTTP 404"),
        "{e}"
    );
    assert!(
        seen.lock().unwrap().token_requests.is_empty(),
        "no token was requested"
    );
}

#[tokio::test]
async fn a_wrong_key_is_rejected_by_github_not_trusted() {
    let (gh, _) = fake().await;
    let other = std::process::Command::new("openssl")
        .args(["genrsa", "-traditional", "2048"])
        .output();
    let Ok(other) = other.map(|o| String::from_utf8(o.stdout).unwrap()) else {
        return;
    };
    let app = App::from_pem(ISSUER, None, &other).unwrap();
    let scope = Scope::new("acme/api", perms(&[("contents", "read")])).unwrap();
    let e = format!("{:#}", gh.mint(&app, &scope).await.unwrap_err());
    assert!(e.contains("HTTP 401") && e.contains("bad signature"), "{e}");
}

#[tokio::test]
async fn finds_an_installation_on_an_organisation_or_a_user() {
    let (gh, _) = fake().await;
    let app = app_from("test-key-pkcs1.pem");
    let org = gh
        .installation_for_owner(&app, "acme")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (org.id, org.repository_selection.as_str()),
        (42, "selected")
    );
    assert_eq!(org.permissions["contents"], "write");
    assert_eq!(
        gh.installation_for_owner(&app, "me")
            .await
            .unwrap()
            .unwrap()
            .id,
        7
    );
    assert!(gh
        .installation_for_owner(&app, "nobody")
        .await
        .unwrap()
        .is_none());
    assert!(gh.installation_for_owner(&app, "../etc").await.is_err());
    assert_eq!(
        gh.installation(&app, 42).await.unwrap().permissions["metadata"],
        "read"
    );
}

#[tokio::test]
async fn lists_the_repositories_a_token_reaches_and_revokes_it() {
    let (gh, seen) = fake().await;
    let good = Token {
        value: TOKEN.into(),
        expires_at: "2030-01-01T00:00:00Z".into(),
    };
    assert_eq!(
        gh.repositories(&good).await.unwrap(),
        ["acme/api", "acme/web"]
    );
    gh.revoke(&good).await.unwrap();
    assert_eq!(seen.lock().unwrap().revoked, [TOKEN]);
    let bad = Token {
        value: "ghs_other".into(),
        expires_at: String::new(),
    };
    assert!(gh.revoke(&bad).await.is_err());
}

#[test]
fn scopes_are_checked_before_anything_is_sent() {
    let ok = perms(&[("contents", "read")]);
    for bad in [
        "api", "acme/", "/api", "acme/a b", "acme/..", "-x/api", "a/b/c",
    ] {
        assert!(Scope::new(bad, ok.clone()).is_err(), "{bad}");
    }
    assert!(
        Scope::new("acme/api", BTreeMap::new()).is_err(),
        "no permissions would mean all of them"
    );
    for bad in [
        ("contents", "owner"),
        ("Contents", "read"),
        ("a b", "read"),
        ("", "read"),
    ] {
        assert!(Scope::new("acme/api", perms(&[bad])).is_err(), "{bad:?}");
    }
}

#[test]
fn credentials_files_are_read_and_checked() {
    let dir = std::env::temp_dir().join(format!("zapper-github-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pem = std::fs::read_to_string(fixture("test-key-pkcs1.pem")).unwrap();
    let indented: String = pem.lines().map(|l| format!("    {l}\n")).collect();
    let write = |name: &str, text: String, mode: u32| {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    };
    let load = |p: &std::path::Path| {
        let mut warned = Vec::new();
        let r = App::load(p, &mut |w| warned.push(w));
        (r, warned)
    };

    let good = write("good.yaml", format!("github:\n  appId: \"1\"\n  clientId: {ISSUER}\n  appSlug: acme-agents\n  privateKey: |\n{indented}"), 0o600);
    let (app, warned) = load(&good);
    assert_eq!(app.unwrap().slug.as_deref(), Some("acme-agents"));
    assert!(warned.is_empty());

    let loose = write("loose.yaml", std::fs::read_to_string(&good).unwrap(), 0o644);
    assert!(load(&loose).1[0].contains("chmod 600"));

    let unindented = write(
        "flat.yaml",
        format!("github:\n  clientId: x\n  privateKey: |\n{pem}"),
        0o600,
    );
    assert!(load(&unindented).0.is_err());

    let no_key = write("nokey.yaml", "github:\n  clientId: x\n".into(), 0o600);
    assert!(format!("{:#}", load(&no_key).0.unwrap_err()).contains("privateKey"));

    let relative = write(
        "rel.yaml",
        "github:\n  clientId: x\n  privateKeyPath: key.pem\n".into(),
        0o600,
    );
    assert!(format!("{:#}", load(&relative).0.unwrap_err()).contains("absolute"));

    let by_path = write(
        "path.yaml",
        format!(
            "github:\n  appId: \"9\"\n  privateKeyPath: {}\n",
            fixture("test-key-pkcs8.pem").display()
        ),
        0o600,
    );
    assert!(load(&by_path).0.is_ok());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_app_does_not_print_its_key_or_a_token() {
    let app = app_from("test-key-pkcs1.pem");
    let shown = format!("{app:?}");
    assert!(
        shown.contains("acme-agents") && !shown.contains("PRIVATE") && !shown.contains("key"),
        "{shown}"
    );
    let t = Token {
        value: "ghs_secret_value".into(),
        expires_at: "x".into(),
    };
    assert!(!format!("{t:?}").contains("secret_value"));
}

mod setup {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use zapper_github::setup::{self, Options};

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zapper-setup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    async fn get(port: u16, path: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        s.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    fn options(
        gh: &GitHub,
        file: PathBuf,
        urls: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Options {
        let mut o = Options::new("late", false);
        o.api = Some(gh.api().to_string());
        o.port = 0;
        o.file = Some(file);
        o.poll = std::time::Duration::from_millis(20);
        o.wait = std::time::Duration::from_secs(10);
        o.open = Some(Box::new(move |u| {
            let _ = urls.send(u.to_string());
        }));
        o
    }

    /// Plays the browser: loads the form, then follows GitHub's redirect.
    async fn browser(urls: &mut tokio::sync::mpsc::UnboundedReceiver<String>, code: &str) {
        let start = urls.recv().await.unwrap();
        let port: u16 = start
            .trim_end_matches('/')
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let page = get(port, "/").await;
        assert!(
            page.contains("github.com/organizations/late/settings/apps/new?state="),
            "{page}"
        );
        assert!(
            page.contains(&format!("http://127.0.0.1:{port}/callback")),
            "the manifest names the redirect"
        );
        let state: String = page
            .split("state=")
            .nth(1)
            .unwrap()
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        let wrong = get(port, &format!("/callback?code={code}&state=0000")).await;
        assert!(
            wrong.starts_with("HTTP/1.1 400"),
            "a redirect with the wrong state is refused"
        );
        let ok = get(port, &format!("/callback?code={code}&state={state}")).await;
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    }

    #[tokio::test]
    async fn creates_the_app_then_waits_for_the_installation() {
        let (gh, _) = fake().await;
        let d = dir("create");
        let file = d.join("late.yaml");
        let (tx, mut urls) = tokio::sync::mpsc::unbounded_channel();
        let (_keep, paste) = tokio::sync::mpsc::channel(1);
        let o = options(&gh, file.clone(), tx);
        let task = tokio::spawn(async move {
            let mut out = Vec::new();
            let r = setup::run(o, &mut out, paste).await;
            (r, String::from_utf8(out).unwrap())
        });
        browser(&mut urls, "abc123").await;
        let install = urls.recv().await.unwrap();
        assert_eq!(
            install,
            "https://github.com/apps/late-agents/installations/new"
        );
        let (r, out) = task.await.unwrap();
        let r = r.unwrap();
        assert!(r.created && r.installation.id == 99, "{out}");
        assert!(
            out.contains("Created GitHub App") && out.contains("all of the account's repositories"),
            "{out}"
        );
        let mut warned = Vec::new();
        App::load(&file, &mut |w| warned.push(w)).unwrap();
        assert!(warned.is_empty(), "written 0600: {warned:?}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test]
    async fn an_existing_app_is_used_not_recreated() {
        let (gh, _) = fake().await;
        let d = dir("existing");
        let file = d.join("late.yaml");
        let src = d.join("elsewhere.yaml");
        std::fs::create_dir_all(&d).unwrap();
        let pem = std::fs::read_to_string(fixture("test-key-pkcs1.pem")).unwrap();
        let body: String = pem.lines().map(|l| format!("    {l}\n")).collect();
        let yaml = format!(
            "github:\n  clientId: {ISSUER}\n  appSlug: late-agents\n  privateKey: |\n{body}"
        );
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&src, &yaml).unwrap();
            std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let (tx, mut urls) = tokio::sync::mpsc::unbounded_channel();
        let (_keep, paste) = tokio::sync::mpsc::channel(1);
        let mut o = options(&gh, file.clone(), tx);
        o.credentials = Some(src.clone());
        let mut out = Vec::new();
        let r = setup::run(o, &mut out, paste).await.unwrap();
        assert!(!r.created);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            yaml,
            "adopted as it was"
        );
        // The install link is the only thing opened, never the manifest form.
        assert_eq!(
            urls.recv().await.unwrap(),
            "https://github.com/apps/late-agents/installations/new"
        );

        // A second run finds the file and leaves it alone.
        let (tx, _urls) = tokio::sync::mpsc::unbounded_channel();
        let (_keep, paste) = tokio::sync::mpsc::channel(1);
        let r = setup::run(options(&gh, file.clone(), tx), &mut Vec::new(), paste)
            .await
            .unwrap();
        assert!(!r.created);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), yaml);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test]
    async fn an_app_created_under_someone_else_never_serves_this_owner() {
        let (gh, _) = fake().await;
        let d = dir("wrong");
        let file = d.join("late.yaml");
        let (tx, mut urls) = tokio::sync::mpsc::unbounded_channel();
        let (_keep, paste) = tokio::sync::mpsc::channel(1);
        let o = options(&gh, file.clone(), tx);
        let task =
            tokio::spawn(async move { setup::run(o, &mut Vec::new(), paste).await.map(|_| ()) });
        browser(&mut urls, "wrongowner").await;
        let e = format!("{:#}", task.await.unwrap().unwrap_err());
        assert!(
            e.contains("someone-else") && e.contains("signed in as someone else?"),
            "{e}"
        );
        assert!(!file.exists(), "late's file was not written");
        assert!(
            d.join("someone-else.yaml").exists(),
            "the key is kept, under the account that owns the App"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test]
    async fn a_pasted_redirect_works_when_the_browser_is_elsewhere() {
        let (gh, _) = fake().await;
        let d = dir("paste");
        let (tx, mut urls) = tokio::sync::mpsc::unbounded_channel();
        let (send, paste) = tokio::sync::mpsc::channel(1);
        let o = options(&gh, d.join("late.yaml"), tx);
        let task = tokio::spawn(async move {
            let mut out = Vec::new();
            setup::run(o, &mut out, paste).await.map(|r| r.created)
        });
        let start = urls.recv().await.unwrap();
        let port: u16 = start
            .trim_end_matches('/')
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let page = get(port, "/").await;
        let state: String = page
            .split("state=")
            .nth(1)
            .unwrap()
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        send.send(format!(
            "http://127.0.0.1:{port}/callback?code=abc123&state={state}\n"
        ))
        .await
        .unwrap();
        assert!(task.await.unwrap().unwrap());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
