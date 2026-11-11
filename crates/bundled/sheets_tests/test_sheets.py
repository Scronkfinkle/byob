"""Tests of the sheets plugin's backend, through a fake byob that runs backend.py as a
subprocess speaking JSON lines and emulates byob's artifact store.

Run (with a Python that has ironcalc, or the ironcalc tests are skipped):
    python3 -B -m unittest discover -s crates/bundled/sheets_tests -v
    BYOB_SHEETS_PYTHON=/path/to/venv/bin/python python3 -B -m unittest ...  (the backend's Python)
BYOB_SHEETS_BENCH=1 also prints timings (recalc, view drawing, xlsx save).
"""

import json
import os
import random
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(os.path.dirname(HERE)))
PLUGIN = os.path.join(ROOT, "crates", "bundled", "plugins", "sheets")
HELPER = os.path.join(ROOT, "crates", "plugins", "src")
PYTHON = os.environ.get("BYOB_SHEETS_PYTHON") or sys.executable
XLSX = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
TYPES = {".xlsx": XLSX, ".csv": "text/csv", ".tsv": "text/tab-separated-values"}


def _has_ironcalc():
    try:
        r = subprocess.run([PYTHON, "-B", "-c", "import ironcalc"], capture_output=True, timeout=30)
        return r.returncode == 0
    except OSError:
        return False


HAS_IC = _has_ironcalc()
needs_ic = unittest.skipUnless(HAS_IC, f"{PYTHON} can't import ironcalc")


class Byob:
    """A fake byob: one backend process, a temp workspace and an artifact store."""

    def __init__(self, no_ironcalc=False):
        self.ws = tempfile.mkdtemp(prefix="sheets-ws-")
        self.chat = "chat-1"
        env = dict(os.environ, PYTHONPATH=HELPER, PYTHONUNBUFFERED="1", PYTHONDONTWRITEBYTECODE="1",
                   BYOB_PLUGIN_DIR=PLUGIN)
        env.pop("BYOB_SHEETS_NO_IRONCALC", None)
        if no_ironcalc:
            env["BYOB_SHEETS_NO_IRONCALC"] = "1"
        self.err = tempfile.TemporaryFile()
        self.p = subprocess.Popen([PYTHON, "-B", "backend.py"], cwd=PLUGIN, env=env, stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=self.err, text=True, bufsize=1)
        self.next_id = 0
        self.msgs = []  # every non-reply message
        self.ids = 0
        self.send({"type": "init", "v": 1, "plugin": "sheets", "name": "Spreadsheets", "version": "1.0.0",
                   "byob": "test", "caps": [], "events": [], "settings": {}, "dir": PLUGIN, "scope": "global",
                   "chat": None, "workspace": None, "restart": 0,
                   "views": [{"id": "sheet", "site": "artifact", "title": "Spreadsheet"}]})

    def close(self):
        try:
            self.send({"type": "shutdown"})
            self.p.stdin.close()
            self.p.wait(timeout=5)
        except Exception:
            self.p.kill()
        self.p.stdout.close()
        self.err.close()
        shutil.rmtree(self.ws, ignore_errors=True)

    def stderr(self):
        self.err.seek(0)
        return self.err.read().decode(errors="replace")

    def send(self, msg):
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()

    def _read_reply(self, rid, timeout=60):
        result = {}

        def read():
            for line in self.p.stdout:
                msg = json.loads(line)
                if msg.get("type") == "reply" and msg.get("id") == rid:
                    result["msg"] = msg
                    return
                self.msgs.append(msg)
                if msg.get("type") == "artifact":
                    self.store(msg, draft=bool(msg.get("draft")))

        t = threading.Thread(target=read, daemon=True)
        t.start()
        t.join(timeout)
        if "msg" not in result:
            raise AssertionError("no reply from the backend; stderr:\n" + self.stderr())
        return result["msg"]

    def request(self, method, params):
        self.next_id += 1
        self.send({"type": "request", "id": self.next_id, "method": method, "params": params})
        msg = self._read_reply(self.next_id)
        if "error" in msg:
            raise RuntimeError(msg["error"])
        return msg["result"]

    # -- the artifact store (like byob's: .byob/artifacts/<id>/artifact.json + v<n> files)
    def meta(self, aid):
        with open(os.path.join(self.ws, ".byob", "artifacts", aid, "artifact.json")) as f:
            return json.load(f)

    def store(self, art, draft=False):
        """Save a version from a tool result's `artifact` (or an artifact message); returns (id, n)."""
        aid = art.get("id")
        if aid is None:
            self.ids += 1
            stem = os.path.splitext(art.get("filename") or "sheet")[0]
            aid = f"{stem}-{self.ids:04x}"
            meta = {"chat": self.chat, "id": aid, "kind": "file", "content_type": "", "title": art.get("title") or stem,
                    "filename": art.get("filename"), "versions": []}
        else:
            meta = self.meta(aid)
        if art.get("filename"):
            meta["filename"] = art["filename"]
        if art.get("title"):
            meta["title"] = art["title"]
        suffix = os.path.splitext(meta["filename"] or "")[1].lower()
        meta["content_type"] = TYPES.get(suffix, "application/octet-stream")
        d = os.path.join(self.ws, ".byob", "artifacts", aid)
        os.makedirs(d, exist_ok=True)
        vs = meta["versions"]
        if draft and vs and vs[-1].get("draft"):
            v = vs[-1]
        else:
            v = {"n": len(vs) + 1, "created_at": time.time(), "note": art.get("note") or "", "draft": draft}
            vs.append(v)
        rel = os.path.join(".byob", "artifacts", aid, f"v{v['n']}{suffix}")
        shutil.copyfile(art["file"], os.path.join(self.ws, rel))
        v["path"] = rel
        v["size"] = os.path.getsize(os.path.join(self.ws, rel))
        with open(os.path.join(d, "artifact.json"), "w") as f:
            json.dump(meta, f)
        self.last_saved = (aid, v["n"])
        return aid, v["n"]

    def add_artifact(self, filename, data, title=None):
        path = os.path.join(self.ws, "upload-" + filename)
        with open(path, "wb") as f:
            f.write(data)
        aid, _ = self.store({"filename": filename, "title": title or filename, "file": path})
        os.remove(path)
        return aid

    def tool(self, tool_name, **args):
        """Call a tool; returns its text with byob's 'Saved: ...' line, like the model sees it."""
        try:
            res = self.request("tool", {"name": tool_name, "args": args, "workspace": self.ws, "chat": self.chat})
        except RuntimeError as e:  # a ToolError: byob shows the model the message
            return f"Error: {e}"
        if isinstance(res, dict) and isinstance(res.get("artifact"), dict):
            aid, n = self.store(res["artifact"])
            self.last_id = aid
            return res["text"] + f"\nSaved: artifact {aid}, version {n}."
        return res if isinstance(res, str) else json.dumps(res)

    def artifact(self, aid, version=None):
        meta = self.meta(aid)
        latest = max(v["n"] for v in meta["versions"])
        n = version or latest
        v = next(v for v in meta["versions"] if v["n"] == n)
        return {"id": aid, "version": n, "latest": latest, "path": os.path.join(self.ws, v["path"]),
                "filename": meta["filename"], "content_type": meta["content_type"], "title": meta["title"],
                "workspace": self.ws, "chat": self.chat}

    def view(self, aid, version=None, stale=False):
        a = self.artifact(aid, version)
        if stale:  # what byob said when the client opened this key, before newer versions
            a["latest"] = a["version"]
        key = f"{aid}@{a['version']}"
        return self.request("view", {"site": "artifact", "view": "sheet", "key": key, "client": "c1",
                                     "chat": self.chat, "artifact": a})

    def event(self, aid, op, version=None, action=None, stale=False, **grid):
        """A view event; returns the messages it caused (redraws, drafts)."""
        a = self.artifact(aid, version)
        if stale:
            a["latest"] = a["version"]
        key = f"{aid}@{a['version']}"
        start = len(self.msgs)
        g = dict(grid, name="sheet", op=op)
        self.send({"type": "event", "event": "view", "view": "sheet", "site": "artifact", "key": key,
                   "client": "c1", "chat": self.chat, "action": action or op, "values": {}, "artifact": a, "grid": g})
        self.view(aid, version)  # a barrier: replies come in order
        return self.msgs[start:]


def grid_of(tree):
    def walk(n):
        if isinstance(n, dict):
            if n.get("type") == "grid":
                return n
            for ch in n.get("children") or []:
                g = walk(ch)
                if g:
                    return g
        return None
    return walk(tree)


def texts_of(tree):
    out = []

    def walk(n):
        if isinstance(n, dict):
            if n.get("type") == "text":
                out.append(n.get("text"))
            for ch in n.get("children") or []:
                walk(ch)
    walk(tree)
    return out


def cell(g, row, col):
    """The cell at 0-based (row, col) from the grid's blocks."""
    for b in g["blocks"]:
        r, c = row - b["row"], col - b["col"]
        if 0 <= r < len(b["cells"]) and 0 <= c < len(b["cells"][r]):
            return b["cells"][r][c]
    return "missing"


def tsv_row(text, row):
    for line in text.splitlines():
        parts = line.split("\t")
        if parts[0] == str(row):
            return parts[1:]
    return None


class Base(unittest.TestCase):
    no_ironcalc = False

    def setUp(self):
        self.b = Byob(no_ironcalc=self.no_ironcalc)

    def tearDown(self):
        self.b.close()

    def new_sheet(self, title="Budget"):
        text = self.b.tool("create_sheet", title=title)
        return self.b.last_id, text


@needs_ic
class WriteTests(Base):
    def test_create_write_recalculates(self):
        aid, text = self.new_sheet()
        self.assertIn("budget.xlsx", text)
        self.assertIn("Saved: artifact", text)
        out = self.b.tool("write_cells", sheet=aid, rows=[
            ["Item", "Qty", "Price", "Total"],
            ["Pens", 3, 1.5, "=B2*C2"],
            ["Paper", "2", "4.25", "=B3*C3"],
            ["Sum", None, None, "=SUM(D2:D3)"]])
        self.assertIn("Wrote 14 cells", out)
        self.assertEqual(tsv_row(out, 2), ["Pens", "3", "1.5", "4.5"])
        self.assertEqual(tsv_row(out, 4)[3], "13")
        self.assertIn("No error cells.", out)
        self.assertEqual(self.b.meta(aid)["versions"][-1]["n"], 2)
        # the next call sees the saved version
        read = self.b.tool("read_cells", sheet=aid, range="D1:D4", what="formulas")
        self.assertIn("=SUM(D2:D3)", read)
        cells = self.b.tool("write_cells", sheet=aid, cells={"B2": 10})
        self.assertIn("Formulas elsewhere that changed", cells)
        self.assertIn("D4 23.5 (was 13)", cells)

    def test_sumifs_summary_by_category(self):
        aid, _ = self.new_sheet("Spend")
        rnd = random.Random(7)
        cats = ["Food", "Rent", "Fun", "Travel"]
        rows = [["Category", "Amount"]]
        want = {c: 0.0 for c in cats}
        for _ in range(120):
            c, a = rnd.choice(cats), round(rnd.uniform(1, 500), 2)
            rows.append([c, a])
            want[c] += a
        self.b.tool("write_cells", sheet=aid, rows=rows)
        summary = [["Category", "Total"]] + [[c, f'=SUMIFS(Data!B:B,Data!A:A,A{i + 2})'] for i, c in enumerate(cats)]
        self.b.tool("edit_sheet", sheet=aid, op="rename_sheet", name="Sheet1", new_name="Data")
        out = self.b.tool("write_cells", sheet=aid, start="Summary!A1", rows=summary)
        self.assertIn("Added sheet 'Summary'", out)
        for i, c in enumerate(cats):
            got = float(tsv_row(out, i + 2)[1])
            self.assertAlmostEqual(got, want[c], places=6)

    def test_fill_relative_and_absolute(self):
        aid, _ = self.new_sheet()
        rows = [["Month", "Units", "Price", "Revenue", "Share"]] + [[f"M{i}", i, 2.5, None, None] for i in range(1, 13)]
        rows.append(["Total", "=SUM(B2:B13)", None, "=SUM(D2:D13)", None])
        self.b.tool("write_cells", sheet=aid, rows=rows)
        out = self.b.tool("fill", sheet=aid, range="D2:D13", formula="=B2*C2")
        self.assertIn("D13 got =B13*C13", out)
        self.assertEqual(tsv_row(out, 13)[0], "30")
        out = self.b.tool("fill", sheet=aid, range="E2:E13", formula="=D2/$D$14")
        self.assertIn("E13 got =D13/$D$14", out)
        f = self.b.tool("read_cells", sheet=aid, range="E2:E13", what="formulas")
        self.assertEqual(tsv_row(f, 7), ["=D7/$D$14"])
        v = self.b.tool("read_cells", sheet=aid, range="E2:E13")
        self.assertAlmostEqual(sum(float(tsv_row(v, r)[0]) for r in range(2, 14)), 1.0, places=9)
        # filling right shifts columns; D2:D picks the last used row
        out = self.b.tool("fill", sheet=aid, range="F2:G", formula="=$B2+B$1")
        self.assertIn("G14 got =$B14+C$1", out)

    def test_errors_and_lints(self):
        aid, _ = self.new_sheet()
        rows = [["A", "B", "Ratio"]] + [[i, i % 3, None] for i in range(1, 11)]
        self.b.tool("write_cells", sheet=aid, rows=rows)
        out = self.b.tool("fill", sheet=aid, range="C2:C11", formula="=A2/B2")
        self.assertIn("#DIV/0!", out)
        self.assertIn("C4 #DIV/0! (=A4/B4)", out)
        out = self.b.tool("write_cells", sheet=aid, cells={"D2": '=IFERROR(VLOOKUP(99,A2:B11,2,FALSE),"")'})
        self.assertIn('IFERROR(…, "")', out)
        out = self.b.tool("write_cells", sheet=aid, cells={"C7": 42})
        self.assertIn("C7 is a typed number in C2:C11 where the rest are formulas", out)
        d = self.b.tool("describe_sheet", sheet=aid)
        self.assertIn("Errors (", d)

    def test_edit_ops(self):
        aid, _ = self.new_sheet()
        rows = [["Name", "Score", "Double"], ["b", 2, "=B2*2"], ["a", 9, "=B3*2"], ["c", 5, "=B4*2"]]
        self.b.tool("write_cells", sheet=aid, rows=rows)
        out = self.b.tool("edit_sheet", sheet=aid, op="sort", range="A1:C4", by="B", descending=True)
        self.assertIn("row 1 kept as the header", out)
        self.assertEqual(tsv_row(out, 2), ["a", "9", "18"])
        self.assertEqual(tsv_row(out, 4), ["b", "2", "4"])
        f = self.b.tool("read_cells", sheet=aid, what="formulas")
        self.assertEqual(tsv_row(f, 2)[2], "=B2*2")
        self.b.tool("edit_sheet", sheet=aid, op="freeze", rows=1, cols=1)
        out = self.b.tool("edit_sheet", sheet=aid, op="insert_rows", at=2, count=2)
        self.assertIn("Inserted 2 empty rows at row 2", out)
        f = self.b.tool("read_cells", sheet=aid, what="formulas")
        self.assertEqual(tsv_row(f, 4)[2], "=B4*2")
        self.b.tool("edit_sheet", sheet=aid, op="delete_rows", at="2", count=2)
        self.b.tool("edit_sheet", sheet=aid, op="insert_cols", at="B")
        out = self.b.tool("read_cells", sheet=aid, what="formulas")
        self.assertEqual(tsv_row(out, 2), ["a", "", "9", "=C2*2"])
        self.b.tool("edit_sheet", sheet=aid, op="delete_cols", at="B")
        self.b.tool("edit_sheet", sheet=aid, op="add_sheet", name="Notes")
        d = self.b.tool("describe_sheet", sheet=aid)
        self.assertIn("frozen 1 rows, 1 columns", d)
        self.assertIn('Sheet "Notes": empty.', d)
        out = self.b.tool("edit_sheet", sheet=aid, op="delete_sheet", name="Notes")
        self.assertIn("sheets are now: Sheet1", out)
        bad = self.b.tool("edit_sheet", sheet=aid, op="delete_sheet", name="Sheet1")
        self.assertIn("only sheet", bad)
        bad = self.b.tool("edit_sheet", sheet=aid, op="explode")
        self.assertTrue(bad.startswith("Error: `op` should be one of"))

    def test_bad_input_is_a_sentence(self):
        aid, _ = self.new_sheet()
        self.assertIn("isn't a cell or range", self.b.tool("read_cells", sheet=aid, range="hello world"))
        self.assertIn("No sheet named 'Nope'", self.b.tool("read_cells", sheet=aid, sheet_name="Nope"))
        self.assertIn("No spreadsheet 'nothing-here'", self.b.tool("describe_sheet", sheet="nothing-here"))
        self.assertIn("Give `rows`", self.b.tool("write_cells", sheet=aid))
        self.assertIn("`fill` should be a colour", self.b.tool("format_cells", sheet=aid, range="A1", fill="blurple"))
        # stringified JSON rows are accepted
        out = self.b.tool("write_cells", sheet=aid, rows='[["x", 1]]')
        self.assertIn("Wrote 2 cells", out)

    def test_format_then_view(self):
        aid, _ = self.new_sheet()
        self.b.tool("write_cells", sheet=aid, rows=[["Amount", "Share"], [1234.5, 0.25], ["=A2*2", 0.5]])
        out = self.b.tool("format_cells", sheet=aid, range="A2:A3", number_format="#,##0.00", bold=True, fill="#ffe599")
        self.assertIn("A2 1,234.50", out)
        self.b.tool("format_cells", sheet=aid, range="B:B", number_format="0%", width=20)
        self.b.tool("format_cells", sheet=aid, range="1:1", bold=True)
        g = grid_of(self.b.view(aid))
        self.assertEqual(cell(g, 1, 0), {"v": "1,234.50", "t": "n", "i": "1234.5", "b": True, "fill": "#ffe599"})
        self.assertEqual(cell(g, 2, 0)["i"], "=A2*2")
        self.assertEqual(cell(g, 2, 0)["v"], "2,469.00")
        self.assertEqual(cell(g, 1, 1)["v"], "25%")
        self.assertEqual(cell(g, 0, 0), {"v": "Amount", "b": True})
        self.assertEqual(g["widths"], {"1": 150.0})
        self.assertTrue(g["editable"])
        self.assertGreaterEqual(g["rows"], 100)
        self.assertGreaterEqual(g["cols"], 26)

    def test_view_frozen_strips_and_window(self):
        aid, _ = self.new_sheet()
        rows = [["H" + str(c) for c in range(40)]] + [[r * 100 + c for c in range(40)] for r in range(1, 300)]
        self.b.tool("write_cells", sheet=aid, rows=rows)
        self.b.tool("edit_sheet", sheet=aid, op="freeze", rows=1, cols=2)
        g = grid_of(self.b.view(aid))
        self.assertEqual((g["frozen_rows"], g["frozen_cols"]), (1, 2))
        self.assertEqual(len(g["blocks"]), 1)  # the window starts at the corner
        self.assertEqual(g["rows"], 350)
        msgs = self.b.event(aid, "window", row=150, col=20, rows=50, cols=15)
        views = [m for m in msgs if m.get("type") == "view"]
        self.assertTrue(views)
        g = grid_of(views[-1]["view"])
        self.assertEqual(len(g["blocks"]), 4)
        main = g["blocks"][0]
        self.assertEqual((main["row"], main["col"], len(main["cells"]), len(main["cells"][0])), (150, 20, 50, 15))
        self.assertEqual(cell(g, 150, 20)["v"], str(150 * 100 + 20))
        self.assertEqual(cell(g, 0, 25), "H25")  # the top strip
        self.assertEqual(cell(g, 160, 1)["v"], str(160 * 100 + 1))  # the left strip
        self.assertEqual(cell(g, 0, 0), "H0")  # the corner
        # the window is kept per artifact: a new version opens at the same place
        self.b.tool("write_cells", sheet=aid, cells={"A2": 1})
        g = grid_of(self.b.view(aid))
        self.assertEqual(g["blocks"][0]["row"], 150)

    def test_grid_edits_save_drafts_and_are_reported(self):
        aid, _ = self.new_sheet()
        self.b.tool("write_cells", sheet=aid, rows=[["x", "y"], [1, "=A2*10"]])
        msgs = self.b.event(aid, "edit", row=1, col=0, value="5")
        arts = [m for m in msgs if m.get("type") == "artifact"]
        self.assertEqual(len(arts), 1)
        self.assertTrue(arts[0]["draft"])
        self.assertEqual(arts[0]["id"], aid)
        self.assertEqual(arts[0]["note"], "Edited in the grid")
        v3 = self.b.meta(aid)["versions"][-1]["n"]
        self.b.event(aid, "edit", row=2, col=0, value="=B2+1")
        self.assertEqual(self.b.meta(aid)["versions"][-1]["n"], v3)  # one draft version
        g = grid_of(self.b.view(aid))
        self.assertEqual(cell(g, 1, 1)["v"], "50")
        self.assertEqual(cell(g, 2, 0)["v"], "51")
        self.b.event(aid, "paste", row=4, col=0, text="a\tb\n1\t2\n")
        self.b.event(aid, "clear", row=4, col=0, to_row=4, to_col=1)
        r = self.b.tool("read_cells", sheet=aid)
        self.assertIn("The user changed A2, A3, A5:B6, A5:B5 in the grid", r)
        self.assertEqual(tsv_row(r, 6), ["1", "2"])
        out = self.b.tool("write_cells", sheet=aid, cells={"C1": "z"})
        self.assertIn("The user changed", out)
        out = self.b.tool("write_cells", sheet=aid, cells={"C2": "z"})
        self.assertNotIn("The user changed", out)
        # an older version is read-only and shows what it had
        g = grid_of(self.b.view(aid, version=2))
        self.assertFalse(g["editable"])
        self.assertEqual(cell(g, 1, 0)["v"], "1")
        msgs = self.b.event(aid, "edit", version=2, row=1, col=0, value="7")
        self.assertFalse([m for m in msgs if m.get("type") == "artifact"])

    def test_stale_key_keeps_unsaved_grid_edits(self):
        aid = self.b.add_artifact("t.csv", b"a,b\n1,2\n")
        self.b.view(aid)
        self.b.event(aid, "edit", row=1, col=0, value="10")  # draft: version 2
        self.assertEqual(self.b.meta(aid)["versions"][-1]["n"], 2)
        # the client still draws its old key (byob said version 1 was the latest then)
        g = grid_of(self.b.view(aid, version=1, stale=True))
        self.assertEqual(cell(g, 1, 0)["v"], "10")
        self.b.event(aid, "edit", version=1, stale=True, row=1, col=1, value="20")
        r = self.b.tool("read_cells", sheet=aid)
        self.assertEqual(tsv_row(r, 2), ["10", "20"])
        self.assertEqual(self.b.meta(aid)["filename"], "t.xlsx")

    def test_tabs(self):
        aid, _ = self.new_sheet()
        self.b.tool("write_cells", sheet=aid, start="Two!B2", rows=[["hi"]])
        tree = self.b.view(aid)
        tabs = [n for n in tree["children"] if n.get("key") == "tabs"][0]
        self.assertEqual([b["action"] for b in tabs["children"]], ["tab:Sheet1", "tab:Two"])
        msgs = self.b.event(aid, "tab", action="tab:Two")
        g = grid_of([m for m in msgs if m.get("type") == "view"][-1]["view"])
        self.assertEqual(cell(g, 1, 1), "hi")

    def test_csv_workspace_file_becomes_a_new_artifact(self):
        path = os.path.join(self.b.ws, "data", "sales.csv")
        os.makedirs(os.path.dirname(path))
        with open(path, "w") as f:
            f.write("region,units,zip\nNorth,10,02134\nSouth,20,01001\n")
        d = self.b.tool("describe_sheet", sheet="data/sales.csv")
        self.assertIn('C "zip": text', d)  # leading zeros stay text
        out = self.b.tool("write_cells", sheet="data/sales.csv", cells={"D1": "double", "D2": "=B2*2"})
        self.assertIn("Saved as a new spreadsheet artifact (sales.xlsx); data/sales.csv itself is unchanged", out)
        aid = self.b.last_id
        self.assertEqual(self.b.meta(aid)["filename"], "sales.xlsx")
        self.assertEqual(self.b.meta(aid)["content_type"], XLSX)
        with open(path) as f:
            self.assertNotIn("double", f.read())
        # the path now leads to that artifact
        out = self.b.tool("write_cells", sheet="data/sales.csv", cells={"D3": "=B3*2"})
        self.assertIn(f"was saved as artifact {aid} earlier", out)
        self.assertEqual(self.b.last_id, aid)
        r = self.b.tool("read_cells", sheet=aid, range="D1:D3")
        self.assertEqual([tsv_row(r, i) for i in (1, 2, 3)], [["double"], ["20"], ["40"]])

    def test_csv_artifact_becomes_xlsx_on_first_write(self):
        aid = self.b.add_artifact("prices.csv", b"item,price\nA,1\nB,2\n")
        g = grid_of(self.b.view(aid))
        self.assertEqual(cell(g, 1, 1), {"v": "1", "t": "n"})
        out = self.b.tool("write_cells", sheet=aid, cells={"B4": "=SUM(B2:B3)"})
        self.assertIn("Saved as prices.xlsx", out)
        self.assertEqual(self.b.last_id, aid)
        self.assertEqual(self.b.meta(aid)["filename"], "prices.xlsx")
        self.assertIn("B4 =SUM(B2:B3) → 3", out)

    def test_create_from_csv(self):
        with open(os.path.join(self.b.ws, "t.tsv"), "w") as f:
            f.write("a\tb\n1\t2\n")
        out = self.b.tool("create_sheet", title="T", **{"from": "t.tsv"})
        self.assertIn("Imported t.tsv", out)
        self.assertIn("used range A1:B2", out)

    def test_describe_500_row_csv_and_paging(self):
        rnd = random.Random(3)
        lines = ["date,category,amount,paid"]
        for i in range(500):
            lines.append(f"2024-{1 + i % 12:02d}-{1 + i % 28:02d},{rnd.choice(['Food', 'Rent', 'Fun'])},{rnd.randint(1, 900)},{rnd.choice(['TRUE', 'FALSE'])}")
        with open(os.path.join(self.b.ws, "big.csv"), "w") as f:
            f.write("\n".join(lines) + "\n")
        d = self.b.tool("describe_sheet", sheet="big.csv")
        self.assertIn("used range A1:D501 (501 rows × 4 columns)", d)
        self.assertIn("Header in row 1; data in rows 2–501.", d)
        self.assertIn('A "date": date, 500 values, 2024-01-01 … 2024-12-', d)
        self.assertRegex(d, r'B "category": text, 500 values, 3 distinct: \w+ \(\d+\)')
        self.assertRegex(d, r'C "amount": number, 500 values, min \d+, max \d+, sum \d+')
        self.assertIn('D "paid": bool', d)
        r = self.b.tool("read_cells", sheet="big.csv")
        self.assertIn("rows 1–200 of 1–501", r)
        self.assertIn('More rows: read_cells(range="A201:D501")', r)
        r = self.b.tool("read_cells", sheet="big.csv", range="A201:D501")
        self.assertIn('More rows: read_cells(range="A401:D501")', r)
        wide = self.b.tool("read_cells", sheet="big.csv", range="a1:b2")
        self.assertEqual(wide.splitlines()[1], "\tA\tB")


class NoIronCalcTests(Base):
    no_ironcalc = True

    def test_fallback(self):
        faults = [m for m in self.b.msgs if m.get("type") == "fault"]
        with open(os.path.join(self.b.ws, "t.csv"), "w") as f:
            f.write("name,qty,day\napple,3,2024-01-02\npear,5,2024-01-03\n")
        d = self.b.tool("describe_sheet", sheet="t.csv")
        faults = [m for m in self.b.msgs if m.get("type") == "fault"]
        self.assertTrue(faults and "ironcalc" in faults[0]["text"])
        self.assertIn('B "qty": number, 2 values, min 3, max 5, sum 8', d)
        self.assertIn('C "day": date', d)
        r = self.b.tool("read_cells", sheet="t.csv", range="A2:B3")
        self.assertEqual(tsv_row(r, 3), ["pear", "5"])
        out = self.b.tool("write_cells", sheet="t.csv", cells={"A1": 1})
        self.assertIn("Spreadsheets need the ironcalc Python package", out)
        self.assertIn("csv files can still be read with read_cells", out)
        self.assertIn("Spreadsheets need the ironcalc", self.b.tool("create_sheet", title="x"))
        # csv artifacts show read-only; xlsx ones a warning
        aid = self.b.add_artifact("t.csv", b"a,b\n1,2\n")
        tree = self.b.view(aid)
        g = grid_of(tree)
        self.assertFalse(g["editable"])
        self.assertEqual(cell(g, 1, 1), {"v": "2", "t": "n"})
        self.assertTrue(any("ironcalc" in t for t in texts_of(tree)))
        xid = self.b.add_artifact("x.xlsx", b"PK not really")
        tree = self.b.view(xid)
        self.assertIsNone(grid_of(tree))
        self.assertTrue(any("ironcalc" in t for t in texts_of(tree)))


@needs_ic
@unittest.skipUnless(os.environ.get("BYOB_SHEETS_BENCH"), "set BYOB_SHEETS_BENCH=1 for timings")
class Bench(Base):
    def test_timings(self):
        aid, _ = self.new_sheet("Bench")
        rows = []
        for r in range(1, 1001):
            row = [r]
            for c in range(2, 101):
                prev = ""
                n = c - 1
                while n:
                    n, k = divmod(n - 1, 26)
                    prev = chr(65 + k) + prev
                row.append(f"={prev}{r}+1")
            rows.append(row)
        t = time.perf_counter()
        out = self.b.tool("write_cells", sheet=aid, rows=rows)
        write_s = time.perf_counter() - t
        self.assertIn("Wrote 100000 cells", out)
        script = (
            "import sys, time; sys.path.insert(0, %r); import sheets_book as book, backend\n"
            "b = book.Book.load(%r)\n"
            "t = time.perf_counter(); b.evaluate(); ev = time.perf_counter() - t\n"
            "t = time.perf_counter(); data = b.to_xlsx(); sv = time.perf_counter() - t\n"
            "st = backend.ViewState(); n = 20\n"
            "t = time.perf_counter()\n"
            "for _ in range(n): backend.grid(b, 0, st, True)\n"
            "dr = (time.perf_counter() - t) / n\n"
            "t = time.perf_counter(); backend.errors(b); er = time.perf_counter() - t\n"
            "print(f'recalc 100k formulas: {ev*1000:.1f} ms; to_xlsx: {sv*1000:.1f} ms ({len(data)} bytes); '\n"
            "      f'draw 100x26 window: {dr*1000:.2f} ms; error scan: {er*1000:.1f} ms')\n"
        ) % (PLUGIN, self.b.artifact(aid)["path"])
        env = dict(os.environ, PYTHONPATH=HELPER + os.pathsep + PLUGIN, PYTHONDONTWRITEBYTECODE="1")
        r = subprocess.run([PYTHON, "-B", "-c", script], capture_output=True, text=True, env=env, cwd=tempfile.gettempdir())
        print(f"\nwrite_cells of 100k formulas (round trip incl. report + save): {write_s * 1000:.0f} ms")
        print(r.stdout or r.stderr)
        t = time.perf_counter()
        self.b.view(aid)
        print(f"view request round trip: {(time.perf_counter() - t) * 1000:.1f} ms")


if __name__ == "__main__":
    unittest.main()
