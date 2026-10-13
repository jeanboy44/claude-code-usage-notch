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
| `MICRO_SECONDARY` | ≤ 10.5 regular, tabular | collapsed weekly % in badge |
| `MICRO_LABEL` | ≤ 9.5 regular | collapsed reset countdown |

Collapsed pieces share one baseline.

## Layout

- Collapsed: exactly the notch height, centred on the camera cutout; both ears share the wider ear's width so the blob is symmetric. Left = Clawd · weekly % in a `TRACK`-filled badge (weekly reset lives in the hover panel); right = session % · session reset (`4h42`, tertiary). Content is anchored to the outer edges. Outer inset `9`, inner `5`, reset gap `2.5`, badge gap `4`, badge padding `3.5 × 2.5`. Nothing can be drawn over the camera cutout itself.
- Expanded: `400` wide, `PAD 22` sides, header `46`, overline `18`, rows `26`, bottom `14`, radius `20`.
- Row columns: name · bar (from `+48`) · % (right edge at `right − 150`) · reset (right-aligned).

## Interaction

- Hover-in delay 0.12 s, hover-out delay 0.12 s (cancelled if the pointer returns).
- Dynamic-Island morph: the black blob is a `CAShapeLayer` that springs between the notch-sized and panel-sized outline (open: stiffness 300 / damping 27, a hint of bounce; close: 420 / 37, near-critical). Content fades in 0.1 s after the blob starts opening and fades out first when closing. Mid-flight reversals start from the on-screen shape. Reduce Motion → instant swap + crossfade.
- No shadow, ever — like the hardware notch and Dynamic Island, it's pure black.
- Collapsed bottom radius ≈ 0.28 × notch height (matches the hardware cutout); expanded radius 22.
- Click = refresh (shows "Refreshing…"), right/ctrl-click = menu (Refresh Now, Open Usage Page, Quit).
- Stale data (error or > 10 min) renders numbers in `TEXT_TERTIARY`.

## Clawd

Drawn from the official Claude Code icon path (24×24 viewBox) as nine rectangles. Moods by the tightest remaining limit: `> 50%` happy, `> 30%` busy, `> 10%` sweating, `≤ 10%` dizzy (squint + sweat), no data = asleep (closed eyes + "z"). Blinks at a random 2.5–6 s interval. Rect edges are snapped to device pixels so the pixel art stays crisp.
