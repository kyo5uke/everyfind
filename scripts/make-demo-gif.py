# Render docs/demo.gif from live command output.
#
# The commands are actually run and their stdout is used verbatim — nothing is fabricated.
# Only the *typing* is synthesized: each keystroke becomes a frame, and the full result
# appears on the very next frame, which is honest — the real round trip (~15 ms) is faster
# than one frame of any GIF. Rendering instead of screen-recording buys three things: no
# fumbled take, no personal paths beyond what the chosen queries return, and a byte-identical
# re-render whenever the output changes.
#
# Needs Pillow (`pip install pillow`) and a built `ef` on PATH or passed as argv[1].
#
#   python scripts/make-demo-gif.py [path\to\ef.exe]
#
# Pauses cost one frame each (GIFs carry per-frame durations), so the file stays small.

import subprocess
import sys
from PIL import Image, ImageDraw, ImageFont

EF = sys.argv[1] if len(sys.argv) > 1 else "ef"
OUT = "docs/demo.gif"

# One entry per scene: the command as the user would type it, its argv, and how long the
# result stays on screen. Order tells the story: scale first (millions of entries, live),
# then the headline (search is instant), then the surprise (du from the same index).
SCENES = [
    ("ef status", [EF, "status"], 3200),
    ("ef readme -n 10", [EF, "readme", "-n", "10"], 3400),
    ("ef du -n 8", [EF, "du", "-n", "8"], 4000),
]

COLS, ROWS = 74, 22
FONT_SIZE = 16
TYPE_MS = 45          # per keystroke
OPEN_MS = 700         # the empty prompt, before anything happens
CLOSE_MS = 1400       # the last look before the loop restarts
PAD = 14
TITLE_H = 30

BG = (12, 12, 12)
FG = (204, 204, 204)
PROMPT = (86, 182, 194)
CMD = (240, 240, 240)
TITLE_BG = (32, 32, 32)
TITLE_FG = (140, 140, 140)

font = ImageFont.truetype(r"C:\Windows\Fonts\CascadiaMono.ttf", FONT_SIZE)
CHAR_W = font.getlength("M")
LINE_H = int(FONT_SIZE * 1.4)
W = int(PAD * 2 + COLS * CHAR_W)
H = TITLE_H + PAD * 2 + ROWS * LINE_H


def run(argv):
    p = subprocess.run(argv, capture_output=True)
    text = (p.stdout + p.stderr).decode("utf-8", errors="replace")
    return [ln.rstrip()[:COLS] for ln in text.splitlines()]


def frame(lines, typed=None, cursor=True):
    """lines: committed (text, color) rows; typed: the partial command on the prompt row."""
    img = Image.new("RGB", (W, H), BG)
    d = ImageDraw.Draw(img)
    # A minimal Windows-Terminal-ish title bar, so it reads as a terminal at a glance.
    d.rectangle([0, 0, W, TITLE_H], fill=TITLE_BG)
    d.text((PAD, (TITLE_H - FONT_SIZE) // 2), "Windows PowerShell", font=font, fill=TITLE_FG)
    d.text((W - PAD - 3 * 22, (TITLE_H - FONT_SIZE) // 2), "—  □  ✕", font=font, fill=TITLE_FG)

    visible = lines[-(ROWS - 1):] if typed is not None else lines[-ROWS:]
    y = TITLE_H + PAD
    for text, color in visible:
        if text.startswith("PS> "):
            d.text((PAD, y), "PS> ", font=font, fill=PROMPT)
            d.text((PAD + 4 * CHAR_W, y), text[4:], font=font, fill=color)
        else:
            d.text((PAD, y), text, font=font, fill=color)
        y += LINE_H
    if typed is not None:
        d.text((PAD, y), "PS> ", font=font, fill=PROMPT)
        d.text((PAD + 4 * CHAR_W, y), typed, font=font, fill=CMD)
        if cursor:
            x = PAD + (4 + len(typed)) * CHAR_W
            d.rectangle([x, y + 2, x + CHAR_W - 2, y + FONT_SIZE + 2], fill=FG)
    return img.convert("P", palette=Image.ADAPTIVE, colors=64)


frames, durations = [], []
buf = []  # committed rows: (text, color)

frames.append(frame(buf, typed=""))
durations.append(OPEN_MS)

for shown, argv, hold in SCENES:
    for i in range(1, len(shown) + 1):
        frames.append(frame(buf, typed=shown[:i]))
        durations.append(TYPE_MS)
    out = run(argv)
    buf.append((f"PS> {shown}", CMD))
    buf.extend((ln, FG) for ln in out)
    buf.append(("", FG))
    frames.append(frame(buf, typed=""))
    durations.append(hold)

durations[-1] += CLOSE_MS

frames[0].save(
    OUT,
    save_all=True,
    append_images=frames[1:],
    duration=durations,
    loop=0,
    optimize=True,
)
import os

print(f"{OUT}: {len(frames)} frames, {os.path.getsize(OUT):,} bytes, "
      f"{sum(durations)/1000:.1f}s per loop, {W}x{H}px")
