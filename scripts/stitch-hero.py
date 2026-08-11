#!/usr/bin/env python3
"""Stitch the landing-page hero video from UI screenshots.

Input: a frames/ directory of 2880x1620 PNGs (bdg screenshots at 1440x810 @2x).
Output: hero.mp4 (1080p, 30fps, crossfaded cuts, faststart) + poster.jpg.

Usage: python3 scripts/stitch-hero.py <frames-dir> <out-dir>
Frame list and durations are the recipe from apps/landing/CONTEXT.md — edit here.
"""
import subprocess
import sys

FRAMES = [  # (filename, seconds on screen)
    ("01-login.png", 1.8),
    ("02-passcode.png", 1.4),
    ("03-stream.png", 2.4),
    ("04-typing.png", 1.8),
    ("05-added.png", 2.6),
    ("06-lightbox.png", 2.2),
    ("07-sync.png", 2.4),
]
FADE = 0.5
POSTER_FRAME = "05-added.png"


def main(frames_dir: str, out_dir: str) -> None:
    inputs, filters = [], []
    for i, (f, d) in enumerate(FRAMES):
        inputs += ["-loop", "1", "-t", str(d + FADE), "-i", f"{frames_dir}/{f}"]
        filters.append(f"[{i}:v]scale=1920:1080:flags=lanczos,setsar=1,format=yuv420p[v{i}]")
    prev, offset = "v0", 0.0
    for i in range(1, len(FRAMES)):
        offset += FRAMES[i - 1][1]
        filters.append(f"[{prev}][v{i}]xfade=transition=fade:duration={FADE}:offset={offset}[x{i}]")
        prev = f"x{i}"
    subprocess.run(
        ["ffmpeg", "-y", "-loglevel", "error", *inputs,
         "-filter_complex", ";".join(filters), "-map", f"[{prev}]",
         "-c:v", "libx264", "-preset", "slow", "-crf", "20", "-r", "30",
         "-movflags", "+faststart", f"{out_dir}/hero.mp4"],
        check=True,
    )
    subprocess.run(
        ["ffmpeg", "-y", "-loglevel", "error", "-i", f"{frames_dir}/{POSTER_FRAME}",
         "-vf", "scale=1920:1080:flags=lanczos", "-q:v", "3", f"{out_dir}/poster.jpg"],
        check=True,
    )
    print(f"wrote {out_dir}/hero.mp4 and {out_dir}/poster.jpg")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2])
