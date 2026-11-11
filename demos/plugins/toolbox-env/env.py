"""The toolbox environment: read tools.txt in the workspace (the current folder), make a
small shell script per listed tool in .toolbox/bin and print the environment as JSON.

tools.txt: one tool per line, `name: what it prints` (lines starting with # are skipped).
byob runs this once per workspace (after the user agreed) and caches the result until
tools.txt changes.
"""

import json
import os
import re
import shlex

ws = os.getcwd()
bin_dir = os.path.join(ws, ".toolbox", "bin")
os.makedirs(bin_dir, exist_ok=True)
names = []
with open("tools.txt", encoding="utf-8") as f:
    for line in f:
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        name, _, says = line.partition(":")
        name = name.strip()
        if not re.fullmatch(r"[A-Za-z0-9_-]{1,40}", name):
            continue
        says = says.strip() or f"{name} from the toolbox"
        script = os.path.join(bin_dir, name)
        with open(script, "w", encoding="utf-8") as out:
            out.write("#!/bin/sh\n")
            out.write(f"echo {shlex.quote('[toolbox] ' + says)} \"$@\"\n")
        os.chmod(script, 0o755)
        names.append(name)

print(json.dumps({
    "PATH": bin_dir + os.pathsep + os.environ.get("PATH", ""),
    "TOOLBOX_TOOLS": " ".join(names),
}))
