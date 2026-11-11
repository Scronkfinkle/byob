#!/usr/bin/env bash
# Run the byob desktop app on a virtual X display (no window on your screen).
# Takes the same options as byob. Always pass --screenshot so the run ends:
#   scripts/headless.sh --inference-url http://localhost:8080/v1 --model local \
#       --send "hello" --screenshot /tmp/byob.png
# HEADLESS_SOFTWARE=1 renders with Mesa lavapipe instead of the GPU.
# HEADLESS_TIMEOUT (default 300 s) kills a stuck run. HEADLESS_DRIVE=script.sh runs alongside
# (with $DISPLAY set), e.g. to drive input with xdotool. HEADLESS_RECORD=out.mp4 records the
# virtual display (ffmpeg) for the whole run; the window sits at the top left, so crop to its size.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
if ! command -v Xvfb >/dev/null || ! command -v cargo >/dev/null; then exec nix develop -c "$0" "$@"; fi
cargo build -q -p byob
display_file="$(mktemp)"
Xvfb -displayfd 3 -screen 0 1600x900x24 -nolisten tcp 3>"$display_file" 2>/dev/null &
xvfb_pid=$!
trap 'kill "$xvfb_pid" 2>/dev/null; rm -f "$display_file"' EXIT
for _ in $(seq 50); do [[ -s "$display_file" ]] && break; sleep 0.1; done
env=(DISPLAY=":$(tr -d '[:space:]' <"$display_file")")
if [[ -n "${HEADLESS_RECORD:-}" ]]; then
  # Fed from a fifo so `q` ends it cleanly (a killed ffmpeg leaves an unplayable mp4).
  rec_ctl="$(mktemp -u)" && mkfifo "$rec_ctl"
  # x11grab is only in ffmpeg-full (pinned by the flake's nixpkgs, fetched on first use); the
  # dev shell's LD_LIBRARY_PATH would break it.
  ffmpeg="$(nix build --no-link --print-out-paths --inputs-from . nixpkgs#ffmpeg-full.bin)/bin/ffmpeg"
  env -u LD_LIBRARY_PATH "$ffmpeg" -loglevel error -y -f x11grab -draw_mouse 0 -framerate 15 -video_size 1600x900 -i "${env[0]#DISPLAY=}" \
    -c:v libx264 -preset veryfast -crf 18 -pix_fmt yuv420p "$HEADLESS_RECORD" <"$rec_ctl" &
  rec_pid=$!
  exec 4>"$rec_ctl"
  trap '{ echo q >&4; } 2>/dev/null; wait "$rec_pid" 2>/dev/null; rm -f "$rec_ctl"; kill "$xvfb_pid" 2>/dev/null; rm -f "$display_file"' EXIT
fi
# HTML apps render inline when the Servo helper is built (cargo build --release in host/artifact-host).
helper=host/artifact-host/target/release/byob-artifact-host
[[ -z "${BYOB_ARTIFACT_HOST:-}" && -x "$helper" ]] && env+=(BYOB_ARTIFACT_HOST="$PWD/$helper")
[[ -n "${HEADLESS_SOFTWARE:-}" ]] && env+=(VK_ICD_FILENAMES=/run/opengl-driver/share/vulkan/icd.d/lvp_icd.x86_64.json)
if [[ -n "${HEADLESS_DRIVE:-}" ]]; then
  env -u WAYLAND_DISPLAY "${env[@]}" timeout "${HEADLESS_TIMEOUT:-300}" target/debug/byob "$@" &
  app=$!
  env "${env[@]}" bash "$HEADLESS_DRIVE" || true
  wait "$app"
else
  env -u WAYLAND_DISPLAY "${env[@]}" timeout "${HEADLESS_TIMEOUT:-300}" target/debug/byob "$@"
fi
