# Spreadsheets: Python plugin vs native twin

byob ships spreadsheets twice, doing the same job on the same engine:

- **`sheets`** (on by default): a Python backend (`crates/bundled/plugins/sheets/backend.py`)
  on IronCalc through the `ironcalc==0.9.1` PyPI package, in a per-plugin venv.
- **`sheets-native`** (off by default): the same backend ported to Rust
  (`crates/sheets`, `byob-sheets`) on the `ironcalc` crate pinned `=0.9.1` (the code the
  Python package wraps), run inside byob as a native plugin backend
  (`"backend": {"native": "sheets"}`).

Both manifests declare the same tools, prompt, artifact types, renderers and grid view (a test
keeps them identical) and `"provides": "spreadsheets"`: switch one on and the other off in
Settings → Plugins (or press "Use this one" on the unused one); with both on, the one switched
on in settings wins, then the smaller id.

Parity is tested, not assumed: the Python plugin's own suite (`sheets_tests/test_sheets.py`,
15 functional tests) passes against the native binary, and `test_parity.py` sends both
backends the same ~65 calls (all seven tools, a 38-formula battery incl. spills and errors,
sorts, freezes, formats, CSV/TSV sniffing and profiles, grid paging, edits, pastes, drafts,
read-only old versions, and wrong input) and requires every reply, view tree and message to
be identical (temp paths aside). A workbook written by one loads in the other.

## Measured

**Measured on 2026-11-10**, AMD Ryzen 9 7950X (16 cores), 30 GB RAM, Linux 6.18, Python
3.13.17, rustc 1.99.1, release build. `cargo test -p byob-sheets --release --test bench --
--ignored --nocapture` with `BYOB_SHEETS_PYTHON` set (`crates/sheets/tests/bench.rs`): both
backends are hosted by byob's own backend host (`byob_plugins::backend::Backend`), which
serializes every request to a JSON line for both; "native as a process" is the same Rust code
behind JSON lines (`byob-sheets-backend`). Two runs agreed within ~10%; the second is shown.

| measure | Python backend | native (in-process) | native as a process |
|---|---:|---:|---:|
| cold first use (fresh `python -m venv` + `pip install ironcalc==0.9.1`, warm pip cache) | 3.7 s (+ 11 ms import) | 0 | 0 |
| backend start → first reply | 45 ms | 0.16 ms | 1.9 ms |
| write_cells 1,000 rows × 5 (one call) | 22 ms | 11 ms | 10 ms |
| write_cells of 1 cell on the 1k-row sheet (median of 15; recalc, report, lints, xlsx save) | 8.2 ms | 5.6 ms | 5.7 ms |
| write_cells 100k formulas (4 calls of 25k; a request is ≤ 1 MiB) | 1425 ms | 870 ms | 828 ms |
| full recalc of 100k formulas (median of 5, engine only) | 14.3 ms | 13.0 ms | — |
| open the grid view (first draw, 100 × 26 window) | 3.9 ms | 1.8 ms | 2.4 ms |
| page the grid (window event → redrawn tree, median of 20) | 1.20 ms | 0.80 ms | 0.84 ms |
| steady RSS after the workload | 61 MB (its process) | +39 MB (in byob's process) | 39 MB (its process) |
| byob release binary (`native-sheets` feature off → on) | — | +6.7 MB (42.8 → 49.6 MB; stripped +6.2 MB, gzip −9 +1.9 MB) | — |

Reading it: the engine is the same, so recalculation is a wash (IronCalc does the work in
both). Everything around it (the per-call report, error scan, lints, view trees, JSON)
is 1.5–2× faster in Rust. Start-up is where they differ most: ~45 ms to start Python and
import the plugin vs nothing, and the venv build on first use (seconds, network needed
without a pip cache) vs nothing. Memory: the Python process carries ~20 MB of interpreter on
top of what the workbooks take; the native backend's workbooks live in byob's own process.
All of these are well under what a model turn costs (seconds), so for the user the two feel
the same once running; the first use after install is the visible difference.

The live run (silicon, model `local`, `sheets-native` on): "Build a monthly budget
spreadsheet with categories, amounts and a summary by category" made a two-sheet workbook in
8 versions (create, write, fills, a rename) with no error cells; the grid showed it beside
the chat. The xlsx it wrote opens in the Python package unchanged.

## Qualitative

| | Python `sheets` | native `sheets-native` |
|---|---|---|
| **Modify it** | Fork it (Settings → Plugins → Fork to edit, or `keep_plugin`) and edit `backend.py`: byob restarts the backend on save, no rebuild. The model can do it too (`edit_plugin`), and `plugin_status` shows it tracebacks. | Edit `crates/sheets` and rebuild byob. Forking the manifest works (same native id) but the code is byob's. |
| **Crash isolation** | Its own process: a crash or a hang in IronCalc's Python bindings can't touch byob; it restarts with backoff. | A panic is caught (`catch_unwind`), reported as a fault and the backend restarts fresh, like a process crash. An abort, a stack overflow or an infinite loop in IronCalc would take byob (or that thread) with it; there is no kill for a stuck thread. |
| **Install / offline** | Needs a Python and network (or a pip cache) once to build the venv (`ironcalc` wheel ~3 MB); without it the backend still reads CSV/TSV, read-only. | Nothing: compiled into byob (+6.7 MB). Works offline and where no Python is installed. |
| **Debugging** | Its stderr/log ring in `plugin_status`, Python tracebacks in error replies, `print()`; run `backend.py` by hand. | `log`/`fault` messages and byob's log; errors are sentences, no tracebacks; a debugger on byob; `byob-sheets-backend` runs it alone on JSON lines. |
| **Versions** | Pinned by the manifest (`ironcalc==0.9.1`); a different pin is a manifest edit. | Pinned in `Cargo.toml` (`=0.9.1`); keep it equal to the Python pin so both compute the same. |

Known differences: none in the tested surface. Untested corners where Python's behaviour
is only approximated: the CSV `Sniffer` quote heuristics on odd quoted files, `str.lower()`
vs Rust lowercasing for rare Unicode, Python tracebacks (the native side answers with a
sentence where Python would raise), and the object-key order of `cells` (byob passes keys
sorted to both, so they agree inside byob).
