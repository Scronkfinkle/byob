# Shared by demos/run.sh and demos/screenshots.sh (source it; bash).
# Sets up one demo's throwaway profile and workspace and fills the array DEMO_ARGS with the
# byob flags for it. Nothing here reads or writes ~/.config/byob or ~/.local/share/byob:
# every demo runs with BYOB_HOME=$DEMO_ROOT/<demo>/home.

DEMOS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$DEMOS_DIR/.." && pwd)"
DEMO_ROOT="${BYOB_DEMO_ROOT:-/tmp/byob-demo}"
DEMO_URL="${BYOB_DEMO_URL:-http://silicon:9931/v1}"
DEMO_MODEL="${BYOB_DEMO_MODEL:-local}"
DEMO_LIST="todo self-plugin hooks env-provider mcp-app sheets sheets-native fork"

# The Servo helper (HTML apps, MCP App cards and plugin web views inline on the desktop).
demo_helper() {
  local c
  for c in "${BYOB_ARTIFACT_HOST:-}" "$REPO/host/artifact-host/target/release/byob-artifact-host" /tmp/byob-helper-bridge/byob-artifact-host; do
    [[ -n "$c" && -x "$c" ]] && { echo "$c"; return; }
  done
}

# demo_setup NAME KEEP: makes $DEMO_ROOT/NAME/{home,workspace} (wiped first unless KEEP=1),
# writes the profile's settings.json, seeds the workspace and sets DEMO_HOME, DEMO_WS, DEMO_ARGS.
demo_setup() {
  local name="$1" keep="${2:-0}" dir="$DEMO_ROOT/$1"
  case " $DEMO_LIST " in *" $name "*) ;; *) echo "unknown demo: $name (one of: $DEMO_LIST, sheets-compare)" >&2; return 1 ;; esac
  [[ "$keep" == 1 ]] || rm -rf "$dir"
  DEMO_HOME="$dir/home"
  DEMO_WS="$dir/workspace"
  mkdir -p "$DEMO_HOME" "$DEMO_WS" "$dir/workspaces"
  # Plugin switches per demo (Settings → Plugins changes them later; that's the point).
  local switches='{}'
  [[ "$name" == sheets-native ]] && switches='{"sheets-native": true}'
  if [[ "$keep" != 1 || ! -f "$DEMO_HOME/settings.json" ]]; then
    cat >"$DEMO_HOME/settings.json" <<EOF
{
  "inference": { "base_url": "$DEMO_URL", "model": "$DEMO_MODEL", "last_good": "$DEMO_URL" },
  "workspace": "$DEMO_WS",
  "workspaces_root": "$dir/workspaces",
  "plugins": $switches
}
EOF
  fi
  DEMO_ARGS=(--inference-url "$DEMO_URL" --model "$DEMO_MODEL" --workspace "$DEMO_WS")
  case "$name" in
    todo) DEMO_ARGS+=(--plugin "$REPO/examples/plugins/todo-demo") ;;
    self-plugin | fork) ;;
    hooks)
      DEMO_ARGS+=(--plugin "$DEMOS_DIR/plugins/guard" --auto-approve)
      [[ -f "$DEMO_WS/notes.txt" ]] || printf 'Shopping: figs, basil, bread\nCall Ana on Friday\n' >"$DEMO_WS/notes.txt"
      ;;
    env-provider)
      DEMO_ARGS+=(--plugin "$DEMOS_DIR/plugins/toolbox-env")
      cp -n "$DEMOS_DIR/seed/env-provider/tools.txt" "$DEMO_WS/"
      ;;
    mcp-app) DEMO_ARGS+=(--plugin "$DEMOS_DIR/plugins/weather" --plugin "$DEMOS_DIR/plugins/notes") ;;
    sheets | sheets-native) cp -n "$DEMOS_DIR/seed/sheets/sales.csv" "$DEMO_WS/" ;;
  esac
  export BYOB_HOME="$DEMO_HOME"
  local helper
  helper="$(demo_helper)"
  [[ -n "$helper" ]] && export BYOB_ARTIFACT_HOST="$helper"
  return 0
}

# The prompts screenshots.sh sends (and run.sh prints as suggestions).
demo_prompts() {
  case "$1" in
    todo) echo "Add three things to my to-do list: buy figs, call the dentist, file the taxes." ;;
    self-plugin)
      echo "Make me a plugin with a counter pane: a big number with + and - buttons and a Reset button."
      echo "Add a status chip to the header that shows the current time (HH:MM)."
      echo "Make a hook plugin that blocks every delete_file call with the message 'deleting is off'."
      ;;
    hooks) echo "Read notes.txt, then write a copy of it to backup.txt, then delete notes.txt." ;;
    env-provider) echo "Run the greet and deploy-check commands and tell me what they print." ;;
    mcp-app) echo "Show me the weather in Lisbon." ;;
    sheets | sheets-native)
      echo "Build a monthly budget spreadsheet with categories, amounts and a summary by category."
      echo "Open sales.csv as a spreadsheet and add a column with the month-over-month growth % of revenue for each region."
      ;;
    fork)
      echo "Fork the built-in html-apps plugin (keep it globally), then change its guidance.md so every app always has a dark/light theme toggle button."
      echo "Make me a tip calculator app."
      ;;
  esac
}
