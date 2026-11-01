//! The SSH connector: keys and the `authorized_keys` lines they become, and
//! `zapper-workbench ssh-proxy --key NAME` run as the host's sshd runs it,
//! against zapper's API on a `TestCore` (no VMs).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use russh::ChannelMsg;
use zapper_core::prelude::*;
use zapper_core::testing::TestCore;
use zapper_core::NewSession;
use zapper_workbench::ssh::{Keys, Reach};

const CONFIG: &str = "profiles:\n  web: {}\n";

fn public_key(seed: u8) -> String {
    let k = russh::keys::PrivateKey::from(
        russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[seed; 32]),
    );
    k.public_key().to_openssh().unwrap()
}

struct Host {
    t: TestCore,
    home: std::path::PathBuf,
    socket: std::path::PathBuf,
    keys: Keys,
}

/// zapper's API on a socket, and a home whose keys the proxy reads.
async fn host() -> Host {
    let t = TestCore::new(CONFIG);
    let socket = t.dir.join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let api = zapper_api::api(t.core.clone(), &zapper_api::Mount::default());
    tokio::spawn(async move { axum::serve(listener, api).await });
    let home = t.dir.join("home");
    let keys = Keys::new(
        home.join(".config/zapper-workbench/ssh-keys.json"),
        home.join(".ssh/authorized_keys"),
        std::path::Path::new(env!("CARGO_BIN_EXE_zapper-workbench")),
        &socket,
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".config/zapper-workbench")).unwrap();
    Host {
        t,
        home,
        socket,
        keys,
    }
}

fn session(h: &Host, labels: &[(&str, &str)], ssh: bool) -> String {
    let mut new = NewSession::plain("web".into(), h.t.workdir(), Default::default());
    new.labels = labels
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    new.ssh = ssh;
    h.t.core.test_session(new, true).unwrap().id.clone()
}

fn proxy(h: &Host, key: &str, target: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(env!("CARGO_BIN_EXE_zapper-workbench"));
    c.args(["ssh-proxy", "--key", key])
        .env("SSH_ORIGINAL_COMMAND", target)
        .env("HOME", &h.home)
        .env("ZAPPER_SOCKET", &h.socket)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    c
}

/// The proxy refused: what it said, and that the SSH stream got nothing.
async fn refused(h: &Host, key: &str, target: &str) -> String {
    let mut c = proxy(h, key, target);
    c.stdin(std::process::Stdio::null());
    let o = c.output().await.unwrap();
    assert!(!o.status.success());
    assert!(o.stdout.is_empty(), "nothing on the SSH stream");
    String::from_utf8_lossy(&o.stderr).into_owned()
}

struct Client;
impl russh::client::Handler for Client {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        _: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

#[test]
fn a_key_is_a_restricted_line_among_the_users_own() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let h = rt.block_on(host());
    let auth = h.home.join(".ssh/authorized_keys");
    std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
    std::fs::write(&auth, "ssh-rsa AAAAB3mine me@laptop\n").unwrap();
    h.keys
        .put(
            "laptop",
            &format!("{} me@laptop", public_key(1)),
            Reach::All,
        )
        .unwrap();
    let text = std::fs::read_to_string(&auth).unwrap();
    assert!(text.starts_with("ssh-rsa AAAAB3mine me@laptop\n"), "{text}");
    let line = text.lines().find(|l| l.starts_with("restrict,")).unwrap();
    assert!(
        line.contains(&format!("ZAPPER_SOCKET={}", h.socket.display())),
        "{line}"
    );
    assert!(line.contains("ssh-proxy --key laptop\""), "{line}");
    assert!(line.ends_with(" laptop"), "{line}");
    use std::os::unix::fs::PermissionsExt as _;
    assert_eq!(
        std::fs::metadata(&auth).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        h.keys.put("other", &public_key(1), Reach::All).is_err(),
        "one name per key"
    );
    assert!(h.keys.put("bad", "not a key", Reach::All).is_err());
    assert!(h.keys.remove("laptop").unwrap());
    assert_eq!(
        std::fs::read_to_string(&auth).unwrap(),
        "ssh-rsa AAAAB3mine me@laptop\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_proxy_reaches_only_the_sandboxes_its_key_does() {
    let h = host().await;
    let x = session(&h, &[("project", "x")], true);
    let y = session(&h, &[("project", "y")], true);
    let off = session(&h, &[("project", "x")], false);
    h.keys
        .put(
            "laptop",
            &public_key(2),
            Reach::Labels(BTreeMap::from([("project".into(), "x".into())])),
        )
        .unwrap();

    let e = refused(&h, "laptop", &format!("{y}.zapper")).await;
    assert!(e.contains(&format!("no sandbox {y}")), "{e}");
    let e = refused(&h, "laptop", &format!("{off}.zapper")).await;
    assert!(e.contains("SSH is off"), "{e}");
    let e = refused(&h, "gone", &format!("{x}.zapper")).await;
    assert!(e.contains("no longer the workbench's"), "{e}");

    // The one it reaches: a command run, and the key named in the record.
    let s = h.t.core.session(&x).unwrap();
    let mut vm = zapper_core::testing::shell_vm(&s);
    let mut child = proxy(&h, "laptop", &format!("{x}.zapper")).spawn().unwrap();
    let io = tokio::io::join(child.stdout.take().unwrap(), child.stdin.take().unwrap());
    let mut c =
        russh::client::connect_stream(Arc::new(russh::client::Config::default()), io, Client)
            .await
            .unwrap();
    assert!(c.authenticate_none("user").await.unwrap().success());
    let mut ch = c.channel_open_session().await.unwrap();
    ch.exec(true, "echo hi").await.unwrap();
    let mut guest = tokio::time::timeout(Duration::from_secs(10), vm.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(guest.request.argv, ["/bin/sh", "-c", "echo hi"]);
    guest.say("hi").await;
    guest.exit(0).await;
    let mut status = None;
    while let Some(m) = ch.wait().await {
        match m {
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    assert_eq!(status, Some(0));
    let shell =
        h.t.core
            .recent_events(Some(&x))
            .into_iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .find(|e| e["type"] == "shell")
            .unwrap();
    assert_eq!(shell["client"], "laptop");

    // A key's reach changed applies at its next connection.
    h.keys
        .put("laptop", &public_key(2), Reach::Sessions(vec![y.clone()]))
        .unwrap();
    let e = refused(&h, "laptop", &format!("{x}.zapper")).await;
    assert!(e.contains(&format!("no sandbox {x}")), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn keys_are_the_owners_to_manage() {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::{Method, Request, StatusCode};
    use hyper_util::rt::TokioIo;
    use serde_json::{json, Value};
    use zapper_workbench::facility::Facilities;
    use zapper_workbench::gateway::{self, Ctx};
    use zapper_workbench::tokens::{Right, Tokens};

    let h = host().await;
    let tokens = Tokens::open(h.t.dir.join("wb")).unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let ssh =
        zapper_workbench::ssh::Ssh::new(h.keys.clone(), "zapper ssh-ed25519 AAAAhostkey".into());
    let ctx = Ctx {
        zapper: zapper_workbench::zapper::Zapper::new(h.socket.clone()),
        tokens: tokens.clone(),
        facilities: Arc::new(Facilities::new(vec![Arc::new(ssh)]).unwrap()),
        work: h.t.dir.join("work"),
        port: addr.port(),
        remote: false,
        live: Default::default(),
    };
    tokio::spawn(async move { axum::serve(tcp, gateway::app(ctx)).await });
    let call = |token: String, method: Method, path: &str, body: Option<Value>| {
        let path = path.to_string();
        async move {
            let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let (mut s, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
                .await
                .unwrap();
            tokio::spawn(conn);
            let mut req = Request::builder()
                .method(method)
                .uri(path)
                .header("host", format!("127.0.0.1:{}", addr.port()))
                .header("authorization", format!("Bearer {token}"));
            if body.is_some() {
                req = req.header("content-type", "application/json");
            }
            let body = Bytes::from(body.map(|b| b.to_string()).unwrap_or_default());
            let r = s
                .send_request(req.body(Full::new(body)).unwrap())
                .await
                .unwrap();
            let status = r.status();
            let b = r.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                serde_json::from_slice::<Value>(&b).unwrap_or(Value::Null),
            )
        }
    };
    let owner = tokens.owner().unwrap();
    let (s, v) = call(
        owner.clone(),
        Method::PUT,
        "/w/v1/ssh/keys/laptop",
        Some(json!({ "key": public_key(3), "reach": "all" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, listed) = call(owner.clone(), Method::GET, "/w/v1/ssh/keys", None).await;
    assert_eq!(listed[0]["name"], "laptop");
    assert!(listed[0]["key"]
        .as_str()
        .unwrap()
        .starts_with("ssh-ed25519 …"));
    let (s, _) = call(
        owner.clone(),
        Method::PUT,
        "/w/v1/ssh/keys/bad",
        Some(json!({ "key": "nope", "reach": "all" })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (_, c) = call(
        owner.clone(),
        Method::GET,
        "/w/v1/ssh/config?host=me@box",
        None,
    )
    .await;
    assert!(
        c["ssh_config"]
            .as_str()
            .unwrap()
            .contains("ProxyCommand ssh -T -i ~/.ssh/YOUR_KEY me@box %h"),
        "{c}"
    );
    assert_eq!(c["known_hosts"], "zapper ssh-ed25519 AAAAhostkey");

    // A token limited to some sessions can't hand out a shell in all.
    let all_but_owner = tokens
        .add("ops", BTreeMap::new(), Right::ALL.into())
        .unwrap();
    let (s, _) = call(all_but_owner.clone(), Method::GET, "/w/v1/ssh/keys", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(
        all_but_owner,
        Method::PUT,
        "/w/v1/ssh/keys/x",
        Some(json!({ "key": public_key(4), "reach": "all" })),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(owner, Method::DELETE, "/w/v1/ssh/keys/laptop", None).await;
    assert_eq!(s, StatusCode::OK);
}
