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
    RateLimited,
    Offline,
    Other(&'static str),
}

impl FetchError {
    fn message(&self) -> &'static str {
        match self {
            FetchError::NoLogin => "Not signed in to Claude Code",
            FetchError::Expired | FetchError::Unauthorized => "Token expired · open Claude Code to renew",
            FetchError::RateLimited => "Rate limited · backing off",
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

        backoff = if matches!(outcome, Err(FetchError::RateLimited)) { (backoff * 2).min(8) } else { 1 };
        let mut wait = Duration::from_secs(cfg.refresh_secs * backoff);
        if matches!(outcome, Err(FetchError::Offline)) {
            wait = wait.min(OFFLINE_RETRY);
        }
        if let Some(at) = publish(outcome) {
            // Re-check just after the next window resets so numbers never go stale.
            wait = wait.min(Duration::from_secs((at - now() + 10).max(5) as u64));
        }

        let started = Instant::now();
        loop {
            match wake.recv_timeout(wait.saturating_sub(started.elapsed())) {
                Ok(()) => {
                    while wake.try_recv().is_ok() {}
                    // Never let clicks hammer the endpoint or skip a 429 backoff.
                    if backoff == 1 {
                        wait = wait.min(MIN_GAP);
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// Stores the result and returns the earliest upcoming reset, if any.
fn publish(outcome: Result<Snapshot, FetchError>) -> Option<i64> {
    let mut guard = state();
    let prev = guard.take();
    let version = prev.as_ref().map_or(0, |p| p.version) + 1;
    let snap = match outcome {
        Ok(s) => Snapshot { version, ..s },
        // Keep the last good numbers; the UI shows their age next to the error.
        Err(e) => Snapshot { error: Some(e.message()), version, ..prev.unwrap_or_default() },
    };
    let t = now();
    let next_reset = snap.limits().filter_map(|l| l.resets_at).filter(|&at| at > t).min();
    *guard = Some(snap);
    next_reset
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
        .args(["-sS", "-m", "10", "-A", "notchi-usage/0.1", "-H", "@-", "-w", "\n%{http_code}", USAGE_URL])
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
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", text.as_ref()));
    match code.trim() {
        "200" => {}
        "401" | "403" => return Err(FetchError::Unauthorized),
        "429" => return Err(FetchError::RateLimited),
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
