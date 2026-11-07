use super::*;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};

const UUID: &str = "6f1c2d3e-0000-4000-8000-000000000001";

/// A backend with one sandbox, whose tools echo what they were asked.
struct Fake;

impl Backend for Fake {
    fn caller(&self, uuid: &str) -> Option<Caller> {
        (uuid == UUID).then(|| Caller {
            session: "zo-000000000001".into(),
            labels: BTreeMap::from([
                ("firm".to_string(), "acme".to_string()),
                ("member".to_string(), "alice".to_string()),
            ]),
        })
    }

    fn tools<'a>(
        &'a self,
        caller: &'a Caller,
        op: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            match op {
                "tools" => {
                    Ok(json!([{ "name": "mail_inbox", "description": "", "inputSchema": {} }]))
                }
                "fail" => bail!("no such luck"),
                _ => Ok(json!({ "op": op, "args": args, "member": caller.labels["member"] })),
            }
        })
    }

    fn instructions(&self) -> Option<String> {
        Some("the office".into())
    }

    fn git<'a>(
        &'a self,
        caller: &'a Caller,
        req: Request<Incoming>,
    ) -> BoxFuture<'a, Response<Body>> {
        Box::pin(async move {
            let uri = req.uri().to_string();
            plain(
                StatusCode::OK,
                &format!("{} {uri}", caller.labels["member"]),
            )
        })
    }
}

/// The driver's signing key, and the JWKS that publishes it.
struct Driver {
    key: EcdsaKeyPair,
    rng: SystemRandom,
}

impl Driver {
    fn new() -> Driver {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        Driver { key, rng }
    }

    fn jwks(&self) -> Vec<u8> {
        let point = self.key.public_key().as_ref();
        serde_json::to_vec(&json!({ "keys": [{
            "kty": "EC", "crv": "P-256", "kid": "k1", "alg": "ES256", "use": "sig",
            "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
            "y": URL_SAFE_NO_PAD.encode(&point[33..65]),
        }]}))
        .unwrap()
    }

    fn sign(&self, claims: Value) -> String {
        let h = URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256","kid":"k1","typ":"JWT"}"#);
        let c = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        let input = format!("{h}.{c}");
        let sig = self.key.sign(&self.rng, input.as_bytes()).unwrap();
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref()))
    }

    /// An SVID as the driver mints one for sandbox `uuid`.
    fn svid(&self, uuid: &str, jti: &str) -> String {
        let s = EdgeSettings::default();
        let now = unix_now();
        self.sign(json!({
            "iss": s.issuer, "sub": format!("spiffe://{}/openshell/sandbox/{uuid}", s.trust_domain),
            "aud": s.token_url, "iat": now, "exp": now + 300, "jti": jti,
        }))
    }
}

fn edge(driver: &Driver) -> Arc<Edge> {
    let e = Arc::new(Edge::new(EdgeSettings::default(), Arc::new(Fake)));
    e.set_keys(&driver.jwks());
    e
}

#[tokio::test]
async fn an_svid_from_the_driver_names_its_sandbox() {
    let d = Driver::new();
    let e = edge(&d);
    assert_eq!(e.verify_svid(&d.svid(UUID, "j1")).await.unwrap(), UUID);
}

#[tokio::test]
async fn an_svid_is_refused_replayed_or_for_another_audience_or_domain() {
    let d = Driver::new();
    let e = edge(&d);
    let svid = d.svid(UUID, "j1");
    e.verify_svid(&svid).await.unwrap();
    assert!(e.verify_svid(&svid).await.is_err(), "replayed");

    let s = EdgeSettings::default();
    let now = unix_now();
    let base = json!({
        "iss": s.issuer, "sub": format!("spiffe://{}/openshell/sandbox/{UUID}", s.trust_domain),
        "aud": s.token_url, "iat": now, "exp": now + 300,
    });
    let with = |k: &str, v: Value, jti: &str| {
        let mut c = base.clone();
        c[k] = v;
        c["jti"] = json!(jti);
        d.sign(c)
    };
    assert!(e
        .verify_svid(&with("aud", json!("http://elsewhere/token"), "a"))
        .await
        .is_err());
    assert!(e
        .verify_svid(&with("aud", json!(["x", s.token_url]), "b"))
        .await
        .is_ok());
    assert!(e
        .verify_svid(&with("iss", json!("http://other"), "c"))
        .await
        .is_err());
    assert!(e
        .verify_svid(&with("exp", json!(now - 1), "d"))
        .await
        .is_err());
    assert!(e
        .verify_svid(&with(
            "sub",
            json!(format!("spiffe://evil.example/openshell/sandbox/{UUID}")),
            "e"
        ))
        .await
        .is_err());

    // Signed by another key with the same kid.
    let other = Driver::new();
    assert!(e.verify_svid(&other.svid(UUID, "f")).await.is_err());
}

/// The edge on a loopback port.
async fn serving(e: Arc<Edge>) -> std::net::SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(e.serve(l));
    addr
}

/// One request to the edge: its status and body.
async fn request(
    addr: std::net::SocketAddr,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, Value) {
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "host.openshell.internal");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = sender
        .send_request(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes).trim().to_string()));
    (status, v)
}

async fn token(addr: std::net::SocketAddr, svid: &str) -> (StatusCode, Value) {
    let form = format!(
        "grant_type=client_credentials&client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer&client_assertion={svid}&scope=office"
    );
    request(
        addr,
        Method::POST,
        "/oauth/token",
        &[("content-type", "application/x-www-form-urlencoded")],
        &form,
    )
    .await
}

#[tokio::test]
async fn a_worker_gets_a_token_and_calls_the_tools_over_streamable_http() {
    let d = Driver::new();
    let addr = serving(edge(&d)).await;

    let (status, grant) = token(addr, &d.svid(UUID, "j1")).await;
    assert_eq!(status, StatusCode::OK, "{grant}");
    assert_eq!(grant["token_type"], "Bearer");
    assert_eq!(grant["expires_in"], 300);
    let bearer = format!("Bearer {}", grant["access_token"].as_str().unwrap());
    let auth = [
        ("authorization", bearer.as_str()),
        ("content-type", "application/json"),
    ];

    let (status, init) = request(
        addr,
        Method::POST,
        "/mcp",
        &auth,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "office");
    assert_eq!(init["result"]["instructions"], "the office");

    // A notification is accepted, with nothing to answer.
    let (status, _) = request(
        addr,
        Method::POST,
        "/mcp",
        &auth,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (_, list) = request(
        addr,
        Method::POST,
        "/mcp",
        &auth,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    )
    .await;
    assert_eq!(list["result"]["tools"][0]["name"], "mail_inbox");

    // The caller is the sandbox the token names, whatever the call says.
    let (_, call) = request(
        addr,
        Method::POST,
        "/mcp",
        &auth,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"mail_inbox","arguments":{"as":"barbara"}}}"#,
    )
    .await;
    assert_eq!(call["result"]["isError"], false);
    let text: Value =
        serde_json::from_str(call["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["member"], "alice");
    assert_eq!(text["args"]["as"], "barbara");

    let (_, failed) = request(
        addr,
        Method::POST,
        "/mcp",
        &auth,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"fail"}}"#,
    )
    .await;
    assert_eq!(failed["result"]["isError"], true);
    assert_eq!(failed["result"]["content"][0]["text"], "no such luck");

    let (_, unknown) = request(
        addr,
        Method::POST,
        "/mcp",
        &auth,
        r#"{"jsonrpc":"2.0","id":5,"method":"resources/list"}"#,
    )
    .await;
    assert_eq!(unknown["error"]["code"], -32601);

    // No server-sent stream.
    let (status, _) = request(addr, Method::GET, "/mcp", &auth, "").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);

    // Git, with the prefix off the path and the query kept.
    let (status, git) = request(
        addr,
        Method::GET,
        "/git/app.git/info/refs?service=git-upload-pack",
        &auth,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(git, "alice /app.git/info/refs?service=git-upload-pack");
}

#[tokio::test]
async fn without_a_token_of_the_edge_nothing_is_served() {
    let d = Driver::new();
    let addr = serving(edge(&d)).await;
    let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    let (status, _) = request(addr, Method::POST, "/mcp", &[], ping).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = request(
        addr,
        Method::POST,
        "/mcp",
        &[("authorization", "Bearer made-up")],
        ping,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = request(
        addr,
        Method::GET,
        "/git/app.git/info/refs",
        &[("authorization", "Basic YWxpY2U6cHc=")],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A sandbox the office doesn't run gets no token.
    let (status, grant) = token(addr, &d.svid("6f1c2d3e-0000-4000-8000-0000000000ff", "j2")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(grant["error"], "invalid_client");

    // Nor an assertion that isn't the driver's.
    let (status, _) = token(addr, "not.a.jwt").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A page naming an origin is refused, token or not.
    let (_, grant) = token(addr, &d.svid(UUID, "j3")).await;
    let bearer = format!("Bearer {}", grant["access_token"].as_str().unwrap());
    let (status, _) = request(
        addr,
        Method::POST,
        "/mcp",
        &[
            ("authorization", bearer.as_str()),
            ("origin", "http://evil.example"),
        ],
        ping,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[test]
fn forms_decode_as_browsers_and_supervisors_send_them() {
    let f = parse_form(b"a=1+2&b=%3A%2F&c=&d=%zz&e=%4");
    assert_eq!(f["a"], "1 2");
    assert_eq!(f["b"], ":/");
    assert_eq!(f["c"], "");
    assert_eq!(f["d"], "%zz");
    assert_eq!(f["e"], "%4");
}
