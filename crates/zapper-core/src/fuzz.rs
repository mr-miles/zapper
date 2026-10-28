//! Entry points for fuzzing (feature `fuzz`): each takes bytes as the guest
//! would send them, runs the parser that reads them, and checks what must
//! hold whatever the input. The targets in `fuzz/` call these.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use tokio::io::BufReader;
use zapper_policy::proto::{Frame, GuestMsg, MAX_FRAME, MAX_LINE, MAX_MCP_LINE};

use crate::daemon::{dns, gateway, httplog, proxy, session, tls};

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(f)
}

/// A DNS query from the guest: parsed, and answered as the resolver would.
pub fn dns_query(data: &[u8]) {
    let Some(parsed) = dns::parse(data) else {
        assert!(data.len() < 12);
        return;
    };
    let (question, rcode, answer) = match &parsed {
        Ok(q) => {
            // A name the resolver passes on is a valid host name; answer with a
            // synthetic address of the queried family (A, else AAAA).
            if zapper_policy::names::hostname(&q.name).is_ok() {
                let ip = if q.qtype == dns::TYPE_AAAA {
                    IpAddr::V6(Ipv6Addr::new(0x2001, 0x2, 0, 0, 0, 0, 0, 1))
                } else {
                    IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))
                };
                (Some(q), dns::NOERROR, Some(ip))
            } else {
                (Some(q), dns::REFUSED, None)
            }
        }
        Err(r) => (None, r.rcode, None),
    };
    let r = dns::respond(data, question, rcode, answer);
    assert!(r.len() >= 12);
    assert_eq!(&r[..2], &data[..2], "the id is echoed");
    assert_ne!(r[2] & 0x80, 0, "a response");
    // When an address was answered, its bytes close the response intact.
    if let Some(ip) = answer {
        match ip {
            IpAddr::V4(a) => assert_eq!(&r[r.len() - 4..], &a.octets()),
            IpAddr::V6(a) => assert_eq!(&r[r.len() - 16..], &a.octets()),
        }
    }
}

/// The first bytes of a guest's TLS connection: the record length and the
/// server name, which decides passthrough.
pub fn tls_hello(data: &[u8]) {
    if let Some(n) = tls::tls_record_len(data) {
        assert!(n >= 5);
    }
    // Passthrough goes ahead only for what isn't TLS, TLS without a server
    // name, or a server name that is the checked host.
    if proxy::server_name_matches(data, "api.example").is_ok() {
        match tls::client_hello_sni(data) {
            Ok(None) => {}
            Ok(Some(name)) => {
                assert_eq!(
                    zapper_policy::names::hostname(&name).as_deref(),
                    Ok("api.example")
                )
            }
            Err(()) => panic!("an unreadable hello went through"),
        }
    }
}

/// A host and port as a guest writes them in CONNECT or Host.
pub fn authority(data: &[u8]) {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    if let Some((host, _port)) = proxy::split_authority(s, 443) {
        let _ = zapper_policy::names::hostname(&host);
    }
    if let Ok(name) = zapper_policy::names::hostname(s) {
        // Validation is a fixed point.
        assert_eq!(
            zapper_policy::names::hostname(&name).as_deref(),
            Ok(name.as_str())
        );
    }
}

/// The guest agent's control channel: bounded lines of JSON messages.
pub fn guest_line(data: &[u8]) {
    let mut r = BufReader::new(data);
    while let Ok(Some(line)) = block_on(session::read_guest_line(&mut r)) {
        assert!(line.len() <= MAX_LINE);
        if let Ok(msg) = serde_json::from_slice::<GuestMsg>(&line) {
            let again = serde_json::to_vec(&msg).unwrap();
            let _: GuestMsg = serde_json::from_slice(&again).unwrap();
        }
    }
}

/// Terminal frames, read by the host and by the guest's codec alike.
pub fn frames(data: &[u8]) {
    let mut host = data;
    while let Ok(Some(_)) = block_on(session::read_frame(&mut host)) {}
    let mut guest = data;
    while let Ok(Some(f)) = Frame::read_from(&mut guest) {
        let mut out = Vec::new();
        f.write_to(&mut out).unwrap();
        assert!(out.len() <= MAX_FRAME + 5);
    }
}

/// The MCP gateway's lines from the guest.
pub fn mcp_line(data: &[u8]) {
    let mut r = BufReader::new(data);
    while let Ok(Some(line)) = block_on(gateway::line(&mut r)) {
        assert!(line.len() <= MAX_MCP_LINE + 1);
        let _ = serde_json::from_slice::<serde_json::Value>(&line);
    }
}

/// Secrets for the header targets: distinct values, each for some hosts.
fn secrets_config() -> &'static zapper_policy::Config {
    static C: std::sync::OnceLock<zapper_policy::Config> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        zapper_policy::Config::from_yaml(
            "secrets:\n  a: { value: VALUEA7f3q, hosts: [api.example] }\n  ab: { value: VALUEAB9c1w, hosts: ['*.example'], plaintext: true }\n  c: { value: VALUEC2e8z, hosts: [other.test] }\n",
        )
        .unwrap()
    })
}

const HOSTS: [&str; 4] = ["api.example", "x.api.example", "other.test", "evil.example"];

/// Splits fuzz data into lines of `name: value`.
fn header_lines(data: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    data.split(|b| *b == b'\n')
        .filter_map(|l| {
            let i = l.iter().position(|b| *b == b':')?;
            Some((
                l[..i].to_vec(),
                l[i + 1..]
                    .strip_prefix(b" ")
                    .unwrap_or(&l[i + 1..])
                    .to_vec(),
            ))
        })
        .collect()
}

/// What goes upstream from a guest's request headers, once cleaned and
/// given its secrets: Host is the checked host, no hop-by-hop header is
/// left, and a secret's value is added only for a host it may go to, and
/// over plain HTTP only with `plaintext`.
///
/// The first byte picks the host, plain HTTP, which secrets the rule adds
/// and whether a `set_headers` template is used; the rest is headers.
pub fn headers(data: &[u8]) {
    let Some((&flags, rest)) = data.split_first() else {
        return;
    };
    let host = HOSTS[(flags & 3) as usize];
    let plaintext = flags & 4 != 0;
    let names = ["a", "ab", "c"];
    let effects = zapper_policy::NetEffects {
        secrets: names
            .iter()
            .enumerate()
            .filter(|(i, _)| flags & (8 << i) != 0)
            .map(|(_, n)| n.to_string())
            .collect(),
        set_headers: if flags & 64 != 0 {
            vec![(
                "x-key".into(),
                format!("k={{{{secret:{}}}}}", names[(flags >> 7) as usize % 3]),
            )]
        } else {
            vec![]
        },
        protocol: Default::default(),
    };
    let mut h = hyper::HeaderMap::new();
    for (k, v) in header_lines(rest) {
        if let (Ok(k), Ok(v)) = (
            hyper::header::HeaderName::from_bytes(&k),
            hyper::header::HeaderValue::from_bytes(&v),
        ) {
            h.append(k, v);
        }
    }
    let count = |h: &hyper::HeaderMap, needle: &str| -> usize {
        h.values()
            .map(|v| {
                String::from_utf8_lossy(v.as_bytes())
                    .matches(needle)
                    .count()
            })
            .sum()
    };
    let config = secrets_config();
    let before: Vec<usize> = names
        .iter()
        .map(|n| count(&h, &config.secrets[*n].source.resolve().unwrap()))
        .collect();
    proxy::clean_headers(&mut h, host, 443, 443);
    assert_eq!(h.get_all("host").iter().count(), 1);
    assert_eq!(h["host"], host);
    for hop in ["connection", "proxy-authorization", "keep-alive", "upgrade"] {
        assert!(h.get(hop).is_none(), "{hop}");
    }
    if proxy::inject_with(
        config,
        &Default::default(),
        host,
        plaintext,
        &effects,
        &mut h,
        &Default::default(),
    )
    .is_err()
    {
        return;
    }
    for (n, was) in names.iter().zip(before) {
        let s = &config.secrets[*n];
        let added = count(&h, &s.source.resolve().unwrap()) > was;
        if added {
            assert!(s.allows_host(host), "secret {n} added for {host}");
            assert!(
                !plaintext || s.plaintext,
                "secret {n} added over plain HTTP"
            );
        }
    }
}

/// The request log's view of a body and its headers: decoded as its
/// content-encoding says, and never showing an added secret.
///
/// The first byte picks the encoding and the secret; the rest is the body.
pub fn log_body(data: &[u8]) {
    let Some((&flags, body)) = data.split_first() else {
        return;
    };
    let encoding = ["", "gzip", "deflate", "br"][(flags & 3) as usize];
    let secret = ["VALUEA7f3q", "abcd", "x-y_z0123456789"][(flags >> 2) as usize % 3].to_string();
    let values = vec![secret.clone()];
    let headers = vec![
        ("content-encoding".to_string(), encoding.to_string()),
        (
            "authorization".to_string(),
            String::from_utf8_lossy(body).into_owned(),
        ),
    ];
    for (_, v) in httplog::redact_headers(&headers, &values) {
        assert!(!v.contains(&secret), "a header shows the secret");
    }
    if let Some(out) = httplog::decode(body, encoding) {
        assert!(out.len() <= 4 * httplog::BODY_CAP, "decoding is bounded");
    }
    let c = httplog::Captured {
        data: body.to_vec(),
        total: body.len() as u64 + (flags as u64 >> 5),
    };
    let v = httplog::body_json(&c, &headers, &values);
    if let Some(t) = v["text"].as_str() {
        assert!(!t.contains(&secret), "the text shows the secret");
    }
    if let Some(b) = v["base64"].as_str() {
        use base64::Engine as _;
        let raw = base64::engine::general_purpose::STANDARD.decode(b).unwrap();
        assert!(
            !raw.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "the bytes shown hold the secret"
        );
    }
}

/// A guest's request against rules with globs, ports, methods, paths and a
/// final deny: the decision names a rule that matches, and nothing gets
/// past the final rule.
///
/// The first bytes pick the port; the rest is `METHOD host path`.
pub fn policy(data: &[u8]) {
    static P: std::sync::OnceLock<(zapper_policy::Policy, zapper_policy::Policy)> =
        std::sync::OnceLock::new();
    let (global, profile) = P.get_or_init(|| {
        let p = |y: &str| serde_yaml::from_str::<zapper_policy::Policy>(y).unwrap();
        (
            p("net:\n  - { id: g-final, action: deny, host: vault.example, final: true }\n  - { id: g-ask, action: ask, host: '*' }\n"),
            p("net:\n  - { id: p-get, action: allow, host: '*.example', ports: [443], methods: [GET], path: '/docs/**' }\n  - { id: p-vault, action: allow, host: vault.example }\n  - { id: p-deny, action: deny, host: 'evil.*' }\n"),
        )
    });
    let Some((port, rest)) = data.split_first_chunk::<2>() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(rest) else {
        return;
    };
    let mut f = text.splitn(3, ' ');
    let (Some(method), Some(host)) = (f.next(), f.next()) else {
        return;
    };
    let req = zapper_policy::NetRequest {
        host: host.to_string(),
        port: u16::from_be_bytes(*port),
        method: (!method.is_empty()).then(|| method.to_string()),
        path: f.next().map(str::to_string),
    };
    let layers = [
        (zapper_policy::Scope::Profile("p".into()), profile),
        (zapper_policy::Scope::Global, global),
    ];
    let (d, _) = zapper_policy::evaluate_net(&layers, &req);
    if zapper_policy::host_matches("vault.example", &req.host) {
        assert_ne!(d.action, zapper_policy::Action::Allow, "past a final rule");
    }
    if let Some(id) = &d.rule {
        let rule = global
            .net
            .iter()
            .chain(&profile.net)
            .find(|r| r.id.as_deref() == Some(id.as_str()))
            .unwrap();
        assert!(
            rule.matches(&req),
            "{id} decided a request it doesn't match"
        );
        assert_eq!(rule.action, d.action);
    }
}

/// The upstream request URI the inspecting proxy builds (`proxy::upstream_uri`):
/// it must never panic, and for HTTP/2 the authority is always the checked
/// host — a request path can never smuggle one in.
pub fn http2_uri(data: &[u8]) {
    let mut it = data.splitn(3, |&b| b == 0);
    let raw = String::from_utf8_lossy(it.next().unwrap_or(b"")).into_owned();
    let port = it
        .next()
        .and_then(|p| std::str::from_utf8(p).ok())
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(443);
    let path = String::from_utf8_lossy(it.next().unwrap_or(b"")).into_owned();
    // Whatever the bytes, neither form may panic.
    let _ = proxy::upstream_uri(&raw, port, &path, false);
    let _ = proxy::upstream_uri(&raw, port, &path, true);
    // Production always passes a validated host; for one, the HTTP/2 authority
    // is exactly that host (IPv6 literals aside, which bracket in the URI).
    if let Ok(host) = zapper_policy::names::host(&raw) {
        if !host.contains(':') {
            if let Some(auth) = proxy::upstream_uri(&host, port, &path, true).authority() {
                assert_eq!(auth.host(), host, "path smuggled an authority: {path:?}");
            }
        }
    }
}

/// The Postgres wire pieces used by database credential injection
/// (`daemon::pgwire`): the SSLRequest/startup parsers and the message framer
/// must never panic or read out of bounds on arbitrary bytes.
pub fn pg_startup(data: &[u8]) {
    use crate::daemon::dbwire::pgwire;
    let _ = pgwire::is_ssl_request(data);
    let _ = pgwire::is_gssenc_request(data);
    let _ = pgwire::parse_startup(data);
    // Frame typed messages; each reported length stays within the buffer and
    // always advances, so the walk terminates.
    let mut rest = data;
    while let Some((_typ, body, consumed)) = pgwire::message(rest) {
        assert!(consumed <= rest.len() && consumed >= 5);
        assert_eq!(body.len() + 5, consumed);
        rest = &rest[consumed..];
    }
}

/// The SCRAM server-first / server-final parsing (`pgwire::scram`), shared by
/// Postgres, MongoDB and Kafka: the proxy reads these from the server, so they
/// must never panic on arbitrary bytes, for either hash.
pub fn scram_server(data: &[u8]) {
    use crate::daemon::dbwire::pgwire::scram;
    for hash in [scram::Hash::Sha256, scram::Hash::Sha512] {
        let client = scram::Client::with_hash(hash, "user", "pencil", "clientnonce");
        if let Ok(fin) = client.client_final(data) {
            let _ = scram::Client::verify_server_final(&fin, data);
        }
    }
}

/// Cassandra (CQL) guest-facing parsing: the frame header bounds and the
/// STARTUP string-map walk must hold on arbitrary bytes.
pub fn cassandra_frame(data: &[u8]) {
    use crate::daemon::dbwire::cassandra;
    let _ = cassandra::startup_has_compression(data);
    let mut src = data;
    let mut buf = Vec::new();
    while let Ok(f) = block_on(cassandra::read_frame(&mut src, &mut buf)) {
        let _ = cassandra::startup_has_compression(&f.body);
    }
}

/// MySQL guest-facing parsing: packet framing, the SSLRequest test, and the
/// initial-handshake parser.
pub fn mysql_packet(data: &[u8]) {
    use crate::daemon::dbwire::mysql;
    let _ = mysql::is_ssl_request(data);
    let _ = mysql::parse_initial(data);
    let mut src = data;
    let mut buf = Vec::new();
    while let Ok((_seq, payload)) = block_on(mysql::read_packet(&mut src, &mut buf)) {
        let _ = mysql::is_ssl_request(&payload);
        let _ = mysql::parse_initial(&payload);
    }
}

/// TDS guest-facing parsing: packet framing, the PRELOGIN encryption option,
/// and the login-response token scan.
pub fn tds_packet(data: &[u8]) {
    use crate::daemon::dbwire::tds;
    let _ = tds::prelogin_encryption(data);
    let _ = tds::login_result(data);
    let mut src = data;
    let mut buf = Vec::new();
    while let Ok((_t, _s, payload)) = block_on(tds::read_packet(&mut src, &mut buf)) {
        let _ = tds::prelogin_encryption(&payload);
    }
}

/// MongoDB reply parsing (`daemon::mongodb`): the little-endian BSON walker and
/// the OP_MSG reply framer, run over arbitrary bytes.
pub fn mongodb_reply(data: &[u8]) {
    use crate::daemon::dbwire::mongodb;
    let _ = mongodb::walk(data);
    let mut src = data;
    let mut buf = Vec::new();
    while block_on(mongodb::read_reply(&mut src, &mut buf)).is_ok() {}
}

/// Kafka response parsing (`daemon::kafka`): the SaslAuthenticate body parser
/// and the response framer, run over arbitrary bytes.
pub fn kafka_response(data: &[u8]) {
    use crate::daemon::dbwire::kafka;
    let _ = kafka::auth_bytes(data);
    let mut src = data;
    let mut buf = Vec::new();
    while let Ok(body) = block_on(kafka::read_response(&mut src, &mut buf)) {
        let _ = kafka::auth_bytes(&body);
    }
}
