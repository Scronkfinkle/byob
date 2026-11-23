#!/usr/bin/env bash
# Regenerate the README's and byob.chat's screenshots in docs/media, headlessly (Xvfb, see
# scripts/headless.sh), against a real model.
#
#   scripts/readme-media.sh [shot...]     (default: all; names below)
#
# Every shot uses a throwaway profile and workspace under /tmp/byob-media/<shot>, wiped first.
# Servo helper (HTML panes/apps inline): as in demos/run.sh; BYOB_ARTIFACT_HOST wins.
# Endpoint: BYOB_DEMO_URL (default cli://claude, Claude Code) / BYOB_DEMO_MODEL (default sonnet).
# Stills only: a GIF of an older interface outlives it, so there are none.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
if ! command -v Xvfb >/dev/null || ! command -v cargo >/dev/null; then exec nix develop -c "$0" "$@"; fi
source demos/lib.sh
out="$PWD/docs/media"
root=/tmp/byob-media
mkdir -p "$out" "$root"
export HEADLESS_TIMEOUT="${HEADLESS_TIMEOUT:-900}" BYOB_SETTLE_SECS="${BYOB_SETTLE_SECS:-600}"
url="${BYOB_DEMO_URL:-cli://claude}" model="${BYOB_DEMO_MODEL:-sonnet}"
pngquant="$(nix build --no-link --print-out-paths --inputs-from . nixpkgs#pngquant)/bin/pngquant"

ALL="beside self-plugin html-app sheets connect phone"
# Sized so the interface reads on the page (it's shown about this wide), drawn at twice the
# pixels so it's sharp on HiDPI screens.
W=1100 H=620
export HEADLESS_SCREEN=2400x1400 WINIT_X11_SCALE_FACTOR=2

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

shrink() { [[ -f "$1" ]] && "$pngquant" --quality 70-90 --speed 1 --force --output "$1" "$1" || true; }

shot() {
  local name="$1" t0=$SECONDS
  echo "== $name ==" >&2
  case "$name" in
    beside)
      # A chart beside the chat (byob.chat's demo; the README's first picture).
      fresh beside
      cp demos/seed/allotment/harvest.csv "$WS/"
      run beside --auto-approve --theme light --send "Chart our allotment harvest by month from harvest.csv (stacked by crop) and tell me in two or three sentences which months were best per hour worked."
      ;;
    self-plugin)
      fresh self-plugin
      run self-plugin --auto-approve --theme light --send "Make me a plugin with a counter pane: a big number with + and − buttons and a Reset button." --open-panel plugin-pane
      ;;
    html-app)
      fresh html-app
      run html-app --auto-approve --theme light --send "Make a small HTML app: a sowing calendar. Pick a vegetable from a few buttons and it shows a 12-month strip with when to sow indoors, sow outdoors and harvest. Calm, friendly design."
      ;;
    sheets)
      fresh sheets
      cp demos/seed/allotment/harvest.csv "$WS/"
      run sheets --auto-approve --theme light --send "Open harvest.csv as a spreadsheet, add a kg_per_hour column (all crops over hours worked) and bold the best month."
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
