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
# Tools needed: ImageMagick 7 (the `magick` command), awk, and ffmpeg for the
# video loops.
#
# The outputs are committed (the package installs them; nothing runs this
# script at build time) and are original work, licensed with the repository
# (MIT OR Apache-2.0). Their provenance is in SOURCES.toml next to them.
#
# It also makes the two "away image" stills for the away gallery, into
# assets/media/away/ (the picture shown in a call when the camera is off):
#   camera-off  calm indigo-graphite card, a faint centred camera-with-a-slash
#               glyph (basic shapes, no text)
#   away-soft   the same card without the glyph
# The away images are made after the backgrounds with their own fixed seeds,
# so they never change the backgrounds' bytes.
#
# Finally it makes two animated backgrounds (video loops) into the backgrounds
# directory (WebM, VP9 profile 0, yuv420p, 1920x1080, 30 fps, 10 s, no audio):
#   slow-aurora     soft colour bands drifting slowly on a dark sky
#   drifting-bokeh  soft out-of-focus light discs drifting slowly
# They loop seamlessly: every motion is a periodic function of the time with a
# period of exactly 10 s (the clip length), so the last frame flows into the
# first. Frames are computed small (640x360, 16-bit planes) with ffmpeg's geq
# filter, scaled up, given a little fixed dither before the step down to 8 bits
# (against banding) and encoded at CRF 28 (8 Mbit/s ceiling, a keyframe every
# 2 s). The loops need ffmpeg with libvpx-vp9 and are made last, so they never
# change the images' bytes. Each loop also gets its poster, NAME.poster.jpg
# beside it: the first frame scaled to 640x360 (JPEG). The daemon copies it into
# its cache as the poster the gallery shows, so a bundled loop has one without
# ffmpeg; it is a derived file of the loop, not a gallery item, and has no
# SOURCES.toml entry of its own.
#
# Usage: scripts/gen-backgrounds.sh [backgrounds-directory [away-directory]]
# (the away directory defaults to "away" next to the backgrounds directory)
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
out=${1:-$root/assets/media/backgrounds}
away_out=${2:-$(dirname -- "$out")/away}
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

# finish SRC NAME SEED ATTENUATE [DIR]: add fixed-seed noise before the 8-bit
# quantisation (dithering against banding) and write NAME.jpg into DIR
# (default: the backgrounds directory).
finish() {
  local dir=${5:-$out}
  magick "$1" -seed "$3" -attenuate "$4" +noise Gaussian \
    -colorspace sRGB -depth 8 -strip -interlace Plane \
    -sampling-factor 4:2:0 -quality 90 "$dir/$2.jpg"
  echo "wrote $dir/$2.jpg"
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

# --- away images -------------------------------------------------------------
# camera_glyph FILE: a greyscale mask (white glyph on black) of a camera with a
# slash through it, centred, about a sixth of the height tall. Outlined shapes
# with round joins; the slash knocks a gap out of the body so it reads clearly.
camera_glyph() {
  local cx=$((W / 2)) cy=$((H / 2)) cap='stroke-linecap round stroke-linejoin round'
  magick -size "$SIZE" xc:black -fill none -stroke white -strokewidth 24 \
    -draw "$cap roundrectangle $((cx - 250)),$((cy - 130)) $((cx + 250)),$((cy + 150)) 56,56" \
    -draw "$cap polyline $((cx - 110)),$((cy - 130)) $((cx - 80)),$((cy - 190)) $((cx + 80)),$((cy - 190)) $((cx + 110)),$((cy - 130))" \
    -draw "$cap circle $cx,$((cy + 10)) $((cx + 84)),$((cy + 10))" \
    -stroke black -strokewidth 76 \
    -draw "$cap line $((cx - 290)),$((cy - 200)) $((cx + 290)),$((cy + 230))" \
    -stroke white -strokewidth 24 \
    -draw "$cap line $((cx - 290)),$((cy - 200)) $((cx + 290)),$((cy + 230))" \
    -blur 0x0.8 "miff:$1"
}

away_images() {
  mkdir -p -- "$away_out"
  clut "$work/away.clut" '#101118' '#181a26' '#242840' '#303656'
  # Soft light a little above the middle, falling off toward the corners.
  field 'exp(-(((i/w-0.5)*1.3)^2 + ((j/h-0.42)*1.0)^2) / (2*0.42^2))' "$work/away.field"
  vignette "$work/away.vig" 70
  magick "$work/away.field" "$work/away.clut" -clut \
    "$work/away.vig" -compose multiply -composite \
    "miff:$work/away.card"
  finish "$work/away.card" away-soft 83 0.5 "$away_out"

  camera_glyph "$work/away.glyph"
  # Light grey glyph at about 35 % opacity.
  magick "$work/away.card" -size "$SIZE" xc:'#d6d9e2' \
    \( "$work/away.glyph" -alpha off -colorspace Gray -evaluate multiply 0.35 \) \
    -compose over -composite "miff:$work/away.marked"
  finish "$work/away.marked" camera-off 83 0.5 "$away_out"
}

dusk
studio
warm_bokeh
cool_bokeh
shapes
graphite_img
away_images

# --- video loops -------------------------------------------------------------
LW=640 # the loops are computed at 640x360 and scaled up to 1920x1080
LH=360

# loop_encode NAME MIX R-EXPR G-EXPR B-EXPR [POST]: render NAME.webm, 300
# frames at 30 fps. The geq expressions (0..65535 per plane, time T in seconds)
# are evaluated in 16-bit RGB planes; MIX is a colorchannelmixer argument
# string that remixes the planes (empty for none) and POST an optional filter
# run after it. Working in 16 bits and dithering the step down to 8 bits keeps
# the dark gradients free of banding.
loop_encode() {
  local name=$1 mix=$2 r=$3 g=$4 b=$5 post=${6:-} dither
  # Fixed dither at full size, before the step down to 8 bits: one pseudo-random
  # value per pixel and frame (a sine hash), +-1.5 steps of 8 bits.
  dither="385*(2*(sin(X*12.9898+Y*78.233+N*37.719)*43758.5453-floor(sin(X*12.9898+Y*78.233+N*37.719)*43758.5453))-1)"
  {
    echo "format=gbrp16le,"
    echo "geq=r='$r':g='$g':b='$b',"
    [[ -z $mix ]] || echo "colorchannelmixer=$mix,"
    [[ -z $post ]] || echo "$post,"
    echo "scale=1920:1080:flags=bicubic:out_color_matrix=bt709:out_range=tv,"
    echo "format=gbrp16le,"
    echo "geq=r='r(X,Y)+$dither':g='g(X,Y)+$dither':b='b(X,Y)+$dither',"
    echo "format=yuv420p"
  } >"$work/$name.filter"
  ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "nullsrc=s=${LW}x${LH}:r=30" \
    -/filter:v "$work/$name.filter" -frames:v 300 -an \
    -map_metadata -1 -fflags +bitexact -flags:v +bitexact \
    -c:v libvpx-vp9 -profile:v 0 -pix_fmt yuv420p -crf 28 -b:v 8M \
    -maxrate 8M -bufsize 16M -g 60 -keyint_min 60 -row-mt 1 -threads 8 \
    -deadline good -cpu-used 2 -auto-alt-ref 1 -lag-in-frames 16 \
    -colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv \
    "$out/$name.webm"
  echo "wrote $out/$name.webm"
  loop_poster "$name"
}

# loop_poster NAME: NAME.poster.jpg, the first frame of NAME.webm at 640x360.
loop_poster() {
  local name=$1
  ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f webm -i "$out/$name.webm" -map 0:v:0 -frames:v 1 -an \
    -vf "scale=640:360:flags=bicubic" -map_metadata -1 \
    -fflags +bitexact -flags:v +bitexact -q:v 3 -c:v mjpeg -f image2 \
    "$out/$name.poster.jpg"
  echo "wrote $out/$name.poster.jpg"
}

# --- slow-aurora -------------------------------------------------------------
# Two curtains of light, teal-green and violet, whose centre lines and
# brightness ripple with the phase p = 2*pi*T/10 (whole multiples of p only, so
# the loop closes). Only slow, wide variations along the width: no fine
# structure, so the bands stay smooth. A soft mask keeps the middle calm.
aurora() {
  local pre
  # Variables: 0 and 1 the position, 2 the phase, 3 the calm mask, 4 and 5 the
  # two curtain intensities.
  pre="st(0,X/W);st(1,Y/H);st(2,2*PI*T/10);"
  pre+="st(3,1-0.78*exp(-(pow((ld(0)-0.5)/0.24,2)+pow((ld(1)-0.55)/0.42,2))));"
  pre+="st(4,exp(-pow((ld(1)-0.30-0.07*sin(2*PI*1.2*ld(0)+ld(2))-0.035*sin(2*PI*2.9*ld(0)-2*ld(2)+1))/0.12,2))"
  pre+="*(0.7+0.3*sin(2*PI*1.3*ld(0)+ld(2)+2)));"
  pre+="st(5,exp(-pow((ld(1)-0.56-0.08*sin(2*PI*0.9*ld(0)-ld(2)+2)-0.03*sin(2*PI*2.1*ld(0)+2*ld(2)))/0.15,2))"
  pre+="*(0.7+0.3*sin(2*PI*1.1*ld(0)-ld(2)+0.5)));"
  pre+="st(4,ld(4)*ld(3));st(5,ld(5)*ld(3));"
  # A dark navy base plus each curtain's colour, per plane.
  loop_encode slow-aurora "" \
    "${pre}257*clip(5+4*ld(1)+ld(4)*32+ld(5)*78,0,255)" \
    "${pre}257*clip(10+6*ld(1)+ld(4)*135+ld(5)*52,0,255)" \
    "${pre}257*clip(22+10*ld(1)+ld(4)*100+ld(5)*135,0,255)"
}

# --- drifting-bokeh ----------------------------------------------------------
# Thirty soft discs on a dark base. Each drifts on a small closed path (whole
# multiples of the phase p = 2*pi*T/10) and breathes slightly; the discs keep
# mostly out of the middle third, and a calm mask dims what drifts into it.
# To keep the render fast each disc colour (amber, dusty rose, soft blue) is
# one geq plane (a third of the discs per plane); colorchannelmixer then turns
# the three planes into RGB and a second geq adds the dark base glow.
bokeh_loop() {
  local terms t0 t1 t2 pre
  terms=$(awk -v w="$LW" -v h="$LH" '
    BEGIN {
      srand(307)
      for (k = 0; k < 30; k++) {
        x = rand() * w; y = rand() * h
        if (x > w * 0.34 && x < w * 0.66 && rand() < 0.8) { k--; continue }
        rad = 14 + 40 * rand() * rand()
        a = 0.10 + 0.20 * rand()
        c = int(rand() * 3)
        ax = 6 + 16 * rand(); ay = 6 + 16 * rand()
        kx = 1 + int(rand() * 2); ky = 1 + int(rand() * 2)
        fx = rand() * 6.2832; fy = rand() * 6.2832; fb = rand() * 6.2832
        # The centre drifts on a closed path; the edge is about 2.5 px soft.
        d = sprintf("clip((%.1f-hypot(X-(%.1f+%.1f*sin(ld(2)*%d+%.2f)),Y-(%.1f+%.1f*sin(ld(2)*%d+%.2f))))/2.5+0.5,0,1)*%.3f*(1+0.18*sin(ld(2)+%.2f))",
          rad, x, ax, kx, fx, y, ay, ky, fy, a, fb)
        t[c] = t[c] (t[c] == "" ? "" : "+") d
      }
      print t[0]; print t[1]; print t[2]
    }')
  { IFS= read -r t0; IFS= read -r t1; IFS= read -r t2; } <<<"$terms"
  # Variables: the position (0, 1), the phase (2) and the calm mask (3).
  pre="st(0,X/W);st(1,Y/H);st(2,2*PI*T/10);"
  pre+="st(3,1-0.55*exp(-(pow((ld(0)-0.5)/0.22,2)+pow((ld(1)-0.55)/0.45,2))));"
  # The dark radial glow of the base, added to the mixed planes (the mixer's
  # alpha path garbles the last row of each thread slice, so it is not used).
  glow="exp(-(pow((X/W-0.5)*1.3,2)+pow(Y/H-0.45,2))/(2*0.45*0.45))"
  # Planes: g amber discs, b rose discs, r blue discs. The mixer gives the
  # output red, green and blue from them.
  loop_encode drifting-bokeh \
    "rr=0.45:rg=1.00:rb=0.95:gr=0.72:gg=0.72:gb=0.52:br=1.00:bg=0.42:bb=0.64" \
    "${pre}257*clip(255*ld(3)*($t2),0,255)" \
    "${pre}257*clip(255*ld(3)*($t0),0,255)" \
    "${pre}257*clip(255*ld(3)*($t1),0,255)" \
    "geq=r='r(X,Y)+257*(8+14*$glow)':g='g(X,Y)+257*(9+14*$glow)':b='b(X,Y)+257*(16+20*$glow)'"
}

command -v ffmpeg >/dev/null || {
  echo "gen-backgrounds: ffmpeg is required for the video loops" >&2
  exit 1
}
aurora
bokeh_loop
