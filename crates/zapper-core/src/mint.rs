//! Secrets minted by a host command (`command:` in a secret).
//!
//! The daemon runs the command, which prints the credential, usually a
//! short-lived one: `{"value": "...", "expires_at": "<RFC 3339>"}`, or a bare
//! value cached for `ttl_secs` (the format is `zapper_policy::mint`'s). The
//! result is kept per command line and refreshed ahead of its expiry, so a
//! request almost never waits for it:
//!
//! - fresh: served from the cache;
//! - inside the refresh margin (a sixth of its life, 30 s to 10 min): served,
//!   and re-minted in the background;
//! - expired or absent: the request waits for one mint (callers of the same
//!   command share it, the background refresh among them), and fails if the
//!   mint does. A failed mint is never cached as a value, and a token is
//!   never served past its expiry.
//!
//! A failed mint's error goes to everyone waiting for it, and for a few
//! seconds to whoever asks next, so a down service is asked once, not once
//! per request. Everything here is async: waiting for a mint holds no thread.
//!
//! The command runs as the daemon's user with a cleared environment, no
//! stdin, and a timeout. Its output is the secret: it is parsed and never put
//! in an error or a log.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::AsyncReadExt;
use tokio::sync::OnceCell;
use zapper_policy::mint::{parse_output, unix_now, Minted, Spec};
use zapper_policy::SecretSource;

/// After a failed background refresh, the next attempt waits this long.
const RETRY_AFTER: u64 = 30;
/// After a mint fails with no value to serve, requests get its error for
/// this long instead of each running the command again.
const FAILED_FOR: u64 = 5;
/// No more than this is read from a command's stdout.
const MAX_OUTPUT: u64 = 64 * 1024;

type MintFuture = Pin<Box<dyn Future<Output = Result<Minted>> + Send>>;
type Runner = dyn Fn(Spec) -> MintFuture + Send + Sync;
type Clock = dyn Fn() -> u64 + Send + Sync;
/// A mint under way: everyone who needs the command's value waits for its
/// one outcome.
type Flight = Arc<OnceCell<Result<String, String>>>;

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
    flights: Mutex<HashMap<String, Flight>>,
    /// A command's last failed mint, when there was nothing to serve: its
    /// error, and until when it is the answer.
    failed: Mutex<HashMap<String, (String, u64)>>,
}

static GLOBAL: LazyLock<Arc<MintCache>> = LazyLock::new(|| {
    Arc::new(MintCache::new(
        Arc::new(|spec| Box::pin(run_command(spec))),
        Arc::new(unix_now),
    ))
});

/// The value of a command secret, from the process-wide cache.
pub async fn resolve(spec: &Spec) -> Result<String> {
    GLOBAL.get(spec).await
}

/// A secret's value: minted for a `command` secret, read for the others.
pub async fn secret_value(source: &SecretSource) -> Result<String> {
    match source.mint_spec() {
        Some(spec) => resolve(&spec).await.context("minting the secret"),
        None => source.resolve(),
    }
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

impl MintCache {
    pub fn new(runner: Arc<Runner>, clock: Arc<Clock>) -> MintCache {
        MintCache {
            runner,
            clock,
            entries: Mutex::new(HashMap::new()),
            flights: Mutex::new(HashMap::new()),
            failed: Mutex::new(HashMap::new()),
        }
    }

    pub fn forget(&self, spec: &Spec) {
        let key = spec.key();
        self.entries.lock().unwrap().remove(&key);
        self.failed.lock().unwrap().remove(&key);
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

    /// Why the last mint of `spec` failed, if it did: a background
    /// refresh's failure is seen nowhere else (its value is still served).
    pub fn last_error(&self, spec: &Spec) -> Option<String> {
        let key = spec.key();
        let entry = self
            .entries
            .lock()
            .unwrap()
            .get(&key)
            .and_then(|e| e.last_error.clone());
        entry.or_else(|| self.failed.lock().unwrap().get(&key).map(|f| f.0.clone()))
    }

    pub async fn get(self: &Arc<Self>, spec: &Spec) -> Result<String> {
        let key = spec.key();
        if let Some(v) = self.cached(&key, spec) {
            return Ok(v);
        }
        if let Some(e) = self.recent_failure(&key) {
            return Err(anyhow!(e));
        }
        self.mint(&key, spec).await.map_err(|e| anyhow!(e))
    }

    /// The cached value if it can be used now. Inside the refresh margin it
    /// starts a background refresh.
    fn cached(self: &Arc<Self>, key: &str, spec: &Spec) -> Option<String> {
        let now = (self.clock)();
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
            tokio::spawn(async move {
                let _ = me.mint(&key, &spec).await;
            });
        }
        Some(e.value.clone())
    }

    fn recent_failure(&self, key: &str) -> Option<String> {
        let now = (self.clock)();
        let mut failed = self.failed.lock().unwrap();
        match failed.get(key) {
            Some((e, until)) if now < *until => Some(e.clone()),
            Some(_) => {
                failed.remove(key);
                None
            }
            None => None,
        }
    }

    /// Joins the command's mint under way, or starts one; its outcome is
    /// everyone's who joined.
    async fn mint(&self, key: &str, spec: &Spec) -> Result<String, String> {
        let flight = self
            .flights
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_default()
            .clone();
        let out = flight
            .get_or_init(|| async {
                let now = (self.clock)();
                // A mint that ended just before this one began may have left
                // a fresh value.
                if let Some(e) = self.entries.lock().unwrap().get(key) {
                    if e.fresh(now) {
                        return Ok(e.value.clone());
                    }
                }
                match (self.runner)(spec.clone())
                    .await
                    .and_then(|m| self.store(key, m, now))
                {
                    Ok(v) => {
                        self.failed.lock().unwrap().remove(key);
                        Ok(v)
                    }
                    Err(e) => {
                        let e = format!("{e:#}");
                        self.note_failure(key, &e);
                        Err(e)
                    }
                }
            })
            .await
            .clone();
        // This mint is over: the next is a new one.
        let mut flights = self.flights.lock().unwrap();
        if flights.get(key).is_some_and(|f| Arc::ptr_eq(f, &flight)) {
            flights.remove(key);
        }
        out
    }

    /// A value still valid keeps being served, and its refresh is tried again
    /// later; with none, the error is the answer for a few seconds.
    fn note_failure(&self, key: &str, e: &str) {
        let now = (self.clock)();
        let mut entries = self.entries.lock().unwrap();
        match entries.get_mut(key) {
            Some(entry) if entry.valid(now) => {
                entry.refreshing = false;
                entry.retry_at = now + RETRY_AFTER;
                entry.last_error = Some(e.to_string());
            }
            _ => {
                self.failed
                    .lock()
                    .unwrap()
                    .insert(key.to_string(), (e.to_string(), now + FAILED_FOR));
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

/// Up to `limit` bytes of a pipe, draining the rest so the command never
/// blocks on a full one.
async fn read_capped(mut r: impl tokio::io::AsyncRead + Unpin, limit: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = (&mut r).take(limit).read_to_end(&mut buf).await;
    let _ = tokio::io::copy(&mut r, &mut tokio::io::sink()).await;
    buf
}

/// Runs the command and parses what it printed.
pub async fn run_command(spec: Spec) -> Result<Minted> {
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
    let mut child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| kept_env(k)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {}", argv[0]))?;
    let (stdout, stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let run = async {
        tokio::join!(
            read_capped(stdout, MAX_OUTPUT),
            read_capped(stderr, 2048),
            child.wait()
        )
    };
    let finished = tokio::time::timeout(spec.timeout, run).await;
    let Ok((out, err, status)) = finished else {
        let _ = child.kill().await;
        bail!("the command timed out after {}s", spec.timeout.as_secs());
    };
    let status = status?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// A cache on a clock the test sets, whose mints are numbered tokens that
    /// live `life` seconds from the clock, after `delay`.
    struct Rig {
        cache: Arc<MintCache>,
        now: Arc<AtomicU64>,
        /// Mints that succeeded.
        mints: Arc<AtomicUsize>,
        /// Times the command ran, failed or not.
        runs: Arc<AtomicUsize>,
        fail: Arc<AtomicBool>,
        spec: Spec,
    }

    fn rig_with(life: u64, delay: Duration) -> Rig {
        let now = Arc::new(AtomicU64::new(1_000_000));
        let mints = Arc::new(AtomicUsize::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let (n, m, r, f) = (now.clone(), mints.clone(), runs.clone(), fail.clone());
        let runner: Arc<Runner> = Arc::new(move |_| {
            let (n, m, r, f) = (n.clone(), m.clone(), r.clone(), f.clone());
            Box::pin(async move {
                r.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(delay).await;
                if f.load(Ordering::SeqCst) {
                    bail!("github is down");
                }
                let i = m.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(Minted {
                    value: format!("tok{i}"),
                    expires: n.load(Ordering::SeqCst) + life,
                })
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
            runs,
            fail,
            spec: Spec::new(&["mint".into(), "x".into()], None, None),
        }
    }

    fn rig(life: u64) -> Rig {
        rig_with(life, Duration::from_millis(20))
    }

    async fn settle(r: &Rig, want_mints: usize) {
        for _ in 0..200 {
            if r.mints.load(Ordering::SeqCst) >= want_mints {
                tokio::time::sleep(Duration::from_millis(30)).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never reached {want_mints} mints");
    }

    /// `n` requests at once, each in its own task.
    async fn together(r: &Rig, n: usize) -> Vec<Result<String>> {
        let hs: Vec<_> = (0..n)
            .map(|_| {
                let (c, s) = (r.cache.clone(), r.spec.clone());
                tokio::spawn(async move { c.get(&s).await })
            })
            .collect();
        let mut out = Vec::new();
        for h in hs {
            out.push(h.await.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn a_fresh_value_is_served_without_running_the_command_again() {
        let r = rig(3600);
        for _ in 0..5 {
            assert_eq!(r.cache.get(&r.spec).await.unwrap(), "tok1");
        }
        r.now.fetch_add(3000, Ordering::SeqCst); // 600 s left: at the margin, not inside it
        assert_eq!(r.cache.get(&r.spec).await.unwrap(), "tok1");
        assert_eq!(r.mints.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn inside_the_margin_the_old_value_is_served_while_a_new_one_is_minted() {
        let r = rig(3600);
        assert_eq!(r.cache.get(&r.spec).await.unwrap(), "tok1");
        r.now.fetch_add(3100, Ordering::SeqCst); // 500 s left
        let t = Instant::now();
        assert_eq!(
            r.cache.get(&r.spec).await.unwrap(),
            "tok1",
            "no waiting for the mint"
        );
        assert!(t.elapsed() < Duration::from_millis(15));
        settle(&r, 2).await;
        assert_eq!(r.cache.get(&r.spec).await.unwrap(), "tok2");
        assert_eq!(
            r.mints.load(Ordering::SeqCst),
            2,
            "one refresh, however many requests"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_requests_inside_the_margin_start_one_refresh() {
        let r = rig(3600);
        r.cache.get(&r.spec).await.unwrap();
        r.now.fetch_add(3100, Ordering::SeqCst);
        for v in together(&r, 16).await {
            let v = v.unwrap();
            assert!(v == "tok1" || v == "tok2");
        }
        settle(&r, 2).await;
        assert_eq!(r.mints.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_expired_value_is_never_served_and_waits_for_a_new_one() {
        let r = rig(3600);
        r.cache.get(&r.spec).await.unwrap();
        r.now.fetch_add(3600, Ordering::SeqCst);
        assert_eq!(r.cache.get(&r.spec).await.unwrap(), "tok2");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_first_uses_share_one_mint() {
        let r = rig(3600);
        for v in together(&r, 16).await {
            assert_eq!(v.unwrap(), "tok1");
        }
        assert_eq!(r.runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failed_mint_is_everyones_who_waited_and_the_answer_for_a_few_seconds() {
        let r = rig_with(3600, Duration::from_millis(100));
        r.fail.store(true, Ordering::SeqCst);
        for v in together(&r, 8).await {
            assert!(format!("{:#}", v.unwrap_err()).contains("github is down"));
        }
        assert_eq!(
            r.runs.load(Ordering::SeqCst),
            1,
            "one attempt for all eight"
        );
        // Just after: the same error, without asking again.
        r.fail.store(false, Ordering::SeqCst);
        assert!(r.cache.get(&r.spec).await.is_err());
        assert_eq!(r.runs.load(Ordering::SeqCst), 1);
        assert!(r
            .cache
            .last_error(&r.spec)
            .unwrap()
            .contains("github is down"));
        // Then the next request tries again.
        r.now.fetch_add(FAILED_FOR, Ordering::SeqCst);
        assert_eq!(r.cache.get(&r.spec).await.unwrap(), "tok1");
        assert_eq!(r.runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_serving_until_expiry_then_fails_closed() {
        let r = rig(3600);
        r.cache.get(&r.spec).await.unwrap();
        r.fail.store(true, Ordering::SeqCst);
        r.now.fetch_add(3100, Ordering::SeqCst);
        assert_eq!(r.cache.get(&r.spec).await.unwrap(), "tok1");
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(r
            .cache
            .last_error(&r.spec)
            .unwrap()
            .contains("github is down"));
        // Retries are spaced out, not made on every request.
        r.now.fetch_add(10, Ordering::SeqCst);
        for _ in 0..5 {
            assert_eq!(r.cache.get(&r.spec).await.unwrap(), "tok1");
        }
        assert_eq!(r.runs.load(Ordering::SeqCst), 2);
        // Once it has expired, there is nothing to serve.
        r.now.fetch_add(500, Ordering::SeqCst);
        assert!(r.cache.get(&r.spec).await.is_err());
        // And a recovered GitHub recovers the secret.
        r.fail.store(false, Ordering::SeqCst);
        r.now.fetch_add(FAILED_FOR, Ordering::SeqCst);
        assert_eq!(r.cache.get(&r.spec).await.unwrap(), "tok2");
    }

    #[tokio::test]
    async fn waiting_for_a_mint_holds_no_thread() {
        // A single-threaded runtime: a blocking wait would stop the ticker.
        let r = rig_with(3600, Duration::from_millis(200));
        let ticks = Arc::new(AtomicUsize::new(0));
        let t = ticks.clone();
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                t.fetch_add(1, Ordering::SeqCst);
            }
        });
        assert_eq!(r.cache.get(&r.spec).await.unwrap(), "tok1");
        ticker.abort();
        assert!(
            ticks.load(Ordering::SeqCst) >= 10,
            "the runtime kept working"
        );
    }

    #[tokio::test]
    async fn peeking_never_runs_the_command() {
        let r = rig(3600);
        assert_eq!(r.cache.peek(&r.spec), None);
        assert_eq!(r.runs.load(Ordering::SeqCst), 0);
        r.cache.get(&r.spec).await.unwrap();
        assert_eq!(r.cache.peek(&r.spec).as_deref(), Some("tok1"));
        r.now.fetch_add(3600, Ordering::SeqCst);
        assert_eq!(r.cache.peek(&r.spec), None, "not past its expiry");
        assert_eq!(r.runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn forgetting_makes_the_next_use_mint_again() {
        let r = rig(3600);
        r.cache.get(&r.spec).await.unwrap();
        r.cache.forget(&r.spec);
        assert_eq!(r.cache.get(&r.spec).await.unwrap(), "tok2");
    }

    #[tokio::test]
    async fn different_command_lines_are_different_secrets() {
        let r = rig(3600);
        let other = Spec::new(&["mint".into(), "y".into()], None, None);
        assert_eq!(r.cache.get(&r.spec).await.unwrap(), "tok1");
        assert_eq!(r.cache.get(&other).await.unwrap(), "tok2");
    }

    #[tokio::test]
    async fn a_value_that_is_already_expired_is_refused() {
        let cache = Arc::new(MintCache::new(
            Arc::new(|_| {
                Box::pin(async {
                    Ok(Minted {
                        value: "old".into(),
                        expires: 5,
                    })
                })
            }),
            Arc::new(|| 1000),
        ));
        let e = cache
            .get(&Spec::new(&["x".into()], None, None))
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("already expired"));
    }

    async fn sh(script: &str, timeout: u64) -> Result<Minted> {
        run_command(Spec::new(
            &["sh".into(), "-c".into(), script.into()],
            Some(timeout),
            Some(60),
        ))
        .await
    }

    #[tokio::test]
    async fn a_real_command_is_run_and_its_output_parsed() {
        let m = sh(
            r#"printf '{"value":"v1","expires_at":"2030-01-01T00:00:00Z"}'"#,
            5,
        )
        .await
        .unwrap();
        assert_eq!(m.value, "v1");
        assert_eq!(sh("echo bare", 5).await.unwrap().value, "bare");
    }

    #[tokio::test]
    async fn a_failing_command_reports_stderr_and_never_stdout() {
        let e = format!(
            "{:#}",
            sh(
                "echo SECRETVALUE; echo 'HTTP 404: Not Found' >&2; exit 3",
                5
            )
            .await
            .unwrap_err()
        );
        assert!(
            e.contains("HTTP 404: Not Found") && e.contains("exit status: 3"),
            "{e}"
        );
        assert!(!e.contains("SECRETVALUE"), "{e}");
        let e = format!(
            "{:#}",
            sh("echo SECRETVALUE extra words\necho more", 5)
                .await
                .unwrap_err()
        );
        assert!(!e.contains("SECRETVALUE"), "{e}");
    }

    #[tokio::test]
    async fn a_command_that_hangs_is_killed() {
        let t = Instant::now();
        let e = format!("{:#}", sh("sleep 30", 1).await.unwrap_err());
        assert!(e.contains("timed out"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn the_command_gets_a_clean_environment() {
        std::env::set_var("ZAPPER_MINT_TEST_KEPT", "kept");
        std::env::set_var("MINT_TEST_DROPPED", "dropped");
        let m = sh(
            r#"echo "${ZAPPER_MINT_TEST_KEPT:-none}-${MINT_TEST_DROPPED:-none}""#,
            5,
        )
        .await
        .unwrap();
        assert_eq!(m.value, "kept-none");
    }

    #[tokio::test]
    async fn a_missing_program_is_an_error_not_a_panic() {
        let e = run_command(Spec::new(&["/nonexistent/zapper-mint".into()], None, None))
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("starting"));
        assert!(run_command(Spec::new(&[], None, None)).await.is_err());
    }
}
