# Design system

Single source of truth: `src/theme.rs`. The view never uses a raw colour or size.

## Principles

1. **Melt into the notch.** The surface is pure `#000000`, the top edge is square with small concave flares, so the panel reads as part of the hardware.
2. **Show what's left.** Every number and bar is *remaining* budget. Bars shrink as you use Claude, so bar length always matches the number next to it.
3. **One brand accent.** Claude clay is the brand colour (Clawd); numbers and bars use the four status bands.
4. **Sharp, quiet type.** SF Pro at regular/medium weights, tabular digits for every number.

## Colour

| Token | Value | Use |
|---|---|---|
| `SURFACE` | `#000000` | panel background |
| `TEXT_PRIMARY` | Ivory `#FAF9F5` | title |
| `TEXT_SECONDARY` | Cloud `#B0AEA5` | labels, captions |
| `TEXT_TERTIARY` | Cloud @ 75% | overlines, unit tags, stale numbers |
| `TRACK` | Ivory @ 12% | empty bar track |
| `CLAWD` | Claude clay `#D97757` | mascot |
| `SWEAT` | Dusty blue `#6A9BCC` | Clawd's sweat drop |

Status colour, by **remaining** % (four vivid bands, kept slightly warm to sit next to Claude clay):

| Remaining | Colour |
|---|---|
| 90–100% | Blue `#4EA8F5` |
| 50–90% | Green `#6FCB63` |
| 20–50% | Yellow `#F7C33C` |
| 0–20% | Red `#F2573F` |

Clawd always stays Claude clay `#D97757`; status colour never touches the mascot.

## Type

| Token | Size / weight | Use |
|---|---|---|
| `TITLE` | 14 semibold | mood line |
| `LABEL` | 11 medium | row names |
| `NUMERAL` | 12 medium, tabular | % left |
| `CAPTION` | 10.5 regular, tabular | reset time, status line |
| `OVERLINE` | 9.5 medium caps | column headers |
| `MICRO_HERO` | ≤ 12.5 medium, tabular | collapsed session % (largest) |
| `AXIS` | 9 regular, tabular | graph hour labels |
| `MICRO_LABEL` | ≤ 9.5 regular | collapsed reset countdown |

Collapsed pieces share one baseline.

## Layout

- Collapsed: exactly the notch height, **everything left of the notch** (the blob's right end stops at the middle of the camera cutout, so its corner and flare are hidden by the hardware and only the notch's own curve shows). Order: Clawd standing on the **weekly bar** (7 day-segments `7 × 7`, gap `1.5`, height `3`; lit length = weekly budget left, draining right → left, lit colour = status band) · gap `8` · session % · session reset (`4h42`, tertiary). Outer inset `9`, inner `5`, reset gap `2.5`. Nothing can be drawn over the camera cutout itself.
- Expanded: `400` wide, `PAD 22` sides, header `46`, overline `18`, rows `26`, bottom `14`, radius `20`.
- Row columns: name · bar (from `+48`) · % (right edge at `right − 150`) · reset (right-aligned).

## Interaction

- Hover-in delay 0.12 s, hover-out delay 0.12 s (cancelled if the pointer returns).
- Dynamic-Island morph: the black blob is a `CAShapeLayer` that springs between the notch-sized and panel-sized outline (open: stiffness 300 / damping 27, a hint of bounce; close: 650 / 51, critically damped — lands in ~0.26 s, then the collapsed content fades in over 0.1 s). Content fades in 0.1 s after the blob starts opening and fades out first when closing. Mid-flight reversals start from the on-screen shape. Reduce Motion → instant swap + crossfade.
- No shadow, ever — like the hardware notch and Dynamic Island, it's pure black.
- Collapsed corners copy the hardware notch: bottom radius `14` (capped at 0.45 × notch height), top flare `6`. The right end hides inside the cutout, so the left end is the only drawn corner and it mirrors the hardware's right side. Expanded radius 22.
- Click = refresh (shows "Refreshing…"), right/ctrl-click = menu (Refresh Now, Open Usage Page, Quit).
- Stale data (error or > 10 min) renders numbers in `TEXT_TERTIARY`.

## Session graph

Expanded panel, under the rows: "THIS SESSION" hairline graph of **session % left** across the current 5 h window (left edge = reset − 5 h, right edge = session reset). Session line `1 pt` in its status colour with a `3.5 pt` dot at the latest point; even-pace diagonal (100% → 0%) and 50/100% gridlines at `0.5 pt`, very faint. The 0% line is the x-axis (Ivory @ 30%) with ticks and `HH:mm` labels (9 pt tabular, tertiary) on **local** hour boundaries — computed from the system time zone's offset, so :30/:45 zones tick on their own hours — and the zone abbreviation (e.g. `KST`) at the top right. A faint vertical line marks *now*. Height `56` + `16` for labels. Gaps > 30 min (app not running) aren't joined. The API has no history, so samples are recorded from our own polls (on change, or every 5 min) and kept 6 h in the cache file.

## Clawd

In the collapsed notch Clawd stands on the weekly bar (sprite ≤ 22 pt, bar under his feet, the pair centred vertically). He only reacts when a **whole segment** goes out (7 segments = 7 days of budget); partial drains within a segment don't move him.

| Weekly left | Band | Behaviour |
|---|---|---|
| ≥ 50% | blue / green | Walks to the middle of the segments still lit and stands calmly |
| 20–50% | yellow | Walks to the middle, then trembles there (±1.6°, ±0.5 pt, 13 Hz for 0.9 s every tick) with a sweat drop |
| < 20% | red | Walks to the very edge of what's left and **teeters**: leaning over the drop by `9° + 9° × severity` with a `± 5–11°` wobble (period `1.1 → 0.65 s`), wide eyes, open mouth, two sweat drops, flapping arms. Severity = `1 − left/20` (min 0.3) |
| 0% | — | **Falls**: drops out of sight (0.25 s), a beat later pops back up so only his head peeks above the bottom edge |

Walking: 10 pt/s (0.4–2.5 s, eased), alternating leg pairs every 0.14 s, 0.4 pt bob, eyes toward the direction of travel. Clawd is clipped to the black blob so he never draws outside it. The redraw timer (12 fps, Clawd's area only) runs only while he moves — walking, tremble bursts, teetering, falling — never when calm. Reduce Motion: no walk/shiver/wobble; static lean in red, head already peeking at 0%. `--demo` sweeps the week 100 → 0% every 40 s. Band floors live in `theme.rs` (`BAND_GREEN`, `BAND_YELLOW`) and are shared with the status colours.


Drawn from the official Claude Code icon path (24×24 viewBox) as nine rectangles. Moods by the tightest remaining limit: `> 50%` happy, `> 30%` busy, `> 10%` sweating, `≤ 10%` dizzy (squint + sweat), no data = asleep (closed eyes + "z"). Blinks at a random 2.5–6 s interval. Rect edges are snapped to device pixels so the pixel art stays crisp.
