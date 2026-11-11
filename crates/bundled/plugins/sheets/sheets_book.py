"""Workbooks: IronCalc's raw Model behind a small interface, and a read-only CSV table for
when ironcalc couldn't be installed. Sheets are 0-based indices, rows and columns 1-based
(IronCalc's convention).
"""

import csv
import io
import os
import re

ic = None
IC_ERROR = None
try:
    if os.environ.get("BYOB_SHEETS_NO_IRONCALC"):
        raise ImportError("BYOB_SHEETS_NO_IRONCALC is set")
    import ironcalc as ic  # noqa: F811
except Exception as e:  # ImportError, or a broken wheel
    ic = None
    IC_ERROR = f"{type(e).__name__}: {e}"

MAX_IMPORT_CELLS = 2_000_000
LOCALE, TZ = "en", "UTC"


class SheetError(Exception):
    """A problem the model can act on; its message is a plain sentence."""


def need_ironcalc():
    if ic is None:
        raise SheetError(
            "Spreadsheets need the ironcalc Python package, which couldn't be installed: "
            f"{IC_ERROR}. Tell the user; csv files can still be read with read_cells.")


_DATE_FMT = re.compile(r'"[^"]*"|\[[^\]]*\]|\\.|([dmyhs])', re.I)


def is_date_format(fmt):
    if not fmt or fmt.lower() == "general":
        return False
    return any(m.group(1) for m in _DATE_FMT.finditer(fmt))


def num_text(v):
    """A number for the model: 12, 0.125, 1234567.5 (no exponent noise)."""
    if isinstance(v, bool):
        return "TRUE" if v else "FALSE"
    if isinstance(v, float) and v.is_integer() and abs(v) < 1e15:
        return str(int(v))
    if isinstance(v, float):
        return f"{v:.10g}"
    return str(v)


def file_format(path):
    low = path.lower()
    if low.endswith((".tsv", ".tab")):
        return "tsv"
    if low.endswith((".csv", ".txt")):
        return "csv"
    return "xlsx"


def read_table(path, fmt=None):
    """Rows of a .csv/.tsv file (lists of strings)."""
    with open(path, "rb") as f:
        raw = f.read()
    text = raw.decode("utf-8-sig", errors="replace")
    if (fmt or file_format(path)) == "tsv":
        delim = "\t"
    else:
        delim = ","
        try:
            delim = csv.Sniffer().sniff(text[:8192], delimiters=",;\t|").delimiter
        except csv.Error:
            pass
    return list(csv.reader(io.StringIO(text), delimiter=delim))


_LEADING_ZERO = re.compile(r"0\d+")
_NUMBER = re.compile(r"[-+]?[$€£]?\s*(\d{1,3}(,\d{3})+|\d+)?(\.\d+)?%?")
_ISO_DATE = re.compile(r"\d{4}-\d{2}-\d{2}([ T]\d{2}:\d{2}(:\d{2})?)?")


class Book:
    """An IronCalc workbook (ironcalc.Model: nothing recalculates until evaluate())."""

    writable = True

    def __init__(self, model):
        self.m = model
        self.gen = 0  # bumped on every change (caches key on it)
        self._names = None

    # -- making one
    @classmethod
    def new(cls, name="Workbook"):
        need_ironcalc()
        return cls(ic.create(name, LOCALE, TZ))

    @classmethod
    def load(cls, path, fmt=None):
        """An .xlsx, .csv or .tsv file (`fmt`: "xlsx", "csv" or "tsv"; else from the suffix)."""
        need_ironcalc()
        if (fmt or file_format(path)) in ("csv", "tsv"):
            b = cls.new(os.path.splitext(os.path.basename(path))[0] or "Workbook")
            b.import_rows(0, read_table(path, fmt))
            b.evaluate()
            return b
        try:
            m = ic.load_from_xlsx(path, LOCALE, TZ)
        except Exception as e:
            raise SheetError(f"{os.path.basename(path)} couldn't be opened as an .xlsx workbook ({e}).") from None
        b = cls(m)
        b.evaluate()
        return b

    def import_rows(self, s, rows, row0=1, col0=1):
        n = sum(len(r) for r in rows)
        if n > MAX_IMPORT_CELLS:
            raise SheetError(f"That table has {n} cells; spreadsheets here hold at most {MAX_IMPORT_CELLS}.")
        for i, row in enumerate(rows):
            for j, v in enumerate(row):
                if v == "":
                    continue
                if _LEADING_ZERO.fullmatch(v):  # ids and zip codes stay text
                    v = "'" + v
                self.m.set_user_input(s, row0 + i, col0 + j, v)
        self.gen += 1

    def to_xlsx(self):
        return self.m.to_xlsx()

    def evaluate(self):
        self.m.evaluate()
        self.gen += 1

    def touch(self):
        self.gen += 1
        self._names = None

    # -- sheets
    def sheet_names(self):
        if self._names is None:
            self._names = [p.get("name", "") for p in self.m.get_worksheets_properties()]
        return self._names

    def used(self, s):
        """(min_row, max_row, min_col, max_col), or None when the sheet is empty."""
        d = self.m.get_sheet_dimensions(s)
        if d == (1, 1, 1, 1) and self.m.is_empty_cell(s, 1, 1):
            return None
        return d

    def frozen(self, s):
        return self.m.get_frozen_rows_count(s), self.m.get_frozen_columns_count(s)

    def set_frozen(self, s, rows, cols):
        self.m.set_frozen_rows_count(s, rows)
        self.m.set_frozen_columns_count(s, cols)
        self.touch()

    def width(self, s, c):
        return self.m.get_column_width(s, c)

    def set_width(self, s, c, w):
        self.m.set_column_width(s, c, w)
        self.touch()

    def cells(self, s):
        """Every non-empty (row, col) of sheet s."""
        return [(r, c) for (sh, r, c) in self.m.get_all_cells() if sh == s]

    def all_cells(self):
        return self.m.get_all_cells()

    # -- cells
    def content(self, s, r, c):
        """What was typed: "=B2*C2", "1200", "Food" ("" when empty)."""
        return self.m.get_cell_content(s, r, c)

    def kind(self, s, r, c):
        """'n', 's', 'b' or 'e' (call only for non-empty cells)."""
        return _KINDS.get(int(self.m.get_cell_type(s, r, c)), "s")

    def value(self, s, r, c):
        return self.m.get_cell_value(s, r, c)

    def text(self, s, r, c):
        """The value as shown: "1,234.50", "12%", "2024-01-05"."""
        return self.m.get_formatted_cell_value(s, r, c)

    def style(self, s, r, c):
        return self.m.get_cell_style(s, r, c)

    def set_style(self, s, r, c, st):
        self.m.set_cell_style(s, r, c, st)

    def is_date(self, s, r, c):
        return is_date_format(self.m.get_cell_style(s, r, c).get("num_fmt"))

    def model_text(self, s, r, c, kind=None):
        """A cell's value for the model: raw numbers (dates as shown), errors as codes."""
        kind = kind or self.kind(s, r, c)
        if kind == "n":
            if self.is_date(s, r, c):
                return self.text(s, r, c)
            return num_text(self.value(s, r, c))
        if kind == "b":
            return num_text(self.value(s, r, c))
        v = self.value(s, r, c)
        return "" if v is None else str(v)

    def put(self, s, r, c, v):
        """Write one value: a str goes through set_user_input ("=…" formula, "12%", "1,200"),
        numbers and bools as they are, "" or None clears."""
        if v is None or v == "":
            self.m.clear_cell_contents(s, r, c)
        elif isinstance(v, bool):
            self.m.update_cell_with_bool(s, r, c, v)
        elif isinstance(v, (int, float)):
            self.m.update_cell_with_number(s, r, c, float(v))
        else:
            self.m.set_user_input(s, r, c, str(v))

    def clear(self, s, r1, c1, r2, c2):
        self.m.range_clear_contents(s, r1, c1, r2, c2)

    # -- structure
    def add_sheet(self, name):
        self.m.add_sheet(name)
        self.touch()

    def rename_sheet(self, s, name):
        self.m.rename_sheet(s, name)
        self.touch()

    def delete_sheet(self, s):
        self.m.delete_sheet(s)
        self.touch()

    def insert_rows(self, s, at, n):
        self.m.insert_rows(s, at, n)
        self.touch()

    def delete_rows(self, s, at, n):
        self.m.delete_rows(s, at, n)
        self.touch()

    def insert_cols(self, s, at, n):
        self.m.insert_columns(s, at, n)
        self.touch()

    def delete_cols(self, s, at, n):
        self.m.delete_columns(s, at, n)
        self.touch()


if ic is not None:
    _KINDS = {int(ic.CellType.Number): "n", int(ic.CellType.Text): "s", int(ic.CellType.LogicalValue): "b",
              int(ic.CellType.ErrorValue): "e"}  # CellType isn't hashable
else:
    _KINDS = {}

_EMPTY_STYLE = {"num_fmt": "general", "fill": {}, "font": {}, "border": {}}


class PlainBook:
    """A .csv/.tsv table read with the csv module (no ironcalc): read-only, one sheet."""

    writable = False

    def __init__(self, rows):
        self.rows = rows
        self.gen = 0
        self.ncols = max((len(r) for r in rows), default=0)
        self._cells = None

    @classmethod
    def load(cls, path, fmt=None):
        fmt = fmt or file_format(path)
        if fmt not in ("csv", "tsv"):
            need_ironcalc()
        return cls(read_table(path, fmt))

    def sheet_names(self):
        return ["Sheet1"]

    def used(self, s):
        cells = self.cells(s)
        if not cells:
            return None
        return (min(r for r, _ in cells), max(r for r, _ in cells),
                min(c for _, c in cells), max(c for _, c in cells))

    def frozen(self, s):
        return 0, 0

    def width(self, s, c):
        return 90.0

    def cells(self, s):
        if self._cells is None:
            self._cells = [(i + 1, j + 1) for i, row in enumerate(self.rows)
                           for j, v in enumerate(row) if v != ""]
        return self._cells

    def all_cells(self):
        return [(0, r, c) for r, c in self.cells(0)]

    def content(self, s, r, c):
        if s != 0 or r < 1 or c < 1 or r > len(self.rows):
            return ""
        row = self.rows[r - 1]
        return row[c - 1] if c <= len(row) else ""

    def _parse(self, t):
        u = t.strip()
        if u.upper() in ("TRUE", "FALSE"):
            return "b", u.upper() == "TRUE"
        if u and _NUMBER.fullmatch(u) and any(ch.isdigit() for ch in u) and not _LEADING_ZERO.fullmatch(u):
            num = u.replace(",", "").replace("$", "").replace("€", "").replace("£", "").replace(" ", "")
            pct = num.endswith("%")
            try:
                v = float(num.rstrip("%"))
            except ValueError:
                return "s", t
            return "n", v / 100 if pct else v
        if u.startswith("#") and u.endswith(("!", "?", "/A")):
            return "e", u
        return "s", t

    def kind(self, s, r, c):
        return self._parse(self.content(s, r, c))[0]

    def value(self, s, r, c):
        t = self.content(s, r, c)
        return self._parse(t)[1] if t else None

    def text(self, s, r, c):
        return self.content(s, r, c)

    def style(self, s, r, c):
        return _EMPTY_STYLE

    def is_date(self, s, r, c):
        return bool(_ISO_DATE.fullmatch(self.content(s, r, c).strip()))

    def model_text(self, s, r, c, kind=None):
        return self.content(s, r, c)

