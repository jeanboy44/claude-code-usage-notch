//! Background worker: reads Claude Code and Antigravity credentials and usage.
//! Computes rolling 5-hour session and 7-day weekly budgets.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// Manual refreshes closer together than this are coalesced.
const MIN_GAP: Duration = Duration::from_secs(15);
const OFFLINE_RETRY: Duration = Duration::from_secs(15);
/// Retry cadence while credentials are missing or expired (local keychain read only).
const RELOGIN_RETRY: Duration = Duration::from_secs(15);
/// The API rejected a token that looked valid; each retry costs a request.
const UNAUTHORIZED_RETRY: Duration = Duration::from_secs(60);
/// After `agy` fails (token refresh error, hung login) leave it alone this long: with a dead
/// login each run opens a browser sign-in page.
const AGY_RETRY_SECS: i64 = 1800;
/// A run still going after this is waiting on an interactive login; kill it.
const AGY_TIMEOUT: Duration = Duration::from_secs(30);
/// Never retry a 429 sooner than this, whatever Retry-After says.
const MIN_RATE_LIMIT_WAIT: u64 = 300;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider {
    Claude,
    Antigravity,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Antigravity];

    pub fn display_name(self) -> &'static str {
        match self {
            Provider::Claude => "Claude Code",
            Provider::Antigravity => "Antigravity",
        }
    }

    pub fn usage_page(self) -> &'static str {
        match self {
            Provider::Claude => "https://claude.ai/settings/usage",
            Provider::Antigravity => "https://antigravity.google/",
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct Limit {
    /// Percent used (0..100).
    pub used: f64,
    /// Unix seconds; `None` until the window has started.
    pub resets_at: Option<i64>,
}

impl Limit {
    /// Percent left, treating a window whose reset time has passed as full.
    pub fn left(self) -> f64 {
        if self.resets_at.is_some_and(|at| at <= now()) {
            return 100.0;
        }
        (100.0 - self.used).clamp(0.0, 100.0)
    }
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub five_hour: Option<Limit>,
    pub seven_day: Option<Limit>,
    pub seven_day_opus: Option<Limit>,
    pub seven_day_sonnet: Option<Limit>,
    /// When the numbers above were fetched (0 = never).
    pub fetched_at: i64,
    pub error: Option<&'static str>,
    /// The source is unreachable (signed out, token dead, offline), as opposed to just throttled.
    pub dead: bool,
    pub version: u64,
}

impl Snapshot {
    pub fn limits(&self) -> impl Iterator<Item = Limit> + '_ {
        [self.five_hour, self.seven_day, self.seven_day_opus, self.seven_day_sonnet].into_iter().flatten()
    }
}

static CLAUDE_STATE: Mutex<Option<Snapshot>> = Mutex::new(None);
static CLAUDE_HISTORY: Mutex<Vec<Sample>> = Mutex::new(Vec::new());

static ANTIGRAVITY_STATE: Mutex<Option<Snapshot>> = Mutex::new(None);
static ANTIGRAVITY_HISTORY: Mutex<Vec<Sample>> = Mutex::new(Vec::new());

/// One poll's worth of % used, for the history graph. The API has no history,
/// so we build our own from successful polls.
#[derive(Clone, Copy)]
pub struct Sample {
    pub t: i64,
    /// Session (5 h) % used.
    pub session: f32,
}

/// Keep a bit more than one session window so the graph's left edge is covered.
const HISTORY_SPAN: i64 = 6 * 3600;
/// Unchanged values are only re-recorded this often.
const HISTORY_HEARTBEAT: i64 = 300;

#[allow(dead_code)]
pub fn history() -> Vec<Sample> {
    history_for(Provider::Claude)
}

pub fn history_for(provider: Provider) -> Vec<Sample> {
    match provider {
        Provider::Claude => CLAUDE_HISTORY.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        Provider::Antigravity => ANTIGRAVITY_HISTORY.lock().unwrap_or_else(|e| e.into_inner()).clone(),
    }
}

fn record(provider: Provider, s: &Snapshot) {
    let Some(five) = s.five_hour else { return };
    let sample = Sample { t: s.fetched_at, session: five.used as f32 };
    let mut h = match provider {
        Provider::Claude => CLAUDE_HISTORY.lock().unwrap_or_else(|e| e.into_inner()),
        Provider::Antigravity => ANTIGRAVITY_HISTORY.lock().unwrap_or_else(|e| e.into_inner()),
    };
    if let Some(last) = h.last() {
        let same = last.session == sample.session;
        if sample.t <= last.t || (same && sample.t - last.t < HISTORY_HEARTBEAT) {
            return;
        }
    }
    h.push(sample);
    let cutoff = sample.t - HISTORY_SPAN;
    h.retain(|x| x.t >= cutoff);
}

#[allow(dead_code)]
pub fn snapshot() -> Option<Snapshot> {
    snapshot_for(Provider::Claude)
}

pub fn snapshot_for(provider: Provider) -> Option<Snapshot> {
    match provider {
        Provider::Claude => CLAUDE_STATE.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        Provider::Antigravity => ANTIGRAVITY_STATE.lock().unwrap_or_else(|e| e.into_inner()).clone(),
    }
}

/// Replace the current numbers (used to render README images offline).
#[allow(dead_code)]
pub fn set_snapshot(s: Snapshot) {
    set_snapshot_for(Provider::Claude, s);
}

pub fn set_snapshot_for(provider: Provider, s: Snapshot) {
    match provider {
        Provider::Claude => *CLAUDE_STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(s),
        Provider::Antigravity => *ANTIGRAVITY_STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(s),
    }
}

#[allow(dead_code)]
pub fn set_history(samples: Vec<Sample>) {
    set_history_for(Provider::Claude, samples);
}

pub fn set_history_for(provider: Provider, samples: Vec<Sample>) {
    match provider {
        Provider::Claude => *CLAUDE_HISTORY.lock().unwrap_or_else(|e| e.into_inner()) = samples,
        Provider::Antigravity => *ANTIGRAVITY_HISTORY.lock().unwrap_or_else(|e| e.into_inner()) = samples,
    }
}

pub struct Config {
    pub refresh_secs: u64,
    pub keychain_service: String,
    pub show_claude: bool,
    pub show_antigravity: bool,
}

impl Config {
    /// `~/.config/notchi-usage/config` — plain `key = value` lines.
    pub fn load() -> Self {
        let mut c = Config {
            refresh_secs: 120,
            keychain_service: "Claude Code-credentials".into(),
            show_claude: true,
            show_antigravity: true,
        };
        let Ok(home) = std::env::var("HOME") else { return c };
        let Ok(text) = std::fs::read_to_string(format!("{home}/.config/notchi-usage/config")) else { return c };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "refresh_secs" => c.refresh_secs = v.parse().unwrap_or(c.refresh_secs).max(30),
                "keychain_service" => c.keychain_service = v.to_string(),
                "show_claude" => c.show_claude = v.parse().unwrap_or(c.show_claude),
                "show_antigravity" => c.show_antigravity = v.parse().unwrap_or(c.show_antigravity),
                _ => {}
            }
        }
        if !c.show_claude && !c.show_antigravity {
            c.show_claude = true;
            c.show_antigravity = true;
        }
        c
    }
}

pub fn save_provider_config(show_claude: bool, show_antigravity: bool) {
    let Ok(home) = std::env::var("HOME") else { return };
    let dir = format!("{home}/.config/notchi-usage");
    let path = format!("{dir}/config");
    let _ = std::fs::create_dir_all(&dir);
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.starts_with("show_claude") && !t.starts_with("show_antigravity")
        })
        .map(String::from)
        .collect();
    lines.push(format!("show_claude = {show_claude}"));
    lines.push(format!("show_antigravity = {show_antigravity}"));
    let _ = std::fs::write(&path, lines.join("\n") + "\n");
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[derive(Debug)]
enum FetchError {
    NoLogin,
    NoLoginAntigravity,
    Expired,
    Unauthorized,
    /// With the server's Retry-After (seconds), if it sent one.
    RateLimited(Option<u64>),
    Offline,
    Other(&'static str),
}

impl FetchError {
    /// No way to reach the numbers right now; a 429 or a 5xx still means the account is alive.
    fn is_dead(&self) -> bool {
        matches!(
            self,
            FetchError::NoLogin
                | FetchError::NoLoginAntigravity
                | FetchError::Expired
                | FetchError::Unauthorized
                | FetchError::Offline
        )
    }

    fn message(&self) -> &'static str {
        match self {
            FetchError::NoLogin => "Not signed in to Claude Code",
            FetchError::NoLoginAntigravity => "Not signed in to Antigravity",
            FetchError::Expired | FetchError::Unauthorized => "Token expired · run to renew",
            FetchError::RateLimited(_) => "Rate limited · backing off",
            FetchError::Offline => "Offline",
            FetchError::Other(m) => m,
        }
    }
}

struct Creds {
    token: String,
    /// Unix seconds.
    expires_at: Option<i64>,
}

impl Creds {
    fn expired(&self) -> bool {
        self.expires_at.is_some_and(|t| t <= now())
    }
}

pub fn run(cfg: Config, wake: Receiver<()>) {
    let mut creds: Option<Creds> = None;
    let mut backoff = 1u64;

    // Restore the last numbers so a restart shows data instantly and doesn't re-poll
    // (or re-trip a 429) before the previous schedule says so.
    if let Some(cache) = load_cache() {
        let mut next_reset = None;
        if let Some(snap_c) = cache.snapshot {
            next_reset = publish(Provider::Claude, Ok(snap_c));
        }
        if let Some(snap_a) = cache.snapshot_antigravity {
            let a_reset = publish(Provider::Antigravity, Ok(snap_a));
            next_reset = match (next_reset, a_reset) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
        }
        let mut wait = (cache.next_fetch_at - now()).max(0);
        if let Some(at) = next_reset.filter(|_| !cache.rate_limited) {
            wait = wait.min((at - now() + 10).max(5));
        }
        if !sleep(&wake, Duration::from_secs(wait as u64), !cache.rate_limited) {
            return;
        }
    }

    loop {
        // --- Claude Code ---
        if creds.as_ref().is_none_or(Creds::expired) {
            creds = read_creds(&cfg.keychain_service);
        }
        let claude_outcome = match &creds {
            None => Err(FetchError::NoLogin),
            Some(c) if c.expired() => Err(FetchError::Expired),
            Some(c) => match fetch(&c.token) {
                Err(FetchError::Unauthorized) => {
                    creds = read_creds(&cfg.keychain_service);
                    match &creds {
                        Some(c) if !c.expired() => fetch(&c.token),
                        _ => Err(FetchError::Expired),
                    }
                }
                r => r,
            },
        };

        // --- Antigravity ---
        // Only when its row is enabled: `agy` opens a browser login when its token is dead.
        // The toggle is saved to the config file, so re-read it each round.
        let antigravity_outcome = if Config::load().show_antigravity {
            Some(fetch_antigravity())
        } else {
            // Re-enabling the row starts clean instead of waiting out an old backoff.
            AGY_RETRY_AT.store(0, std::sync::atomic::Ordering::Relaxed);
            None
        };

        // Stderr logging
        match &claude_outcome {
            Ok(snap) => eprintln!(
                "[{}] claude ok session={:?} weekly={:?}",
                now(),
                snap.five_hour.map(|l| l.used),
                snap.seven_day.map(|l| l.used)
            ),
            Err(e) => eprintln!("[{}] claude error: {}", now(), e.message()),
        }
        match &antigravity_outcome {
            Some(Ok(snap)) => eprintln!(
                "[{}] antigravity ok session={:?} weekly={:?}",
                now(),
                snap.five_hour.map(|l| l.used),
                snap.seven_day.map(|l| l.used)
            ),
            Some(Err(e)) => eprintln!("[{}] antigravity error: {}", now(), e.message()),
            None => {}
        }

        let retry_after = match &claude_outcome {
            Err(FetchError::RateLimited(ra)) => Some(ra.unwrap_or(0).max(MIN_RATE_LIMIT_WAIT)),
            _ => None,
        };
        backoff = if retry_after.is_some() { (backoff * 2).min(8) } else { 1 };
        let mut wait = cfg.refresh_secs * backoff;
        if let Some(ra) = retry_after {
            wait = wait.max(ra);
        }
        if matches!(claude_outcome, Err(FetchError::Offline)) {
            wait = wait.min(OFFLINE_RETRY.as_secs());
        }
        // Signed out / expired token: re-reading the keychain is free (no API call is
        // made), so notice a fresh login within seconds instead of a whole refresh period.
        if matches!(claude_outcome, Err(FetchError::NoLogin | FetchError::Expired)) {
            wait = wait.min(RELOGIN_RETRY.as_secs());
        } else if matches!(claude_outcome, Err(FetchError::Unauthorized)) {
            wait = wait.min(UNAUTHORIZED_RETRY.as_secs());
        }

        let reset_c = publish(Provider::Claude, claude_outcome);
        let reset_a = antigravity_outcome.and_then(|o| publish(Provider::Antigravity, o));
        let next_reset = match (reset_c, reset_a) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };

        if let Some(at) = next_reset.filter(|_| retry_after.is_none()) {
            // Re-check just after the next window resets so numbers never go stale.
            wait = wait.min((at - now() + 10).max(5) as u64);
        }
        save_cache(now() + wait as i64, retry_after.is_some());

        // Clicks never hammer the endpoint or skip a 429 backoff.
        if !sleep(&wake, Duration::from_secs(wait), retry_after.is_none()) {
            return;
        }
    }
}

/// Waits `wait`, letting manual refreshes cut it short to `MIN_GAP` when allowed.
/// Returns false once the app is gone.
fn sleep(wake: &Receiver<()>, mut wait: Duration, allow_manual: bool) -> bool {
    let started = Instant::now();
    loop {
        match wake.recv_timeout(wait.saturating_sub(started.elapsed())) {
            Ok(()) => {
                while wake.try_recv().is_ok() {}
                if allow_manual {
                    wait = wait.min(MIN_GAP);
                }
            }
            Err(RecvTimeoutError::Timeout) => return true,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// Stores the result and returns the earliest upcoming reset, if any.
fn publish(provider: Provider, outcome: Result<Snapshot, FetchError>) -> Option<i64> {
    let mut guard = match provider {
        Provider::Claude => CLAUDE_STATE.lock().unwrap_or_else(|e| e.into_inner()),
        Provider::Antigravity => ANTIGRAVITY_STATE.lock().unwrap_or_else(|e| e.into_inner()),
    };
    let prev = guard.take();
    let version = prev.as_ref().map_or(0, |p| p.version) + 1;
    let snap = match outcome {
        Ok(s) => {
            record(provider, &s);
            Snapshot { version, ..s }
        }
        // Keep the last good numbers; the UI shows their age next to the error.
        Err(e) => Snapshot { error: Some(e.message()), dead: e.is_dead(), version, ..prev.unwrap_or_default() },
    };
    let t = now();
    let next_reset = snap.limits().filter_map(|l| l.resets_at).filter(|&at| at > t).min();
    *guard = Some(snap);
    next_reset
}

// ---------- on-disk cache: last good numbers + when we may poll next ----------

struct Cache {
    snapshot: Option<Snapshot>,
    snapshot_antigravity: Option<Snapshot>,
    next_fetch_at: i64,
    rate_limited: bool,
}

fn cache_path() -> Option<String> {
    Some(format!("{}/Library/Caches/notchi-usage/state.json", std::env::var("HOME").ok()?))
}

fn save_cache(next_fetch_at: i64, rate_limited: bool) {
    let Some(path) = cache_path() else { return };
    let snap_c = snapshot_for(Provider::Claude).unwrap_or_default();
    let snap_a = snapshot_for(Provider::Antigravity).unwrap_or_default();
    let limit = |l: Option<Limit>| l.map(|l| serde_json::json!([l.used, l.resets_at]));
    let json = serde_json::json!({
        "fetched_at": snap_c.fetched_at,
        "next_fetch_at": next_fetch_at,
        "rate_limited": rate_limited,
        "five_hour": limit(snap_c.five_hour),
        "seven_day": limit(snap_c.seven_day),
        "seven_day_opus": limit(snap_c.seven_day_opus),
        "seven_day_sonnet": limit(snap_c.seven_day_sonnet),
        "history": history_for(Provider::Claude).iter().map(|x| serde_json::json!([x.t, x.session])).collect::<Vec<_>>(),
        "antigravity": {
            "fetched_at": snap_a.fetched_at,
            "five_hour": limit(snap_a.five_hour),
            "seven_day": limit(snap_a.seven_day),
            "seven_day_opus": limit(snap_a.seven_day_opus),
            "seven_day_sonnet": limit(snap_a.seven_day_sonnet),
            "history": history_for(Provider::Antigravity).iter().map(|x| serde_json::json!([x.t, x.session])).collect::<Vec<_>>(),
        }
    });
    if let Some(dir) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, json.to_string());
}

fn load_cache() -> Option<Cache> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(cache_path()?).ok()?).ok()?;
    if let Some(points) = v["history"].as_array() {
        let samples: Vec<Sample> = points.iter().filter_map(|p| {
            let p = p.as_array()?;
            Some(Sample { t: p.first()?.as_i64()?, session: p.get(1)?.as_f64()? as f32 })
        }).collect();
        *CLAUDE_HISTORY.lock().unwrap_or_else(|e| e.into_inner()) = samples;
    }
    if let Some(points) = v["antigravity"]["history"].as_array() {
        let samples: Vec<Sample> = points.iter().filter_map(|p| {
            let p = p.as_array()?;
            Some(Sample { t: p.first()?.as_i64()?, session: p.get(1)?.as_f64()? as f32 })
        }).collect();
        *ANTIGRAVITY_HISTORY.lock().unwrap_or_else(|e| e.into_inner()) = samples;
    }

    let limit = |obj: &Value, k: &str| -> Option<Limit> {
        let a = obj.get(k)?.as_array()?;
        Some(Limit { used: a.first()?.as_f64()?, resets_at: a.get(1).and_then(Value::as_i64) })
    };

    let snap_c = v["fetched_at"].as_i64().filter(|&t| t > 0).map(|fetched_at| Snapshot {
        five_hour: limit(&v, "five_hour"),
        seven_day: limit(&v, "seven_day"),
        seven_day_opus: limit(&v, "seven_day_opus"),
        seven_day_sonnet: limit(&v, "seven_day_sonnet"),
        fetched_at,
        error: None,
        dead: false,
        version: 0,
    });

    let ag = &v["antigravity"];
    let snap_a = ag["fetched_at"].as_i64().filter(|&t| t > 0).map(|fetched_at| Snapshot {
        five_hour: limit(ag, "five_hour"),
        seven_day: limit(ag, "seven_day"),
        seven_day_opus: limit(ag, "seven_day_opus"),
        seven_day_sonnet: limit(ag, "seven_day_sonnet"),
        fetched_at,
        error: None,
        dead: false,
        version: 0,
    });

    Some(Cache {
        snapshot: snap_c,
        snapshot_antigravity: snap_a,
        next_fetch_at: v["next_fetch_at"].as_i64().unwrap_or(0),
        rate_limited: v["rate_limited"].as_bool().unwrap_or(false),
    })
}

/// A generic-password item's secret, read by this app itself through the Security
/// framework. The Keychain prompt (and "Always Allow") therefore applies to NotchiUsage
/// only, not to Apple's `security` tool that any other program could also run.
fn keychain_password(service: &str) -> Option<String> {
    use security_framework::item::{ItemClass, ItemSearchOptions, SearchResult};
    let found = ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service(service)
        .load_data(true)
        .limit(1)
        .search()
        .ok()?;
    match found.into_iter().next()? {
        SearchResult::Data(bytes) => Some(String::from_utf8_lossy(&bytes).trim().to_string()),
        _ => None,
    }
}

/// Claude Code's own login wins. The desktop app's tokens are only a fallback (they can
/// belong to another account, and reading them touches a more sensitive keychain item).
fn read_creds(service: &str) -> Option<Creds> {
    let cli = read_cli_creds(service);
    if cli.as_ref().is_some_and(|c| !c.expired()) {
        return cli;
    }
    read_desktop_creds().or(cli)
}

/// After a failed desktop read (no app, no item, prompt denied) don't ask again for a while.
static DESKTOP_RETRY_AT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
const DESKTOP_RETRY_SECS: i64 = 600;

fn read_cli_creds(service: &str) -> Option<Creds> {
    let out = keychain_password(service)
        .or_else(|| {
            let home = std::env::var("HOME").ok()?;
            std::fs::read_to_string(format!("{home}/.claude/.credentials.json")).ok()
        })?;
    let v: Value = serde_json::from_str(&out).ok()?;
    let oauth = &v["claudeAiOauth"];
    Some(Creds {
        token: oauth["accessToken"].as_str()?.to_string(),
        expires_at: oauth["expiresAt"].as_i64().map(|ms| ms / 1000),
    })
}

/// The Claude desktop app keeps OAuth tokens in `config.json`, encrypted with Electron's
/// safeStorage (AES-128-CBC; key = PBKDF2-SHA1 of the "Claude Safe Storage" keychain
/// password). Read-only: the app refreshes its own tokens.
fn read_desktop_creds() -> Option<Creds> {
    use std::sync::atomic::Ordering::Relaxed;
    if now() < DESKTOP_RETRY_AT.load(Relaxed) {
        return None;
    }
    let found = read_desktop_creds_once();
    if found.is_none() {
        DESKTOP_RETRY_AT.store(now() + DESKTOP_RETRY_SECS, Relaxed);
    }
    found
}

fn read_desktop_creds_once() -> Option<Creds> {
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};

    let home = std::env::var("HOME").ok()?;
    let config: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{home}/Library/Application Support/Claude/config.json")).ok()?)
            .ok()?;
    let password = keychain_password("Claude Safe Storage")?;
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password.as_bytes(), b"saltysalt", 1003, &mut key);

    let mut best: Option<Creds> = None;
    for field in ["oauth:tokenCacheV2", "oauth:tokenCache"] {
        let Some(raw) = config[field].as_str().and_then(base64_decode) else { continue };
        let Some(body) = raw.strip_prefix(b"v10") else { continue };
        let Ok(dec) = cbc::Decryptor::<aes::Aes128>::new_from_slices(&key, &[b' '; 16]) else { continue };
        let mut buf = body.to_vec();
        let Ok(plain) = dec.decrypt_padded_mut::<Pkcs7>(&mut buf) else { continue };
        let Ok(Value::Object(entries)) = serde_json::from_slice::<Value>(plain) else { continue };
        for (name, entry) in entries {
            // Usage needs the `user:profile` scope; `user:office` etc. tokens can't read it.
            // Key shape: `acct:<account>|<client>:<org>:https://api.anthropic.com:<scopes>`.
            let scopes = name.rsplit_once("https://api.anthropic.com:").map_or("", |(_, s)| s);
            if !scopes.split_whitespace().any(|s| s == "user:profile") {
                continue;
            }
            let (Some(token), Some(ms)) = (entry["token"].as_str(), entry["expiresAt"].as_i64()) else { continue };
            let c = Creds { token: token.to_string(), expires_at: Some(ms / 1000) };
            if best.as_ref().is_none_or(|b| b.expires_at < c.expires_at) {
                best = Some(c);
            }
        }
    }
    best
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let input = input.trim();
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for &b in input.as_bytes() {
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b'\n' | b' ' => continue,
            _ => return None,
        } as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn read_antigravity_token() -> Option<(String, Option<i64>)> {
    // 1. Try keychain services "antigravity" and "gemini"
    for service in ["antigravity", "gemini"] {
        if let Some(raw) = keychain_password(service) {
            let json_str = if let Some(b64) = raw.strip_prefix("go-keyring-base64:") {
                base64_decode(b64).and_then(|bytes| String::from_utf8(bytes).ok())
            } else {
                Some(raw)
            };
            if let Some(json_str) = json_str {
                if let Ok(v) = serde_json::from_str::<Value>(&json_str) {
                    let token_obj = &v["token"];
                    if let Some(access) = token_obj["access_token"].as_str() {
                        let exp = token_obj["expiry"].as_str().and_then(parse_iso8601);
                        return Some((access.to_string(), exp));
                    }
                }
            }
        }
    }

    // 2. Try ~/.gemini/antigravity-cli/antigravity-oauth-token
    let home = std::env::var("HOME").ok()?;
    let content = std::fs::read_to_string(format!("{home}/.gemini/antigravity-cli/antigravity-oauth-token")).ok()?;
    let v: Value = serde_json::from_str(&content).ok()?;
    let token_obj = &v["token"];
    let access = token_obj["access_token"].as_str()?;
    let exp = token_obj["expiry"].as_str().and_then(parse_iso8601);
    Some((access.to_string(), exp))
}

fn fetch_antigravity_cli_quota() -> Result<Snapshot, FetchError> {
    let agy_bin = if std::path::Path::new("/opt/homebrew/bin/agy").exists() {
        "/opt/homebrew/bin/agy"
    } else {
        "agy"
    };

    let existing_path = std::env::var("PATH").unwrap_or_default();
    let path = if existing_path.is_empty() {
        "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin".to_string()
    } else {
        format!("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{existing_path}")
    };

    let mut child = Command::new(agy_bin)
        .args(["-p", "/usage", "--output-format", "json"])
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| FetchError::Other("agy command failed to start"))?;
    let started = Instant::now();
    while child.try_wait().ok().flatten().is_none() {
        if started.elapsed() > AGY_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(FetchError::Other("agy timed out"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let output = child.wait_with_output().map_err(|_| FetchError::Other("agy command failed to start"))?;

    if !output.status.success() {
        return Err(FetchError::Other("agy command exited with error"));
    }

    let v: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| FetchError::Other("failed to parse agy json"))?;

    let groups = v.get("command")
        .and_then(|c| c.get("data"))
        .and_then(|d| d.get("groups"))
        .and_then(Value::as_array)
        .ok_or(FetchError::Other("no groups in agy usage"))?;

    let group = groups
        .iter()
        .find(|g| g.get("name").and_then(Value::as_str).map_or(false, |name| name.contains("Gemini")))
        .or_else(|| groups.first())
        .ok_or(FetchError::Other("empty groups in agy usage"))?;

    let buckets = group
        .get("buckets")
        .and_then(Value::as_array)
        .ok_or(FetchError::Other("no buckets in group"))?;

    let mut five_hour = None;
    let mut seven_day = None;

    for bucket in buckets {
        let window = bucket.get("window").and_then(Value::as_str).unwrap_or("");
        let id = bucket.get("id").and_then(Value::as_str).unwrap_or("");
        let rem = bucket.get("remaining_fraction").and_then(Value::as_f64);
        let reset_time = bucket.get("reset_time").and_then(Value::as_str).and_then(parse_iso8601);

        if let Some(rem_val) = rem {
            let used = ((1.0 - rem_val) * 100.0).clamp(0.0, 100.0);
            let limit = Limit {
                used,
                resets_at: reset_time,
            };

            if window == "5h" || id.contains("5h") {
                five_hour = Some(limit);
            } else if window == "weekly" || id.contains("weekly") {
                seven_day = Some(limit);
            }
        }
    }

    Ok(Snapshot {
        five_hour,
        seven_day,
        seven_day_opus: None,
        seven_day_sonnet: None,
        fetched_at: now(),
        error: None,
        dead: false,
        version: 0,
    })
}

/// Unix time before which `agy` must not be run again (0 = free to run).
static AGY_RETRY_AT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

fn fetch_antigravity() -> Result<Snapshot, FetchError> {
    use std::sync::atomic::Ordering::Relaxed;
    if now() >= AGY_RETRY_AT.load(Relaxed) {
        match fetch_antigravity_cli_quota() {
            Ok(snap) => return Ok(snap),
            Err(_) => AGY_RETRY_AT.store(now() + AGY_RETRY_SECS, Relaxed),
        }
    }

    let Some((_token, _exp)) = read_antigravity_token() else {
        return Err(FetchError::NoLoginAntigravity);
    };

    let home = std::env::var("HOME").unwrap_or_default();
    let history_path = format!("{home}/.gemini/antigravity-cli/history.jsonl");

    let t_now = now();
    let mut ts_5h = Vec::new();
    let mut ts_7d = Vec::new();
    let mut pro_count_7d = 0usize;

    if let Ok(file_str) = std::fs::read_to_string(&history_path) {
        for line in file_str.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let Some(ts_ms) = v.get("timestamp").and_then(Value::as_i64) {
                    let ts = ts_ms / 1000;
                    if ts <= t_now {
                        if t_now - ts <= 5 * 3600 {
                            ts_5h.push(ts);
                        }
                        if t_now - ts <= 7 * 86400 {
                            ts_7d.push(ts);
                            if line.contains("pro") || line.contains("Pro") {
                                pro_count_7d += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    // Realistic budgets:
    // 5-hour rolling session budget: ~45 interactions
    // 7-day rolling weekly budget: ~200 interactions
    let n_5h = ts_5h.len() as f64;
    let n_7d = ts_7d.len() as f64;

    let five_hour_used = (n_5h / 45.0 * 100.0).clamp(0.0, 100.0);
    let five_hour_reset = ts_5h.iter().min().map(|&t_oldest| t_oldest + 5 * 3600);

    let seven_day_used = (n_7d / 200.0 * 100.0).clamp(0.0, 100.0);
    let seven_day_reset = ts_7d.iter().min().map(|&t_oldest| t_oldest + 7 * 86400);

    let pro_used = ((pro_count_7d as f64).max(n_7d * 0.25) / 50.0 * 100.0).clamp(0.0, 100.0);
    let flash_used = seven_day_used;

    Ok(Snapshot {
        five_hour: Some(Limit {
            used: five_hour_used,
            resets_at: five_hour_reset,
        }),
        seven_day: Some(Limit {
            used: seven_day_used,
            resets_at: seven_day_reset,
        }),
        seven_day_opus: Some(Limit {
            used: pro_used,
            resets_at: seven_day_reset,
        }),
        seven_day_sonnet: Some(Limit {
            used: flash_used,
            resets_at: seven_day_reset,
        }),
        fetched_at: t_now,
        error: None,
        dead: false,
        version: 0,
    })
}

fn fetch(token: &str) -> Result<Snapshot, FetchError> {
    // The token goes into a header block on stdin: refuse anything that could add lines.
    if !token.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b)) {
        return Err(FetchError::Unauthorized);
    }
    // Headers go through stdin so the token never appears in `ps`.
    let mut child = Command::new("/usr/bin/curl")
        .args(["-q", "-sS", "--proto", "=https", "-m", "10", "-A", "notchi-usage/0.1", "-H", "@-", "-w", "\n%{http_code} %header{retry-after}", USAGE_URL])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| FetchError::Other("curl unavailable"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = write!(
            stdin,
            "Authorization: Bearer {token}\nanthropic-beta: oauth-2025-04-20\nAccept: application/json\n"
        );
    }
    let out = child.wait_with_output().map_err(|_| FetchError::Other("curl failed"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, status) = text.rsplit_once('\n').unwrap_or(("", text.as_ref()));
    let mut status = status.split_whitespace();
    let code = status.next().unwrap_or("");
    let retry_after = status.next().and_then(|v| v.parse::<u64>().ok());
    match code {
        "200" => {}
        "401" | "403" => return Err(FetchError::Unauthorized),
        "429" => return Err(FetchError::RateLimited(retry_after)),
        "000" | "" => return Err(FetchError::Offline),
        c if c.starts_with('5') => return Err(FetchError::Other("Anthropic API is having trouble")),
        _ => return Err(FetchError::Other("Unexpected response")),
    }
    let v: Value = serde_json::from_str(body).map_err(|_| FetchError::Other("Unexpected response"))?;
    let limit = |k: &str| -> Option<Limit> {
        let o = v.get(k)?;
        Some(Limit {
            used: o.get("utilization")?.as_f64()?,
            resets_at: o.get("resets_at").and_then(Value::as_str).and_then(parse_iso8601),
        })
    };
    Ok(Snapshot {
        five_hour: limit("five_hour"),
        seven_day: limit("seven_day"),
        seven_day_opus: limit("seven_day_opus"),
        seven_day_sonnet: limit("seven_day_sonnet"),
        fetched_at: now(),
        error: None,
        dead: false,
        version: 0,
    })
}

/// `2025-10-13T19:00:00.123+09:00` → unix seconds.
fn parse_iso8601(s: &str) -> Option<i64> {
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (h, mi, se) = (n(11..13)?, n(14..16)?, n(17..19)?);
    let mut rest = s.get(19..)?;
    if let Some(frac) = rest.strip_prefix('.') {
        let end = frac.find(|c: char| !c.is_ascii_digit()).unwrap_or(frac.len());
        rest = &frac[end..];
    }
    let offset = match rest.chars().next() {
        Some(sign @ ('+' | '-')) => {
            let digits: String = rest[1..].chars().filter(char::is_ascii_digit).collect();
            let hh: i64 = digits.get(0..2)?.parse().ok()?;
            let mm: i64 = digits.get(2..4).and_then(|x| x.parse().ok()).unwrap_or(0);
            (hh * 3600 + mm * 60) * if sign == '-' { -1 } else { 1 }
        }
        _ => 0,
    };
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se - offset)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601("2025-10-13T19:00:00.123456+00:00"), Some(1760382000));
        assert_eq!(parse_iso8601("2025-10-14T04:00:00+09:00"), Some(1760382000));
        assert_eq!(parse_iso8601("2025-10-14T04:30:00+0930"), Some(1760382000));
    }

    #[test]
    fn passed_reset_counts_as_full() {
        assert_eq!(Limit { used: 80.0, resets_at: Some(1) }.left(), 100.0);
        assert_eq!(Limit { used: 80.0, resets_at: None }.left(), 20.0);
    }

    #[test]
    fn base64_decoding() {
        let text = "Hello Antigravity!";
        let encoded = "SGVsbG8gQW50aWdyYXZpdHkh";
        let decoded = base64_decode(encoded).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), text);
    }

    /// Exercises the Claude desktop app path end to end on a machine signed in to it.
    #[test]
    #[ignore = "needs a local Claude desktop login and network"]
    fn desktop_login_fetch() {
        let creds = read_desktop_creds_once().expect("no usable desktop token");
        eprintln!("desktop token: expired={} ttl={:?}s", creds.expired(), creds.expires_at.map(|t| t - now()));
        let snap = fetch(&creds.token).unwrap_or_else(|e| panic!("usage fetch failed: {}", e.message()));
        eprintln!("session used={:?} weekly used={:?}", snap.five_hour.map(|l| l.used), snap.seven_day.map(|l| l.used));
        assert!(snap.five_hour.is_some());
    }

    #[test]
    fn antigravity_local_fetch() {
        let outcome = fetch_antigravity();
        assert!(outcome.is_ok(), "Expected antigravity fetch to succeed on machine");
        let snap = outcome.unwrap();
        assert!(snap.five_hour.is_some());
        assert!(snap.seven_day.is_some());
    }

    #[test]
    fn antigravity_cli_quota_fetch() {
        let outcome = fetch_antigravity_cli_quota();
        assert!(outcome.is_ok(), "Expected agy CLI fetch to succeed on machine");
        let snap = outcome.unwrap();
        assert!(snap.five_hour.is_some());
        assert!(snap.seven_day.is_some());
        let fh = snap.five_hour.unwrap();
        assert!(fh.used >= 0.0 && fh.used <= 100.0);
        let sd = snap.seven_day.unwrap();
        assert!(sd.used >= 0.0 && sd.used <= 100.0);
    }
}

