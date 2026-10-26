//! The database auth-terminating relay (proposal 07): a child of the egress
//! proxy that handles a database `protocol:` rule. The guest connects with no
//! password; the proxy presents it as authenticated, authenticates to the real
//! server with the host-held `dbauth` secret, then splices. Each protocol's
//! wire module does its handshake; this module is the orchestration: resolve
//! the secret, negotiate each leg's transport, dispatch, and splice.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use zapper_policy::{NetProtocol, NetRequest};

use super::{dial, log, note_failure, note_verdict, NetLog, Prefixed, Target, Verdict};
use crate::daemon::session::Session;
use crate::daemon::{
    cassandra, httplog, kafka, mongodb, mysql, pgwire, redis, services, tds, tls, Daemon,
};

/// A `dbauth` credential resolved from the config: the user the proxy
/// authenticates as, a pinned database (else the guest's), the host-held
/// password, and whether a non-TLS server leg is permitted.
struct DbCred {
    user: String,
    database: Option<String>,
    password: String,
    plaintext: bool,
    /// The SASL mechanism, for Kafka; ignored by the other protocols.
    mechanism: String,
}

/// The outcome of a database handshake: the two authenticated streams, and the
/// bytes to send the guest before splicing — whatever presents it as logged in
/// (an `AuthenticationOk`, a fabricated `READY`/OK packet, the server's
/// pipelined post-auth bytes).
struct Relayed {
    guest: Box<dyn services::Io>,
    server: Box<dyn services::Io>,
    to_guest: Vec<u8>,
}

/// Resolves the one `dbauth` secret a database rule names (the config validates
/// there is exactly one with a user).
fn db_cred(d: &Daemon, name: Option<&str>) -> Result<DbCred, String> {
    let c = d.config.read().unwrap();
    let sec = name
        .and_then(|n| c.secrets.get(n))
        .ok_or("database rule has no dbauth secret")?;
    let user = sec.user.clone().ok_or("dbauth secret has no user")?;
    let password = sec
        .source
        .resolve()
        .map_err(|e| format!("resolving dbauth secret: {e:#}"))?;
    Ok(DbCred {
        user,
        database: sec.database.clone(),
        password,
        plaintext: sec.plaintext,
        mechanism: sec
            .mechanism
            .clone()
            .unwrap_or_else(|| "SCRAM-SHA-256".into()),
    })
}

/// The database auth-terminating relay (proposal 07): the guest connects with
/// no password; the proxy presents it as authenticated, authenticates to the
/// real server with the host-held `dbauth` secret, then splices the session.
/// Only the startup/auth handshake is parsed — never the query stream. The
/// per-protocol `*_exchange` does the handshake; this driver is the shared
/// plumbing (record, secret, dial, logging, splice).
pub(super) async fn relay_database<S>(
    d: &Daemon,
    s: &Arc<Session>,
    guest: S,
    first: Vec<u8>,
    endpoint: &NetRequest,
    addr: &Target,
    v: Verdict,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let proto = v.effects.protocol;
    let rec = s.requests.lock().unwrap().start(
        proto.scheme(),
        "CONNECT",
        &endpoint.host,
        endpoint.port,
        None,
        vec![],
    );
    note_verdict(&rec, &v);
    let fail = |status, error: String| {
        log(
            d,
            s,
            NetLog {
                req: endpoint,
                verdict: Some(&v),
                inspected: false,
                status: None,
                secrets: vec![],
                error: Some(error.clone()),
            },
        );
        note_failure(s, &rec, status, Some(error));
    };

    // The relay only serves a real network address, never a daemon service.
    let Target::Addr(server_addr) = addr else {
        return fail(
            403,
            format!(
                "{} is a service; a {proto} rule can't relay to it",
                endpoint.host
            ),
        );
    };
    let name = v.effects.secrets.first().cloned();
    let cred = match db_cred(d, name.as_deref()) {
        Ok(c) => c,
        Err(e) => return fail(500, e),
    };
    let server = match dial(*server_addr).await {
        Ok(c) => c,
        Err(e) => return fail(502, format!("connecting to {}: {e}", endpoint.host)),
    };

    let host = endpoint.host.as_str();
    let outcome = match proto {
        NetProtocol::Postgres => pg_exchange(s, guest, first, server, host, &cred).await,
        NetProtocol::Redis => redis_exchange(s, guest, first, server, host, &cred).await,
        NetProtocol::Cassandra => cassandra_exchange(s, guest, first, server, host, &cred).await,
        NetProtocol::Mongodb => mongodb_exchange(s, guest, first, server, host, &cred).await,
        NetProtocol::Mysql => mysql_exchange(s, guest, first, server, host, &cred).await,
        NetProtocol::Tds => tds_exchange(s, guest, first, server, host, &cred).await,
        NetProtocol::Kafka => kafka_exchange(s, guest, first, server, host, &cred).await,
        NetProtocol::Tcp => unreachable!("tcp is not a database protocol"),
    };
    let Relayed {
        mut guest,
        mut server,
        to_guest,
    } = match outcome {
        Ok(r) => r,
        Err(e) => return fail(502, format!("relaying to {host}: {e}")),
    };

    // The handshake carried the secret; present the guest as authenticated and
    // splice the rest byte for byte.
    log(
        d,
        s,
        NetLog {
            req: endpoint,
            verdict: Some(&v),
            inspected: false,
            status: None,
            secrets: name.into_iter().collect(),
            error: None,
        },
    );
    httplog::mark_ttfb(&rec);
    if guest.write_all(&to_guest).await.is_ok() {
        if let Ok((up, down)) = tokio::io::copy_bidirectional(&mut guest, &mut server).await {
            let mut r = rec.lock().unwrap();
            r.req_body.total = up;
            r.resp_body.total = down + to_guest.len() as u64;
        }
    }
    httplog::finish(&s.requests, &rec);
}

/// Postgres: read the guest's startup — terminating its TLS with the session
/// CA when it asks for it (the guest already trusts our root, so sslmode
/// require/verify work; verify-full too, since we issue a per-host leaf) and
/// declining a GSSAPI-encryption preamble — authenticate to the server over
/// SCRAM, and hand back the `AuthenticationOk` plus the server's pipelined
/// post-auth bytes for the guest.
async fn pg_exchange<S>(
    s: &Arc<Session>,
    guest: S,
    first: Vec<u8>,
    server: TcpStream,
    host: &str,
    cred: &DbCred,
) -> Result<Relayed, String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (guest, guest_tls, startup) = pg_guest_startup(s, guest, first, host).await?;
    // The secret defines the user; the database is pinned, else the guest's.
    let database = cred.database.clone().or(startup.database);
    let mut server = pg_server_leg(server, host, guest_tls, cred.plaintext).await?;
    let leftover = pgwire::authenticate_server(
        &mut server,
        &cred.user,
        database.as_deref(),
        &cred.password,
        &scram_nonce(),
    )
    .await?;
    let mut to_guest = pgwire::AUTH_OK.to_vec();
    to_guest.extend_from_slice(&leftover);
    Ok(Relayed {
        guest,
        server,
        to_guest,
    })
}

/// Handles the guest's Postgres opening: declines a `GSSENCRequest` (`N`),
/// accepts an `SSLRequest` (`S`) and terminates TLS with the per-session CA,
/// then reads the `StartupMessage`. A plaintext `StartupMessage`
/// (`sslmode=disable`) is read as-is. Returns the (possibly TLS-wrapped) guest
/// stream and the parsed startup.
async fn pg_guest_startup<S>(
    s: &Arc<Session>,
    mut guest: S,
    mut buf: Vec<u8>,
    host: &str,
) -> Result<(Box<dyn services::Io>, bool, pgwire::Startup), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut chunk = [0u8; 4096];
    loop {
        if buf.len() >= 8 {
            if pgwire::is_gssenc_request(&buf) {
                guest
                    .write_all(b"N")
                    .await
                    .map_err(|e| format!("declining the guest's GSSAPI request: {e}"))?;
                buf.drain(..8);
                continue;
            }
            if pgwire::is_ssl_request(&buf) {
                guest
                    .write_all(b"S")
                    .await
                    .map_err(|e| format!("accepting the guest's SSLRequest: {e}"))?;
                buf.drain(..8);
                let config = s.ca.server_config(host).map_err(|e| format!("{e:#}"))?;
                let tls = TlsAcceptor::from(config)
                    .accept(Prefixed::new(buf, guest))
                    .await
                    .map_err(|e| format!("TLS from guest: {e}"))?;
                let mut tls: Box<dyn services::Io> = Box::new(tls);
                let startup = pgwire::read_startup(&mut tls, Vec::new()).await?;
                return Ok((tls, true, startup));
            }
            // Not a preamble: a plaintext StartupMessage (sslmode=disable).
            break;
        }
        let n = guest
            .read(&mut chunk)
            .await
            .map_err(|e| format!("reading the guest preamble: {e}"))?;
        if n == 0 {
            return Err("guest closed before sending a startup".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let mut guest: Box<dyn services::Io> = Box::new(Prefixed::new(buf, guest));
    let startup = pgwire::read_startup(&mut guest, Vec::new()).await?;
    Ok((guest, false, startup))
}

/// Redis/Valkey: the guest holds no password and never authenticates; the proxy
/// issues one `AUTH` on the server leg with the host-held credential, then
/// splices. The guest's commands run on that pre-authenticated connection.
async fn redis_exchange<S>(
    s: &Arc<Session>,
    guest: S,
    first: Vec<u8>,
    server: TcpStream,
    host: &str,
    cred: &DbCred,
) -> Result<Relayed, String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (guest, guest_tls) = guest_leg(s, guest, first, host).await?;
    let mut server = server_leg(server, host, guest_tls, cred.plaintext).await?;
    redis::authenticate(&mut server, &cred.user, &cred.password).await?;
    Ok(Relayed {
        guest,
        server,
        to_guest: vec![],
    })
}

/// Cassandra (CQL): relay the guest's OPTIONS/STARTUP to the server and, when
/// the server demands auth, answer it with SASL/PLAIN using the host-held
/// credential, then fabricate a READY for the guest. Then splice.
async fn cassandra_exchange<S>(
    s: &Arc<Session>,
    guest: S,
    first: Vec<u8>,
    server: TcpStream,
    host: &str,
    cred: &DbCred,
) -> Result<Relayed, String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut guest, guest_tls) = guest_leg(s, guest, first, host).await?;
    let mut server = server_leg(server, host, guest_tls, cred.plaintext).await?;
    cassandra::handshake(&mut guest, &mut server, &cred.user, &cred.password).await?;
    Ok(Relayed {
        guest,
        server,
        to_guest: vec![],
    })
}

/// MongoDB: the guest holds no password and never authenticates; the proxy
/// authenticates the server leg over SCRAM-SHA-256 (`OP_MSG` SASL), then
/// splices. The guest's `hello` and commands run on that connection. The
/// `database` pin is the auth source (default `admin`).
async fn mongodb_exchange<S>(
    s: &Arc<Session>,
    guest: S,
    first: Vec<u8>,
    server: TcpStream,
    host: &str,
    cred: &DbCred,
) -> Result<Relayed, String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (guest, guest_tls) = guest_leg(s, guest, first, host).await?;
    let mut server = server_leg(server, host, guest_tls, cred.plaintext).await?;
    let auth_db = cred.database.as_deref().unwrap_or("admin");
    mongodb::authenticate(
        &mut server,
        &cred.user,
        &cred.password,
        auth_db,
        &scram_nonce(),
    )
    .await?;
    Ok(Relayed {
        guest,
        server,
        to_guest: vec![],
    })
}

/// MySQL/MariaDB: the server speaks first, so the proxy plays a server to the
/// guest (Initial Handshake → read its response → OK) and a client to the real
/// server, authenticating with the host-held credential and upgrading the
/// server leg to TLS mid-handshake unless the secret is plaintext. The guest's
/// credentials are ignored; it is told OK once the server leg authenticates.
async fn mysql_exchange<S>(
    s: &Arc<Session>,
    guest: S,
    first: Vec<u8>,
    mut server: TcpStream,
    host: &str,
    cred: &DbCred,
) -> Result<Relayed, String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    use rand::Rng;
    // MySQL's server speaks first, so the peeked `first` is empty; treat the
    // guest as a plaintext stream to start.
    let mut guest: Box<dyn services::Io> = Box::new(Prefixed::new(first, guest));

    // Guest leg: present as a server offering mysql_native_password and TLS.
    let mut scramble = [0u8; 20];
    rand::thread_rng().fill(&mut scramble);
    guest
        .write_all(&mysql::packet(
            0,
            &mysql::initial_handshake(&scramble, "mysql_native_password"),
        ))
        .await
        .map_err(|e| format!("sending the guest handshake: {e}"))?;
    let mut gbuf = Vec::new();
    let (gseq, gpkt) = mysql::read_packet(&mut guest, &mut gbuf).await?;
    // A TLS client first sends a 32-byte SSLRequest packet; terminate TLS with
    // the session CA, then read its real handshake response over TLS.
    let (mut guest, guest_tls, resp_seq) = if mysql::is_ssl_request(&gpkt) {
        let config = s.ca.server_config(host).map_err(|e| format!("{e:#}"))?;
        let tls = TlsAcceptor::from(config)
            .accept(Prefixed::new(std::mem::take(&mut gbuf), guest))
            .await
            .map_err(|e| format!("TLS from guest: {e}"))?;
        let mut tls: Box<dyn services::Io> = Box::new(tls);
        let (seq, _real) = mysql::read_packet(&mut tls, &mut Vec::new()).await?;
        (tls, true, seq)
    } else {
        (guest, false, gseq)
    };

    // Server leg: read the server's Initial Handshake and authenticate,
    // mirroring the guest's transport.
    let mut sbuf = Vec::new();
    let (_s0, initial) = mysql::read_packet(&mut server, &mut sbuf).await?;
    let hs = mysql::parse_initial(&initial)?;
    let db = cred.database.as_deref();
    let auth = mysql::auth_response(&hs.plugin, &cred.password, &hs.scramble);
    let mut server: Box<dyn services::Io> = if guest_tls {
        if hs.capabilities & mysql::CLIENT_SSL == 0 {
            return Err("the client requested TLS but the server does not offer it".into());
        }
        // Mid-handshake upgrade: SSLRequest, TLS, then the response over TLS.
        server
            .write_all(&mysql::packet(1, &mysql::ssl_request(db.is_some())))
            .await
            .map_err(|e| format!("sending SSLRequest: {e}"))?;
        let mut b: Box<dyn services::Io> = Box::new(connect_server_tls(server, host).await?);
        b.write_all(&mysql::packet(
            2,
            &mysql::handshake_response(true, &cred.user, &auth, db, &hs.plugin),
        ))
        .await
        .map_err(|e| format!("sending the handshake response: {e}"))?;
        b
    } else {
        plaintext_leg_ok(cred.plaintext, host)?;
        server
            .write_all(&mysql::packet(
                1,
                &mysql::handshake_response(false, &cred.user, &auth, db, &hs.plugin),
            ))
            .await
            .map_err(|e| format!("sending the handshake response: {e}"))?;
        Box::new(server)
    };
    mysql::complete_auth(&mut server, &mut sbuf, &cred.password, guest_tls).await?;

    // Tell the guest it is logged in (OK follows its handshake response).
    guest
        .write_all(&mysql::packet(resp_seq + 1, &mysql::ok_packet()))
        .await
        .map_err(|e| format!("sending OK to the guest: {e}"))?;
    Ok(Relayed {
        guest,
        server,
        to_guest: vec![],
    })
}

/// MS SQL Server (TDS), SQL authentication only: answer the guest's PRELOGIN
/// with ENCRYPT_NOT_SUP (plaintext login leg), read and ignore its LOGIN7, log
/// in to the real server with the host-held credential — tunnelling TLS in
/// PRELOGIN packets when the server wants encryption — then forward the
/// server's login response to the guest and splice.
async fn tds_exchange<S>(
    s: &Arc<Session>,
    guest: S,
    first: Vec<u8>,
    mut server: TcpStream,
    host: &str,
    cred: &DbCred,
) -> Result<Relayed, String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // TDS always opens with a plaintext PRELOGIN (never a bare ClientHello),
    // so guest_leg stays plaintext; TLS is negotiated in PRELOGIN below.
    let (mut guest, _) = guest_leg(s, guest, first, host).await?;

    // Guest leg: read its PRELOGIN. If it wants encryption, terminate the
    // TLS-in-PRELOGIN handshake with the session CA (the guest trusts our
    // root); otherwise take a plaintext login leg. Then read (ignore) LOGIN7.
    let mut gbuf = Vec::new();
    let (gtyp, _gs, pre) = tds::read_packet(&mut guest, &mut gbuf).await?;
    if gtyp != tds::PKT_PRELOGIN {
        return Err("the guest did not open with a TDS PRELOGIN".into());
    }
    let guest_wants_tls = matches!(
        tds::prelogin_encryption(&pre).unwrap_or(tds::ENCRYPT_NOT_SUP),
        tds::ENCRYPT_ON | tds::ENCRYPT_REQ
    );
    let reply_enc = if guest_wants_tls {
        tds::ENCRYPT_ON
    } else {
        tds::ENCRYPT_NOT_SUP
    };
    guest
        .write_all(&tds::frame(tds::PKT_RESPONSE, &tds::prelogin(reply_enc)))
        .await
        .map_err(|e| format!("sending PRELOGIN to the guest: {e}"))?;
    let mut guest: Box<dyn services::Io> = if guest_wants_tls {
        let handshaking = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let wrapper = tds::TdsTls::new(guest, handshaking.clone());
        let config = s.ca.server_config(host).map_err(|e| format!("{e:#}"))?;
        let tls = TlsAcceptor::from(config)
            .accept(wrapper)
            .await
            .map_err(|e| format!("TLS from guest: {e}"))?;
        handshaking.store(false, Ordering::Relaxed);
        Box::new(tls)
    } else {
        guest
    };
    let _ = tds::read_message(&mut guest, &mut gbuf).await?; // LOGIN7, ignored

    // Server leg: mirror the client — request encryption only if the guest
    // used it. (TDS declares the client's intent in PRELOGIN before the server
    // replies, so this is the one knob.)
    let mut sbuf = Vec::new();
    let want = if guest_wants_tls {
        tds::ENCRYPT_ON
    } else {
        tds::ENCRYPT_NOT_SUP
    };
    server
        .write_all(&tds::frame(tds::PKT_PRELOGIN, &tds::prelogin(want)))
        .await
        .map_err(|e| format!("sending PRELOGIN: {e}"))?;
    let (_t, _s, preresp) = tds::read_packet(&mut server, &mut sbuf).await?;
    let server_enc =
        tds::prelogin_encryption(&preresp).ok_or("server PRELOGIN had no ENCRYPTION option")?;

    let mut server: Box<dyn services::Io> = if guest_wants_tls {
        if server_enc == tds::ENCRYPT_NOT_SUP {
            return Err("the client requested TLS but the server does not support it".into());
        }
        // Tunnel the TLS handshake in PRELOGIN packets, then pass through.
        let handshaking = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let wrapper = tds::TdsTls::new(server, handshaking.clone());
        let tls = connect_server_tls(wrapper, host).await?;
        handshaking.store(false, Ordering::Relaxed);
        Box::new(tls)
    } else {
        // The client connected plaintext; a server that requires encryption
        // (anything but OFF/NOT_SUP) can't be served without it.
        if !matches!(server_enc, tds::ENCRYPT_OFF | tds::ENCRYPT_NOT_SUP) {
            return Err("server requires TLS but the client connected without it".into());
        }
        plaintext_leg_ok(cred.plaintext, host)?;
        Box::new(server)
    };

    // LOGIN7 with the host-held credential; a server rejection fails closed.
    server
        .write_all(&tds::frame(
            tds::PKT_LOGIN7,
            &tds::login7(&cred.user, &cred.password, cred.database.as_deref(), host),
        ))
        .await
        .map_err(|e| format!("sending LOGIN7: {e}"))?;
    let (_rt, resp) = tds::read_message(&mut server, &mut sbuf).await?;
    tds::login_result(&resp)?;

    Ok(Relayed {
        guest,
        server,
        to_guest: tds::frame(tds::PKT_RESPONSE, &resp),
    })
}

/// Kafka: the guest holds no credential and connects as a plaintext Kafka
/// client; the proxy authenticates the server leg with SASL (the secret's
/// mechanism), then splices. The guest's requests run on the authenticated
/// connection.
async fn kafka_exchange<S>(
    s: &Arc<Session>,
    guest: S,
    first: Vec<u8>,
    server: TcpStream,
    host: &str,
    cred: &DbCred,
) -> Result<Relayed, String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (guest, guest_tls) = guest_leg(s, guest, first, host).await?;
    let mut server = server_leg(server, host, guest_tls, cred.plaintext).await?;
    kafka::authenticate(
        &mut server,
        &cred.mechanism,
        &cred.user,
        &cred.password,
        &scram_nonce(),
    )
    .await?;
    Ok(Relayed {
        guest,
        server,
        to_guest: vec![],
    })
}

/// Guards a non-TLS server leg: the credential may be sent in the clear only if
/// the secret is marked `plaintext`, otherwise fail closed. Called once a leg is
/// known to be plaintext — which, since the proxy mirrors the client rather than
/// forcing TLS, happens when the guest itself connected without TLS.
fn plaintext_leg_ok(plaintext: bool, host: &str) -> Result<(), String> {
    if plaintext {
        Ok(())
    } else {
        Err(format!(
            "refusing to send the credential to {host} over a non-TLS connection: the client did not use TLS and the secret is not marked plaintext"
        ))
    }
}

/// Connects the server leg over TLS with real certificate validation.
async fn connect_server_tls<S>(
    stream: S,
    host: &str,
) -> Result<tokio_rustls::client::TlsStream<S>, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("{host}: {e}"))?;
    TlsConnector::from(tls::client_config())
        .connect(name, stream)
        .await
        .map_err(|e| format!("TLS to {host}: {e}"))
}

/// The guest leg for a database whose TLS (if any) is a normal ClientHello:
/// terminate it with the per-session CA when the guest uses TLS, otherwise take
/// the plaintext bytes as they are. Either carries no secret. Returns the
/// stream and whether the guest used TLS (which the server leg mirrors).
async fn guest_leg<S>(
    s: &Arc<Session>,
    io: S,
    first: Vec<u8>,
    host: &str,
) -> Result<(Box<dyn services::Io>, bool), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    if first.first() == Some(&0x16) {
        let config = s.ca.server_config(host).map_err(|e| format!("{e:#}"))?;
        let tls = TlsAcceptor::from(config)
            .accept(Prefixed::new(first, io))
            .await
            .map_err(|e| format!("TLS from guest: {e}"))?;
        Ok((Box::new(tls), true))
    } else {
        Ok((Box::new(Prefixed::new(first, io)), false))
    }
}

/// The server leg for a database whose TLS is a normal ClientHello: mirror the
/// client — real TLS (with certificate validation) when the guest used TLS,
/// otherwise the connection in the clear, which the credential guard allows
/// only for a `plaintext` secret.
async fn server_leg(
    tcp: TcpStream,
    host: &str,
    use_tls: bool,
    plaintext: bool,
) -> Result<Box<dyn services::Io>, String> {
    if !use_tls {
        plaintext_leg_ok(plaintext, host)?;
        return Ok(Box::new(tcp));
    }
    Ok(Box::new(connect_server_tls(tcp, host).await?))
}

/// Negotiates the proxy→server leg: sends Postgres' `SSLRequest` and, on the
/// server's one-byte reply, either runs TLS (`S`) or — only if the secret is
/// `plaintext` — continues in the clear (`N`). The proxy mirrors the client:
/// it offers the server TLS only when the guest used TLS; when the guest was
/// plaintext it connects in the clear (no SSLRequest), which the credential
/// guard allows only for a `plaintext` secret.
async fn pg_server_leg(
    mut tcp: TcpStream,
    host: &str,
    use_tls: bool,
    plaintext: bool,
) -> Result<Box<dyn services::Io>, String> {
    if !use_tls {
        plaintext_leg_ok(plaintext, host)?;
        return Ok(Box::new(tcp));
    }
    tcp.write_all(&pgwire::SSL_REQUEST)
        .await
        .map_err(|e| format!("requesting server TLS: {e}"))?;
    let mut reply = [0u8; 1];
    tcp.read_exact(&mut reply)
        .await
        .map_err(|e| format!("reading the server's TLS reply: {e}"))?;
    match reply[0] {
        b'S' => Ok(Box::new(connect_server_tls(tcp, host).await?)),
        // The guest used TLS but the server offers none: only a plaintext
        // secret permits sending the credential in the clear.
        b'N' => {
            plaintext_leg_ok(plaintext, host)?;
            Ok(Box::new(tcp))
        }
        other => Err(format!("{host} gave an unexpected TLS reply {other:#x}")),
    }
}

/// A fresh SCRAM client nonce: printable, comma-free ASCII (RFC 5802).
fn scram_nonce() -> String {
    use rand::Rng;
    rand::thread_rng()
        .sample_iter(rand::distributions::Alphanumeric)
        .take(24)
        .map(char::from)
        .collect()
}
