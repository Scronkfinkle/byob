#!/usr/bin/env bash
# Regenerate demos/screenshots/*.png: each demo run headlessly (scripts/headless.sh, Xvfb),
# with the demo's prompt sent to the model where one is needed.
#
#   demos/screenshots.sh [shot...]      (default: all; names below, e.g. todo hooks)
#
# Uses the same throwaway profiles as run.sh (/tmp/byob-demo/<demo>, wiped per shot) and the
# same endpoint (BYOB_DEMO_URL / BYOB_DEMO_MODEL). The model is asked one shot at a time.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
if ! command -v Xvfb >/dev/null || ! command -v cargo >/dev/null; then exec nix develop -c "$0" "$@"; fi
source demos/lib.sh
out="$REPO/demos/screenshots"
logs="$DEMO_ROOT/shot-logs"
mkdir -p "$out" "$logs"
export HEADLESS_TIMEOUT="${HEADLESS_TIMEOUT:-900}" BYOB_SETTLE_SECS="${BYOB_SETTLE_SECS:-600}"

ALL="todo todo-settings self-plugin hooks env-provider env-provider-consent mcp-app sheets fork-settings fork"

# shot NAME: the demo it belongs to and the extra flags for it.
shot() {
  local name="$1" demo prefork="" extra=()
  case "$name" in
    todo) demo=todo; extra=(--auto-approve --send "$(demo_prompts todo)" --open-panel plugin-pane:todo-demo/list) ;;
    todo-settings) demo=todo; extra=(--open-panel plugin:todo-demo) ;;
    self-plugin)
      demo=self-plugin
      mapfile -t p < <(demo_prompts self-plugin)
      extra=(--auto-approve --send "${p[0]}" --send "${p[1]}" --open-panel plugin-pane)
      ;;
    hooks) demo=hooks; extra=(--send "$(demo_prompts hooks)") ;;
    env-provider) demo=env-provider; extra=(--auto-approve --send "$(demo_prompts env-provider)") ;;
    env-provider-consent) demo=env-provider; extra=(--send "$(demo_prompts env-provider)") ;;
    mcp-app) demo=mcp-app; extra=(--auto-approve --send "$(demo_prompts mcp-app)" --open-panel plugin-pane:notes/notes) ;;
    sheets)
      demo=sheets
      mapfile -t p < <(demo_prompts sheets)
      extra=(--auto-approve --send "${p[1]}" --window-size 1500x850)
      ;;
    fork-settings) demo=fork; extra=(--open-panel plugin:html-apps) ;;
    fork)
      # Forking always asks a person (even with --auto-approve), so this shot starts from what
      # Settings → Plugins → HTML apps → Fork to edit makes: a copy in the profile's plugins.
      demo=fork prefork=1
      extra=(--auto-approve --send "Change your html-apps plugin's guidance.md so every app always has a dark/light theme toggle button." --send "$(demo_prompts fork | tail -1)" --window-size 1500x850)
      ;;
    *) echo "unknown shot $name (one of: $ALL)" >&2; return 1 ;;
  esac
  (
    demo_setup "$demo" 0
    if [[ -n "$prefork" ]]; then
      mkdir -p "$DEMO_HOME/plugins" && cp -r crates/bundled/plugins/html-apps "$DEMO_HOME/plugins/"
    fi
    echo "== $name ==" >&2
    local t0=$SECONDS
    scripts/headless.sh "${DEMO_ARGS[@]}" "${extra[@]}" --screenshot "$out/$name.png" >"$logs/$name.log" 2>&1 \
      || echo "   $name: byob exited with an error (log: $logs/$name.log)" >&2
    # 256 colours keep the committed PNGs ~4x smaller (text stays sharp).
    if command -v magick >/dev/null && [[ -f "$out/$name.png" ]]; then
      magick "$out/$name.png" -colors 256 "PNG8:$out/$name.png" || true
    fi
    echo "   $((SECONDS - t0)) s -> demos/screenshots/$name.png" >&2
  )
}

for name in ${@:-$ALL}; do
  shot "$name" || true
done
