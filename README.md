# Notchi Usage

A tiny, cute Claude usage meter that lives in your MacBook notch. Native Rust + AppKit (objc2), no webview, ~420 KB binary, ~0% CPU at idle.

## What it shows

- **Collapsed** — Clawd on the left of the notch; on the right, what's **left** of your session (5h) and weekly (7d) limits.
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

With several accounts, the one you last `/login`'d to is shown. If you separate profiles with `CLAUDE_CONFIG_DIR`, find the item name with `security dump-keychain | grep "Claude Code"` and set `keychain_service`.

## Design

See [DESIGN.md](DESIGN.md). All tokens live in `src/theme.rs`.
