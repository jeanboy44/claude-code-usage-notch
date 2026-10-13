# Design system

Single source of truth: `src/theme.rs`. The view never uses a raw colour or size.

## Principles

1. **Melt into the notch.** The surface is pure `#000000`, the top edge is square with small concave flares, so the panel reads as part of the hardware.
2. **Show what's left.** Every number and bar is *remaining* budget. Bars shrink as you use Claude, so bar length always matches the number next to it.
3. **One accent.** Claude orange is the only accent; colour shifts only to warn.
4. **Sharp, quiet type.** SF Pro at regular/medium weights, tabular digits for every number.

## Colour

| Token | Value | Use |
|---|---|---|
| `SURFACE` | `#000000` | panel background |
| `TEXT_PRIMARY` | Ivory `#FAF9F5` | title |
| `TEXT_SECONDARY` | Cloud `#B0AEA5` | labels, captions |
| `TEXT_TERTIARY` | Cloud @ 75% | overlines, unit tags, stale numbers |
| `TRACK` | Ivory @ 12% | empty bar track |
| `CLAWD` | Claude orange `#D97757` | mascot, healthy budget |
| `SWEAT` | Sky `#6A9BCC` | Clawd's sweat drop |

Status colour, by **remaining** %: `> 30%` Claude orange · `10–30%` Amber `#E3A857` · `≤ 10%` Brick `#D4524A`.

## Type

| Token | Size / weight | Use |
|---|---|---|
| `TITLE` | 14 semibold | mood line |
| `LABEL` | 11 medium | row names |
| `NUMERAL` | 12 medium, tabular | % left |
| `CAPTION` | 10.5 regular, tabular | reset time, status line |
| `OVERLINE` | 9.5 medium caps | column headers |
| `MICRO` / `MICRO_LABEL` | ≤ 11 / 9.5, scaled to notch height | collapsed readout |

## Layout

- Collapsed: `notch width + 2 × (WING 50 + FLARE 6)` wide, exactly the notch height. Left ear = Clawd, right ear = `5h` / `7d` stacked.
- Expanded: `400` wide, `PAD 22` sides, header `46`, overline `18`, rows `26`, bottom `14`, radius `20`.
- Row columns: name · bar (from `+48`) · % (right edge at `right − 150`) · reset (right-aligned).

## Interaction

- Hover-in delay 0.22 s, hover-out delay 0.3 s (cancelled if the pointer returns).
- During the resize animation only the black shape is drawn; content appears at the final size. Reduce Motion → no animation, slower blinks.
- Soft window shadow only while expanded, so it separates from dark windows.
- Click = refresh (shows "Refreshing…"), right/ctrl-click = menu (Refresh Now, Open Usage Page, Quit).
- Stale data (error or > 10 min) renders numbers in `TEXT_TERTIARY`.

## Clawd

Drawn from the official Claude Code icon path (24×24 viewBox) as nine rectangles. Moods by the tightest remaining limit: `> 50%` happy, `> 30%` busy, `> 10%` sweating, `≤ 10%` dizzy (squint + sweat), no data = asleep (closed eyes + "z"). Blinks at a random 2.5–6 s interval. Rect edges are snapped to device pixels so the pixel art stays crisp.
