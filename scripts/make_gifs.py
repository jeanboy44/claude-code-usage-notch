"""Build README media from frames rendered by `notchi-usage --render-readme <dir>`.

    cargo build --release
    ./target/release/notchi-usage --render-readme /tmp/notchi-readme
    python3 scripts/make_gifs.py /tmp/notchi-readme docs
"""

import sys
from pathlib import Path

from PIL import Image

FPS = 20


def main(src: str, out: str) -> None:
    src_dir, out_dir = Path(src), Path(out)
    out_dir.mkdir(parents=True, exist_ok=True)
    frames = [Image.open(p).convert("RGB") for p in sorted((src_dir / "frames").glob("*.png"))]
    # One shared palette keeps colours stable across frames (no flicker) and the file small.
    palette = frames[len(frames) // 2].quantize(colors=96, method=Image.Quantize.MEDIANCUT)
    gif = [f.quantize(palette=palette, dither=Image.Dither.NONE) for f in frames]
    gif[0].save(
        out_dir / "notch-states.gif",
        save_all=True,
        append_images=gif[1:],
        duration=int(1000 / FPS),
        loop=0,
        optimize=True,
        disposal=1,
    )
    Image.open(src_dir / "expanded.png").save(out_dir / "notch-expanded.png", optimize=True)
    print(f"{len(frames)} frames -> {out_dir / 'notch-states.gif'}, {out_dir / 'notch-expanded.png'}")


if __name__ == "__main__":
    main(*sys.argv[1:3])
