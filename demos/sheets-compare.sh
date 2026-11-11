#!/usr/bin/env bash
# demos/run.sh sheets-compare: the same spreadsheet prompt with the Python `sheets` plugin and
# then the native `sheets-native` twin, each in a fresh profile on a virtual display (Xvfb),
# timed, with a screenshot of each; then the measured comparison from docs/sheets-comparison.md.
#   BYOB_COMPARE_PROMPT  the prompt (default: the growth % on the seeded sales.csv)
#   BYOB_COMPARE_BENCH=1 also run the backend benchmark (cargo test -p byob-sheets --release)
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
if ! command -v Xvfb >/dev/null || ! command -v cargo >/dev/null; then exec nix develop -c "$0" "$@"; fi
source demos/lib.sh
cargo build --release -q -p byob
mapfile -t prompts < <(demo_prompts sheets)
prompt="${BYOB_COMPARE_PROMPT:-${prompts[1]}}"
out="$DEMO_ROOT/sheets-compare"
mkdir -p "$out"

display_file="$(mktemp)"
Xvfb -displayfd 3 -screen 0 1600x900x24 -nolisten tcp 3>"$display_file" 2>/dev/null &
xvfb=$!
trap 'kill "$xvfb" 2>/dev/null; rm -f "$display_file"' EXIT
for _ in $(seq 50); do [[ -s "$display_file" ]] && break; sleep 0.1; done
display=":$(tr -d '[:space:]' <"$display_file")"

declare -A took
for demo in sheets sheets-native; do
  (
    demo_setup "$demo" 0
    echo "== $demo: \"$prompt\" ==" >&2
    env -u WAYLAND_DISPLAY DISPLAY="$display" BYOB_SETTLE_SECS=600 timeout 900 target/release/byob "${DEMO_ARGS[@]}" \
      --auto-approve --send "$prompt" --screenshot "$out/$demo.png" >"$out/$demo.log" 2>&1 \
      || echo "   byob exited with an error (log: $out/$demo.log)" >&2
  ) &
  t0=$SECONDS
  wait $!
  took[$demo]=$((SECONDS - t0))
  echo "   ${took[$demo]} s wall (model turn included); screenshot $out/$demo.png; log $out/$demo.log" >&2
done

echo
echo "This run (one model turn each, shared endpoint, so mostly model time):"
printf '  %-14s %4s s\n' "sheets" "${took[sheets]}" "sheets-native" "${took[sheets-native]}"
echo
echo "Measured backend numbers (docs/sheets-comparison.md):"
sed -n '/^| measure /,/^$/p' docs/sheets-comparison.md
echo "Switch between them in Settings → Plugins: the unused one says \"not used\" with a \"Use this one\" button."
if [[ "${BYOB_COMPARE_BENCH:-}" == 1 ]]; then
  # The Python plugin's own venv (with ironcalc) from the sheets run above.
  py="$(find "$DEMO_ROOT/sheets/home/plugin-python" -path '*/bin/python' 2>/dev/null | head -1)"
  if [[ -z "$py" ]]; then
    echo "no sheets venv under $DEMO_ROOT/sheets/home/plugin-python; skipping the benchmark" >&2
  else
    echo
    echo "== BYOB_SHEETS_PYTHON=$py cargo test -p byob-sheets --release --test bench -- --ignored --nocapture =="
    BYOB_SHEETS_PYTHON="$py" cargo test -p byob-sheets --release --test bench -- --ignored --nocapture
  fi
fi
