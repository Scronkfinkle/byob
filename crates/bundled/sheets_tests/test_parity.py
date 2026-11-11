"""Parity of the two sheets backends: the Python plugin (backend.py) and its native twin
(crates/sheets, plugin `sheets-native`) get the same calls, and every reply, view tree and
message must be the same, character for character (temp file paths and workspace folders
aside).

Needs both: BYOB_SHEETS_PARITY=/path/to/byob-sheets-backend and a Python with ironcalc
(BYOB_SHEETS_PYTHON); skipped otherwise.
"""

import json
import os
import random
import unittest

import test_sheets as t

NATIVE = os.environ.get("BYOB_SHEETS_PARITY")

# (name, formula): values are compared, not expected answers (both run IronCalc 0.9.1)
BATTERY = [
    ("SUM", "=SUM(Data!C2:C11)"), ("AVERAGE", "=AVERAGE(Data!C2:C11)"),
    ("IF", '=IF(SUM(Data!C2:C11)>100,"big","small")'), ("VLOOKUP", '=VLOOKUP("Banana",Data!B2:D11,2,FALSE)'),
    ("XLOOKUP", '=XLOOKUP("West",Data!A2:A11,Data!C2:C11)'), ("INDEX/MATCH", '=INDEX(Data!C2:C11,MATCH("East",Data!A2:A11,0))'),
    ("SUMIFS", '=SUMIFS(Data!C2:C11,Data!A2:A11,"North",Data!B2:B11,"Cherry")'),
    ("COUNTIFS", '=COUNTIFS(Data!A2:A11,"South",Data!C2:C11,">10")'), ("MAXIFS", '=MAXIFS(Data!C2:C11,Data!A2:A11,"South")'),
    ("TEXT num", '=TEXT(1234.5,"#,##0.00")'), ("TEXT date", '=TEXT(DATE(2026,3,1),"yyyy-mm-dd")'),
    ("DATE diff", "=DATE(2026,3,1)-DATE(2026,1,5)"), ("EOMONTH", "=EOMONTH(DATE(2026,2,10),0)"),
    ("NETWORKDAYS", "=NETWORKDAYS(DATE(2026,1,1),DATE(2026,1,31))"), ("SUMPRODUCT", "=SUMPRODUCT(Data!C2:C11,Data!D2:D11)"),
    ("PMT/ROUND", "=ROUND(PMT(0.05/12,360,200000),2)"), ("TEXTJOIN", '=TEXTJOIN(",",TRUE,"a","","b")'),
    ("IFERROR", '=IFERROR(1/0,"err")'), ("LEN/LEFT", '=LEN(LEFT("spreadsheet",5))'), ("CONCAT", '=CONCAT(Data!A2,"-",Data!B2)'),
    ("ISERROR", '=ISERROR(VLOOKUP("Kiwi",Data!B2:D11,2,FALSE))'), ("INDIRECT", '=INDIRECT("Data!C3")'),
    ("OFFSET", "=SUM(OFFSET(Data!C2,0,0,3,1))"), ("array math", "=SUM(Data!C2:C11*Data!D2:D11)"), ("LET", "=LET(x,2,x*3)"),
    ("SEQUENCE", "=SUM(SEQUENCE(10))"), ("UNIQUE", "=ROWS(UNIQUE(Data!A2:A11))"),
    ("FILTER", '=SUM(FILTER(Data!C2:C11,Data!A2:A11="North"))'), ("SORT", "=INDEX(SORT(UNIQUE(Data!A2:A11)),2)"),
    ("div0", "=1/0"), ("na", '=VLOOKUP("Kiwi",Data!B2:D11,2,FALSE)'), ("ref", "=INDEX(Data!C2:C11,99)"),
    ("name", "=NOPE(1)"), ("bool", "=Data!C2>5"), ("pct", "=Data!C2/Data!C3"), ("big", "=10^20"), ("small", "=1/3/10^6"),
    ("spill", "=SORT(UNIQUE(Data!A2:A11))"),
]
DATA = [["Region", "Fruit", "Units", "Price", "Day"],
        ["North", "Apple", 10, 1.5, "2026-01-05"], ["South", "Banana", 20, 0.5, "2026-01-10"],
        ["North", "Banana", 5, 0.5, "2026-02-01"], ["East", "Apple", 8, 1.5, "2026-02-15"],
        ["South", "Cherry", 12, 3, "2026-03-01"], ["North", "Cherry", 7, 3, "2026-03-12"],
        ["East", "Banana", 15, 0.5, "2026-03-20"], ["West", "Apple", 9, 1.5, "2026-04-02"],
        ["West", "Cherry", 4, 3, "2026-04-18"], ["South", "Apple", 11, 1.5, "2026-05-01"]]


def _norm(v, ws):
    """Temp files and the workspace folder differ between the two runs."""
    if isinstance(v, str):
        v = v.replace(ws, "<ws>")
        if "byob-sheets-" in v and os.path.isabs(v):
            return "<tmp>/" + os.path.basename(v).split("-", 1)[-1]
        return v
    if isinstance(v, list):
        return [_norm(x, ws) for x in v]
    if isinstance(v, dict):
        return {k: _norm(x, ws) for k, x in v.items()}
    return v


@unittest.skipUnless(NATIVE and t.HAS_IC, "set BYOB_SHEETS_PARITY to the native backend and BYOB_SHEETS_PYTHON to a Python with ironcalc")
class Parity(unittest.TestCase):
    def setUp(self):
        self.py = t.Byob()
        self.rs = t.Byob(native=NATIVE)
        self.calls = 0

    def tearDown(self):
        self.py.close()
        self.rs.close()

    def both(self, fn):
        """fn(byob) on both; the results (and the messages they caused) must match."""
        starts = (len(self.py.msgs), len(self.rs.msgs))
        a, b = fn(self.py), fn(self.rs)
        self.calls += 1
        na, nb = _norm(a, self.py.ws), _norm(b, self.rs.ws)
        self.assertEqual(na, nb, f"call {self.calls}")
        ma = _norm(self.py.msgs[starts[0]:], self.py.ws)
        mb = _norm(self.rs.msgs[starts[1]:], self.rs.ws)
        self.assertEqual(ma, mb, f"messages of call {self.calls}")
        return a

    def tool(self, tool_name, **args):
        return self.both(lambda b: b.tool(tool_name, **args))

    def files(self, name, data):
        for b in (self.py, self.rs):
            path = os.path.join(b.ws, name)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "wb") as f:
                f.write(data)

    def test_tools_and_views_match(self):
        self.tool("create_sheet", title="Fruit sales")
        aid = self.py.last_id
        self.assertEqual(aid, self.rs.last_id)
        self.tool("write_cells", sheet=aid, rows=DATA)
        self.tool("edit_sheet", sheet=aid, op="rename_sheet", name="Sheet1", new_name="Data")
        self.tool("format_cells", sheet=aid, range="E2:E11", number_format="yyyy-mm-dd")
        rows = [["Name", "Formula", "Value"]] + [[n, "'" + f, f] for n, f in BATTERY]
        self.tool("write_cells", sheet=aid, start="Calc!A1", rows=rows)
        self.tool("read_cells", sheet=aid, sheet_name="Calc", range="A1:C60")
        self.tool("read_cells", sheet=aid, sheet_name="Calc", what="formulas")
        self.tool("describe_sheet", sheet=aid)
        self.tool("fill", sheet=aid, range="F2:F11", formula="=C2*D2", sheet_name="Data")
        self.tool("fill", sheet=aid, range="G2:G", formula="=F2/SUM($F$2:$F$11)", sheet_name="Data")
        self.tool("write_cells", sheet=aid, cells={"Data!C3": 25, "Data!H2": '=IFERROR(1/0,"")'})
        self.tool("fill", sheet=aid, range="I2:I11", formula="=C2+1", sheet_name="Data")
        self.tool("write_cells", sheet=aid, cells={"Data!I6": 3})
        self.tool("format_cells", sheet=aid, range="Data!F:G", number_format="0.0%", bold=True, fill="#ffe599",
                  align="right", width=14)
        self.tool("format_cells", sheet=aid, range="Data!1:1", bold=True, fill="none")
        self.tool("edit_sheet", sheet=aid, op="sort", range="Data!A1:I11", by="Units", descending=True)
        self.tool("edit_sheet", sheet=aid, op="freeze", rows=1, cols=1, sheet_name="Data")
        self.tool("edit_sheet", sheet=aid, op="insert_rows", at=3, count=2, sheet_name="Data")
        self.tool("edit_sheet", sheet=aid, op="delete_cols", at="H", sheet_name="Data")
        self.tool("edit_sheet", sheet=aid, op="add_sheet", name="Notes")
        self.tool("describe_sheet", sheet=aid)
        # views: open, page, select, tab, edit, paste, clear; an older version read-only
        self.both(lambda b: b.view(aid))
        self.both(lambda b: b.event(aid, "window", row=0, col=0, rows=60, cols=12))
        self.both(lambda b: b.event(aid, "tab", action="tab:Data"))
        self.both(lambda b: b.event(aid, "select", row=2, col=1, to_row=4, to_col=3))
        self.both(lambda b: b.event(aid, "edit", row=1, col=2, value="=1+1"))
        self.both(lambda b: b.event(aid, "paste", row=20, col=0, text="a\tb\n1\t=A22*2\n"))
        self.both(lambda b: b.event(aid, "clear", row=20, col=0, to_row=20, to_col=1))
        self.both(lambda b: b.view(aid, version=2))
        self.tool("read_cells", sheet=aid, sheet_name="Data")
        # mistakes are the same sentences
        for args in [dict(sheet=aid, range="hello world"), dict(sheet=aid, sheet_name="Nope"), dict(sheet="nothing-here"),
                     dict(sheet=aid, range="A0"), dict(sheet=aid, range="'x!A1")]:
            self.tool("read_cells", **args)
        self.tool("write_cells", sheet=aid)
        self.tool("write_cells", sheet=aid, rows='[["x", 1]]')
        self.tool("write_cells", sheet=aid, rows="a\tb\nc\td")
        self.tool("format_cells", sheet=aid, range="A1", fill="blurple")
        self.tool("format_cells", sheet=aid, range="A1", width=999)
        self.tool("edit_sheet", sheet=aid, op="explode")
        self.tool("edit_sheet", sheet=aid, op="delete_rows", at="x")
        self.tool("edit_sheet", sheet=aid, op="sort", by="Q", range="Data!A1:C4")
        self.tool("fill", sheet=aid, range="Z1:Z2")

    def test_csv_files_and_profiles_match(self):
        rnd = random.Random(5)
        lines = ["date,category,amount,paid,zip,note"]
        for i in range(400):
            lines.append(f"2024-{1 + i % 12:02d}-{1 + i % 28:02d},{rnd.choice(['Food', 'Rent', 'Fun', 'Travel'])},"
                         f"{rnd.randint(1, 900)}.{rnd.randint(0, 99):02d},{rnd.choice(['TRUE', 'FALSE'])},"
                         f"0{rnd.randint(1000, 9999)},\"x, {i}\"")
        self.files("data/big.csv", ("\n".join(lines) + "\n").encode())
        self.files("semi.csv", b"a;b;c\n1;2;3\n4;5;6\n")
        self.files("t.tsv", b"k\tv\nx\t1\ny\t\n")
        for name in ("data/big.csv", "semi.csv", "t.tsv"):
            self.tool("describe_sheet", sheet=name)
            self.tool("read_cells", sheet=name)
        self.tool("read_cells", sheet="data/big.csv", range="A201:F")
        self.tool("write_cells", sheet="data/big.csv", cells={"G1": "double", "G2": "=C2*2"})
        aid = self.py.last_id
        self.tool("fill", sheet="data/big.csv", range="G2:G", formula="=C2*2")
        self.tool("edit_sheet", sheet=aid, op="sort", by="C")
        self.tool("describe_sheet", sheet=aid)
        self.tool("create_sheet", title="T", **{"from": "t.tsv"})
        self.both(lambda b: b.view(b.last_id))


if __name__ == "__main__":
    unittest.main()
