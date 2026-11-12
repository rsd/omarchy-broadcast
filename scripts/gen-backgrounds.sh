#!/usr/bin/env bash
# Regenerate the bundled procedural background images.
#
# Makes six 3840x2160 stills for the background gallery, into
# assets/media/backgrounds/ (JPEG, quality 90, 4:2:0, sRGB):
#   dusk-gradient  deep indigo to warm amber diagonal gradient, soft vignette
#   studio-blue    dark blue studio backdrop, radial light behind head height
#   warm-bokeh     out-of-focus warm light discs on dark brown
#   cool-bokeh     the same in teal and blue
#   soft-shapes    large blurred pastel shapes, like an out-of-focus office wall
#   graphite       neutral dark grey, gentle top-left light, fine noise
#
# Every image is built from gradients, fixed-seed pseudo-random shapes and a
# little fixed-seed noise (which also avoids banding), so reruns give the same
# files with the same ImageMagick version. They are calm and low-contrast in
# the centre, as they sit behind a person.
#
# Tools needed: ImageMagick 7 (the `magick` command) and awk.
#
# The outputs are committed (the package installs them; nothing runs this
# script at build time) and are original work, licensed with the repository
# (MIT OR Apache-2.0). Their provenance is in SOURCES.toml next to them.
#
# Usage: scripts/gen-backgrounds.sh [output-directory]
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
out=${1:-$root/assets/media/backgrounds}
W=3840
H=2160
SIZE=${W}x${H}

command -v magick >/dev/null || {
  echo "gen-backgrounds: ImageMagick 7 (magick) is required" >&2
  exit 1
}

work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
mkdir -p -- "$out"

# clut FILE COLOUR...: a 1024x1 colour ramp through the given colours.
clut() {
  local file=$1 args=() c
  shift
  for c in "$@"; do args+=("xc:$c"); done
  magick "${args[@]}" +append -filter Triangle -resize 1024x1! "miff:$file"
}

# field EXPR FILE: a smooth full-size greyscale field (values 0..1) from an
# ImageMagick -fx expression, computed small and scaled up.
field() {
  magick -size 480x270 xc: -fx "$1" -filter Mitchell -resize "${SIZE}!" "miff:$2"
}

# vignette FILE LEVEL: a multiply mask, white in the middle, LEVEL (0..100 %
# grey) at the corners.
vignette() {
  magick -size "$SIZE" "radial-gradient:white-gray($2%)" "miff:$1"
}

# finish SRC NAME SEED ATTENUATE: add fixed-seed noise before the 8-bit
# quantisation (dithering against banding) and write NAME.jpg.
finish() {
  magick "$1" -seed "$3" -attenuate "$4" +noise Gaussian \
    -colorspace sRGB -depth 8 -strip -interlace Plane \
    -sampling-factor 4:2:0 -quality 90 "$out/$2.jpg"
  echo "wrote $out/$2.jpg"
}

# discs SEED COUNT RMIN RMAX R G B ALPHAMAX FILE: a black layer of soft
# discs (colour R,G,B 0..255), as an out-of-focus light layer. Positions,
# sizes and strengths come from a seeded awk generator; discs avoid the
# middle third horizontally so the layer stays calm behind the speaker.
discs() {
  local seed=$1 count=$2 rmin=$3 rmax=$4 r=$5 g=$6 b=$7 amax=$8 file=$9
  local draw
  draw=$(awk -v seed="$seed" -v n="$count" -v rmin="$rmin" -v rmax="$rmax" \
    -v r="$r" -v g="$g" -v b="$b" -v amax="$amax" -v w="$W" -v h="$H" '
    BEGIN {
      srand(seed)
      for (k = 0; k < n; k++) {
        x = rand() * w
        y = rand() * h
        # Keep most discs out of the central column.
        if (x > w * 0.36 && x < w * 0.64 && rand() < 0.75) { k--; continue }
        rad = rmin + (rmax - rmin) * rand() * rand()
        a = amax * (0.35 + 0.65 * rand())
        # Pull the colour a little toward neutral on some discs.
        m = 0.75 + 0.25 * rand()
        sa = a * 1.6
        if (sa > 1) sa = 1
        printf "fill rgba(%d,%d,%d,%.3f) stroke rgba(%d,%d,%d,%.3f) stroke-width %d circle %.0f,%.0f %.0f,%.0f\n",
          r * m, g * m, b * m, a,
          r, g, b, sa, rad * 0.04 + 2,
          x, y, x + rad, y
      }
    }')
  local args=() line
  while IFS= read -r line; do args+=(-draw "$line"); done <<<"$draw"
  magick -size "$SIZE" xc:black "${args[@]}" -blur 0x14 "miff:$file"
}

# --- dusk-gradient -----------------------------------------------------------
dusk() {
  clut "$work/dusk.clut" '#140f3d' '#2d1d5c' '#6b3a78' '#c2606a' '#eaa352'
  field '1.15*(0.62*(i/w) + 0.38*(j/h)) - 0.1' "$work/dusk.field"
  vignette "$work/dusk.vig" 62
  magick "$work/dusk.field" "$work/dusk.clut" -clut \
    -colorspace sRGB "$work/dusk.vig" -compose multiply -composite \
    "miff:$work/dusk.miff"
  finish "$work/dusk.miff" dusk-gradient 11 0.35
}

# --- studio-blue -------------------------------------------------------------
studio() {
  clut "$work/studio.clut" '#050c1a' '#0b1c38' '#173a6a' '#2c5f9c'
  # A wide elliptical light centred at about head height (38 % from the top).
  field 'exp(-(((i/w-0.5)*1.25)^2 + ((j/h-0.38)*1.0)^2) / (2*0.36^2))' "$work/studio.field"
  magick "$work/studio.field" "$work/studio.clut" -clut "miff:$work/studio.miff"
  # Grain is a little stronger here than elsewhere: it reads as film grain.
  finish "$work/studio.miff" studio-blue 23 0.5
}

# --- bokeh -------------------------------------------------------------------
# bokeh NAME SEED CLUT-STOPS(3) R G B R2 G2 B2 : a dark radial base, a far
# layer of large faint discs and a near layer of smaller brighter ones.
bokeh() {
  local name=$1 seed=$2 dark=$3 mid=$4 r=$5 g=$6 b=$7 r2=$8 g2=$9 b2=${10}
  clut "$work/$name.clut" "$dark" "$mid"
  field 'exp(-(((i/w-0.5)*1.3)^2 + ((j/h-0.45)*1.0)^2) / (2*0.45^2))' "$work/$name.field"
  magick "$work/$name.field" "$work/$name.clut" -clut "miff:$work/$name.base"
  discs "$seed" 26 180 430 "$r" "$g" "$b" 0.22 "$work/$name.far"
  discs "$((seed + 1))" 40 70 230 "$r2" "$g2" "$b2" 0.30 "$work/$name.near"
  vignette "$work/$name.mask" 55
  magick "$work/$name.base" \
    \( "$work/$name.far" -blur 0x30 \) -compose screen -composite \
    \( "$work/$name.near" "$work/$name.mask" -compose multiply -composite \) \
    -compose screen -composite "miff:$work/$name.miff"
}

warm_bokeh() {
  bokeh warm 101 '#120905' '#3b2314' 255 165 70 255 205 120
  finish "$work/warm.miff" warm-bokeh 31 0.35
}

cool_bokeh() {
  bokeh cool 201 '#04111a' '#0f3040' 70 190 205 130 190 255
  finish "$work/cool.miff" cool-bokeh 41 0.35
}

# --- soft-shapes -------------------------------------------------------------
shapes() {
  # Pastel base, lighter toward the top.
  clut "$work/shapes.clut" '#aeb9c6' '#c9d0d8' '#dcdce0'
  field '1 - (j/h)*0.9' "$work/shapes.field"
  magick "$work/shapes.field" "$work/shapes.clut" -clut "miff:$work/shapes.base"
  # Large ellipses and tall bands in pastel colours (fixed layout).
  magick "$work/shapes.base" \
    -fill '#f0c8b4' -draw 'ellipse 700,520 900,640 0,360' \
    -fill '#b8d4c4' -draw 'ellipse 3200,640 760,520 0,360' \
    -fill '#c8c0e6' -draw 'ellipse 2000,1850 1100,520 0,360' \
    -fill '#aed0ea' -draw 'ellipse 3350,1750 620,700 0,360' \
    -fill '#eedcb4' -draw 'ellipse 420,1700 640,560 0,360' \
    -fill '#e6b8c8' -draw 'ellipse 1750,260 700,300 0,360' \
    -fill '#bcd8d8' -draw 'rectangle 2480,0 2820,1500' \
    -fill '#d8c4a8' -draw 'rectangle 1180,300 1420,2160' \
    -blur 0x110 \
    "miff:$work/shapes.shapes"
  # Calm the middle: blend toward the plain base behind the speaker.
  magick -size "$SIZE" 'radial-gradient:gray(50%)-black' "miff:$work/shapes.mask"
  magick "$work/shapes.shapes" "$work/shapes.base" "$work/shapes.mask" \
    -compose over -composite -blur 0x30 \
    -modulate 96,82 "miff:$work/shapes.miff"
  finish "$work/shapes.miff" soft-shapes 53 0.3
}

# --- graphite ----------------------------------------------------------------
graphite_img() {
  clut "$work/graphite.clut" '#141517' '#242528' '#3d3f43' '#55575c'
  # Gentle light from the top left.
  field 'exp(-(((i/w-0.12)*1.3)^2 + ((j/h-0.05)*1.0)^2) / (2*0.55^2))' "$work/graphite.field"
  magick "$work/graphite.field" "$work/graphite.clut" -clut "miff:$work/graphite.miff"
  finish "$work/graphite.miff" graphite 67 0.5
}

dusk
studio
warm_bokeh
cool_bokeh
shapes
graphite_img
