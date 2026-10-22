//! The REST API and the UI against a core without VMs
//! (`zapper_core::testing::TestCore`), request by request.

use axum::body::Body;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use zapper_core::testing::TestCore;

use super::Mount;
use zapper_core::NewSession;
use zapper_policy::Policy;

const CONFIG: &str = "
policy:
  net:
    - { id: global-deny, action: deny, host: '*.bad.example' }
profiles:
  web: {}
  bot:
    agent: { kind: claude-code, driven: true }
";

const PORT: u16 = 7878;
const HOST: &str = "127.0.0.1:7878";

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Reply {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

async fn get(app: &Router, uri: &str) -> Reply {
    send(app, Method::GET, uri, &[], "").await
}

async fn post(app: &Router, uri: &str, body: &str) -> Reply {
    send(app, Method::POST, uri, &[], body).await
}

fn session(t: &TestCore, profile: &str, labels: &[(&str, &str)], running: bool) -> String {
    let mut new = NewSession::plain(profile.into(), t.workdir(), Policy::default());
    new.labels = labels
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    t.core.test_session(new, running).unwrap().id.clone()
}

// The API.

#[tokio::test]
async fn status() {
    let t = TestCore::new(CONFIG);
    let app = super::api(t.core.clone(), &Mount::default());
    let r = get(&app, "/v1/status").await;
    assert_eq!(r.status, StatusCode::OK);
    let v = r.json();
    assert_eq!(v["sessions"], 0);
    assert_eq!(v["unsaved_changes"], false);
    assert_eq!(v["pending_approvals"], 0);
}

#[tokio::test]
async fn the_config_read_replaced_diffed_saved_and_reloaded() {
    let t = TestCore::new(CONFIG);
    let app = super::api(t.core.clone(), &Mount::default());
    let yaml = get(&app, "/v1/config").await;
    assert_eq!(yaml.status, StatusCode::OK);
    assert_eq!(yaml.headers[header::CONTENT_TYPE], "application/yaml");
    assert!(yaml.body.contains("web:"), "{}", yaml.body);
    assert!(get(&app, "/v1/config?format=json").await.json()["profiles"]["bot"].is_object());

    // Invalid as a whole: refused, nothing changes.
    let mut bad = t.core.config();
    bad.profiles.insert(
        "orphan".into(),
        serde_yaml::from_str("{ extends: nowhere }").unwrap(),
    );
    let r = send(&app, Method::PUT, "/v1/config", &[], &bad.to_yaml()).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    assert!(!t.core.config().profiles.contains_key("orphan"));

    let mut good = t.core.config();
    good.profiles.insert("extra".into(), Default::default());
    let r = send(&app, Method::PUT, "/v1/config", &[], &good.to_yaml()).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(get(&app, "/v1/config/diff").await.body.contains("extra"));
    assert_eq!(
        get(&app, "/v1/status").await.json()["unsaved_changes"],
        true
    );

    assert_eq!(
        post(&app, "/v1/config/save", "").await.status,
        StatusCode::OK
    );
    assert!(std::fs::read_to_string(t.startup())
        .unwrap()
        .contains("extra:"));
    assert!(get(&app, "/v1/config/startup")
        .await
        .body
        .contains("extra:"));

    let mut more = t.core.config();
    more.profiles.insert("unsaved".into(), Default::default());
    send(&app, Method::PUT, "/v1/config", &[], &more.to_yaml()).await;
    assert_eq!(
        post(&app, "/v1/config/reload", "").await.status,
        StatusCode::OK
    );
    assert!(!t.core.config().profiles.contains_key("unsaved"));
}

#[tokio::test]
async fn rules_added_listed_and_removed() {
    let t = TestCore::new(CONFIG);
    let app = super::api(t.core.clone(), &Mount::default());
    let r = post(
        &app,
        "/v1/rules",
        r#"{"scope":"profile:web","rule":{"kind":"net","action":"allow","host":"api.example.com"}}"#,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let id = r.json()["id"].as_str().unwrap().to_string();
    assert_eq!(r.json()["scope"], "profile:web");

    let all = get(&app, "/v1/rules").await.json();
    let ids: Vec<&str> = all
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["id"].as_str())
        .collect();
    assert!(
        ids.contains(&id.as_str()) && ids.contains(&"global-deny"),
        "{ids:?}"
    );
    let web = get(&app, "/v1/rules?scope=profile:web").await.json();
    assert_eq!(web.as_array().unwrap().len(), 1);
    assert_eq!(
        get(&app, "/v1/rules?scope=nonsense").await.status,
        StatusCode::BAD_REQUEST
    );

    let gone = send(&app, Method::DELETE, &format!("/v1/rules/{id}"), &[], "").await;
    assert_eq!(gone.status, StatusCode::OK, "{}", gone.body);
    let again = send(&app, Method::DELETE, &format!("/v1/rules/{id}"), &[], "").await;
    assert_eq!(again.status, StatusCode::NOT_FOUND, "{}", again.body);
}

#[tokio::test]
async fn profiles_set_read_and_removed() {
    let t = TestCore::new(CONFIG);
    let app = super::api(t.core.clone(), &Mount::default());
    let r = send(&app, Method::PUT, "/v1/profiles/p", &[], "{ extends: web }").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(get(&app, "/v1/profiles/p").await.status, StatusCode::OK);
    let names = get(&app, "/v1/profiles").await.body;
    assert!(names.contains("\"p\""), "{names}");
    assert_eq!(
        send(&app, Method::DELETE, "/v1/profiles/p", &[], "")
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        send(&app, Method::DELETE, "/v1/profiles/p", &[], "")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn evaluating_a_request() {
    let t = TestCore::new(CONFIG);
    let app = super::api(t.core.clone(), &Mount::default());
    let denied = post(
        &app,
        "/v1/evaluate",
        r#"{"net":{"host":"x.bad.example","port":443}}"#,
    )
    .await;
    assert_eq!(denied.status, StatusCode::OK, "{}", denied.body);
    assert_eq!(denied.json()["decision"]["action"], "deny");
    assert_eq!(denied.json()["decision"]["rule"], "global-deny");
    let by_profile = post(
        &app,
        "/v1/evaluate",
        r#"{"profile":"web","net":{"host":"ok.example","port":443}}"#,
    )
    .await;
    assert_eq!(
        by_profile.json()["decision"]["action"],
        "deny",
        "the default"
    );
    assert_eq!(
        post(&app, "/v1/evaluate", "{}").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post(
            &app,
            "/v1/evaluate",
            r#"{"session":"nosuch","net":{"host":"a","port":1}}"#
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn audit_logs_by_session_id_only() {
    let t = TestCore::new(CONFIG);
    let app = super::api(t.core.clone(), &Mount::default());
    assert_eq!(
        get(&app, "/v1/sessions/not-an-id/audit").await.status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn sessions_with_their_labels() {
    let t = TestCore::new(CONFIG);
    let app = super::api(t.core.clone(), &Mount::default());
    let id = session(&t, "web", &[("firm", "acme"), ("member", "pat")], true);
    let all = get(&app, "/v1/sessions").await.json();
    assert_eq!(all[0]["id"], id);
    assert_eq!(all[0]["labels"]["member"], "pat");
    assert_eq!(all[0]["firm"], "acme");
    let one = get(&app, &format!("/v1/sessions/{id}")).await;
    assert_eq!(one.status, StatusCode::OK);
    assert_eq!(
        get(&app, "/v1/sessions/nosuch").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn deleting_a_session_stops_and_forgets_it() {
    let t = TestCore::new(CONFIG);
    let app = super::api(t.core.clone(), &Mount::default());
    let id = session(&t, "web", &[], true);
    let r = send(&app, Method::DELETE, &format!("/v1/sessions/{id}"), &[], "").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(t.core.sessions().is_empty());
}

#[tokio::test]
async fn prompts_go_to_driven_sessions_of_no_firm() {
    let t = TestCore::new(CONFIG);
    let app = super::api(t.core.clone(), &Mount::default());
    let task = r#"{"text":"write hello.txt"}"#;

    let r = post(&app, "/v1/sessions/nosuch/prompt", task).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.body);
    assert_eq!(
        post(&app, "/v1/sessions/nosuch/prompt", "{}").await.status,
        StatusCode::BAD_REQUEST
    );

    let worker = session(&t, "bot", &[("firm", "acme"), ("member", "pat")], true);
    let r = post(&app, &format!("/v1/sessions/{worker}/prompt"), task).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert!(r.body.contains("pat works for firm acme"), "{}", r.body);

    let plain = session(&t, "web", &[], true);
    let r = post(&app, &format!("/v1/sessions/{plain}/prompt"), task).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert!(r.body.contains("not driven"), "{}", r.body);

    let starting = session(&t, "bot", &[], false);
    let r = post(&app, &format!("/v1/sessions/{starting}/prompt"), task).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert!(r.body.contains("not running"), "{}", r.body);

    let bot = session(&t, "bot", &[], true);
    let r = post(&app, &format!("/v1/sessions/{bot}/prompt"), task).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.body);
    assert_eq!(r.json()["queued"], 1);
    assert_eq!(r.json()["driver"]["queued"], 1);
}

#[tokio::test]
async fn with_the_driver_off_prompts_are_refused() {
    let t = TestCore::with_daemon("driver: false", "profiles:\n  web: {}\n");
    let app = super::api(t.core.clone(), &Mount::default());
    let id = session(&t, "web", &[], true);
    let r = post(
        &app,
        &format!("/v1/sessions/{id}/prompt"),
        r#"{"text":"hi"}"#,
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert!(r.body.contains("the driver is disabled"), "{}", r.body);
    assert_eq!(
        get(&app, &format!("/v1/sessions/{id}")).await.json()["driver"],
        "disabled"
    );
}

#[tokio::test]
async fn answering_an_unknown_approval() {
    let t = TestCore::new(CONFIG);
    let app = super::api(t.core.clone(), &Mount::default());
    assert_eq!(get(&app, "/v1/approvals").await.json(), json!([]));
    let r = post(&app, "/v1/approvals/nosuch", r#"{"allow":true}"#).await;
    assert!(r.status.is_client_error(), "{}: {}", r.status, r.body);
}

// What sits behind the routes: rule layers, the layers a request is decided
// by, audit logs and a session's own rules.

const RULES: &str = "
policy:
  net:
    - { id: global-deny, action: deny, host: '*.bad.example' }
profiles:
  web:
    policy:
      net:
        - { id: web-allow, action: allow, host: example.com }
";

#[tokio::test]
async fn rule_layers_and_removing_rules() {
    let t = TestCore::new(RULES);
    let app = super::api(t.core.clone(), &Mount::default());
    let all = get(&app, "/v1/rules").await.json();
    let scopes: Vec<&str> = all
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["scope"].as_str())
        .collect();
    assert!(
        scopes.contains(&"global") && scopes.contains(&"profile:web"),
        "{scopes:?}"
    );

    let r = send(&app, Method::DELETE, "/v1/rules/web-allow", &[], "").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.json()["scope"], "profile:web");
    assert_eq!(r.json()["kind"], "net");
    assert!(t.core.config().profiles["web"].policy.net.is_empty());
    for id in ["web-allow", "s-nothing"] {
        let again = send(&app, Method::DELETE, &format!("/v1/rules/{id}"), &[], "").await;
        assert_eq!(again.status, StatusCode::NOT_FOUND, "{}", again.body);
        assert!(again.json()["error"]
            .as_str()
            .unwrap()
            .starts_with("no rule"));
    }
}

#[tokio::test]
async fn the_layers_a_request_is_decided_by() {
    let t = TestCore::new(RULES);
    let app = super::api(t.core.clone(), &Mount::default());
    let rule = |body: &'static str| {
        let app = app.clone();
        async move { post(&app, "/v1/evaluate", body).await.json()["decision"]["rule"].clone() }
    };
    // Global only, then the profile's before it.
    assert_eq!(
        rule(r#"{"net":{"host":"example.com","port":443}}"#).await,
        Value::Null
    );
    assert_eq!(
        rule(r#"{"profile":"web","net":{"host":"example.com","port":443}}"#).await,
        "web-allow"
    );
    assert_eq!(
        rule(r#"{"profile":"web","net":{"host":"x.bad.example","port":443}}"#).await,
        "global-deny"
    );
    let s = session(&t, "web", &[], true);
    let by_session = format!(r#"{{"session":"{s}","net":{{"host":"example.com","port":443}}}}"#);
    assert_eq!(
        post(&app, "/v1/evaluate", &by_session).await.json()["decision"]["rule"],
        "web-allow"
    );
    let r = post(
        &app,
        "/v1/evaluate",
        r#"{"profile":"nosuch","net":{"host":"a","port":1}}"#,
    )
    .await;
    assert!(r.status.is_client_error(), "{}", r.status);
}

#[tokio::test]
async fn audit_logs_of_sessions_listed_or_ended() {
    let t = TestCore::new(RULES);
    let app = super::api(t.core.clone(), &Mount::default());
    for bad in ["..%2Fetc", "a%2Fb"] {
        let r = get(&app, &format!("/v1/sessions/{bad}/audit")).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    // A session id's form, of a session the daemon no longer lists.
    let r = get(&app, "/v1/sessions/abcdef/audit").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.body);
    let id = session(&t, "web", &[], true);
    let r = get(&app, &format!("/v1/sessions/{id}/audit")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("starting"), "{}", r.body);
}

#[tokio::test]
async fn rules_added_to_a_session_are_its_own_and_an_event() {
    let t = TestCore::new(RULES);
    let app = super::api(t.core.clone(), &Mount::default());
    let s = session(&t, "web", &[], true);
    let body = format!(
        r#"{{"scope":"session:{s}","rule":{{"kind":"net","action":"allow","host":"api.example.com"}}}}"#
    );
    let r = post(&app, "/v1/rules", &body).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let id = r.json()["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("s-"), "{id}");
    let last = serde_json::to_value(t.core.recent_events(Some(&s)).last().unwrap()).unwrap();
    assert_eq!(last["type"], "config", "{last}");
    let gone = send(&app, Method::DELETE, &format!("/v1/rules/{id}"), &[], "").await;
    assert_eq!(gone.json()["scope"], format!("session:{s}"));
}

#[tokio::test]
async fn new_sessions_labels_are_checked() {
    let t = TestCore::new(CONFIG);
    let app = super::api(t.core.clone(), &Mount::default());
    let body = |labels: &str| {
        format!(
            r#"{{"profile":"web","workdir":"{}","labels":{labels}}}"#,
            t.workdir().display()
        )
    };
    let r = post(&app, "/v1/sessions", &body(r#"{"firm":"acme"}"#)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("label firm is the office's"), "{}", r.body);
    let r = post(&app, "/v1/sessions", &body(r#"{"a b":"c"}"#)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("label a b=c"), "{}", r.body);
    // Good labels pass; with no VM to boot, the session fails later.
    let r = post(&app, "/v1/sessions", &body(r#"{"team":"a.1"}"#)).await;
    assert!(!r.body.contains("label"), "{}", r.body);
}

#[tokio::test]
async fn services_added_listed_and_removed() {
    let t = TestCore::new(CONFIG);
    let app = super::api(t.core.clone(), &Mount::default());
    assert_eq!(get(&app, "/v1/services").await.json(), json!([]));
    let id = session(&t, "web", &[], true);
    let add = |body: String| {
        let app = app.clone();
        async move { post(&app, "/v1/services", &body).await }
    };
    let r = add(format!(
        r#"{{"name":"db.internal","port":5432,"sessions":{{"team":"a"}},"to":{{"session":"{id}","port":5432}}}}"#
    ))
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let svc = r.json();
    assert_eq!(svc["to"], format!("session {id} port 5432"));
    assert_eq!(svc["owner"], "operator");
    let r = add(r#"{"name":"lan.internal","port":80,"to":{"addr":"10.0.0.5:80"}}"#.into()).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(
        get(&app, "/v1/services")
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let r =
        add(r#"{"name":"x.internal","port":1,"to":{"session":"nosuch","port":1}}"#.into()).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.body);
    let r = add(r#"{"name":"not a name","port":1,"to":{"addr":"10.0.0.5:80"}}"#.into()).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    let r = add(r#"{"name":"x.internal","port":1,"to":{"nowhere":1}}"#.into()).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);

    // Another client's service is theirs to remove.
    let firms = t
        .core
        .add_service(zapper_core::ServiceSpec {
            name: "git.acme.internal".into(),
            port: 443,
            sessions: Default::default(),
            target: zapper_core::ServiceTarget::Addr("10.0.0.6:443".parse().unwrap()),
            owner: "firm acme".into(),
        })
        .unwrap();
    let del = |id: String| {
        let app = app.clone();
        async move { send(&app, Method::DELETE, &format!("/v1/services/{id}"), &[], "").await }
    };
    let r = del(firms.to_string()).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.body);
    assert!(r.body.contains("is firm acme's"), "{}", r.body);
    let r = del(svc["id"].to_string()).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.json()["name"], "db.internal");
    assert_eq!(
        del(svc["id"].to_string()).await.status,
        StatusCode::NOT_FOUND
    );
}

// The UI.

/// The UI's cookie, from the token in the URL `zapper serve` prints.
async fn signed_in(app: &Router, token: &str) -> String {
    let r = send(
        app,
        Method::GET,
        &format!("/?token={token}"),
        &[("host", HOST)],
        "",
    )
    .await;
    assert!(r.status.is_redirection(), "{}", r.status);
    let cookie = r.headers[header::SET_COOKIE].to_str().unwrap().to_string();
    assert!(
        cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"),
        "{cookie}"
    );
    cookie.split(';').next().unwrap().to_string()
}

#[tokio::test]
async fn the_ui_wants_its_token_a_local_host_and_marked_writes() {
    let t = TestCore::new(CONFIG);
    let app = super::ui(t.core.clone(), PORT, false, &Mount::default());
    let token = t.core.ui_token().to_string();

    let r = send(&app, Method::GET, "/api/status", &[("host", HOST)], "").await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = send(
        &app,
        Method::GET,
        "/api/status",
        &[("host", "evil.example:7878")],
        "",
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "a foreign Host");
    let r = send(&app, Method::GET, "/?token=wrong", &[("host", HOST)], "").await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    let cookie = signed_in(&app, &token).await;
    let r = send(
        &app,
        Method::GET,
        "/api/status",
        &[("host", HOST), ("cookie", &cookie)],
        "",
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["x-content-type-options"], "nosniff");
    assert!(r.headers.contains_key("content-security-policy"));

    // Writes: marked, and from the UI's own origin.
    let answer = r#"{"allow":true}"#;
    let unmarked = send(
        &app,
        Method::POST,
        "/api/approvals/nosuch",
        &[("host", HOST), ("cookie", &cookie)],
        answer,
    )
    .await;
    assert_eq!(unmarked.status, StatusCode::FORBIDDEN);
    let foreign = send(
        &app,
        Method::POST,
        "/api/approvals/nosuch",
        &[
            ("host", HOST),
            ("cookie", &cookie),
            ("x-zapper-ui", "1"),
            ("origin", "http://evil.example"),
        ],
        answer,
    )
    .await;
    assert_eq!(foreign.status, StatusCode::FORBIDDEN);
    let ok = send(
        &app,
        Method::POST,
        "/api/approvals/nosuch",
        &[
            ("host", HOST),
            ("cookie", &cookie),
            ("x-zapper-ui", "1"),
            ("origin", &format!("http://{HOST}")),
        ],
        answer,
    )
    .await;
    assert!(
        ok.status != StatusCode::FORBIDDEN && ok.status != StatusCode::UNAUTHORIZED,
        "past the guard: {}",
        ok.status
    );
}

/// The front page and `/sessions` of a UI with `mount`, and `GET /api/x`.
async fn pages(t: &TestCore, mount: &Mount) -> (Reply, Reply, Reply) {
    let app = super::ui(t.core.clone(), PORT, false, mount);
    let cookie = signed_in(&app, t.core.ui_token()).await;
    let get = |path: &'static str| {
        let (app, cookie) = (app.clone(), cookie.clone());
        async move {
            send(
                &app,
                Method::GET,
                path,
                &[("host", HOST), ("cookie", &cookie)],
                "",
            )
            .await
        }
    };
    (get("/").await, get("/sessions").await, get("/api/x").await)
}

#[tokio::test]
async fn the_ui_front_page_is_the_mounted_one_or_the_sessions() {
    let t = TestCore::new(CONFIG);
    let (front, flagged, x) = pages(&t, &Mount::default()).await;
    assert_eq!(front.status, StatusCode::OK);
    assert!(
        front.body.contains("office-disabled"),
        "the sessions page, flagged"
    );
    assert_eq!(front.body, flagged.body);
    assert_eq!(x.status, StatusCode::NOT_FOUND);

    // A client's page and routes, behind the same guard.
    let mount = Mount {
        routes: Router::new().route("/x", axum::routing::get(|| async { "mounted" })),
        page: Some("<title>The Office</title>"),
    };
    let (front, sessions, x) = pages(&t, &mount).await;
    assert_eq!(front.body, "<title>The Office</title>");
    assert_ne!(sessions.body, flagged.body, "unflagged: it links back");
    assert_eq!(x.body, "mounted");
    let app = super::ui(t.core.clone(), PORT, false, &mount);
    let r = send(&app, Method::GET, "/api/x", &[("host", HOST)], "").await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "guarded");
    let api = super::api(t.core.clone(), &mount);
    assert_eq!(get(&api, "/v1/x").await.body, "mounted");
}
