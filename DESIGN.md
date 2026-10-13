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
| `TEXT_TERTIARY` | Cloud @ 60% | overlines, unit tags |
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
| `MICRO` / `MICRO_LABEL` | ≤ 10.5 / 9, scaled to notch height | collapsed readout |

## Layout

- Collapsed: `notch width + 2 × (WING 50 + FLARE 6)` wide, exactly the notch height. Left ear = Clawd, right ear = `5h` / `7d` stacked.
- Expanded: `400` wide, `PAD 22` sides, header `46`, overline `18`, rows `26`, bottom `14`, radius `20`.
- Row columns: name · bar (from `+48`) · % (right edge at `right − 150`) · reset (right-aligned).

## Clawd

Drawn from the official Claude Code icon path (24×24 viewBox) as nine rectangles. Moods by session remaining: `> 50%` happy, `> 30%` busy, `> 10%` sweating, `≤ 10%` dizzy (squint + sweat), no data = asleep (closed eyes + "z"). Blinks every ~4 s.
