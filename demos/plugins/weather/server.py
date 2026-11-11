"""A tiny MCP Apps server (stdio): one tool with a UI, one app-only helper."""
import json, sys, random

HTML = r"""<!doctype html>
<html><head><title>Weather</title>
<style>
:root { --color-background-primary: #fff; --color-text-primary: #111; --color-text-secondary: #666;
        --color-border-primary: #ddd; --color-background-info: #36f; --border-radius-md: 6px; --font-sans: sans-serif; }
body { margin: 0; padding: 14px; font-family: var(--font-sans); background: var(--color-background-primary); color: var(--color-text-primary); }
.row { display: flex; gap: 14px; align-items: center; }
.temp { font-size: 44px; font-weight: 700; }
.muted { color: var(--color-text-secondary); font-size: 13px; }
button { background: var(--color-background-info); color: #fff; border: 0; border-radius: var(--border-radius-md); padding: 6px 12px; cursor: pointer; }
.box { border: 1px solid var(--color-border-primary); border-radius: var(--border-radius-md); padding: 8px 10px; margin-top: 10px; font-size: 13px; }
</style></head>
<body>
<div class="row"><div class="temp" id="temp">…</div><div><div id="city" style="font-weight:600">waiting for the tool</div><div class="muted" id="sky"></div></div></div>
<div class="row" style="margin-top:10px"><button id="refresh">Refresh (app-only tool)</button><button id="ask">Ask byob about it</button></div>
<div class="box" id="log">Handshake…</div>
<script>
let next = 1; const waits = {};
const send = (method, params) => { const id = next++; parent.postMessage({jsonrpc: "2.0", id, method, params}, "*");
  return new Promise((ok, no) => waits[id] = [ok, no]); };
const note = (method, params) => parent.postMessage({jsonrpc: "2.0", method, params}, "*");
const log = t => document.getElementById("log").textContent = t;
let city = "?";
function show(r) {
  const s = r && r.structuredContent; if (!s) return;
  document.getElementById("temp").textContent = s.temp + "°C";
  document.getElementById("city").textContent = city = s.city;
  document.getElementById("sky").textContent = s.sky + " · via tools/call " + (s.n || 1);
}
addEventListener("message", e => {
  const d = e.data; if (!d || typeof d !== "object") return;
  if (d.id in waits && !d.method) { const [ok, no] = waits[d.id]; delete waits[d.id]; d.error ? no(d.error) : ok(d.result); return; }
  if (d.method === "ui/notifications/tool-result") show(d.params);
  if (d.method === "ui/notifications/tool-input") city = (d.params.arguments || {}).city || city;
  if (d.method === "ui/resource-teardown") parent.postMessage({jsonrpc: "2.0", id: d.id, result: {}}, "*");
});
async function main() {
  const init = await send("ui/initialize", {protocolVersion: "2026-01-26", appInfo: {name: "weather", version: "1"}, appCapabilities: {}});
  const vars = (init.hostContext.styles || {}).variables || {};
  for (const [k, v] of Object.entries(vars)) document.documentElement.style.setProperty(k, v);
  note("ui/notifications/initialized", {});
  // Bridge round trips: ping (answered by the host) and tools/call (to the MCP server).
  let t0 = performance.now(); for (let i = 0; i < 20; i++) await send("ping", {}); const ping = (performance.now() - t0) / 20;
  t0 = performance.now(); let r; for (let i = 0; i < 5; i++) r = await send("tools/call", {name: "refresh", arguments: {city}}); const call = (performance.now() - t0) / 5;
  show(r);
  await send("ui/update-model-context", {content: [{type: "text", text: "The weather app shows " + city}]});
  log("Theme: " + init.hostContext.theme + " · " + Object.keys(vars).length + " CSS variables · ping " + ping.toFixed(2) + " ms · tools/call " + call.toFixed(1) + " ms");
}
document.getElementById("refresh").onclick = async () => show(await send("tools/call", {name: "refresh", arguments: {city}}));
document.getElementById("ask").onclick = () => send("ui/message", {role: "user", content: [{type: "text", text: "What should I wear in " + city + "?"}]});
main().catch(e => log("error: " + JSON.stringify(e)));
</script></body></html>"""

TOOLS = [
    {"name": "show_weather", "description": "Show the current weather for a city in an interactive card.",
     "inputSchema": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
     "_meta": {"ui": {"resourceUri": "ui://weather/dash"}}},
    {"name": "refresh", "description": "Refresh the weather (for the app)", "inputSchema": {"type": "object", "properties": {"city": {"type": "string"}}},
     "annotations": {"readOnlyHint": True}, "_meta": {"ui": {"visibility": ["app"]}}},
]
n = 0
for line in sys.stdin:
    try:
        m = json.loads(line)
    except Exception:
        continue
    mid, meth, p = m.get("id"), m.get("method"), m.get("params") or {}
    if mid is None:
        continue
    if meth == "initialize":
        r = {"protocolVersion": p.get("protocolVersion"), "capabilities": {"tools": {}, "resources": {}}, "serverInfo": {"name": "weather", "version": "1"}}
    elif meth == "tools/list":
        r = {"tools": TOOLS}
    elif meth == "tools/call":
        n += 1
        city = (p.get("arguments") or {}).get("city", "Lisbon")
        t = 18 + (hash(city) % 7) + (n % 3)
        r = {"content": [{"type": "text", "text": f"{city}: {t}°C, sunny"}], "structuredContent": {"city": city, "temp": t, "sky": "sunny", "n": n}}
    elif meth == "resources/read":
        r = {"contents": [{"uri": p.get("uri"), "mimeType": "text/html;profile=mcp-app", "text": HTML}]}
    else:
        r = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": r}) + "\n")
    sys.stdout.flush()
