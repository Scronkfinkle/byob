#!/usr/bin/env bash
# Try one of byob's extensibility demos in a throwaway profile. See demos/README.md.
#
#   demos/run.sh <demo> [--browser | --serve] [--keep]
#
#   <demo>     todo, self-plugin, hooks, env-provider, mcp-app, sheets, sheets-native, fork,
#              or sheets-compare (runs sheets and sheets-native headlessly, back to back)
#   (default)  the desktop window
#   --serve    the desktop window and the browser UI at http://127.0.0.1:7470
#   --browser  the browser UI only (byob --headless); open the printed URL
#   --keep     keep the demo's profile and workspace from the last run (default: start fresh)
#
# Every run uses BYOB_HOME=/tmp/byob-demo/<demo>/home and the workspace
# /tmp/byob-demo/<demo>/workspace; your own byob settings and chats are never touched.
# Environment: BYOB_DEMO_URL (default http://silicon:9931/v1), BYOB_DEMO_MODEL (default local),
# BYOB_DEMO_ROOT (default /tmp/byob-demo), BYOB_API_KEY, BYOB_ARTIFACT_HOST (the Servo helper).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
if ! command -v cargo >/dev/null; then exec nix develop -c "$0" "$@"; fi
source demos/lib.sh

demo="" mode=desktop keep=0
for a in "$@"; do
  case "$a" in
    --browser) mode=browser ;;
    --serve) mode=serve ;;
    --keep) keep=1 ;;
    -h | --help) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "unknown option $a (try --help)" >&2; exit 2 ;;
    *) demo="$a" ;;
  esac
done
[[ -n "$demo" ]] || { echo "usage: demos/run.sh <demo> [--browser | --serve] [--keep]; demos: $DEMO_LIST sheets-compare" >&2; exit 2; }

case " $DEMO_LIST sheets-compare " in *" $demo "*) ;; *) echo "unknown demo $demo; one of: $DEMO_LIST sheets-compare" >&2; exit 2 ;; esac

# The browser UI is compiled into the release binary from `cargo xtask web`'s output.
if [[ "$mode" != desktop && ! -f crates/web-assets/assets/index.html ]]; then
  echo "== building the browser UI (cargo xtask web) ==" >&2
  if command -v wasm-bindgen >/dev/null; then cargo xtask web; else nix develop -c cargo xtask web; fi
fi
echo "== building byob (release) ==" >&2
cargo build --release -q -p byob
BYOB=target/release/byob

if [[ "$demo" == sheets-compare ]]; then
  exec bash demos/sheets-compare.sh
fi

demo_setup "$demo" "$keep"
if ! curl -s -m 5 -o /dev/null "${DEMO_URL%/}/models"; then
  echo "warning: $DEMO_URL doesn't answer; set BYOB_DEMO_URL (and BYOB_DEMO_MODEL) to your endpoint" >&2
fi

echo
echo "demo:       $demo"
echo "profile:    $DEMO_HOME   (BYOB_HOME)"
echo "workspace:  $DEMO_WS"
echo "model:      $DEMO_MODEL at $DEMO_URL"
echo "web helper: ${BYOB_ARTIFACT_HOST:-none (HTML views fall back to Open in browser on the desktop)}"
echo "try:"
demo_prompts "$demo" | sed 's/^/  > /'
# Desktop: the first prompt is already typed into the composer, so the window isn't a blank chat.
first="$(demo_prompts "$demo" | head -n 1)"
[[ "$mode" != browser && -n "$first" ]] && DEMO_ARGS+=(--draft "$first") && echo "(the first one is typed into the composer: press Enter to send it)"
echo "(demos/README.md has what to click and what to look for)"
echo

case "$mode" in
  desktop) exec "$BYOB" "${DEMO_ARGS[@]}" ;;
  serve) exec "$BYOB" --serve "${DEMO_ARGS[@]}" ;;
  browser)
    echo "open http://127.0.0.1:7470 in your browser (Ctrl-C to stop)"
    exec "$BYOB" --headless "${DEMO_ARGS[@]}"
    ;;
esac
