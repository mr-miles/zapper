//! A firm's names as its sessions use them: answered by the daemon's DNS,
//! never the host's, and through the proxy the git server over TLS, the
//! cluster's API over vsock, and names the firm doesn't serve going
//! nowhere; a session of no firm resolves them on the host.

use std::net::Ipv4Addr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zapper_core::testing::{dial, echoing_vm, guest_tls, net_event, query_dns, TestCore};
use zapper_core::{NewSession, Session};

use crate::{facility, labels, test_core, test_run, FirmRun};

/// An A query for `name`.
fn query(name: &str) -> Vec<u8> {
    let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in name.split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]);
    q
}

/// The answer to an A query for `name`: `noerror` with the address,
/// or the response code.
async fn ask(t: &TestCore, s: &Arc<Session>, name: &str) -> Result<Ipv4Addr, u8> {
    let r = query_dns(&t.core, s, &query(name)).await.unwrap();
    let rcode = r[3] & 0x0f;
    if rcode != 0 {
        return Err(rcode);
    }
    let a = &r[r.len() - 4..];
    Ok(Ipv4Addr::new(a[0], a[1], a[2], a[3]))
}

const NXDOMAIN: u8 = 3;

const DNS_FIRM: &str = "
profiles:
  w:
    agent: { kind: claude-code }
    policy:
      net:
        - { action: allow, host: '*.acme.internal' }
        - { action: allow, host: acme.internal }
firms:
  acme:
    roles:
      ceo: { profile: w, skills: { work: ceo } }
    workers:
      - { name: ceo, role: ceo }
    git: { repos: [api] }
";

#[tokio::test]
async fn a_firms_names_are_answered_for_its_sessions_never_by_the_host() {
    let t = test_core(DNS_FIRM);
    let run = test_run(&t.core, "acme");
    let ceo = run
        .rt("ceo")
        .unwrap()
        .session
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    let d = run.domain.clone();
    assert_eq!(d, "acme.internal");

    // The host has no such names: an address means the daemon answered.
    let git = ask(&t, &ceo, &format!("git.{d}")).await.unwrap();
    assert_eq!(git.octets()[..2], [198, 18], "{git}");
    assert_eq!(ask(&t, &ceo, &format!("nope.{d}")).await, Err(NXDOMAIN));
    assert_eq!(ask(&t, &ceo, &d).await, Err(NXDOMAIN));
    // Until the cluster boots, its name isn't served either.
    assert_eq!(ask(&t, &ceo, &format!("cluster.{d}")).await, Err(NXDOMAIN));

    // A session of no firm, with the same rules: the host is asked, and
    // knows nothing of the firm's names.
    let plain = t
        .core
        .test_session(
            NewSession::plain("w".into(), t.workdir(), Default::default()),
            true,
        )
        .unwrap();
    assert!(ask(&t, &plain, &format!("git.{d}")).await.is_err());
}

const FIRM: &str = "
profiles:
  w: { agent: { kind: claude-code } }
firms:
  acme:
    roles:
      ceo: { profile: w, skills: { work: ceo } }
      eng: { profile: w, skills: { work: software-engineer } }
    workers:
      - { name: ceo, role: ceo }
      - { name: pat, role: eng, reports_to: ceo }
    git: { repos: [api] }
";

fn worker(run: &FirmRun, name: &str) -> Arc<Session> {
    run.rt(name)
        .unwrap()
        .session
        .lock()
        .unwrap()
        .clone()
        .unwrap()
}

fn allow(t: &TestCore, s: &Arc<Session>, host: &str) {
    let rule =
        serde_yaml::from_str(&format!("{{ kind: net, action: allow, host: '{host}' }}")).unwrap();
    t.core.add_session_rule(s, rule, None);
}

#[tokio::test]
async fn the_git_server_answers_over_tls_with_the_sessions_ca() {
    let t = test_core(FIRM);
    let run = test_run(&t.core, "acme");
    // The CEO holds every repository: their git rule allows the name.
    let ceo = worker(&run, "ceo");
    let host = format!("git.{}", run.domain);
    let io = dial(&t, &ceo, &host, 443);
    let name = rustls::pki_types::ServerName::try_from(host.clone()).unwrap();
    let mut tls = guest_tls(&ceo).connect(name, io).await.unwrap();
    tls.write_all(
        format!("GET /api.git/info/refs HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut reply = String::new();
    let _ = tls.read_to_string(&mut reply).await;
    assert!(reply.starts_with("HTTP/1.1 401"), "{reply}");
    assert!(reply.contains("credentials required"), "{reply}");
}

#[tokio::test]
async fn the_clusters_api_is_its_vms_port() {
    let t = test_core(FIRM);
    let run = test_run(&t.core, "acme");
    let mut new = NewSession::plain("w".into(), t.workdir(), Default::default());
    new.labels = labels(&run.firm, &run.id, facility::CLUSTER, false);
    let cluster = t.core.test_session(new, true).unwrap();
    echoing_vm(&cluster);
    facility::test_cluster(&t.core, &run, &cluster);

    let pat = worker(&run, "pat");
    let host = format!("cluster.{}", run.domain);
    allow(&t, &pat, &host);
    let mut io = dial(&t, &pat, &host, 6443);
    io.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    io.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
    let e = net_event(&t, &pat, 0).await;
    assert_eq!(e["action"], "allow", "{e}");
}

#[tokio::test]
async fn a_name_the_firm_doesnt_serve_goes_nowhere() {
    let t = test_core(FIRM);
    let run = test_run(&t.core, "acme");
    let pat = worker(&run, "pat");
    let d = run.domain.clone();
    allow(&t, &pat, &format!("*.{d}"));
    // One net event each, refused after the rules allowed it.
    for (i, (name, port)) in [(format!("x.{d}"), 443), (format!("git.{d}"), 80)]
        .into_iter()
        .enumerate()
    {
        let _io = dial(&t, &pat, &name, port);
        let e = net_event(&t, &pat, i).await;
        let error = e["error"].as_str().unwrap_or_default();
        assert!(
            error.contains(&format!("nothing serves {name}:{port}")),
            "{e}"
        );
    }

    // A session of no firm: the name is the host's to resolve.
    let plain = t
        .core
        .test_session(
            NewSession::plain("w".into(), t.workdir(), Default::default()),
            true,
        )
        .unwrap();
    allow(&t, &plain, &format!("*.{d}"));
    let _io = dial(&t, &plain, &format!("git.{d}"), 443);
    let e = net_event(&t, &plain, 0).await;
    let error = e["error"].as_str().unwrap_or_default();
    assert!(error.starts_with("resolving git."), "{e}");
}
