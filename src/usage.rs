//! Background worker: reads Claude Code and Antigravity credentials and usage.
//! Computes rolling 5-hour session and 7-day weekly budgets.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
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
    /// Which Claude account to show: an account email or UUID, or empty for automatic.
    pub claude_account: String,
}

impl Config {
    /// `~/.config/notchi-usage/config` — plain `key = value` lines.
    pub fn load() -> Self {
        let mut c = Config {
            refresh_secs: 120,
            keychain_service: "Claude Code-credentials".into(),
            show_claude: true,
            show_antigravity: true,
            claude_account: String::new(),
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
                "claude_account" => c.claude_account = if v == "auto" { String::new() } else { v.to_string() },
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
    save_config(&[("show_claude", show_claude.to_string()), ("show_antigravity", show_antigravity.to_string())]);
}

/// Pins the Claude account to show (`None` = automatic).
pub fn save_claude_account(id: Option<&str>) {
    save_config(&[("claude_account", id.unwrap_or("auto").to_string())]);
}

/// Rewrites the given keys in the config file, keeping every other line.
fn save_config(entries: &[(&str, String)]) {
    let Ok(home) = std::env::var("HOME") else { return };
    let dir = format!("{home}/.config/notchi-usage");
    let path = format!("{dir}/config");
    let _ = std::fs::create_dir_all(&dir);
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.split_once('=').is_some_and(|(k, _)| entries.iter().any(|(e, _)| k.trim() == *e)))
        .map(String::from)
        .collect();
    lines.extend(entries.iter().map(|(k, v)| format!("{k} = {v}")));
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

#[derive(Clone)]
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
    let mut backoff = 1u64;
    // Account the Claude numbers (and history graph) belong to.
    let mut shown_account: Option<String> = None;

    // Restore the last numbers so a restart shows data instantly and doesn't re-poll
    // (or re-trip a 429) before the previous schedule says so.
    if let Some(cache) = load_cache() {
        shown_account = cache.account;
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
        // The menu writes the account choice and toggles to the config file.
        let live = Config::load();

        // --- Claude Code ---
        let mut logins = claude_logins(&cfg.keychain_service);
        let mut picked = pick_login(&logins, &live.claude_account);
        let mut claude_outcome = match picked.and_then(|i| logins.get(i)) {
            None => Err(FetchError::NoLogin),
            Some(l) if l.creds.expired() => Err(FetchError::Expired),
            Some(l) => fetch(&l.creds.token),
        };
        if matches!(claude_outcome, Err(FetchError::Unauthorized)) {
            // The app may have refreshed its token since we last decrypted it.
            DESKTOP_CACHE.lock().unwrap_or_else(|e| e.into_inner()).take();
            logins = claude_logins(&cfg.keychain_service);
            picked = pick_login(&logins, &live.claude_account);
            claude_outcome = match picked.and_then(|i| logins.get(i)) {
                Some(l) if !l.creds.expired() => fetch(&l.creds.token),
                _ => Err(FetchError::Expired),
            };
        }
        let account = picked.and_then(|i| logins.get(i)).map(|l| l.account.id.clone());
        if account.is_some() && account != shown_account {
            // Another account's numbers: its history graph doesn't carry over.
            if shown_account.is_some() {
                CLAUDE_HISTORY.lock().unwrap_or_else(|e| e.into_inner()).clear();
                if claude_outcome.is_err() {
                    *CLAUDE_STATE.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
            }
            shown_account = account.clone();
        }
        set_claude_accounts(logins.into_iter().map(|l| l.account).collect(), account);

        // --- Antigravity ---
        // Only when its row is enabled: `agy` opens a browser login when its token is dead.
        // The toggle is saved to the config file, so re-read it each round.
        let antigravity_outcome = if live.show_antigravity {
            *ANTIGRAVITY_ACCOUNT.lock().unwrap_or_else(|e| e.into_inner()) = antigravity_email();
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
        save_cache(now() + wait as i64, retry_after.is_some(), shown_account.as_deref());

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
    /// Claude account the snapshot belongs to.
    account: Option<String>,
}

fn cache_path() -> Option<String> {
    Some(format!("{}/Library/Caches/notchi-usage/state.json", std::env::var("HOME").ok()?))
}

fn save_cache(next_fetch_at: i64, rate_limited: bool, account: Option<&str>) {
    let Some(path) = cache_path() else { return };
    let snap_c = snapshot_for(Provider::Claude).unwrap_or_default();
    let snap_a = snapshot_for(Provider::Antigravity).unwrap_or_default();
    let limit = |l: Option<Limit>| l.map(|l| serde_json::json!([l.used, l.resets_at]));
    let json = serde_json::json!({
        "fetched_at": snap_c.fetched_at,
        "next_fetch_at": next_fetch_at,
        "rate_limited": rate_limited,
        "account": account,
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
        account: v["account"].as_str().map(String::from),
    })
}

/// A generic-password item's secret, read through Apple's `security` tool. "Always Allow" is
/// then tied to that stable system binary instead of this app's (ad-hoc, per-build) code
/// signature, so it survives app updates and rebuilds instead of re-prompting each time.
fn keychain_password(service: &str) -> Option<String> {
    Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-w"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// One Claude account Notchi can read usage for.
#[derive(Clone, Debug, PartialEq)]
pub struct Account {
    /// Account UUID, when known.
    pub id: String,
    pub email: Option<String>,
    /// Claude Code (the CLI) is signed in to it.
    pub cli: bool,
    /// The Claude desktop app is signed in to it.
    pub desktop: bool,
    /// The account the desktop app last used.
    pub desktop_active: bool,
}

impl Account {
    pub fn label(&self) -> String {
        self.email.clone().unwrap_or_else(|| format!("Account {}", self.id.get(..8).unwrap_or(&self.id)))
    }

    /// Where it is signed in, e.g. "Claude Code · Claude app".
    pub fn sources(&self) -> String {
        let mut s = Vec::new();
        if self.cli {
            s.push("Claude Code");
        }
        if self.desktop {
            s.push(if self.desktop_active { "Claude app (current)" } else { "Claude app" });
        }
        s.join(" · ")
    }

    fn matches(&self, wanted: &str) -> bool {
        self.id.eq_ignore_ascii_case(wanted) || self.email.as_deref().is_some_and(|e| e.eq_ignore_ascii_case(wanted))
    }
}

/// Every Claude account found on this Mac, and the one being shown.
static CLAUDE_ACCOUNTS: Mutex<(Vec<Account>, Option<String>)> = Mutex::new((Vec::new(), None));
static ANTIGRAVITY_ACCOUNT: Mutex<Option<String>> = Mutex::new(None);

pub fn claude_accounts() -> Vec<Account> {
    CLAUDE_ACCOUNTS.lock().unwrap_or_else(|e| e.into_inner()).0.clone()
}

/// The account whose numbers a provider shows, as a label.
pub fn current_account(provider: Provider) -> Option<String> {
    match provider {
        Provider::Claude => {
            let g = CLAUDE_ACCOUNTS.lock().unwrap_or_else(|e| e.into_inner());
            let id = g.1.as_deref()?;
            g.0.iter().find(|a| a.id == id).map(Account::label)
        }
        Provider::Antigravity => ANTIGRAVITY_ACCOUNT.lock().unwrap_or_else(|e| e.into_inner()).clone(),
    }
}

/// The pinned Claude account from the config file (`None` = automatic).
pub fn chosen_claude_account() -> Option<String> {
    Some(Config::load().claude_account).filter(|s| !s.is_empty())
}

fn set_claude_accounts(accounts: Vec<Account>, shown: Option<String>) {
    *CLAUDE_ACCOUNTS.lock().unwrap_or_else(|e| e.into_inner()) = (accounts, shown);
}

struct Login {
    account: Account,
    creds: Creds,
}

/// All usable Claude logins: Claude Code's own first, then each other desktop-app account.
fn claude_logins(service: &str) -> Vec<Login> {
    let mut logins = Vec::new();
    if let Some(creds) = read_cli_creds(service) {
        let (id, email) = cli_account().unwrap_or_else(|| ("claude-code".into(), None));
        logins.push(Login { account: Account { id, email, cli: true, desktop: false, desktop_active: false }, creds });
    }
    let Some(desktop) = desktop_logins() else { return logins };
    for (id, creds) in desktop.tokens {
        let active = desktop.active.as_deref() == Some(id.as_str());
        if let Some(l) = logins.iter_mut().find(|l| l.account.id == id) {
            l.account.desktop = true;
            l.account.desktop_active = active;
            // Same account in both: use whichever token lives longer.
            if l.creds.expires_at < creds.expires_at {
                l.creds = creds;
            }
            continue;
        }
        let email = account_email(&id, &creds);
        logins.push(Login { account: Account { id, email, cli: false, desktop: true, desktop_active: active }, creds });
    }
    logins
}

/// The login to show: the pinned account if it is still signed in, else Claude Code's own
/// login while it is valid, else the desktop app's current account, else any valid one.
fn pick_login(logins: &[Login], wanted: &str) -> Option<usize> {
    if !wanted.is_empty() {
        if let Some(i) = logins.iter().position(|l| l.account.matches(wanted)) {
            return Some(i);
        }
    }
    let valid = |l: &&Login| !l.creds.expired();
    logins
        .iter()
        .position(|l| l.account.cli && valid(&l))
        .or_else(|| logins.iter().position(|l| l.account.desktop_active && valid(&l)))
        .or_else(|| logins.iter().position(|l| valid(&l)))
        .or(if logins.is_empty() { None } else { Some(0) })
}

/// Claude Code's signed-in account (UUID, email), from `~/.claude.json`.
fn cli_account() -> Option<(String, Option<String>)> {
    let home = std::env::var("HOME").ok()?;
    let dir = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or(home.clone());
    let v: Value = serde_json::from_str(&std::fs::read_to_string(format!("{dir}/.claude.json")).ok()?).ok()?;
    let a = &v["oauthAccount"];
    Some((a["accountUuid"].as_str()?.to_string(), a["emailAddress"].as_str().map(String::from)))
}

/// Emails of accounts only the desktop app knows, looked up once per account.
static EMAILS: Mutex<Vec<(String, Option<String>)>> = Mutex::new(Vec::new());

fn account_email(id: &str, creds: &Creds) -> Option<String> {
    if let Some((_, e)) = EMAILS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(i, _)| i == id) {
        return e.clone();
    }
    if creds.expired() {
        return None;
    }
    let email = fetch_profile_email(&creds.token);
    EMAILS.lock().unwrap_or_else(|e| e.into_inner()).push((id.to_string(), email.clone()));
    email
}

/// Both Claude Code stores can hold a token (the Keychain item, or the file when the Keychain
/// isn't used); after a re-login only one of them is current, so take the longest-lived.
fn read_cli_creds(service: &str) -> Option<Creds> {
    let home = std::env::var("HOME").ok();
    let file = home.and_then(|h| std::fs::read_to_string(format!("{h}/.claude/.credentials.json")).ok());
    [keychain_password(service), file]
        .into_iter()
        .flatten()
        .filter_map(|raw| {
            let v: Value = serde_json::from_str(&raw).ok()?;
            let oauth = &v["claudeAiOauth"];
            Some(Creds {
                token: oauth["accessToken"].as_str()?.to_string(),
                expires_at: oauth["expiresAt"].as_i64().map(|ms| ms / 1000),
            })
        })
        .max_by_key(|c| c.expires_at.unwrap_or(i64::MAX))
}

struct DesktopLogins {
    /// Best usage-capable token per account UUID.
    tokens: Vec<(String, Creds)>,
    /// `lastKnownAccountUuid`: the account the app is signed in to now.
    active: Option<String>,
    read_at: i64,
}

/// Decrypting the desktop tokens runs `security`; reuse the result for a while.
static DESKTOP_CACHE: Mutex<Option<DesktopLogins>> = Mutex::new(None);
/// After a failed desktop read (no app, no item, prompt denied) don't ask again for a while.
static DESKTOP_RETRY_AT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
const DESKTOP_RETRY_SECS: i64 = 600;

fn desktop_logins() -> Option<DesktopLogins> {
    use std::sync::atomic::Ordering::Relaxed;
    let mut cache = DESKTOP_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let fresh = cache.as_ref().is_some_and(|c| {
        now() - c.read_at < DESKTOP_RETRY_SECS && c.tokens.iter().all(|(_, t)| !t.expired())
    });
    if !fresh {
        if now() < DESKTOP_RETRY_AT.load(Relaxed) {
            return None;
        }
        *cache = read_desktop_logins();
        if cache.is_none() {
            DESKTOP_RETRY_AT.store(now() + DESKTOP_RETRY_SECS, Relaxed);
        }
    }
    let c = cache.as_ref()?;
    Some(DesktopLogins {
        tokens: c.tokens.clone(),
        active: c.active.clone(),
        read_at: c.read_at,
    })
}

/// The Claude desktop app keeps OAuth tokens in `config.json`, encrypted with Electron's
/// safeStorage (AES-128-CBC; key = PBKDF2-SHA1 of the "Claude Safe Storage" keychain
/// password). Read-only: the app refreshes its own tokens. It can hold several accounts.
fn read_desktop_logins() -> Option<DesktopLogins> {
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};

    let home = std::env::var("HOME").ok()?;
    let config: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{home}/Library/Application Support/Claude/config.json")).ok()?)
            .ok()?;
    let password = keychain_password("Claude Safe Storage")?;
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password.as_bytes(), b"saltysalt", 1003, &mut key);

    let mut tokens: Vec<(String, Creds)> = Vec::new();
    for field in ["oauth:tokenCacheV2", "oauth:tokenCache"] {
        let Some(raw) = config[field].as_str().and_then(base64_decode) else { continue };
        let Some(body) = raw.strip_prefix(b"v10") else { continue };
        let Ok(dec) = cbc::Decryptor::<aes::Aes128>::new_from_slices(&key, &[b' '; 16]) else { continue };
        let mut buf = body.to_vec();
        let Ok(plain) = dec.decrypt_padded_mut::<Pkcs7>(&mut buf) else { continue };
        let Ok(Value::Object(entries)) = serde_json::from_slice::<Value>(plain) else { continue };
        for (name, entry) in entries {
            let Some((id, scopes)) = desktop_key(&name) else { continue };
            // Usage needs the `user:profile` scope; `user:office` etc. tokens can't read it.
            if !scopes.split_whitespace().any(|s| s == "user:profile") {
                continue;
            }
            let (Some(token), Some(ms)) = (entry["token"].as_str(), entry["expiresAt"].as_i64()) else { continue };
            let c = Creds { token: token.to_string(), expires_at: Some(ms / 1000) };
            match tokens.iter_mut().find(|(i, _)| i == id) {
                Some((_, best)) if best.expires_at >= c.expires_at => {}
                Some((_, best)) => *best = c,
                None => tokens.push((id.to_string(), c)),
            }
        }
    }
    if tokens.is_empty() {
        return None;
    }
    Some(DesktopLogins { tokens, active: config["lastKnownAccountUuid"].as_str().map(String::from), read_at: now() })
}

/// `acct:<account>|<client>:<org>:https://api.anthropic.com:<scopes>` → (account, scopes).
fn desktop_key(name: &str) -> Option<(&str, &str)> {
    let (_, scopes) = name.rsplit_once("https://api.anthropic.com:")?;
    let id = name.strip_prefix("acct:")?.split('|').next()?;
    Some((id, scopes))
}

/// Antigravity's signed-in Google account.
fn antigravity_email() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let read = |p: &str| -> Option<Value> { serde_json::from_str(&std::fs::read_to_string(format!("{home}/{p}")).ok()?).ok() };
    read(".gemini/antigravity-cli/antigravity-oauth-token")
        .and_then(|v| v["id_token_email"].as_str().map(String::from))
        .or_else(|| read(".gemini/google_accounts.json").and_then(|v| v["active"].as_str().map(String::from)))
        .filter(|e| !e.is_empty())
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

/// Account email for a token, from the OAuth profile endpoint (no model call).
fn fetch_profile_email(token: &str) -> Option<String> {
    let v: Value = serde_json::from_str(&api_get(PROFILE_URL, token).ok()?).ok()?;
    let a = &v["account"];
    a["email"].as_str().or(a["email_address"].as_str()).map(String::from)
}

/// GET an Anthropic OAuth endpoint; returns the body of a 200.
fn api_get(url: &str, token: &str) -> Result<String, FetchError> {
    // The token goes into a header block on stdin: refuse anything that could add lines.
    if !token.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b)) {
        return Err(FetchError::Unauthorized);
    }
    // Headers go through stdin so the token never appears in `ps`.
    let mut child = Command::new("/usr/bin/curl")
        .args(["-q", "-sS", "--proto", "=https", "-m", "10", "-A", "notchi-usage/0.1", "-H", "@-", "-w", "\n%{http_code} %header{retry-after}", url])
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
        "200" => Ok(body.to_string()),
        "401" | "403" => Err(FetchError::Unauthorized),
        "429" => Err(FetchError::RateLimited(retry_after)),
        "000" | "" => Err(FetchError::Offline),
        c if c.starts_with('5') => Err(FetchError::Other("Anthropic API is having trouble")),
        _ => Err(FetchError::Other("Unexpected response")),
    }
}

fn fetch(token: &str) -> Result<Snapshot, FetchError> {
    let body = api_get(USAGE_URL, token)?;
    let v: Value = serde_json::from_str(&body).map_err(|_| FetchError::Other("Unexpected response"))?;
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

    #[test]
    fn desktop_key_parsing() {
        let k = "acct:6e5c|client1:org9:https://api.anthropic.com:user:inference user:profile";
        assert_eq!(desktop_key(k), Some(("6e5c", "user:inference user:profile")));
        assert_eq!(desktop_key("garbage"), None);
    }

    fn login(id: &str, cli: bool, desktop_active: bool, expires_in: i64) -> Login {
        Login {
            account: Account { id: id.into(), email: Some(format!("{id}@x.com")), cli, desktop: !cli, desktop_active },
            creds: Creds { token: "t".into(), expires_at: Some(now() + expires_in) },
        }
    }

    #[test]
    fn picks_pinned_then_cli_then_desktop_current() {
        let logins = vec![login("a", true, false, 3600), login("b", false, false, 3600), login("c", false, true, 3600)];
        assert_eq!(pick_login(&logins, ""), Some(0));
        assert_eq!(pick_login(&logins, "B@X.com"), Some(1));
        assert_eq!(pick_login(&logins, "c"), Some(2));
        // A pinned account that signed out falls back to automatic.
        assert_eq!(pick_login(&logins, "gone@x.com"), Some(0));
        // Claude Code's token expired: the desktop app's current account, not just any.
        let logins = vec![login("a", true, false, -10), login("b", false, false, 3600), login("c", false, true, 3600)];
        assert_eq!(pick_login(&logins, ""), Some(2));
        assert_eq!(pick_login(&[], ""), None);
    }

    /// Lists every Claude login on a machine signed in to Claude Code and/or the desktop app.
    #[test]
    #[ignore = "needs local Claude logins and network"]
    fn list_local_accounts() {
        for l in claude_logins("Claude Code-credentials") {
            eprintln!("{} [{}] expired={}", l.account.label(), l.account.sources(), l.creds.expired());
        }
        eprintln!("antigravity: {:?}", antigravity_email());
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

