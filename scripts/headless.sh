#!/usr/bin/env bash
# Run the yah desktop app on a virtual X display (no window on your screen).
# Takes the same options as yah. Always pass --screenshot so the run ends:
#   scripts/headless.sh --inference-url http://silicon:9931/v1 --model local \
#       --send "hello" --screenshot /tmp/yah.png
# HEADLESS_SOFTWARE=1 renders with Mesa lavapipe instead of the GPU.
# HEADLESS_TIMEOUT (default 300 s) kills a stuck run.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
if ! command -v Xvfb >/dev/null || ! command -v cargo >/dev/null; then exec nix develop -c "$0" "$@"; fi
cargo build -q -p yah
display_file="$(mktemp)"
Xvfb -displayfd 3 -screen 0 1600x900x24 -nolisten tcp 3>"$display_file" 2>/dev/null &
xvfb_pid=$!
trap 'kill "$xvfb_pid" 2>/dev/null; rm -f "$display_file"' EXIT
for _ in $(seq 50); do [[ -s "$display_file" ]] && break; sleep 0.1; done
env=(DISPLAY=":$(tr -d '[:space:]' <"$display_file")")
[[ -n "${HEADLESS_SOFTWARE:-}" ]] && env+=(VK_ICD_FILENAMES=/run/opengl-driver/share/vulkan/icd.d/lvp_icd.x86_64.json)
env -u WAYLAND_DISPLAY "${env[@]}" timeout "${HEADLESS_TIMEOUT:-300}" target/debug/yah "$@"
