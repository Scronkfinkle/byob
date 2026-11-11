#!/usr/bin/env python3
"""Screenshot a page in headless Chromium after waiting in real time (the browser UI's wasm
loads and then talks over a WebSocket, which --virtual-time-budget doesn't wait for).

    scripts/browser-shot.py URL OUT.png [--size 390x844] [--scale 2] [--wait 12] [--dark]

Talks to Chromium over --remote-debugging-pipe (fd 3 in, fd 4 out, NUL-separated JSON), so it
needs nothing beyond the standard library.
"""
import argparse, base64, json, os, subprocess, sys, time

p = argparse.ArgumentParser()
p.add_argument("url"); p.add_argument("out")
p.add_argument("--size", default="390x844"); p.add_argument("--scale", type=float, default=2.0)
p.add_argument("--wait", type=float, default=12.0); p.add_argument("--dark", action="store_true")
a = p.parse_args()
w, h = (int(x) for x in a.size.split("x"))

to_chrome_r, to_chrome_w = os.pipe()
from_chrome_r, from_chrome_w = os.pipe()
env = {k: v for k, v in os.environ.items() if k != "LD_LIBRARY_PATH"}
# Chromium wants the pipe on fds 3 (it reads) and 4 (it writes).
def child():
    os.dup2(to_chrome_r, 3); os.dup2(from_chrome_w, 4)
proc = subprocess.Popen(
    ["chromium", "--headless=new", "--remote-debugging-pipe", "--use-angle=swiftshader",
     "--enable-unsafe-swiftshader", "--hide-scrollbars", "--no-first-run", f"--window-size={w},{h}", f"--force-device-scale-factor={a.scale}",
     "about:blank"],
    env=env, preexec_fn=child, pass_fds=(3, 4), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
os.close(to_chrome_r); os.close(from_chrome_w)
out = os.fdopen(to_chrome_w, "wb", buffering=0)
inp = os.fdopen(from_chrome_r, "rb", buffering=0)
buf = b""; next_id = 0

def call(method, params=None, session=None):
    global buf, next_id
    next_id += 1
    msg = {"id": next_id, "method": method, "params": params or {}}
    if session: msg["sessionId"] = session
    out.write(json.dumps(msg).encode() + b"\0")
    while True:
        while b"\0" not in buf:
            chunk = inp.read(65536)
            if not chunk: sys.exit(f"chromium closed the pipe during {method}")
            buf += chunk
        raw, buf = buf.split(b"\0", 1)
        reply = json.loads(raw)
        if reply.get("id") == next_id:
            if "error" in reply: sys.exit(f"{method}: {reply['error']}")
            return reply.get("result", {})

try:
    target = call("Target.createTarget", {"url": "about:blank"})["targetId"]
    s = call("Target.attachToTarget", {"targetId": target, "flatten": True})["sessionId"]
    call("Emulation.setEmulatedMedia", {"features": [{"name": "prefers-color-scheme", "value": "dark" if a.dark else "light"}]}, s)
    call("Page.enable", {}, s)
    call("Page.navigate", {"url": a.url}, s)
    time.sleep(a.wait)
    shot = call("Page.captureScreenshot", {"format": "png"}, s)
    with open(a.out, "wb") as f: f.write(base64.b64decode(shot["data"]))
    print(f"{a.out}: {w}x{h} @{a.scale}x")
finally:
    proc.kill()
