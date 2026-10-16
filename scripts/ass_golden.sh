#!/usr/bin/env bash
# Regenerate the libass reference frames for the ASS golden tests
# (player/src/renderers/subtitle/ass/golden.rs).
#
# Needs an ffmpeg built with libass (the `subtitles` filter). Frames are
# rendered over opaque mid grey (so black borders and shadows show) in RGB (not the default yuv420p, whose chroma
# subsampling would blur the colours), at exact millisecond timestamps, with
# only the fonts in player/assets/fonts made available.
#
#   scripts/ass_golden.sh            # all scripts in player/tests/ass_golden
#   FFMPEG=/path/to/ffmpeg scripts/ass_golden.sh
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dir="$root/player/tests/ass_golden"
ffmpeg="${FFMPEG:-ffmpeg}"
w=960
h=540

"$ffmpeg" -hide_banner -filters 2>/dev/null | grep -q " subtitles " || {
    echo "ffmpeg has no subtitles filter (libass)" >&2
    exit 1
}
"$ffmpeg" -hide_banner -version | head -1

mkdir -p "$dir/ref"
cd "$dir"
# Fonts next to the script so the filter's fontsdir is a plain relative path.
rm -rf fonts && mkdir fonts
cp "$root/player/assets/fonts/"*.ttf fonts/

while read -r name times; do
    [ -z "$name" ] && continue
    case "$name" in \#*) continue ;; esac
    for ms in $times; do
        out="ref/${name}_${ms}.png"
        "$ffmpeg" -hide_banner -loglevel error -y \
            -f lavfi -i "color=c=0x808080:s=${w}x${h}:r=1:d=1" \
            -vf "format=rgb24,settb=1/1000,setpts=PTS+${ms},subtitles=${name}.ass:fontsdir=fonts" \
            -frames:v 1 -update 1 "$out"
        echo "$out"
    done
done < frames.txt
rm -rf fonts
