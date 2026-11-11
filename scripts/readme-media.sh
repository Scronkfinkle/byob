#!/usr/bin/env bash
# Regenerate the README's screenshots and clips in docs/media, headlessly (Xvfb, see
# scripts/headless.sh), against a real model.
#
#   scripts/readme-media.sh [shot...]     (default: all; names below)
#
# Every shot uses a throwaway profile and workspace under /tmp/byob-media/<shot>, wiped first.
# Servo helper (HTML panes/apps inline): as in demos/run.sh; BYOB_ARTIFACT_HOST wins.
# Endpoint: BYOB_DEMO_URL (default http://silicon:9931/v1) / BYOB_DEMO_MODEL (default local).
# Clips: the run is recorded, idle stretches (the model thinking) are cut, and the result is
# written as a GIF.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
if ! command -v Xvfb >/dev/null || ! command -v cargo >/dev/null; then exec nix develop -c "$0" "$@"; fi
source demos/lib.sh
out="$PWD/docs/media"
root=/tmp/byob-media
mkdir -p "$out" "$root"
export HEADLESS_TIMEOUT="${HEADLESS_TIMEOUT:-900}" BYOB_SETTLE_SECS="${BYOB_SETTLE_SECS:-600}"
url="${BYOB_DEMO_URL:-http://silicon:9931/v1}" model="${BYOB_DEMO_MODEL:-local}"
ffbin="$(nix build --no-link --print-out-paths --inputs-from . nixpkgs#ffmpeg-full.bin)/bin"
ff() { env -u LD_LIBRARY_PATH "$ffbin/ffmpeg" -loglevel error -y "$@"; }

ALL="hero self-plugin html-app sheets connect phone"
W=1440 H=810

# fresh NAME: a clean profile + workspace for one shot; sets HOME_DIR / WS.
fresh() {
  HOME_DIR="$root/$1/home" WS="$root/$1/workspace"
  rm -rf "${root:?}/$1" && mkdir -p "$HOME_DIR" "$WS"
  export BYOB_HOME="$HOME_DIR"
  local helper
  helper="$(demo_helper)"
  [[ -n "$helper" ]] && export BYOB_ARTIFACT_HOST="$helper"
  return 0
}

# run NAME ARGS...: the desktop app headless at W×H, screenshot to docs/media/NAME.png.
run() {
  local name="$1"; shift
  scripts/headless.sh --inference-url "$url" --model "$model" --workspace "$WS" --window-size "${W}x${H}" \
    "$@" --screenshot "$out/$name.png" >"$root/$name.log" 2>&1 || echo "   $name: byob exited with an error (log: $root/$name.log)" >&2
}

# clip NAME ARGS...: like run, recorded; writes docs/media/NAME.gif.
clip() {
  local name="$1"; shift
  local mp4="$root/$name.mp4"
  HEADLESS_RECORD="$mp4" run "$name" "$@"
  # Skip the black before the window maps and after it closes.
  local black from=0 to
  black="$(env -u LD_LIBRARY_PATH "$ffbin/ffmpeg" -i "$mp4" -vf blackdetect=d=0.05:pix_th=0.05 -f null - 2>&1 | grep -o 'black_start:[0-9.]* black_end:[0-9.]*' || true)"
  to="$(env -u LD_LIBRARY_PATH "$ffbin/ffprobe" -v error -show_entries format=duration -of csv=p=0 "$mp4")"
  while read -r start end; do
    [[ -z "$start" ]] && continue
    if [[ "$start" == 0 ]]; then from="$end"; else to="$start"; fi
  done < <(sed -E 's/black_start:([0-9.]*) black_end:([0-9.]*)/\1 \2/' <<<"$black")
  # Crop to the window; drop frames that barely changed (the model thinking) and play the rest
  # a little faster; hold the end; a GIF that only stores what changed between frames (smaller
  # than an animated WebP of the same thing here).
  ff -ss "$from" -to "$to" -i "$mp4" -filter_complex "crop=$W:$H:0:0,mpdecimate=hi=64*12:lo=64*5:frac=0.33,setpts=N/18/TB,fps=8,tpad=stop_mode=clone:stop_duration=3,scale=1100:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=128:stats_mode=diff[p];[b][p]paletteuse=dither=none:diff_mode=rectangle" \
    -loop 0 "$out/$name.gif"
  rm -f "$out/$name.png"
}

shrink() { command -v magick >/dev/null && [[ -f "$1" ]] && magick "$1" -colors 256 "PNG8:$1" || true; }

shot() {
  local name="$1" t0=$SECONDS
  echo "== $name ==" >&2
  case "$name" in
    hero)
      fresh hero
      clip hero --auto-approve --send "Build a monthly budget spreadsheet with categories, amounts and a summary by category."
      ;;
    self-plugin)
      fresh self-plugin
      clip self-plugin --auto-approve --send "Make me a plugin with a counter pane: a big number with + and - buttons and a Reset button." --open-panel plugin-pane
      ;;
    html-app)
      fresh html-app
      run html-app --auto-approve --send "Make me a tip calculator app with a dark theme."
      ;;
    sheets)
      fresh sheets
      cp demos/seed/sheets/sales.csv "$WS/"
      run sheets --auto-approve --send "Open sales.csv as a spreadsheet and add a column with the month-over-month growth % of revenue for each region."
      ;;
    connect)
      # First run: nothing configured, so the Connect a model screen.
      fresh connect
      scripts/headless.sh --workspace "$WS" --window-size "${W}x${H}" --screenshot "$out/connect.png" >"$root/connect.log" 2>&1 || true
      ;;
    phone)
      # The browser UI at phone size, on a chat made first on the desktop.
      fresh phone
      run phone-desktop --auto-approve --send "Plot y = sin(x) and y = cos(x) from 0 to 2π on one chart."
      rm -f "$out/phone-desktop.png"
      local port=7479
      target/debug/byob --headless "127.0.0.1:$port" --inference-url "$url" --model "$model" --workspace "$WS" >"$root/phone-server.log" 2>&1 &
      local srv=$!
      for _ in $(seq 100); do curl -sf "http://127.0.0.1:$port/" >/dev/null && break; sleep 0.2; done
      # Headless Chromium won't go narrower than 500 px; that's already the phone layout.
      python3 -I scripts/browser-shot.py "http://127.0.0.1:$port/" "$out/phone.png" --size 500x900 --scale 2 --dark --wait 15 \
        >"$root/phone-chromium.log" 2>&1 || true
      kill "$srv" 2>/dev/null || true
      ;;
    *) echo "unknown shot $name (one of: $ALL)" >&2; return 1 ;;
  esac
  shrink "$out/$name.png"
  echo "   $((SECONDS - t0)) s" >&2
}

for name in ${@:-$ALL}; do
  shot "$name" || true
done
ls -la "$out" >&2
