//! Background worker: reads the Claude Code OAuth token from the Keychain and
//! polls the (token-free) usage endpoint. No LLM calls, no tokens consumed.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// Manual refreshes closer together than this are coalesced.
const MIN_GAP: Duration = Duration::from_secs(15);
const OFFLINE_RETRY: Duration = Duration::from_secs(15);
/// Never retry a 429 sooner than this, whatever Retry-After says.
const MIN_RATE_LIMIT_WAIT: u64 = 300;

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
    pub version: u64,
}

impl Snapshot {
    fn limits(&self) -> impl Iterator<Item = Limit> + '_ {
        [self.five_hour, self.seven_day, self.seven_day_opus, self.seven_day_sonnet].into_iter().flatten()
    }
}

static STATE: Mutex<Option<Snapshot>> = Mutex::new(None);

/// One poll's worth of % used, for the history graph. The API has no history,
/// so we build our own from successful polls.
#[derive(Clone, Copy)]
pub struct Sample {
    pub t: i64,
    /// Session (5 h) % used.
    pub session: f32,
}

static HISTORY: Mutex<Vec<Sample>> = Mutex::new(Vec::new());
/// Keep a bit more than one session window so the graph's left edge is covered.
const HISTORY_SPAN: i64 = 6 * 3600;
/// Unchanged values are only re-recorded this often.
const HISTORY_HEARTBEAT: i64 = 300;

pub fn history() -> Vec<Sample> {
    HISTORY.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn record(s: &Snapshot) {
    let Some(five) = s.five_hour else { return };
    let sample = Sample { t: s.fetched_at, session: five.used as f32 };
    let mut h = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
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

fn state() -> MutexGuard<'static, Option<Snapshot>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn snapshot() -> Option<Snapshot> {
    state().clone()
}

pub struct Config {
    pub refresh_secs: u64,
    pub keychain_service: String,
}

impl Config {
    /// `~/.config/notchi-usage/config` — plain `key = value` lines.
    pub fn load() -> Self {
        let mut c = Config { refresh_secs: 120, keychain_service: "Claude Code-credentials".into() };
        let Ok(home) = std::env::var("HOME") else { return c };
        let Ok(text) = std::fs::read_to_string(format!("{home}/.config/notchi-usage/config")) else { return c };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "refresh_secs" => c.refresh_secs = v.parse().unwrap_or(c.refresh_secs).max(30),
                "keychain_service" => c.keychain_service = v.to_string(),
                _ => {}
            }
        }
        c
    }
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

enum FetchError {
    NoLogin,
    Expired,
    Unauthorized,
    /// With the server's Retry-After (seconds), if it sent one.
    RateLimited(Option<u64>),
    Offline,
    Other(&'static str),
}

impl FetchError {
    fn message(&self) -> &'static str {
        match self {
            FetchError::NoLogin => "Not signed in to Claude Code",
            FetchError::Expired | FetchError::Unauthorized => "Token expired · open Claude Code to renew",
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
        let next_reset = cache.snapshot.and_then(|snap| publish(Ok(snap)));
        let mut wait = (cache.next_fetch_at - now()).max(0);
        if let Some(at) = next_reset.filter(|_| !cache.rate_limited) {
            wait = wait.min((at - now() + 10).max(5));
        }
        if !sleep(&wake, Duration::from_secs(wait as u64), !cache.rate_limited) {
            return;
        }
    }

    loop {
        // Only touch the Keychain when we have nothing usable.
        if creds.as_ref().is_none_or(Creds::expired) {
            creds = read_creds(&cfg.keychain_service);
        }
        let outcome = match &creds {
            None => Err(FetchError::NoLogin),
            Some(c) if c.expired() => Err(FetchError::Expired),
            Some(c) => match fetch(&c.token) {
                Err(FetchError::Unauthorized) => {
                    // Claude Code may have rotated the token meanwhile; re-read once.
                    creds = read_creds(&cfg.keychain_service);
                    match &creds {
                        Some(c) if !c.expired() => fetch(&c.token),
                        _ => Err(FetchError::Expired),
                    }
                }
                r => r,
            },
        };

        // One line per fetch on stderr (never the token) — handy when run from a terminal.
        match &outcome {
            Ok(snap) => eprintln!(
                "[{}] ok session={:?} weekly={:?}",
                now(),
                snap.five_hour.map(|l| l.used),
                snap.seven_day.map(|l| l.used)
            ),
            Err(e) => eprintln!("[{}] error: {}", now(), e.message()),
        }

        let retry_after = match &outcome {
            Err(FetchError::RateLimited(ra)) => Some(ra.unwrap_or(0).max(MIN_RATE_LIMIT_WAIT)),
            _ => None,
        };
        backoff = if retry_after.is_some() { (backoff * 2).min(8) } else { 1 };
        let mut wait = cfg.refresh_secs * backoff;
        if let Some(ra) = retry_after {
            wait = wait.max(ra);
        }
        if matches!(outcome, Err(FetchError::Offline)) {
            wait = wait.min(OFFLINE_RETRY.as_secs());
        }
        let next_reset = publish(outcome);
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
fn publish(outcome: Result<Snapshot, FetchError>) -> Option<i64> {
    let mut guard = state();
    let prev = guard.take();
    let version = prev.as_ref().map_or(0, |p| p.version) + 1;
    let snap = match outcome {
        Ok(s) => {
            record(&s);
            Snapshot { version, ..s }
        }
        // Keep the last good numbers; the UI shows their age next to the error.
        Err(e) => Snapshot { error: Some(e.message()), version, ..prev.unwrap_or_default() },
    };
    let t = now();
    let next_reset = snap.limits().filter_map(|l| l.resets_at).filter(|&at| at > t).min();
    *guard = Some(snap);
    next_reset
}

// ---------- on-disk cache: last good numbers + when we may poll next ----------

struct Cache {
    /// `None` if we never got numbers (e.g. rate limited from the very first poll).
    snapshot: Option<Snapshot>,
    next_fetch_at: i64,
    rate_limited: bool,
}

fn cache_path() -> Option<String> {
    Some(format!("{}/Library/Caches/notchi-usage/state.json", std::env::var("HOME").ok()?))
}

fn save_cache(next_fetch_at: i64, rate_limited: bool) {
    let Some(path) = cache_path() else { return };
    let snap = snapshot().unwrap_or_default();
    let limit = |l: Option<Limit>| l.map(|l| serde_json::json!([l.used, l.resets_at]));
    let json = serde_json::json!({
        "fetched_at": snap.fetched_at,
        "next_fetch_at": next_fetch_at,
        "rate_limited": rate_limited,
        "five_hour": limit(snap.five_hour),
        "seven_day": limit(snap.seven_day),
        "seven_day_opus": limit(snap.seven_day_opus),
        "seven_day_sonnet": limit(snap.seven_day_sonnet),
        "history": history().iter().map(|x| serde_json::json!([x.t, x.session])).collect::<Vec<_>>(),
    });
    if let Some(dir) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, json.to_string());
}

fn load_cache() -> Option<Cache> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(cache_path()?).ok()?).ok()?;
    if let Some(points) = v["history"].as_array() {
        let samples = points.iter().filter_map(|p| {
            let p = p.as_array()?;
            Some(Sample { t: p.first()?.as_i64()?, session: p.get(1)?.as_f64()? as f32 })
        });
        *HISTORY.lock().unwrap_or_else(|e| e.into_inner()) = samples.collect();
    }
    let limit = |k: &str| -> Option<Limit> {
        let a = v.get(k)?.as_array()?;
        Some(Limit { used: a.first()?.as_f64()?, resets_at: a.get(1).and_then(Value::as_i64) })
    };
    Some(Cache {
        snapshot: v["fetched_at"].as_i64().filter(|&t| t > 0).map(|fetched_at| Snapshot {
            five_hour: limit("five_hour"),
            seven_day: limit("seven_day"),
            seven_day_opus: limit("seven_day_opus"),
            seven_day_sonnet: limit("seven_day_sonnet"),
            fetched_at,
            error: None,
            version: 0,
        }),
        next_fetch_at: v["next_fetch_at"].as_i64().unwrap_or(0),
        rate_limited: v["rate_limited"].as_bool().unwrap_or(false),
    })
}

fn read_creds(service: &str) -> Option<Creds> {
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-w"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
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

fn fetch(token: &str) -> Result<Snapshot, FetchError> {
    // Headers go through stdin so the token never appears in `ps`.
    let mut child = Command::new("/usr/bin/curl")
        .args(["-sS", "-m", "10", "-A", "notchi-usage/0.1", "-H", "@-", "-w", "\n%{http_code} %header{retry-after}", USAGE_URL])
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
}
