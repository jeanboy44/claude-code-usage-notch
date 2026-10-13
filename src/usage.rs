//! Background worker: reads the Claude Code OAuth token from the Keychain and
//! polls the (token-free) usage endpoint. No LLM calls, no tokens consumed.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

#[derive(Clone, Copy, Default)]
pub struct Limit {
    /// Percent used (0..100).
    pub used: f64,
    /// Unix seconds.
    pub resets_at: Option<i64>,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub five_hour: Option<Limit>,
    pub seven_day: Option<Limit>,
    pub seven_day_opus: Option<Limit>,
    pub seven_day_sonnet: Option<Limit>,
    pub fetched_at: i64,
    pub error: Option<String>,
    pub version: u64,
}

pub static STATE: Mutex<Option<Snapshot>> = Mutex::new(None);

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
    Unauthorized,
    RateLimited,
    Other(String),
}

pub fn run(cfg: Config, wake: Receiver<()>) {
    let mut token: Option<String> = None;
    let mut backoff = 1u64;
    loop {
        if token.is_none() {
            token = read_token(&cfg.keychain_service);
        }
        let result = match &token {
            None => Err("Not signed in to Claude Code".to_string()),
            Some(t) => match fetch(t) {
                Err(FetchError::Unauthorized) => {
                    // Claude Code may have refreshed the token meanwhile; re-read once.
                    token = read_token(&cfg.keychain_service);
                    match token.as_deref().map(fetch) {
                        Some(Ok(s)) => Ok(s),
                        _ => Err("Token expired · open Claude Code once".to_string()),
                    }
                }
                Err(FetchError::RateLimited) => Err("Rate limited (429) · backing off".to_string()),
                Err(FetchError::Other(e)) => Err(e),
                Ok(s) => Ok(s),
            },
        };

        let rate_limited = matches!(&result, Err(e) if e.contains("429"));
        backoff = if rate_limited { (backoff * 2).min(8) } else { 1 };
        publish(result);

        match wake.recv_timeout(Duration::from_secs(cfg.refresh_secs * backoff)) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn publish(result: Result<Snapshot, String>) {
    let mut guard = STATE.lock().unwrap();
    let prev = guard.take();
    let version = prev.as_ref().map_or(0, |p| p.version) + 1;
    let snap = match result {
        Ok(mut s) => {
            s.version = version;
            s
        }
        // Keep last good numbers, just flag the error.
        Err(e) => Snapshot { error: Some(e), version, ..prev.unwrap_or_default() },
    };
    *guard = Some(snap);
}

fn read_token(service: &str) -> Option<String> {
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
    v["claudeAiOauth"]["accessToken"].as_str().map(str::to_string)
}

fn fetch(token: &str) -> Result<Snapshot, FetchError> {
    // Headers go through stdin so the token never appears in `ps`.
    let mut child = Command::new("/usr/bin/curl")
        .args(["-sS", "-m", "15", "-H", "@-", "-w", "\n%{http_code}", USAGE_URL])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| FetchError::Other(e.to_string()))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = write!(
            stdin,
            "Authorization: Bearer {token}\nanthropic-beta: oauth-2025-04-20\nAccept: application/json\n"
        );
    }
    let out = child.wait_with_output().map_err(|e| FetchError::Other(e.to_string()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", text.as_ref()));
    match code.trim() {
        "200" => {}
        "401" | "403" => return Err(FetchError::Unauthorized),
        "429" => return Err(FetchError::RateLimited),
        "000" | "" => return Err(FetchError::Other("Offline".into())),
        c => return Err(FetchError::Other(format!("HTTP {c}"))),
    }
    let v: Value = serde_json::from_str(body).map_err(|_| FetchError::Other("Unexpected response".into()))?;
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
            let hh: i64 = rest.get(1..3)?.parse().ok()?;
            let mm: i64 = rest.get(4..6).and_then(|x| x.parse().ok()).unwrap_or(0);
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
    }
}
