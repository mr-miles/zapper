//! Secrets minted by a host command (`command:` in a secret).
//!
//! The daemon runs the command, which prints the credential, usually a
//! short-lived one: `{"value": "...", "expires_at": "<RFC 3339>"}`, or a bare
//! value cached for `ttl_secs`. The result is kept per command line and
//! refreshed ahead of its expiry, so a request almost never waits for it:
//!
//! - fresh: served from the cache;
//! - inside the refresh margin (a sixth of its life, 30 s to 10 min): served,
//!   and re-minted in the background;
//! - expired or absent: the request waits for one mint (callers of the same
//!   command share it), and fails if the mint does. A failed mint is never
//!   cached, and a token is never served past its expiry.
//!
//! The command runs as the daemon's user with a cleared environment, no
//! stdin, and a timeout. Its output is the secret: it is parsed here and never
//! put in an error or a log.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};

/// A mint timeout, unless the secret sets one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a bare value (one with no `expires_at`) is kept, unless the
/// secret sets `ttl_secs`.
pub const DEFAULT_TTL: Duration = Duration::from_secs(300);
/// After a failed background refresh, the next attempt waits this long.
const RETRY_AFTER: u64 = 30;
/// No more than this is read from a command's stdout.
const MAX_OUTPUT: u64 = 64 * 1024;

/// What to run and how long to wait for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub argv: Vec<String>,
    pub timeout: Duration,
    /// The life of a bare value.
    pub ttl: Duration,
}

impl Spec {
    pub fn new(argv: &[String], timeout_secs: Option<u64>, ttl_secs: Option<u64>) -> Spec {
        Spec {
            argv: argv.to_vec(),
            timeout: timeout_secs.map_or(DEFAULT_TIMEOUT, Duration::from_secs),
            ttl: ttl_secs.map_or(DEFAULT_TTL, Duration::from_secs),
        }
    }

    /// What identifies a secret in the cache: its command line.
    pub fn key(&self) -> String {
        self.argv.join("\0")
    }
}

/// A freshly minted value and when it stops working (unix seconds).
#[derive(Clone)]
pub struct Minted {
    pub value: String,
    pub expires: u64,
}

impl std::fmt::Debug for Minted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Minted(expires {})", self.expires)
    }
}

type Runner = dyn Fn(&Spec) -> Result<Minted> + Send + Sync;
type Clock = dyn Fn() -> u64 + Send + Sync;

struct Entry {
    value: String,
    minted: u64,
    expires: u64,
    /// A background refresh is under way.
    refreshing: bool,
    /// No background refresh before this (after a failure).
    retry_at: u64,
    /// Why the last refresh failed, for whoever looks.
    last_error: Option<String>,
}

impl Entry {
    /// How long before expiry the value is refreshed: a sixth of its life.
    fn margin(&self) -> u64 {
        ((self.expires.saturating_sub(self.minted)) / 6).clamp(30, 600)
    }

    fn fresh(&self, now: u64) -> bool {
        now + self.margin() < self.expires
    }

    fn valid(&self, now: u64) -> bool {
        now < self.expires
    }
}

pub struct MintCache {
    runner: Arc<Runner>,
    clock: Arc<Clock>,
    entries: Mutex<HashMap<String, Entry>>,
    /// One lock per command, so concurrent callers share a single mint.
    flights: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

static GLOBAL: LazyLock<Arc<MintCache>> =
    LazyLock::new(|| Arc::new(MintCache::new(Arc::new(run_command), Arc::new(unix_now))));

/// The value of a command secret, from the process-wide cache.
pub fn resolve(spec: &Spec) -> Result<String> {
    GLOBAL.get(spec)
}

/// The cached value if there is a usable one; never runs the command. For
/// whoever must revoke what was minted before it is forgotten.
pub fn peek(spec: &Spec) -> Option<String> {
    GLOBAL.peek(spec)
}

/// Drops a cached value, so the next use mints again (a grant changed).
pub fn forget(spec: &Spec) {
    GLOBAL.forget(spec)
}

/// Why the last background refresh of a command's value failed, if it did.
pub fn last_error(spec: &Spec) -> Option<String> {
    GLOBAL.last_error(spec)
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl MintCache {
    pub fn new(runner: Arc<Runner>, clock: Arc<Clock>) -> MintCache {
        MintCache {
            runner,
            clock,
            entries: Mutex::new(HashMap::new()),
            flights: Mutex::new(HashMap::new()),
        }
    }

    pub fn forget(&self, spec: &Spec) {
        self.entries.lock().unwrap().remove(&spec.key());
    }

    pub fn peek(&self, spec: &Spec) -> Option<String> {
        let now = (self.clock)();
        self.entries
            .lock()
            .unwrap()
            .get(&spec.key())
            .filter(|e| e.valid(now))
            .map(|e| e.value.clone())
    }

    pub fn last_error(&self, spec: &Spec) -> Option<String> {
        self.entries
            .lock()
            .unwrap()
            .get(&spec.key())
            .and_then(|e| e.last_error.clone())
    }

    pub fn get(self: &Arc<Self>, spec: &Spec) -> Result<String> {
        let key = spec.key();
        let now = (self.clock)();
        if let Some(v) = self.cached(&key, now, spec) {
            return Ok(v);
        }
        // Absent or expired: one caller mints, the rest wait for it.
        let flight = self
            .flights
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_default()
            .clone();
        let _one = flight.lock().unwrap();
        let now = (self.clock)();
        if let Some(v) = self.cached(&key, now, spec) {
            return Ok(v);
        }
        let minted = (self.runner)(spec)?;
        self.store(&key, minted, now)
    }

    /// The cached value if it can be used now. Inside the refresh margin it
    /// starts a background refresh.
    fn cached(self: &Arc<Self>, key: &str, now: u64, spec: &Spec) -> Option<String> {
        let mut entries = self.entries.lock().unwrap();
        let e = entries.get_mut(key)?;
        if e.fresh(now) {
            return Some(e.value.clone());
        }
        if !e.valid(now) {
            return None;
        }
        if !e.refreshing && now >= e.retry_at {
            e.refreshing = true;
            let (me, key, spec) = (self.clone(), key.to_string(), spec.clone());
            std::thread::spawn(move || me.refresh(&key, &spec));
        }
        Some(e.value.clone())
    }

    fn refresh(&self, key: &str, spec: &Spec) {
        let flight = self
            .flights
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_default()
            .clone();
        let _one = flight.lock().unwrap();
        let result = (self.runner)(spec);
        let now = (self.clock)();
        match result.and_then(|m| self.store(key, m, now)) {
            Ok(_) => {}
            Err(e) => {
                if let Some(entry) = self.entries.lock().unwrap().get_mut(key) {
                    entry.refreshing = false;
                    entry.retry_at = now + RETRY_AFTER;
                    entry.last_error = Some(format!("{e:#}"));
                }
            }
        }
    }

    fn store(&self, key: &str, m: Minted, now: u64) -> Result<String> {
        if m.expires <= now {
            bail!("the command printed a value that has already expired");
        }
        self.entries.lock().unwrap().insert(
            key.to_string(),
            Entry {
                value: m.value.clone(),
                minted: now,
                expires: m.expires,
                refreshing: false,
                retry_at: 0,
                last_error: None,
            },
        );
        Ok(m.value)
    }
}

/// Environment a minting command keeps: where to find programs and its own
/// configuration (`zapper mint` reads the App's credentials file under it).
fn kept_env(name: &str) -> bool {
    matches!(name, "PATH" | "HOME" | "XDG_CONFIG_HOME") || name.starts_with("ZAPPER_")
}

/// Runs the command and parses what it printed.
pub fn run_command(spec: &Spec) -> Result<Minted> {
    let mut argv = spec.argv.clone();
    if argv.first().is_none_or(|a| a.is_empty()) {
        bail!("the command is empty");
    }
    // `zapper` means this very binary, wherever it was installed.
    if argv[0] == "zapper" {
        if let Ok(exe) = std::env::current_exe() {
            argv[0] = exe.to_string_lossy().into_owned();
        }
    }
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| kept_env(k)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", argv[0]))?;
    let read = |mut r: Box<dyn Read + Send>, limit: u64| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut r).take(limit).read_to_end(&mut buf);
            // Keep draining, so the child never blocks on a full pipe.
            let _ = std::io::copy(&mut r, &mut std::io::sink());
            buf
        })
    };
    let out = read(Box::new(child.stdout.take().unwrap()), MAX_OUTPUT);
    let err = read(Box::new(child.stderr.take().unwrap()), 2048);
    let deadline = Instant::now() + spec.timeout;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the command timed out after {}s", spec.timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let (out, err) = (
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    );
    if !status.success() {
        // Stderr is the command's to keep free of the secret; stdout is not
        // shown at all.
        let line = String::from_utf8_lossy(&err);
        let line = line.lines().next().unwrap_or("").trim();
        bail!(
            "the command failed ({status}){}",
            if line.is_empty() {
                String::new()
            } else {
                format!(": {line}")
            }
        );
    }
    parse_output(&String::from_utf8_lossy(&out), spec.ttl, unix_now())
}

/// `{"value", "expires_at"}` or a bare value that lives for `ttl`.
pub fn parse_output(text: &str, ttl: Duration, now: u64) -> Result<Minted> {
    let text = text.trim();
    if text.is_empty() {
        bail!("the command printed nothing");
    }
    if text.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(text)
            .map_err(|_| anyhow!("the command's output is not valid JSON"))?;
        let value = v["value"].as_str().filter(|s| !s.is_empty());
        let value = value.ok_or_else(|| anyhow!("the command's JSON has no \"value\""))?;
        let expires = match v["expires_at"].as_str() {
            Some(t) => {
                time::OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339)
                    .map_err(|_| anyhow!("expires_at is not an RFC 3339 time"))?
                    .unix_timestamp()
                    .max(0) as u64
            }
            None => now + ttl.as_secs(),
        };
        return Ok(Minted {
            value: value.into(),
            expires,
        });
    }
    if text.contains('\n') {
        bail!("the command printed several lines; a bare value is one line");
    }
    Ok(Minted {
        value: text.into(),
        expires: now + ttl.as_secs(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    /// A cache on a clock the test sets, whose mints are numbered tokens that
    /// live `life` seconds from the clock.
    struct Rig {
        cache: Arc<MintCache>,
        now: Arc<AtomicU64>,
        mints: Arc<AtomicUsize>,
        fail: Arc<std::sync::atomic::AtomicBool>,
        spec: Spec,
    }

    fn rig(life: u64) -> Rig {
        let now = Arc::new(AtomicU64::new(1_000_000));
        let mints = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (n, m, f) = (now.clone(), mints.clone(), fail.clone());
        let runner: Arc<Runner> = Arc::new(move |_| {
            std::thread::sleep(Duration::from_millis(20));
            if f.load(Ordering::SeqCst) {
                bail!("github is down");
            }
            let i = m.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Minted {
                value: format!("tok{i}"),
                expires: n.load(Ordering::SeqCst) + life,
            })
        });
        let n = now.clone();
        let cache = Arc::new(MintCache::new(
            runner,
            Arc::new(move || n.load(Ordering::SeqCst)),
        ));
        Rig {
            cache,
            now,
            mints,
            fail,
            spec: Spec::new(&["mint".into(), "x".into()], None, None),
        }
    }

    fn settle(r: &Rig, want_mints: usize) {
        for _ in 0..200 {
            if r.mints.load(Ordering::SeqCst) >= want_mints {
                std::thread::sleep(Duration::from_millis(30));
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("never reached {want_mints} mints");
    }

    #[test]
    fn a_fresh_value_is_served_without_running_the_command_again() {
        let r = rig(3600);
        for _ in 0..5 {
            assert_eq!(r.cache.get(&r.spec).unwrap(), "tok1");
        }
        r.now.fetch_add(3000, Ordering::SeqCst); // 600 s left: at the margin, not inside it
        assert_eq!(r.cache.get(&r.spec).unwrap(), "tok1");
        assert_eq!(r.mints.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn inside_the_margin_the_old_value_is_served_while_a_new_one_is_minted() {
        let r = rig(3600);
        assert_eq!(r.cache.get(&r.spec).unwrap(), "tok1");
        r.now.fetch_add(3100, Ordering::SeqCst); // 500 s left
        let t = Instant::now();
        assert_eq!(
            r.cache.get(&r.spec).unwrap(),
            "tok1",
            "no waiting for the mint"
        );
        assert!(t.elapsed() < Duration::from_millis(15));
        settle(&r, 2);
        assert_eq!(r.cache.get(&r.spec).unwrap(), "tok2");
        assert_eq!(
            r.mints.load(Ordering::SeqCst),
            2,
            "one refresh, however many requests"
        );
    }

    #[test]
    fn many_requests_inside_the_margin_start_one_refresh() {
        let r = rig(3600);
        r.cache.get(&r.spec).unwrap();
        r.now.fetch_add(3100, Ordering::SeqCst);
        let hs: Vec<_> = (0..16)
            .map(|_| {
                let (c, s) = (r.cache.clone(), r.spec.clone());
                std::thread::spawn(move || c.get(&s).unwrap())
            })
            .collect();
        for h in hs {
            let v = h.join().unwrap();
            assert!(v == "tok1" || v == "tok2");
        }
        settle(&r, 2);
        assert_eq!(r.mints.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn an_expired_value_is_never_served_and_waits_for_a_new_one() {
        let r = rig(3600);
        r.cache.get(&r.spec).unwrap();
        r.now.fetch_add(3600, Ordering::SeqCst);
        assert_eq!(r.cache.get(&r.spec).unwrap(), "tok2");
    }

    #[test]
    fn concurrent_first_uses_share_one_mint() {
        let r = rig(3600);
        let hs: Vec<_> = (0..16)
            .map(|_| {
                let (c, s) = (r.cache.clone(), r.spec.clone());
                std::thread::spawn(move || c.get(&s).unwrap())
            })
            .collect();
        for h in hs {
            assert_eq!(h.join().unwrap(), "tok1");
        }
        assert_eq!(r.mints.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failed_first_mint_fails_the_request_and_is_not_cached() {
        let r = rig(3600);
        r.fail.store(true, Ordering::SeqCst);
        assert!(format!("{:#}", r.cache.get(&r.spec).unwrap_err()).contains("github is down"));
        r.fail.store(false, Ordering::SeqCst);
        assert_eq!(
            r.cache.get(&r.spec).unwrap(),
            "tok1",
            "the next request tries again"
        );
    }

    #[test]
    fn a_failed_refresh_keeps_serving_until_expiry_then_fails_closed() {
        let r = rig(3600);
        r.cache.get(&r.spec).unwrap();
        r.fail.store(true, Ordering::SeqCst);
        r.now.fetch_add(3100, Ordering::SeqCst);
        assert_eq!(r.cache.get(&r.spec).unwrap(), "tok1");
        std::thread::sleep(Duration::from_millis(80));
        assert!(r
            .cache
            .last_error(&r.spec)
            .unwrap()
            .contains("github is down"));
        // Retries are spaced out, not made on every request.
        r.now.fetch_add(10, Ordering::SeqCst);
        for _ in 0..5 {
            assert_eq!(r.cache.get(&r.spec).unwrap(), "tok1");
        }
        // Once it has expired, there is nothing to serve.
        r.now.fetch_add(500, Ordering::SeqCst);
        assert!(r.cache.get(&r.spec).is_err());
        // And a recovered GitHub recovers the secret.
        r.fail.store(false, Ordering::SeqCst);
        assert_eq!(r.cache.get(&r.spec).unwrap(), "tok2");
    }

    #[test]
    fn peeking_never_runs_the_command() {
        let r = rig(3600);
        assert_eq!(r.cache.peek(&r.spec), None);
        assert_eq!(r.mints.load(Ordering::SeqCst), 0);
        r.cache.get(&r.spec).unwrap();
        assert_eq!(r.cache.peek(&r.spec).as_deref(), Some("tok1"));
        r.now.fetch_add(3600, Ordering::SeqCst);
        assert_eq!(r.cache.peek(&r.spec), None, "not past its expiry");
        assert_eq!(r.mints.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn forgetting_makes_the_next_use_mint_again() {
        let r = rig(3600);
        r.cache.get(&r.spec).unwrap();
        r.cache.forget(&r.spec);
        assert_eq!(r.cache.get(&r.spec).unwrap(), "tok2");
    }

    #[test]
    fn different_command_lines_are_different_secrets() {
        let r = rig(3600);
        let other = Spec::new(&["mint".into(), "y".into()], None, None);
        assert_eq!(r.cache.get(&r.spec).unwrap(), "tok1");
        assert_eq!(r.cache.get(&other).unwrap(), "tok2");
    }

    #[test]
    fn a_value_that_is_already_expired_is_refused() {
        let cache = Arc::new(MintCache::new(
            Arc::new(|_| {
                Ok(Minted {
                    value: "old".into(),
                    expires: 5,
                })
            }),
            Arc::new(|| 1000),
        ));
        let e = cache
            .get(&Spec::new(&["x".into()], None, None))
            .unwrap_err();
        assert!(format!("{e:#}").contains("already expired"));
    }

    #[test]
    fn output_is_json_or_a_bare_value() {
        let m = parse_output(
            r#"{"value":"ghs_x","expires_at":"2030-01-01T00:00:00Z"}"#,
            DEFAULT_TTL,
            5,
        )
        .unwrap();
        assert_eq!((m.value.as_str(), m.expires), ("ghs_x", 1_893_456_000));
        let m = parse_output(" plain \n", Duration::from_secs(60), 100).unwrap();
        assert_eq!((m.value.as_str(), m.expires), ("plain", 160));
        let m = parse_output(r#"{"value":"v"}"#, Duration::from_secs(60), 100).unwrap();
        assert_eq!(m.expires, 160);
        for bad in [
            "",
            "  ",
            "a\nb",
            "{",
            r#"{"value":""}"#,
            r#"{"expires_at":"2030-01-01T00:00:00Z"}"#,
        ] {
            assert!(parse_output(bad, DEFAULT_TTL, 0).is_err(), "{bad:?}");
        }
        let e = parse_output(
            r#"{"value":"topsecret","expires_at":"soon"}"#,
            DEFAULT_TTL,
            0,
        )
        .unwrap_err();
        assert!(!format!("{e:#}").contains("topsecret"));
        let e = parse_output("topsecret\nsecond", DEFAULT_TTL, 0).unwrap_err();
        assert!(!format!("{e:#}").contains("topsecret"));
    }

    fn sh(script: &str, timeout: u64) -> Result<Minted> {
        run_command(&Spec::new(
            &["sh".into(), "-c".into(), script.into()],
            Some(timeout),
            Some(60),
        ))
    }

    #[test]
    fn a_real_command_is_run_and_its_output_parsed() {
        let m = sh(
            r#"printf '{"value":"v1","expires_at":"2030-01-01T00:00:00Z"}'"#,
            5,
        )
        .unwrap();
        assert_eq!(m.value, "v1");
        assert_eq!(sh("echo bare", 5).unwrap().value, "bare");
    }

    #[test]
    fn a_failing_command_reports_stderr_and_never_stdout() {
        let e = format!(
            "{:#}",
            sh(
                "echo SECRETVALUE; echo 'HTTP 404: Not Found' >&2; exit 3",
                5
            )
            .unwrap_err()
        );
        assert!(
            e.contains("HTTP 404: Not Found") && e.contains("exit status: 3"),
            "{e}"
        );
        assert!(!e.contains("SECRETVALUE"), "{e}");
        let e = format!(
            "{:#}",
            sh("echo SECRETVALUE extra words\necho more", 5).unwrap_err()
        );
        assert!(!e.contains("SECRETVALUE"), "{e}");
    }

    #[test]
    fn a_command_that_hangs_is_killed() {
        let t = Instant::now();
        let e = format!("{:#}", sh("sleep 30", 1).unwrap_err());
        assert!(e.contains("timed out"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn the_command_gets_a_clean_environment() {
        std::env::set_var("ZAPPER_MINT_TEST_KEPT", "kept");
        std::env::set_var("MINT_TEST_DROPPED", "dropped");
        let m = sh(
            r#"echo "${ZAPPER_MINT_TEST_KEPT:-none}-${MINT_TEST_DROPPED:-none}""#,
            5,
        )
        .unwrap();
        assert_eq!(m.value, "kept-none");
    }

    #[test]
    fn a_missing_program_is_an_error_not_a_panic() {
        let e =
            run_command(&Spec::new(&["/nonexistent/zapper-mint".into()], None, None)).unwrap_err();
        assert!(format!("{e:#}").contains("starting"));
        assert!(run_command(&Spec::new(&[], None, None)).is_err());
    }
}
