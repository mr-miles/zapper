//! What the guest sends is bounded: lines on the control channel, frames on
//! terminals, connections through the proxy, and the approvals a session
//! may have waiting.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use zapper_policy::proto::{Frame, MAX_FRAME, MAX_LINE};
use zapper_policy::{AnyRule, Kind};

use super::session::{read_frame, read_guest_line};
use crate::testing::TestCore;
use crate::{NewSession, Session};

fn session(t: &TestCore) -> Arc<Session> {
    t.core
        .test_session(
            NewSession::plain("web".into(), t.workdir(), Default::default()),
            true,
        )
        .unwrap()
}

/// A control line longer than `MAX_LINE` ends the channel, however the
/// guest splits it; one of `MAX_LINE` is read.
#[tokio::test]
async fn a_control_line_is_at_most_max_line() {
    let read = |bytes: Vec<u8>| async move {
        let mut r = BufReader::new(&bytes[..]);
        read_guest_line(&mut r).await
    };
    let mut ok = vec![b'x'; MAX_LINE - 1];
    ok.push(b'\n');
    assert_eq!(read(ok).await.unwrap().unwrap().len(), MAX_LINE);
    let long = vec![b'x'; MAX_LINE + 1];
    let e = read(long).await.unwrap_err();
    assert!(e.to_string().contains("oversized"), "{e}");
    assert!(read(vec![]).await.unwrap().is_none(), "the end is None");
}

/// A frame header naming more than `MAX_FRAME` bytes is refused before
/// anything is read or allocated, on the host and in the guest's codec.
#[tokio::test]
async fn a_frame_is_at_most_max_frame() {
    let header = |len: usize| {
        let mut h = vec![0u8];
        h.extend_from_slice(&(len as u32).to_be_bytes());
        h
    };
    let mut big = header(MAX_FRAME + 1);
    let e = read_frame(&mut &big[..]).await.unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{e}");
    let e = Frame::read_from(&mut &big[..]).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{e}");
    // The largest frame is read.
    big = header(MAX_FRAME);
    big.extend(vec![b'x'; MAX_FRAME]);
    assert!(read_frame(&mut &big[..]).await.unwrap().is_some());
}

/// A session holds at most 256 connections through the proxy at once:
/// the next is closed unread until one ends.
#[tokio::test]
async fn a_session_holds_at_most_256_connections() {
    let t = TestCore::new("profiles:\n  web: {}\n");
    let s = session(&t);
    let path = t.dir.join("proxy.sock");
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(super::nic::serve(
        t.core.daemon().clone(),
        s.clone(),
        listener,
    ));
    let mut held = Vec::new();
    for _ in 0..256 {
        held.push(UnixStream::connect(&path).await.unwrap());
    }
    // Every permit taken: the next connection is closed at once.
    for _ in 0..200 {
        if s.conns.available_permits() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(s.conns.available_permits(), 0);
    let mut over = UnixStream::connect(&path).await.unwrap();
    let mut buf = [0u8; 1];
    let n = tokio::time::timeout(Duration::from_secs(5), over.read(&mut buf))
        .await
        .expect("closed, not held")
        .unwrap_or(0);
    assert_eq!(n, 0);
    // One ends: a new one is held again.
    drop(held.pop());
    for _ in 0..200 {
        if s.conns.available_permits() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut again = UnixStream::connect(&path).await.unwrap();
    again.write_all(b"").await.unwrap();
    let r = tokio::time::timeout(Duration::from_millis(300), again.read(&mut buf)).await;
    assert!(r.is_err(), "held open, waiting for its header");
}

/// A session can't flood the operator: with 32 of its asks waiting, the
/// next is denied at once and never shown; another session still asks.
#[tokio::test]
async fn a_session_has_at_most_32_approvals_waiting() {
    let t = TestCore::with_daemon("ask_timeout_secs: 30", "profiles:\n  web: {}\n");
    let (s, other) = (session(&t), session(&t));
    let d = t.core.daemon().clone();
    let rule = || {
        AnyRule::Net(
            serde_json::from_value(serde_json::json!({ "action": "allow", "host": "x.example" }))
                .unwrap(),
        )
    };
    for i in 0..32 {
        let (d, s, rule) = (d.clone(), s.clone(), rule());
        tokio::spawn(async move {
            d.ask(&s, Kind::Net, format!("connect a{i}.example:443"), rule)
                .await
        });
    }
    for _ in 0..200 {
        if d.approvals().len() == 32 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(d.approvals().len(), 32);
    let denied = tokio::time::timeout(
        Duration::from_secs(1),
        d.ask(&s, Kind::Net, "connect over.example:443".into(), rule()),
    )
    .await
    .expect("answered at once");
    assert!(!denied);
    assert_eq!(d.approvals().len(), 32, "never shown");
    let d2 = d.clone();
    tokio::spawn(async move {
        d2.ask(&other, Kind::Net, "connect b.example:443".into(), rule())
            .await
    });
    for _ in 0..200 {
        if d.approvals().len() == 33 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(d.approvals().len(), 33, "another session's ask waits");
}
