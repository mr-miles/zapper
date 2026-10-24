//! Entry points for fuzzing (feature `fuzz`): each takes bytes as the guest
//! would send them, runs the parser that reads them, and checks what must
//! hold whatever the input. The targets in `fuzz/` call these.

use std::net::Ipv4Addr;

use tokio::io::BufReader;
use zapper_policy::proto::{Frame, GuestMsg, MAX_FRAME, MAX_LINE, MAX_MCP_LINE};

use crate::daemon::{dns, gateway, proxy, session, tls};

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
            // A name the resolver passes on is a valid host name.
            if zapper_policy::names::hostname(&q.name).is_ok() {
                (Some(q), dns::NOERROR, Some(Ipv4Addr::new(10, 0, 0, 1)))
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
