"""A1 references: parsing forgiving ranges, and shifting formulas like Excel's fill and copy.

Pure Python (no ironcalc), so the CSV fallback can use it too. Rows and columns are 1-based
here, like IronCalc's API.
"""

import re

MAX_ROWS = 1048576
MAX_COLS = 16384


class RangeError(ValueError):
    """A range or cell reference the model wrote that we can't read (the message says why)."""


def col_num(letters):
    n = 0
    for ch in letters.upper():
        n = n * 26 + (ord(ch) - 64)
    return n


def col_name(n):
    s = ""
    while n > 0:
        n, r = divmod(n - 1, 26)
        s = chr(65 + r) + s
    return s


def cell_name(row, col):
    return f"{col_name(col)}{row}"


def range_name(r1, c1, r2, c2):
    a = cell_name(r1, c1)
    return a if (r1, c1) == (r2, c2) else f"{a}:{cell_name(r2, c2)}"


def quote_sheet(name):
    if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_.]*", name) and not re.fullmatch(r"[A-Za-z]{1,3}\d+", name):
        return name
    return "'" + name.replace("'", "''") + "'"


def split_sheet(text):
    """'Sheet 2'!A1:B3 -> ("Sheet 2", "A1:B3"); A1 -> (None, "A1")."""
    text = text.strip()
    if text.startswith("'"):
        end = 1
        while True:
            q = text.find("'", end)
            if q < 0:
                raise RangeError(f"{text!r} has an unclosed quote around the sheet name.")
            if text[q + 1:q + 2] == "'":
                end = q + 2
                continue
            name = text[1:q].replace("''", "'")
            rest = text[q + 1:]
            if not rest.startswith("!"):
                raise RangeError(f"{text!r}: expected ! after the sheet name, like 'My Sheet'!A1:B4.")
            return name, rest[1:].strip()
    if "!" in text:
        name, rest = text.rsplit("!", 1)
        return name.strip(), rest.strip()
    return None, text


_CELL = re.compile(r"\$?([A-Za-z]{1,3})\$?(\d+)")
_COL = re.compile(r"\$?([A-Za-z]{1,3})")
_ROW = re.compile(r"\$?(\d+)")


def _part(p, whole):
    """One side of a range: (row|None, col|None)."""
    m = _CELL.fullmatch(p)
    if m:
        return int(m.group(2)), col_num(m.group(1))
    m = _COL.fullmatch(p)
    if m:
        return None, col_num(m.group(1))
    m = _ROW.fullmatch(p)
    if m:
        return int(m.group(1)), None
    raise RangeError(f"{whole!r} isn't a cell or range; write it like A1, B2:D9, B:B, 3:3 or Sheet2!A1:C5.")


def parse_range(text):
    """A forgiving A1 range -> (sheet|None, r1, c1, r2, c2); open sides are None
    ("B:B" has no rows, "2:2" no columns, "A2:A" no last row)."""
    if not isinstance(text, str) or not text.strip():
        raise RangeError("The range is empty; write it like A1:C10.")
    sheet, ref = split_sheet(text)
    ref = ref.replace(" ", "").replace("..", ":")
    parts = ref.split(":")
    if len(parts) > 2 or not all(parts):
        raise RangeError(f"{text!r} isn't a cell or range; write it like A1, B2:D9, B:B, 3:3 or Sheet2!A1:C5.")
    r1, c1 = _part(parts[0], text)
    r2, c2 = _part(parts[-1], text)
    if len(parts) == 1 and (r1 is None or c1 is None):
        # "B" alone is the column, "3" alone the row
        r2, c2 = r1, c1
    if r1 is not None and r2 is not None and r2 < r1:
        r1, r2 = r2, r1
    if c1 is not None and c2 is not None and c2 < c1:
        c1, c2 = c2, c1
    for r in (r1, r2):
        if r is not None and not 1 <= r <= MAX_ROWS:
            raise RangeError(f"{text!r}: rows go from 1 to {MAX_ROWS}.")
    for c in (c1, c2):
        if c is not None and not 1 <= c <= MAX_COLS:
            raise RangeError(f"{text!r}: columns go from A to XFD.")
    return sheet, r1, c1, r2, c2


def parse_cell(text):
    """A single cell -> (sheet|None, row, col)."""
    sheet, r1, c1, r2, c2 = parse_range(text)
    if r1 is None or c1 is None:
        raise RangeError(f"{text!r} isn't a single cell; write it like B2.")
    return sheet, r1, c1


def parse_col(value):
    """A column given as a letter ("C"), a number (3) or a cell ("C1") -> 3."""
    if isinstance(value, bool):
        raise RangeError(f"{value!r} isn't a column; give a letter like C.")
    if isinstance(value, (int, float)):
        n = int(value)
    else:
        s = str(value).strip().replace("$", "")
        if s.isdigit():
            n = int(s)
        else:
            m = re.fullmatch(r"([A-Za-z]{1,3})\d*", s)
            if not m:
                raise RangeError(f"{value!r} isn't a column; give a letter like C.")
            n = col_num(m.group(1))
    if not 1 <= n <= MAX_COLS:
        raise RangeError(f"{value!r} isn't a column; columns go from A to XFD.")
    return n


def parse_row(value):
    if isinstance(value, bool):
        raise RangeError(f"{value!r} isn't a row number.")
    try:
        n = int(str(value).strip())
    except ValueError:
        m = _CELL.fullmatch(str(value).strip())
        if not m:
            raise RangeError(f"{value!r} isn't a row number; rows are 1, 2, 3, ...") from None
        n = int(m.group(2))
    if not 1 <= n <= MAX_ROWS:
        raise RangeError(f"{value!r}: rows go from 1 to {MAX_ROWS}.")
    return n


# One token of a formula: a string literal, a quoted sheet name (both kept as they are), a
# cell reference, a column range (B:D) or a row range (2:5).
_REF = re.compile(r"""
    "(?:[^"]|"")*"
  | '(?:[^']|'')*'
  | (?<![A-Za-z0-9_.$])(?P<cd>\$?)(?P<c>[A-Za-z]{1,3})(?P<rd>\$?)(?P<r>\d+)(?![A-Za-z0-9_(!])
  | (?<![A-Za-z0-9_.$])(?P<ad>\$?)(?P<a>[A-Za-z]{1,3}):(?P<bd>\$?)(?P<b>[A-Za-z]{1,3})(?![A-Za-z0-9_(!])
  | (?<![A-Za-z0-9_.$:])(?P<pd>\$?)(?P<p>\d+):(?P<qd>\$?)(?P<q>\d+)(?![A-Za-z0-9_(!.:])
""", re.X)


def shift_formula(formula, drow, dcol):
    """Move the relative references of `formula` by (drow, dcol), like copying the cell
    that far in Excel; $ parts stay. A reference pushed off the sheet becomes #REF!."""
    if not isinstance(formula, str) or not formula.startswith("=") or (drow == 0 and dcol == 0):
        return formula

    def col(dollar, letters):
        if dollar:
            return dollar + letters
        n = col_num(letters) + dcol
        if not 1 <= n <= MAX_COLS:
            raise _OffSheet
        return col_name(n)

    def row(dollar, digits):
        if dollar:
            return dollar + digits
        n = int(digits) + drow
        if not 1 <= n <= MAX_ROWS:
            raise _OffSheet
        return str(n)

    def sub(m):
        try:
            if m.group("c") is not None:
                if col_num(m.group("c")) > MAX_COLS:
                    return m.group(0)
                return col(m.group("cd"), m.group("c")) + row(m.group("rd"), m.group("r"))
            if m.group("a") is not None:
                return col(m.group("ad"), m.group("a")) + ":" + col(m.group("bd"), m.group("b"))
            if m.group("p") is not None:
                return row(m.group("pd"), m.group("p")) + ":" + row(m.group("qd"), m.group("q"))
        except _OffSheet:
            return "#REF!"
        return m.group(0)

    return _REF.sub(sub, formula)


class _OffSheet(Exception):
    pass
