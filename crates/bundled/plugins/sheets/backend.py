"""byob's sheets plugin: spreadsheet artifacts (.xlsx, .csv, .tsv) with IronCalc.

Tools for the model (describe_sheet, read_cells, write_cells, fill, edit_sheet,
format_cells, create_sheet), and the grid view the artifact pane shows (view:sheet), which
the user can edit (saved as draft versions). Without the ironcalc package, csv/tsv files
can still be described, read and shown.
"""

import functools
import hashlib
import json
import os
import re
import sys
import tempfile
import time

sys.dont_write_bytecode = True  # no __pycache__ in the plugin folder (byob reloads on changes)

import byob_plugin as byob  # noqa: E402

import sheets_book as bk  # noqa: E402
from sheets_book import Book, PlainBook, SheetError, ic, need_ironcalc, num_text  # noqa: E402
from sheets_refs import (MAX_COLS, MAX_ROWS, RangeError, cell_name, col_name, parse_cell, parse_col,
                         parse_range, parse_row, quote_sheet, range_name, shift_formula)  # noqa: E402

XLSX = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
CSV = "text/csv"
TSV = "text/tab-separated-values"
SHEET_TYPES = {XLSX: "xlsx", CSV: "csv", TSV: "tsv"}
SHEET_SUFFIXES = (".xlsx", ".csv", ".tsv", ".tab")

MAX_WRITE_CELLS = 100_000
READ_CELLS, READ_ROWS, READ_COLS = 2000, 200, 100
SHOW_CELLS, SHOW_ROWS = 400, 40  # values shown after a write
MAX_ERRORS = 30
CHAR_WIDTH = 7.5  # column width units per character (IronCalc's default 90 ≈ 12 characters)
DEFAULT_WIDTH = 90.0

_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
_HEX = re.compile(r"#?([0-9A-Fa-f]{6})")


# ---------------------------------------------------------------- errors for the model

def _tool(fn):
    """A backend tool whose expected failures are plain sentences for the model."""
    @functools.wraps(fn)
    def run(**kw):
        try:
            return fn(**kw)
        except (SheetError, RangeError) as e:
            msg = str(e)
        except Exception as e:
            if ic is None or not isinstance(e, ic.WorkbookError):
                raise
            msg = f"The spreadsheet engine refused: {e}."
        err = getattr(byob, "ToolError", None)
        if err is not None:
            raise err(msg) from None
        return "Error: " + msg
    byob.tool(run, name=fn.__name__)
    return fn


# ---------------------------------------------------------------- artifacts on disk

def _workspace():
    ws = byob.context().get("workspace") or ""
    if not ws:
        raise SheetError("No workspace is open, so there are no spreadsheets to work on.")
    return ws


def _art_dir(ws, aid):
    return os.path.join(ws, ".byob", "artifacts", aid)


def _read_meta(ws, aid):
    if not _ID.fullmatch(aid) or ".." in aid:
        return None
    try:
        with open(os.path.join(_art_dir(ws, aid), "artifact.json"), encoding="utf-8") as f:
            meta = json.load(f)
    except (OSError, ValueError):
        return None
    return meta if isinstance(meta, dict) else None


def _metas(ws):
    root = os.path.join(ws, ".byob", "artifacts")
    try:
        ids = sorted(os.listdir(root))
    except OSError:
        return []
    out = []
    for aid in ids:
        m = _read_meta(ws, aid)
        if m is not None:
            m.setdefault("id", aid)
            out.append(m)
    return out


def _latest(ws, meta):
    """(n, absolute path) of the newest version whose file exists, or (0, None)."""
    vs = [v for v in meta.get("versions") or [] if isinstance(v, dict)]
    vs.sort(key=lambda v: v.get("n") if isinstance(v.get("n"), int) else -1, reverse=True)
    for v in vs:
        p = v.get("path")
        if isinstance(p, str) and p:
            ap = p if os.path.isabs(p) else os.path.join(ws, p)
            if os.path.isfile(ap):
                return v.get("n"), ap
    return 0, None


def _sheet_format(content_type, path):
    f = SHEET_TYPES.get((content_type or "").split(";")[0].strip())
    if f:
        return f
    if path and path.lower().endswith(SHEET_SUFFIXES):
        return bk.file_format(path)
    return None


def _is_sheet_meta(meta):
    return _sheet_format(meta.get("content_type"), meta.get("filename") or "") is not None


def _xlsx_name(filename, fallback="sheet"):
    stem = os.path.splitext(os.path.basename(filename or ""))[0] or fallback
    return stem + ".xlsx"


class Source:
    """What a tool's `sheet` argument named: an artifact or a workspace file."""

    def __init__(self, kind, path, fmt, aid=None, meta=None, version=0, rel=None, note=""):
        self.kind, self.path, self.fmt = kind, path, fmt
        self.aid, self.meta, self.version, self.rel, self.note = aid, meta, version, rel, note

    @property
    def label(self):
        if self.kind == "artifact":
            title = self.meta.get("title") or self.aid
            return f'"{title}" ({self.aid}, version {self.version})'
        return self.rel

    def title(self):
        if self.kind == "artifact":
            return self.meta.get("title") or self.aid
        return os.path.splitext(os.path.basename(self.rel))[0]


def _artifact_source(ws, meta, note=""):
    aid = meta.get("id") or ""
    n, path = _latest(ws, meta)
    if path is None:
        raise SheetError(f"Artifact {aid} has no saved version on disk.")
    fmt = _sheet_format(meta.get("content_type"), path)
    if fmt is None:
        raise SheetError(f"{aid} is a {meta.get('content_type') or 'non-spreadsheet'} artifact, not a spreadsheet.")
    return Source("artifact", path, fmt, aid=aid, meta=meta, version=n, note=note)


def _derived(ws, chat, rel):
    """The artifact a write (or create_sheet from=) made from workspace file `rel`, if any."""
    want = f"From {rel}"
    best = None
    for m in _metas(ws):
        vs = m.get("versions") or []
        if vs and isinstance(vs[0], dict) and vs[0].get("note") == want and _is_sheet_meta(m):
            if m.get("chat") == chat:
                return m
            best = best or m
    return best


def _sheet_artifacts(ws, chat):
    return [m for m in _metas(ws) if _is_sheet_meta(m) and (chat is None or m.get("chat") == chat)]


def resolve(arg, follow=True):
    """The Source for a tool's `sheet` argument (an artifact id, a workspace path, or an
    artifact's filename or title)."""
    ws = _workspace()
    chat = byob.context().get("chat")
    if not isinstance(arg, str) or not arg.strip():
        raise SheetError("Give `sheet`: an artifact id (like budget-3f2a) or a workspace path of an .xlsx, .csv or .tsv file.")
    s = arg.strip().strip("\"'`")
    head, _, tail = s.rpartition("@")
    if head and tail.isdigit():
        s = head
    if s.startswith("artifact:"):
        s = s[len("artifact:"):]
    meta = _read_meta(ws, s) if _ID.fullmatch(s) else None
    if meta is not None:
        meta.setdefault("id", s)
        return _artifact_source(ws, meta)
    path = s if os.path.isabs(s) else os.path.join(ws, s)
    if os.path.isfile(path):
        if not path.lower().endswith(SHEET_SUFFIXES):
            raise SheetError(f"{s} isn't a spreadsheet file; use an .xlsx, .csv or .tsv file.")
        rel = os.path.relpath(path, ws)
        if follow:
            d = _derived(ws, chat, rel)
            if d is not None:
                return _artifact_source(ws, d, note=f"({rel} was saved as artifact {d.get('id')} earlier; using that. Pass {d.get('id')} as `sheet` from now on.)")
        return Source("file", path, bk.file_format(path), rel=rel)
    low = s.lower()
    found = [m for m in _metas(ws) if _is_sheet_meta(m) and low in (
        (m.get("filename") or "").lower(), (m.get("title") or "").lower(),
        os.path.splitext(m.get("filename") or "")[0].lower())]
    found.sort(key=lambda m: m.get("chat") != chat)
    if found:
        return _artifact_source(ws, found[0])
    have = _sheet_artifacts(ws, chat)
    hint = ""
    if have:
        hint = " Spreadsheets in this chat: " + ", ".join(
            f"{m.get('id')} ({m.get('filename') or m.get('title')})" for m in have[:10]) + "."
    raise SheetError(f"No spreadsheet {s!r}: it isn't an artifact id or a file in the workspace.{hint} "
                     "To make one, call create_sheet.")


# ---------------------------------------------------------------- workbooks in memory

def _sha(data):
    return hashlib.sha1(data).hexdigest()


def _file_sha(path):
    try:
        with open(path, "rb") as f:
            return _sha(f.read())
    except OSError:
        return None


def _stat(path):
    try:
        st = os.stat(path)
        return (path, st.st_size, st.st_mtime_ns)
    except OSError:
        return None


def _load(path, fmt):
    if ic is None:
        return PlainBook.load(path, fmt)
    return Book.load(path, fmt)


class Entry:
    """The live workbook of one artifact: tool writes and grid edits go into it."""

    def __init__(self, book, sha, stat, version):
        self.book, self.sha, self.stat = book, sha, stat
        self.base = version if isinstance(version, int) else 0  # the version our state builds on
        self.ours = {}  # sha -> when we wrote it (a version byob may not have stored yet)
        self.user_edits = []  # cells the user changed in the grid since the model's last write

    def wrote(self, data, base):
        self.sha = _sha(data)
        self.ours[self.sha] = time.time()
        if len(self.ours) > 32:
            for k in sorted(self.ours, key=self.ours.get)[:-32]:
                del self.ours[k]
        self.stat = None
        if isinstance(base, int):
            self.base = max(self.base, base)

    def user_edited(self, label):
        if label in self.user_edits:
            self.user_edits.remove(label)
        self.user_edits.append(label)
        del self.user_edits[:-200]


_entries = {}  # artifact id -> Entry
_old = {}  # path of an older version -> book (read-only views), newest last
_files = {}  # workspace file path -> (stat, book)


def artifact_entry(aid, path, fmt, version):
    """The live workbook of artifact `aid`, whose latest version (as far as the caller knows)
    is `version` at `path`. The one in memory wins while that file is what we last wrote,
    or older than what our state builds on (byob copies our drafts a moment later, and a
    view may still hold an older key), or one of ours byob is still storing; otherwise
    (a restore, a file changed elsewhere) it is loaded from the file."""
    e = _entries.get(aid)
    sig = _stat(path)
    if e is not None and sig is not None and e.stat == sig:
        return e
    if e is not None:
        v = version if isinstance(version, int) else 0
        sha = _file_sha(path)
        if sha == e.sha:
            e.stat = sig
            e.base = max(e.base, v)
            return e
        if v <= e.base:
            return e
        if sha in e.ours and time.time() - e.ours[sha] < 10:
            return e
    try:
        book = _load(path, fmt)
    except OSError as err:
        raise SheetError(f"Artifact {aid}'s file couldn't be read ({err}).") from None
    e = Entry(book, _file_sha(path), sig, version)
    _entries[aid] = e
    return e


def version_book(path, fmt):
    b = _old.pop(path, None)
    if b is None:
        b = _load(path, fmt)
    _old[path] = b
    while len(_old) > 6:
        _old.pop(next(iter(_old)))
    return b


def file_book(path, fmt):
    sig = _stat(path)
    hit = _files.get(path)
    if hit is not None and hit[0] == sig:
        return hit[1]
    b = _load(path, fmt)
    _files[path] = (sig, b)
    while len(_files) > 6:
        _files.pop(next(iter(_files)))
    return b


def read_book(src):
    if src.kind == "artifact":
        return artifact_entry(src.aid, src.path, src.fmt, src.version).book
    return file_book(src.path, src.fmt)


_tmp = {"dir": None, "files": [], "n": 0}


def save_tmp(data, filename):
    """Write the bytes for byob to copy (it copies them right after our reply)."""
    if _tmp["dir"] is None or not os.path.isdir(_tmp["dir"]):
        _tmp["dir"] = tempfile.mkdtemp(prefix="byob-sheets-")
    _tmp["n"] += 1
    name = re.sub(r"[^A-Za-z0-9._-]+", "_", os.path.basename(filename or "sheet.xlsx"))
    path = os.path.join(_tmp["dir"], f"{_tmp['n']}-{name}")
    with open(path, "wb") as f:
        f.write(data)
    _tmp["files"].append(path)
    while len(_tmp["files"]) > 16:
        old = _tmp["files"].pop(0)
        try:
            os.remove(old)
        except OSError:
            pass
    return path


# ---------------------------------------------------------------- ranges

def sheet_index(book, name, create=False, added=None):
    names = book.sheet_names()
    if name is None or (isinstance(name, str) and not name.strip()):
        return 0
    name = str(name).strip()
    if name in names:
        return names.index(name)
    low = [n.lower() for n in names]
    if name.lower() in low:
        return low.index(name.lower())
    if create:
        book.add_sheet(name)
        if added is not None:
            added.append(name)
        return book.sheet_names().index(name)
    raise SheetError(f"No sheet named {name!r}; the sheets are: {', '.join(names)}.")


def resolve_range(book, text, sheet_name=None, create=False, added=None, clamp=False):
    """(sheet index, r1, c1, r2, c2): open sides filled from the used range; `clamp` cuts
    the range to the used range (for reading)."""
    if text is None or (isinstance(text, str) and not text.strip()):
        sname, r1, c1, r2, c2 = None, None, None, None, None
    else:
        sname, r1, c1, r2, c2 = parse_range(str(text))
    s = sheet_index(book, sname if sname is not None else sheet_name, create, added)
    used = book.used(s)
    ur1, ur2, uc1, uc2 = used if used else (1, 1, 1, 1)
    if r1 is None:
        r1, r2 = (1 if text else ur1), ur2
    elif r2 is None:
        r2 = max(r1, ur2)
    if c1 is None:
        c1, c2 = (1 if text else uc1), uc2
    elif c2 is None:
        c2 = max(c1, uc2)
    if clamp and used:
        r2, c2 = min(r2, ur2), min(c2, uc2)
        if r1 > r2 or c1 > c2:
            return s, r1, c1, r1 - 1, c1 - 1  # empty
    return s, r1, c1, r2, c2


def _qualify(book, s, ref, always=False):
    names = book.sheet_names()
    if always or len(names) > 1:
        return f"{quote_sheet(names[s])}!{ref}"
    return ref


# ---------------------------------------------------------------- text for the model

def _clean(v):
    return str(v).replace("\t", " ").replace("\r", " ").replace("\n", " ")


def tsv(book, s, r1, c1, r2, c2, what="values"):
    """Lines: a header of column letters, then each non-empty row with its number."""
    lines = ["\t" + "\t".join(col_name(c) for c in range(c1, c2 + 1))]
    for r in range(r1, r2 + 1):
        vals = []
        for c in range(c1, c2 + 1):
            content = book.content(s, r, c)
            if content == "":
                vals.append("")
            elif what == "formulas" and content.startswith("="):
                vals.append(content)
            else:
                vals.append(_clean(book.model_text(s, r, c)))
        if any(vals):
            lines.append(f"{r}\t" + "\t".join(vals))
    return lines


def errors(book):
    """Every error cell: [(sheet, row, col, code, formula)]; cached per change."""
    cache = getattr(book, "_errors", None)
    if cache is not None and cache[0] == book.gen:
        return cache[1]
    out = []
    for (s, r, c) in book.all_cells():
        if book.kind(s, r, c) == "e":
            out.append((s, r, c, str(book.value(s, r, c)), book.content(s, r, c)))
    out.sort()
    try:
        book._errors = (book.gen, out)
    except AttributeError:
        pass
    return out


def error_lines(book, sheet=None):
    errs = [e for e in errors(book) if sheet is None or e[0] == sheet]
    if not errs:
        return ["No error cells."]
    parts = []
    for (s, r, c, code, f) in errs[:MAX_ERRORS]:
        ref = _qualify(book, s, cell_name(r, c))
        parts.append(f"{ref} {code}" + (f" ({f})" if f.startswith("=") else ""))
    more = f" (and {len(errs) - MAX_ERRORS} more)" if len(errs) > MAX_ERRORS else ""
    return [f"Errors ({len(errs)}): " + "; ".join(parts) + more]


def _detect_header(book, s, r1, r2, c1, c2):
    """Is row r1 a header: text over typed data, or distinct labels over other text?"""
    if r2 <= r1:
        return False
    texts = {}
    for c in range(c1, c2 + 1):
        content = book.content(s, r1, c)
        if content == "":
            continue
        if content.startswith("=") or book.kind(s, r1, c) != "s" or book.is_date(s, r1, c):
            return False
        texts[c] = content.strip().lower()
    if not texts:
        return False
    if len(set(texts.values())) < len(texts):
        return False
    probe = range(r1 + 1, min(r2, r1 + 50) + 1)
    for c, h in texts.items():
        for r in probe:
            if book.content(s, r, c) == "":
                continue
            k = book.kind(s, r, c)
            if k in ("n", "b") or book.content(s, r, c).startswith("="):
                return True
    # all text: a header if its labels don't occur again below
    for c, h in texts.items():
        for r in probe:
            if book.content(s, r, c).strip().lower() == h:
                return False
    return len(texts) >= 2


def _profile_column(book, s, c, rows):
    nums, dates, texts, bools, errs, forms, blank = [], [], {}, 0, 0, [], 0
    for r in rows:
        content = book.content(s, r, c)
        if content == "":
            blank += 1
            continue
        if content.startswith("="):
            forms.append((r, content))
        k = book.kind(s, r, c)
        if k == "n":
            if book.is_date(s, r, c):
                dates.append((book.value(s, r, c), book.text(s, r, c)))
            else:
                v = book.value(s, r, c)
                nums.append(v if isinstance(v, (int, float)) else 0)
        elif k == "b":
            bools += 1
        elif k == "e":
            errs += 1
        else:
            if book.is_date(s, r, c):  # csv fallback: ISO dates are text
                t = book.text(s, r, c)
                dates.append((t, t))
            else:
                t = book.text(s, r, c)
                texts[t] = texts.get(t, 0) + 1
    n = len(nums) + len(dates) + sum(texts.values()) + bools + errs
    if n == 0:
        return "empty"
    counts = {"number": len(nums), "date": len(dates), "text": sum(texts.values()), "bool": bools, "error": errs}
    top = max(counts, key=counts.get)
    if counts[top] >= 0.9 * n:
        kind = top
    else:
        kind = "mixed (" + ", ".join(f"{v} {k}" for k, v in counts.items() if v) + ")"
    if forms and len(forms) >= n / 2:
        kind = f"formula → {kind}"
    parts = [kind, f"{n} values"]
    if blank:
        parts.append(f"{blank} blank")
    if nums and (top == "number" or kind.startswith("mixed")):
        parts.append(f"min {num_text(min(nums))}, max {num_text(max(nums))}, sum {num_text(sum(nums))}")
    if dates and top == "date":
        lo = min(dates, key=lambda d: str(d[0]) if not isinstance(d[0], (int, float)) else f"{d[0]:020.6f}")
        hi = max(dates, key=lambda d: str(d[0]) if not isinstance(d[0], (int, float)) else f"{d[0]:020.6f}")
        parts.append(f"{lo[1]} … {hi[1]}")
    if texts:
        distinct = sorted(texts.items(), key=lambda kv: (-kv[1], kv[0]))
        if len(distinct) <= 10:
            parts.append(f"{len(distinct)} distinct: " + ", ".join(f"{_clean(t)} ({k})" for t, k in distinct))
        else:
            parts.append(f"{len(distinct)} distinct, e.g. " + ", ".join(_clean(t)[:40] for t, _ in distinct[:5]))
    if bools and top == "bool":
        parts.append(f"{bools} TRUE/FALSE")
    if forms:
        r, f = forms[0]
        parts.append(f"e.g. {col_name(c)}{r} {f}")
    return ", ".join(parts)


PROFILE_CELLS = 300_000


def describe_one(book, s):
    name = book.sheet_names()[s]
    used = book.used(s)
    if used is None:
        return [f'Sheet "{name}": empty.']
    r1, r2, c1, c2 = used
    fr, fc = book.frozen(s)
    head = f'Sheet "{name}": used range {range_name(r1, c1, r2, c2)} ({r2 - r1 + 1} rows × {c2 - c1 + 1} columns)'
    if fr or fc:
        head += f", frozen {fr} rows, {fc} columns"
    lines = [head + "."]
    header = _detect_header(book, s, r1, r2, c1, c2)
    d1 = r1 + 1 if header else r1
    last = r2
    if (r2 - d1 + 1) * (c2 - c1 + 1) > PROFILE_CELLS:
        last = d1 + PROFILE_CELLS // (c2 - c1 + 1)
        lines.append(f"(Profiled rows {d1}–{last} only.)")
    if header:
        lines.append(f"Header in row {r1}; data in rows {d1}–{r2}.")
    else:
        lines.append(f"No header row detected; data in rows {r1}–{r2}.")
    rows = range(d1, last + 1)
    for c in range(c1, min(c2, c1 + 99) + 1):
        label = f'{col_name(c)} "{_clean(book.content(s, r1, c))}"' if header else col_name(c)
        lines.append(f"  {label}: {_profile_column(book, s, c, rows)}")
    if c2 - c1 >= 100:
        lines.append(f"  … {c2 - c1 - 99} more columns (read_cells shows them).")
    sample_end = min(r2, d1 + 2)
    cc2 = min(c2, c1 + 29)
    lines.append("First rows:")
    lines += tsv(book, s, r1, c1, sample_end, cc2)
    lines += error_lines(book, s)
    return lines


def describe(book, src):
    names = book.sheet_names()
    lines = []
    if src is not None:
        if src.note:
            lines.append(src.note)
        lines.append(f"{src.label}: {len(names)} sheet{'s' if len(names) != 1 else ''} ({', '.join(names)}).")
    for s in range(len(names)):
        lines += describe_one(book, s)
    return lines


def user_edit_lines(entry):
    if entry is None or not entry.user_edits:
        return []
    cells = entry.user_edits[-30:]
    more = len(entry.user_edits) - len(cells)
    return ["The user changed " + ", ".join(cells) + (f" (and {more} more)" if more > 0 else "")
            + " in the grid since your last change."]


# ---------------------------------------------------------------- lints

_IFERROR_BLANK = re.compile(r'IFERROR\s*\(.*,\s*""\s*\)', re.I | re.S)


def lint_iferror(book, written):
    bad = [(s, r, c) for (s, r, c) in written if _IFERROR_BLANK.search(book.content(s, r, c) or "")]
    if not bad:
        return []
    refs = ", ".join(_qualify(book, s, cell_name(r, c)) for s, r, c in bad[:5])
    more = f" and {len(bad) - 5} more" if len(bad) > 5 else ""
    return [f'Check: {refs}{more} use IFERROR(…, "") which hides errors as blanks; fix what causes the error '
            '(e.g. a lookup value that is missing) or show a visible fallback.']


def lint_hardcoded(book, written):
    """Typed numbers inside a column run that is otherwise formulas."""
    by_col = {}
    for (s, r, c) in written:
        lo, hi = by_col.get((s, c), (r, r))
        by_col[(s, c)] = (min(lo, r), max(hi, r))
    if not by_col:
        return []
    out = []
    rows_by = {}
    for (s, r, c) in book.all_cells():
        if (s, c) in by_col:
            rows_by.setdefault((s, c), []).append(r)
    for (s, c), (lo, hi) in sorted(by_col.items()):
        rows = sorted(set(rows_by.get((s, c), [])))
        # the contiguous runs of filled cells that overlap the written rows
        run = []
        for r in rows + [None]:
            if r is not None and (not run or r == run[-1] + 1):
                run.append(r)
                continue
            if run and run[0] <= hi and run[-1] >= lo:
                forms = [x for x in run if book.content(s, x, c).startswith("=")]
                if len(forms) >= 3:
                    typed = [x for x in run if forms[0] < x < forms[-1] and not book.content(s, x, c).startswith("=")
                             and book.kind(s, x, c) == "n"]
                    if typed and len(forms) >= 2 * len(typed):
                        span = _qualify(book, s, f"{col_name(c)}{forms[0]}:{col_name(c)}{forms[-1]}")
                        cells = ", ".join(cell_name(x, c) for x in typed[:5])
                        verb = "is a typed number" if len(typed) == 1 else "are typed numbers"
                        out.append(f"Check: {cells} {verb} in {span} where the rest are formulas.")
            run = [r] if r is not None else []
        if len(out) >= 5:
            break
    return out


# ---------------------------------------------------------------- writing

def _snapshot(book, limit=50_000):
    snap = {}
    for (s, r, c) in book.all_cells():
        if book.content(s, r, c).startswith("="):
            snap[(s, r, c)] = book.model_text(s, r, c)
            if len(snap) > limit:
                return None
    return snap


class Change:
    """What a write did, for its report."""

    def __init__(self):
        self.lines = []  # first lines of the report
        self.shown = []  # (sheet, r1, c1, r2, c2) to show values of
        self.cells = []  # (sheet, r, c) written (for lints and "changed elsewhere")
        self.listed = None  # [(sheet, r, c)] to list one per line instead of a table
        self.added = []  # sheets made on the way


def _open_write(sheet):
    need_ironcalc()
    src = resolve(sheet)
    if src.kind == "artifact":
        entry = artifact_entry(src.aid, src.path, src.fmt, src.version)
        return src, entry, entry.book
    return src, None, Book.load(src.path, src.fmt)


def write_op(sheet, note, action):
    """Run action(book, change) on the workbook `sheet` names, recalculate, and save a new
    version; return the tool result (text + the artifact to save)."""
    src, entry, book = _open_write(sheet)
    before = _snapshot(book)
    ch = Change()
    try:
        action(book, ch)
        book.evaluate()
    except Exception:
        if src.kind == "artifact":
            _entries.pop(src.aid, None)  # half-applied: reload from disk next time
        raise
    lines = []
    if src.note:
        lines.append(src.note)
    lines += ch.lines
    for name in ch.added:
        lines.append(f"Added sheet {name!r}.")
    written = set(ch.cells)
    if ch.listed is not None:
        listed = ch.listed[:SHOW_ROWS]
        for (s, r, c) in listed:
            content = book.content(s, r, c)
            ref = _qualify(book, s, cell_name(r, c))
            if content.startswith("="):
                lines.append(f"{ref} {content} → {_clean(book.model_text(s, r, c))}")
            else:
                lines.append(f"{ref} = {_clean(book.model_text(s, r, c)) if content else '(empty)'}")
        if len(ch.listed) > len(listed):
            lines.append(f"… {len(ch.listed) - len(listed)} more cells.")
    for (s, r1, c1, r2, c2) in ch.shown[:3]:
        ncols = c2 - c1 + 1
        rows = max(1, min(SHOW_ROWS, SHOW_CELLS // max(1, ncols)))
        end = min(r2, r1 + rows - 1)
        cend = min(c2, c1 + 39)
        lines.append(f"{_qualify(book, s, range_name(r1, c1, r2, c2))} now:")
        lines += tsv(book, s, r1, c1, end, cend)
        if end < r2 or cend < c2:
            lines.append(f"(Showing {range_name(r1, c1, end, cend)}; read_cells shows the rest.)")
    if before is not None:
        after = _snapshot(book)
        changed = []
        for k, v in (after or {}).items():
            if k not in written and k in before and before[k] != v:
                changed.append((k, before[k], v))
        changed.sort()
        if changed:
            parts = [f"{_qualify(book, s, cell_name(r, c))} {_clean(v)} (was {_clean(old)})"
                     for (s, r, c), old, v in changed[:10]]
            more = f" and {len(changed) - 10} more" if len(changed) > 10 else ""
            lines.append("Formulas elsewhere that changed: " + "; ".join(parts) + more + ".")
    lines += error_lines(book)
    lines += lint_iferror(book, sorted(written))
    lines += lint_hardcoded(book, sorted(written))
    lines += user_edit_lines(entry)
    data = book.to_xlsx()
    art = {"note": note}
    if src.kind == "artifact":
        art["id"] = src.aid
        filename = src.meta.get("filename") or src.aid
        if src.fmt != "xlsx":
            art["filename"] = _xlsx_name(filename, src.aid)
            lines.append(f"(Saved as {art['filename']}: spreadsheets with formulas are stored as .xlsx.)")
        art["file"] = save_tmp(data, art.get("filename") or filename)
        entry.wrote(data, src.version)
        entry.user_edits.clear()
    else:
        art["title"] = src.title()
        art["filename"] = _xlsx_name(src.rel)
        art["note"] = f"From {src.rel}"
        art["file"] = save_tmp(data, art["filename"])
        lines.append(f"Saved as a new spreadsheet artifact ({art['filename']}); {src.rel} itself is unchanged. "
                     "Use the artifact id below as `sheet` from now on.")
    return {"text": "\n".join(lines), "artifact": art}


def _parse_json_arg(v, what):
    if isinstance(v, str):
        t = v.strip()
        if t.startswith(("[", "{")):
            try:
                return json.loads(t)
            except ValueError:
                raise SheetError(f"`{what}` isn't valid JSON.") from None
        if what == "rows":  # TSV / CSV-ish text
            return [line.split("\t") if "\t" in line else [line] for line in t.splitlines()]
    return v


def _cell_value(v):
    if isinstance(v, dict):
        for k in ("formula", "value", "v"):
            if k in v:
                return _cell_value(v[k])
        return json.dumps(v)
    if isinstance(v, list):
        return json.dumps(v)
    if isinstance(v, str):
        t = v.lstrip()
        if t.startswith("="):
            return t
    return v


# ---------------------------------------------------------------- tools

@_tool
def create_sheet(title=None, filename=None, **kw):
    src_arg = kw.get("from") or kw.get("source") or kw.get("from_file")
    need_ironcalc()
    title = str(title or "").strip() or (os.path.splitext(os.path.basename(str(src_arg)))[0] if src_arg else "Spreadsheet")
    if filename:
        filename = _xlsx_name(str(filename))
    else:
        slug = re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-")[:60] or "sheet"
        filename = slug + ".xlsx"
    note = "Created"
    lines = []
    if src_arg:
        src = resolve(str(src_arg), follow=False)
        if src.kind == "artifact":
            b = Book.load(src.path, src.fmt)
            note = f"From artifact {src.aid}"
        else:
            b = Book.load(src.path, src.fmt)
            note = f"From {src.rel}"
            lines.append(f"Imported {src.rel} (the file itself is unchanged).")
    else:
        b = Book.new(title)
    lines.append(f'New spreadsheet "{title}" ({filename}).')
    lines += describe(b, None)
    lines.append("Use the artifact id byob reports below as `sheet` in write_cells, fill, read_cells and the other sheet tools.")
    path = save_tmp(b.to_xlsx(), filename)
    return {"text": "\n".join(lines), "artifact": {"title": title, "filename": filename, "file": path, "note": note}}


@_tool
def describe_sheet(sheet=None, **kw):
    src = resolve(sheet)
    book = read_book(src)
    lines = describe(book, src)
    if src.kind == "artifact":
        lines += user_edit_lines(_entries.get(src.aid))
    if ic is None and src.fmt != "xlsx":
        lines.append("(Read-only: the ironcalc package is missing, so this can't be changed.)")
    return "\n".join(lines)


@_tool
def read_cells(sheet=None, range=None, what="values", sheet_name=None, **kw):
    src = resolve(sheet)
    book = read_book(src)
    what = "formulas" if str(what or "").lower().startswith("form") else "values"
    s, r1, c1, r2, c2 = resolve_range(book, range, sheet_name, clamp=True)
    name = book.sheet_names()[s]
    lines = [src.note] if src.note else []
    if r2 < r1 or c2 < c1 or book.used(s) is None:
        lines.append(f'{quote_sheet(name)}!{range or "A1"} is empty.' if range else f'Sheet "{name}" is empty.')
        return "\n".join(lines)
    note = ""
    if c2 - c1 + 1 > READ_COLS:
        note = f" (columns {col_name(c1)}–{col_name(c1 + READ_COLS - 1)} of {col_name(c1)}–{col_name(c2)})"
        c2 = c1 + READ_COLS - 1
    per = max(1, min(READ_ROWS, READ_CELLS // (c2 - c1 + 1)))
    end = min(r2, r1 + per - 1)
    lines.append(f"{quote_sheet(name)}!{range_name(r1, c1, end, c2)} {what}, rows {r1}–{end} of {r1}–{r2}{note}:")
    lines += tsv(book, s, r1, c1, end, c2, what)
    if end < r2:
        nxt = range_name(end + 1, c1, r2, c2)
        lines.append(f'More rows: read_cells(range="{_qualify(book, s, nxt)}").')
    if src.kind == "artifact":
        lines += user_edit_lines(_entries.get(src.aid))
    return "\n".join(lines)


@_tool
def write_cells(sheet=None, start=None, rows=None, cells=None, sheet_name=None, **kw):
    if rows is None:
        rows = kw.get("values") if kw.get("values") is not None else kw.get("data")
    if start is None:
        start = kw.get("range") or kw.get("cell") or "A1"
    rows = _parse_json_arg(rows, "rows")
    cells = _parse_json_arg(cells, "cells")
    if rows is None and cells is None:
        raise SheetError('Give `rows` (a 2D list written from `start`, e.g. [["Item","Cost"],["Rent",1200]]) '
                         'or `cells` (e.g. {"B2": "=SUM(B3:B9)", "C1": 5}).')
    if rows is not None and not isinstance(rows, list):
        raise SheetError("`rows` must be a list of rows, each a list of values.")
    if cells is not None and not isinstance(cells, dict):
        raise SheetError('`cells` must be an object like {"B2": "=SUM(B3:B9)", "C1": 5}.')
    if rows is not None and rows and not any(isinstance(r, list) for r in rows):
        rows = [rows]  # one row given flat
    # check every reference before changing anything
    sname, r0, c0 = parse_cell(str(start).split(":")[0]) if rows is not None else (None, 1, 1)
    targets = []
    if cells:
        for k in cells:
            targets.append((parse_cell(str(k)), cells[k]))
    count = sum(len(r) if isinstance(r, list) else 1 for r in rows or []) + len(targets)
    if count > MAX_WRITE_CELLS:
        raise SheetError(f"That's {count} cells; write at most {MAX_WRITE_CELLS} per call, or use fill for formula columns.")
    for i, row in enumerate(rows or []):
        if r0 + i > MAX_ROWS or c0 + (len(row) if isinstance(row, list) else 1) - 1 > MAX_COLS:
            raise SheetError("Those rows run off the end of the sheet.")

    def act(book, ch):
        n = 0
        if rows is not None:
            s = sheet_index(book, sname if sname is not None else sheet_name, True, ch.added)
            width = 0
            for i, row in enumerate(rows):
                row = row if isinstance(row, list) else [row]
                width = max(width, len(row))
                for j, v in enumerate(row):
                    if v is None:
                        continue
                    book.put(s, r0 + i, c0 + j, _cell_value(v))
                    ch.cells.append((s, r0 + i, c0 + j))
                    n += 1
            if rows and width:
                ch.shown.append((s, r0, c0, r0 + len(rows) - 1, c0 + width - 1))
        if targets:
            ch.listed = []
            for (tsn, r, c), v in targets:
                s = sheet_index(book, tsn if tsn is not None else sheet_name, True, ch.added)
                book.put(s, r, c, _cell_value(v))
                ch.cells.append((s, r, c))
                ch.listed.append((s, r, c))
                n += 1
        ch.lines.append(f"Wrote {n} cell{'s' if n != 1 else ''}.")

    return write_op(sheet, "write_cells", act)


@_tool
def fill(sheet=None, range=None, formula=None, sheet_name=None, **kw):
    if formula is None:
        formula = kw.get("value")
    if formula is None or (isinstance(formula, str) and not formula.strip()):
        raise SheetError('Give `formula`: what the first cell of the range gets, like "=B2*C2"; '
                         "the other cells get it with their references moved, like Excel's fill.")
    if not range:
        raise SheetError('Give `range`, like "D2:D13" (or "D2:D" for down to the last used row).')
    formula = _cell_value(formula)
    parse_range(str(range))

    def act(book, ch):
        s, r1, c1, r2, c2 = resolve_range(book, range, sheet_name, True, ch.added)
        n = (r2 - r1 + 1) * (c2 - c1 + 1)
        if n > MAX_WRITE_CELLS:
            raise SheetError(f"{range} has {n} cells; fill at most {MAX_WRITE_CELLS} per call.")
        for r in builtins_range(r1, r2 + 1):
            for c in builtins_range(c1, c2 + 1):
                book.put(s, r, c, shift_formula(formula, r - r1, c - c1) if isinstance(formula, str) else formula)
                ch.cells.append((s, r, c))
        last = shift_formula(formula, r2 - r1, c2 - c1) if isinstance(formula, str) else formula
        ref = _qualify(book, s, range_name(r1, c1, r2, c2))
        msg = f"Filled {ref} ({n} cells) with {formula}"
        if n > 1:
            msg += f"; {cell_name(r2, c2)} got {last}"
        ch.lines.append(msg + ".")
        ch.shown.append((s, r1, c1, r2, c2))

    return write_op(sheet, f"fill {range}", act)


builtins_range = range


def _sort_key(book, s, r, c):
    content = book.content(s, r, c)
    if content == "":
        return None
    k = book.kind(s, r, c)
    v = book.value(s, r, c)
    if k == "n":
        return (0, v if isinstance(v, (int, float)) else 0, "")
    if k == "s":
        return (1, 0, str(v).lower())
    if k == "b":
        return (2, 1 if v else 0, "")
    return (3, 0, str(v))


def _sort(book, ch, s, r1, c1, r2, c2, by, descending, header):
    if by is None or str(by).strip() == "":
        by_c = c1
    else:
        try:
            by_c = parse_col(by)
        except RangeError:
            by_c = None
            for c in builtins_range(c1, c2 + 1):  # a header name
                if book.content(s, r1, c).strip().lower() == str(by).strip().lower():
                    by_c = c
            if by_c is None:
                raise SheetError(f"`by` should be a column letter of the range (like C); {by!r} isn't one.") from None
    if not c1 <= by_c <= c2:
        raise SheetError(f"Column {col_name(by_c)} isn't inside the range {range_name(r1, c1, r2, c2)}.")
    if header is None:
        header = _detect_header(book, s, r1, r2, c1, c2)
    start = r1 + 1 if header else r1
    rows = []
    for r in builtins_range(start, r2 + 1):
        cells = [(book.content(s, r, c), book.style(s, r, c)) for c in builtins_range(c1, c2 + 1)]
        rows.append((_sort_key(book, s, r, by_c), r, cells))
    full = sorted((x for x in rows if x[0] is not None), key=lambda x: x[0], reverse=bool(descending))
    order = full + [x for x in rows if x[0] is None]
    moved = 0
    for i, (_, old, cells) in enumerate(order):
        new = start + i
        moved += new != old
        for j, (content, st) in enumerate(cells):
            c = c1 + j
            if content.startswith("="):
                content = shift_formula(content, new - old, 0)
            book.put(s, new, c, content)
            book.set_style(s, new, c, st)
    ref = _qualify(book, s, range_name(start, c1, r2, c2))
    ch.lines.append(f"Sorted {ref} by column {col_name(by_c)}, {'descending' if descending else 'ascending'}"
                    + (f" (row {r1} kept as the header)" if header else "") + f"; {moved} rows moved.")
    ch.shown.append((s, r1, c1, r2, c2))


def _count(v, default=1):
    if v is None or v == "":
        return default
    try:
        n = int(v)
    except (TypeError, ValueError):
        raise SheetError(f"`count` should be a number; got {v!r}.") from None
    if not 1 <= n <= 100_000:
        raise SheetError("`count` should be between 1 and 100000.")
    return n


def _flag(v):
    if isinstance(v, str):
        return v.strip().lower() in ("true", "yes", "1", "y")
    return bool(v)


EDIT_OPS = ("add_sheet", "rename_sheet", "delete_sheet", "insert_rows", "delete_rows", "insert_cols",
            "delete_cols", "sort", "freeze")


@_tool
def edit_sheet(sheet=None, op=None, sheet_name=None, name=None, new_name=None, at=None, count=None,
               range=None, by=None, descending=False, header=None, rows=None, cols=None, **kw):
    op = str(op or "").strip().lower().replace("-", "_").replace(" ", "_")
    op = {"insert_columns": "insert_cols", "delete_columns": "delete_cols", "add": "add_sheet",
          "rename": "rename_sheet", "delete": "delete_sheet", "freeze_panes": "freeze"}.get(op, op)
    if op not in EDIT_OPS:
        raise SheetError(f"`op` should be one of: {', '.join(EDIT_OPS)}.")

    def act(book, ch):
        target = sheet_name
        if op in ("rename_sheet", "delete_sheet"):
            target = name or sheet_name
            if not target:
                raise SheetError(f"{op} needs `name`: the sheet to {op.split('_')[0]}.")
        s = sheet_index(book, target if op != "add_sheet" else sheet_name)
        names = book.sheet_names()
        if op == "add_sheet":
            nm = str(name or new_name or "").strip()
            if not nm:
                raise SheetError("add_sheet needs `name`.")
            if nm.lower() in (x.lower() for x in names):
                raise SheetError(f"There is already a sheet named {nm!r}.")
            book.add_sheet(nm)
            ch.lines.append(f"Added sheet {nm!r}; the sheets are now: {', '.join(book.sheet_names())}.")
        elif op == "rename_sheet":
            nm = str(new_name or "").strip()
            if not nm:
                raise SheetError("rename_sheet needs `new_name`.")
            old = names[s]
            book.rename_sheet(s, nm)
            ch.lines.append(f"Renamed sheet {old!r} to {nm!r}; formulas that pointed at it were updated.")
        elif op == "delete_sheet":
            if len(names) == 1:
                raise SheetError("That's the only sheet; a workbook needs at least one.")
            old = names[s]
            book.delete_sheet(s)
            ch.lines.append(f"Deleted sheet {old!r}; the sheets are now: {', '.join(book.sheet_names())}.")
        elif op in ("insert_rows", "delete_rows"):
            if at is None:
                raise SheetError(f"{op} needs `at`: a row number.")
            r, n = parse_row(at), _count(count)
            if op == "insert_rows":
                book.insert_rows(s, r, n)
                ch.lines.append(f"Inserted {n} empty row{'s' if n > 1 else ''} at row {r} of {names[s]!r}; "
                                f"rows below moved down by {n} and formulas were adjusted.")
            else:
                book.delete_rows(s, r, n)
                ch.lines.append(f"Deleted rows {r}–{r + n - 1} of {names[s]!r}; rows below moved up and formulas were adjusted.")
        elif op in ("insert_cols", "delete_cols"):
            if at is None:
                raise SheetError(f"{op} needs `at`: a column letter.")
            c, n = parse_col(at), _count(count)
            if op == "insert_cols":
                book.insert_cols(s, c, n)
                ch.lines.append(f"Inserted {n} empty column{'s' if n > 1 else ''} at {col_name(c)} of {names[s]!r}; "
                                "columns to the right moved and formulas were adjusted.")
            else:
                book.delete_cols(s, c, n)
                ch.lines.append(f"Deleted columns {col_name(c)}–{col_name(c + n - 1)} of {names[s]!r}; formulas were adjusted.")
        elif op == "sort":
            ss, r1, c1, r2, c2 = resolve_range(book, range, sheet_name)
            if book.used(ss) is None:
                raise SheetError("That sheet is empty; there is nothing to sort.")
            if (r2 - r1 + 1) * (c2 - c1 + 1) > MAX_WRITE_CELLS:
                raise SheetError(f"That range is too big to sort here (at most {MAX_WRITE_CELLS} cells).")
            hdr = None if header is None or header == "" else _flag(header)
            _sort(book, ch, ss, r1, c1, r2, c2, by, _flag(descending), hdr)
        elif op == "freeze":
            fr0, fc0 = book.frozen(s)
            fr = fr0 if rows is None or rows == "" else int(rows)
            fc = fc0 if cols is None or cols == "" else int(cols)
            if not (0 <= fr <= 1000 and 0 <= fc <= 100):
                raise SheetError("freeze takes `rows` (0–1000) and `cols` (0–100).")
            book.set_frozen(s, fr, fc)
            ch.lines.append(f"{names[s]!r}: frozen {fr} row{'s' if fr != 1 else ''} and {fc} column{'s' if fc != 1 else ''}.")

    return write_op(sheet, f"edit_sheet {op}", act)


_ALIGN = {"left": "left", "center": "center", "centre": "center", "right": "right"}


@_tool
def format_cells(sheet=None, range=None, number_format=None, bold=None, fill=None, width=None, align=None,
                 sheet_name=None, **kw):
    if number_format is None:
        number_format = kw.get("format") or kw.get("num_fmt")
    if not range:
        raise SheetError('Give `range`, like "B2:B13" or "1:1".')
    parse_range(str(range))
    color = None
    if fill is not None and fill != "":
        if str(fill).lower() in ("none", "no", "clear", "false"):
            color = ""
        else:
            m = _HEX.fullmatch(str(fill).strip())
            if not m:
                raise SheetError(f'`fill` should be a colour like "#ffe599"; got {fill!r}.')
            color = "#" + m.group(1).upper()
    if align is not None and align != "" and str(align).lower() not in _ALIGN:
        raise SheetError("`align` should be left, center or right.")
    if width is not None and width != "":
        try:
            width = float(width)
        except (TypeError, ValueError):
            raise SheetError("`width` should be a number of characters, like 14.") from None
        if not 1 <= width <= 255:
            raise SheetError("`width` should be between 1 and 255 characters.")
    else:
        width = None
    if number_format is None and bold is None and color is None and width is None and not align:
        raise SheetError("Give at least one of number_format, bold, fill, width or align.")

    def act(book, ch):
        s, r1, c1, r2, c2 = resolve_range(book, range, sheet_name, True, ch.added)
        n = (r2 - r1 + 1) * (c2 - c1 + 1)
        if n > MAX_WRITE_CELLS:
            raise SheetError(f"{range} has {n} cells; format at most {MAX_WRITE_CELLS} per call.")
        did = []
        if number_format is not None or bold is not None or color is not None or align:
            for r in builtins_range(r1, r2 + 1):
                for c in builtins_range(c1, c2 + 1):
                    st = book.style(s, r, c)
                    if number_format is not None:
                        st["num_fmt"] = str(number_format) or "general"
                    if bold is not None:
                        st.setdefault("font", {})["b"] = _flag(bold)
                    if color is not None:
                        st["fill"] = {"color": color} if color else {}
                    if align:
                        al = st.get("alignment") or {}
                        al["horizontal"] = _ALIGN[str(align).lower()]
                        st["alignment"] = al
                    book.set_style(s, r, c, st)
            if number_format is not None:
                did.append(f"number format {number_format}")
            if bold is not None:
                did.append("bold" if _flag(bold) else "not bold")
            if color is not None:
                did.append(f"fill {color}" if color else "no fill")
            if align:
                did.append(f"aligned {_ALIGN[str(align).lower()]}")
        if width is not None:
            for c in builtins_range(c1, c2 + 1):
                book.set_width(s, c, width * CHAR_WIDTH)
            did.append(f"columns {col_name(c1)}–{col_name(c2)} {num_text(width)} characters wide")
        book.touch()
        ref = _qualify(book, s, range_name(r1, c1, r2, c2))
        ch.lines.append(f"Formatted {ref}: {', '.join(did)}.")
        if number_format is not None:
            shown = []
            for r in builtins_range(r1, min(r2, r1 + 4) + 1):
                for c in builtins_range(c1, min(c2, c1 + 3) + 1):
                    if book.content(s, r, c) != "":
                        shown.append(f"{cell_name(r, c)} {_clean(book.text(s, r, c))}")
            if shown:
                ch.lines.append("Shown as: " + "; ".join(shown[:8]) + ".")

    return write_op(sheet, "format_cells", act)


# ---------------------------------------------------------------- the grid view

class ViewState:
    """Per artifact (not per version): the sheet shown, and each sheet's window and selection."""

    def __init__(self):
        self.sheet = None
        self.win = {}  # sheet name -> (row, col, rows, cols), 0-based
        self.sel = {}


_views = {}
MAX_BLOCK_CELLS = 40_000


def _int(v, default, lo, hi):
    try:
        n = int(v)
    except (TypeError, ValueError):
        return default
    return max(lo, min(hi, n))


def view_book(a):
    """(book, entry or None, editable, note) for the artifact dict byob gave a view."""
    aid = a.get("id") or ""
    path = a.get("path") or ""
    fmt = _sheet_format(a.get("content_type"), a.get("filename") or path) or bk.file_format(path)
    version, latest = a.get("version"), a.get("latest")
    if latest is not None and version is not None and version != latest:
        return version_book(path, fmt), None, False, f"version {version}, read-only (the latest is {latest})"
    e = artifact_entry(aid, path, fmt, version)
    return e.book, e, bool(e.book.writable), ""


def _cell_json(book, s, r, c):
    content = book.content(s, r, c)
    st = book.style(s, r, c)
    font = st.get("font") or {}
    fillc = (st.get("fill") or {}).get("color")
    extra = {}
    if font.get("b"):
        extra["b"] = True
    if font.get("i"):
        extra["it"] = True
    if isinstance(fillc, str) and _HEX.fullmatch(fillc):
        extra["fill"] = "#" + fillc.lstrip("#").lower()
    fc = font.get("color")
    if isinstance(fc, str) and _HEX.fullmatch(fc) and fc.lstrip("#").lower() != "000000":
        extra["color"] = "#" + fc.lstrip("#").lower()
    al = (st.get("alignment") or {}).get("horizontal")
    if al in ("left", "center", "right"):
        extra["align"] = al
    if content == "":
        if extra:
            extra["v"] = ""
            return extra
        return None
    k = book.kind(s, r, c)
    text = book.text(s, r, c)
    if k == "s" and content == text and not extra:
        return text
    d = {"v": text}
    if k != "s":
        d["t"] = k
    if content != text:
        d["i"] = content
    d.update(extra)
    return d


def _block(book, s, row, col, rows, cols):
    """Cells of rows row..row+rows, cols col..col+cols (0-based)."""
    cells = [[_cell_json(book, s, r + 1, c + 1) for c in builtins_range(col, col + cols)]
             for r in builtins_range(row, row + rows)]
    return {"row": row, "col": col, "cells": cells}


def grid(book, s, state, editable):
    name = book.sheet_names()[s]
    used = book.used(s)
    ur2, uc2 = (used[1], used[3]) if used else (0, 0)
    total_rows = min(MAX_ROWS, max(ur2 + 50, 100))
    total_cols = min(MAX_COLS, max(uc2 + 10, 26))
    row, col, rows, cols = state.win.get(name, (0, 0, 100, 26))
    rows = _int(rows, 100, 1, 500)
    cols = _int(cols, 26, 1, 200)
    while rows * cols > MAX_BLOCK_CELLS:
        rows = max(1, rows // 2)
    row = _int(row, 0, 0, max(0, total_rows - 1))
    col = _int(col, 0, 0, max(0, total_cols - 1))
    rows = min(rows, total_rows - row)
    cols = min(cols, total_cols - col)
    blocks = [_block(book, s, row, col, rows, cols)]
    fr, fc = book.frozen(s)
    top, left = min(fr, row), min(fc, col)
    if top:
        blocks.append(_block(book, s, 0, col, top, cols))
    if left:
        blocks.append(_block(book, s, row, 0, rows, left))
    if top and left:
        blocks.append(_block(book, s, 0, 0, top, left))
    widths = {}
    for c in list(builtins_range(0, left)) + list(builtins_range(col, col + cols)):
        w = book.width(s, c + 1)
        if abs(w - DEFAULT_WIDTH) > 0.01:
            widths[str(c)] = round(w, 1)
    node = {"type": "grid", "key": "g", "name": "sheet", "rows": total_rows, "cols": total_cols,
            "blocks": blocks, "frozen_rows": fr, "frozen_cols": fc, "widths": widths,
            "default_width": DEFAULT_WIDTH, "editable": bool(editable), "headers": True}
    sel = state.sel.get(name)
    if sel:
        node["selection"] = sel
    return node


def _current_sheet(book, state):
    names = book.sheet_names()
    if state.sheet in names:
        return names.index(state.sheet)
    state.sheet = names[0]
    return 0


@byob.view("sheet")
def draw(key):
    a = byob.artifact(key, "sheet")
    if not isinstance(a, dict):
        return {"type": "text", "text": "This spreadsheet isn't available.", "tone": "dim"}
    try:
        book, entry, editable, note = view_book(a)
    except SheetError as e:
        return {"type": "column", "children": [{"type": "text", "text": str(e), "tone": "warning"}]}
    state = _views.setdefault(a.get("id") or "", ViewState())
    s = _current_sheet(book, state)
    names = book.sheet_names()
    children = []
    if len(names) > 1:
        children.append({"type": "row", "key": "tabs", "children": [
            {"type": "button", "label": n, "action": f"tab:{n}", "primary": i == s} for i, n in enumerate(names)]})
    used = book.used(s)
    status = [names[s]]
    if used:
        status.append(f"{used[1]} rows × {used[3]} columns")
    else:
        status.append("empty")
    ne = sum(1 for e in errors(book) if e[0] == s)
    if ne:
        status.append(f"{ne} error{'s' if ne != 1 else ''}")
    if note:
        status.append(note)
    if ic is None:
        status.append("read-only (the ironcalc package is missing)")
    children.append({"type": "text", "key": "status", "style": "small", "tone": "dim", "text": " · ".join(status)})
    children.append(grid(book, s, state, editable))
    return {"type": "column", "children": children}


def _label(book, s, r, c, r2=None, c2=None):
    ref = range_name(r, c, r2 or r, c2 or c)
    return _qualify(book, s, ref)


@byob.on_event("sheet")
def on_sheet_event(ev):
    a = ev.get("artifact") if isinstance(ev.get("artifact"), dict) else byob.artifact(ev.get("key") or "", "sheet")
    if not isinstance(a, dict):
        return None
    state = _views.setdefault(a.get("id") or "", ViewState())
    action = str(ev.get("action") or "")
    if action.startswith("tab:"):
        state.sheet = action[4:]
        return None
    g = ev.get("grid") if isinstance(ev.get("grid"), dict) else {}
    op = g.get("op") or action
    try:
        book, entry, editable, _ = view_book(a)
    except SheetError as e:
        byob.notice(str(e), "error")
        return None
    s = _current_sheet(book, state)
    name = book.sheet_names()[s]
    row = _int(g.get("row"), 0, 0, MAX_ROWS - 1)
    col = _int(g.get("col"), 0, 0, MAX_COLS - 1)
    if op == "window":
        state.win[name] = (row, col, _int(g.get("rows"), 100, 1, 500), _int(g.get("cols"), 26, 1, 200))
        return None
    if op == "select":
        sel = {"row": row, "col": col, "to_row": _int(g.get("to_row"), row, 0, MAX_ROWS - 1),
               "to_col": _int(g.get("to_col"), col, 0, MAX_COLS - 1)}
        state.sel[name] = sel
        return None
    if op not in ("edit", "paste", "clear"):
        return None
    if not editable or entry is None:
        byob.notice("This version of the spreadsheet is read-only; open the latest one to edit it.", "info")
        return None
    labels = []
    if op == "edit":
        v = g.get("value")
        v = "" if v is None else str(v)
        book.put(s, row + 1, col + 1, v)
        labels.append(_label(book, s, row + 1, col + 1))
    elif op == "paste":
        text = str(g.get("text") or "")
        lines = text.replace("\r\n", "\n").replace("\r", "\n").split("\n")
        if lines and lines[-1] == "":
            lines.pop()
        grid_rows = [ln.split("\t") for ln in lines]
        n = sum(len(x) for x in grid_rows)
        if n > MAX_WRITE_CELLS:
            byob.notice(f"That paste has {n} cells; at most {MAX_WRITE_CELLS} fit in one go.", "error")
            return None
        width = 0
        for i, cells in enumerate(grid_rows):
            width = max(width, len(cells))
            for j, v in enumerate(cells):
                if row + i + 1 <= MAX_ROWS and col + j + 1 <= MAX_COLS:
                    book.put(s, row + i + 1, col + j + 1, v)
        if grid_rows and width:
            labels.append(_label(book, s, row + 1, col + 1, row + len(grid_rows), col + width))
    else:
        tr = _int(g.get("to_row"), row, row, MAX_ROWS - 1)
        tc = _int(g.get("to_col"), col, col, MAX_COLS - 1)
        book.clear(s, row + 1, col + 1, tr + 1, tc + 1)
        labels.append(_label(book, s, row + 1, col + 1, tr + 1, tc + 1))
    book.evaluate()
    for lb in labels:
        entry.user_edited(lb)
    data = book.to_xlsx()
    filename = a.get("filename") or ""
    fmt = _sheet_format(a.get("content_type"), filename or a.get("path") or "")
    new_name = _xlsx_name(filename, a.get("id") or "sheet") if fmt != "xlsx" else None
    path = save_tmp(data, new_name or filename or "sheet.xlsx")
    entry.wrote(data, a.get("version"))
    byob.save_artifact(path, a.get("chat") or ev.get("chat"), id=a.get("id"), filename=new_name,
                       note="Edited in the grid", draft=True)
    return None


@byob.on_start
def started(init):
    if ic is None:
        byob.fault("Spreadsheets need the ironcalc Python package, which couldn't be imported "
                   f"({bk.IC_ERROR}); only csv/tsv files can be read and shown.")


if __name__ == "__main__":
    byob.run()
