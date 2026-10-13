# Notchi Usage

A tiny, cute Claude usage meter that lives in your MacBook notch. Native Rust + AppKit/Core Animation (objc2), no webview, ~440 KB binary, ~0% CPU at idle. Hover and it springs open like the Dynamic Island. Requires macOS 14+.

## What it shows

- **Collapsed** — all to the left of the notch: Clawd standing on a 7-day weekly bar that drains as you use your weekly budget (when it runs short, he panics and teeters on the edge), then what's **left** of your session and when it resets. Try `--preview-week=15` to see the teeter.
- **Hover** — expands to show every limit (Session, Weekly, plus Opus/Sonnet weekly when your plan has them) with a remaining-bar, percentage left, and time until reset.
- **Click** to refresh now. **Right-click** for Refresh / Quit.
- Clawd's mood follows your session budget: happy → busy → sweating → dizzy. He blinks now and then.

## Data & token cost

Notchi reads the OAuth token that Claude Code stores in the macOS Keychain (`Claude Code-credentials`) and calls `https://api.anthropic.com/api/oauth/usage`. That endpoint is not a model call, so it costs **zero tokens**. The token is only read, never refreshed, so your Claude Code session is untouched. The first launch shows a Keychain prompt — choose "Always Allow". If the token expires, just open Claude Code once and it refreshes. The token is passed to curl via stdin, so it never shows up in `ps`.

## Build / install

```sh
cargo build --release && ./target/release/notchi-usage   # run directly
./bundle.sh --install                                    # /Applications/NotchiUsage.app
```

Launch at login: System Settings → General → Login Items → add `NotchiUsage.app`.

## Config (optional)

`~/.config/notchi-usage/config`

```
refresh_secs = 120                           # poll interval, min 30; backs off automatically on 429
keychain_service = Claude Code-credentials   # point at another account/profile's Keychain item
```

Multiple accounts: by default the account behind the default Keychain item is shown. Recent Claude Code versions store a separate item per `CLAUDE_CONFIG_DIR` (named like `Claude Code-credentials-<hash>`); find yours with `security dump-keychain | grep "Claude Code-credentials"` and set `keychain_service` to show that profile instead.

## Behaviour notes

- Hover must linger ~0.12 s before the panel opens, so sweeping toward menu-bar items doesn't trigger it. It never takes keyboard focus.
- Clicks are coalesced (at most one fetch per 15 s) and never bypass a 429 backoff. On 429 it waits at least 5 min (or the server's `Retry-After`, if longer) and doubles on repeats.
- The last numbers and the next allowed poll time are cached in `~/Library/Caches/notchi-usage/state.json`, so restarting shows data instantly and never polls early or re-trips a rate limit.
- Each poll logs one line to stderr (status only, never the token).
- An expired token is detected from `expiresAt` locally, so no request is sent until Claude Code renews it.
- Numbers dim when they're stale (error or > 10 min old) and the status line shows their age. When a window's reset time passes it shows as full, and a fetch is scheduled right after each reset. After wake from sleep it refreshes within a few seconds; offline retries every 15 s.
- At 0% left, the collapsed notch shows the countdown to reset instead.
- Respects Reduce Motion; VoiceOver reads a full summary and "press" refreshes.

## Design

See [DESIGN.md](DESIGN.md). All tokens live in `src/theme.rs`.
