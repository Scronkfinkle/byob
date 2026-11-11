# View trees (schema v1)

A view tree is UI described as JSON. byob draws it natively, in the user's theme, on the
desktop and in the browser. You describe *what* to show (cards, text, buttons, fields,
tables, charts); byob decides how it looks. When the user clicks or edits something, your
code gets an **event** and answers with a new view or a few **patches**.

The code lives in `byob_protocol::tree` (schema, decoding, patches, diff) and
`byob_ui_egui::tree` (the renderer). This page is the authoring guide.

## A view plugin in Python

A view plugin is a Python 3 script (standard library only). byob starts it once and keeps it
running; it talks JSON lines on stdin/stdout.

- On start, print ONE line: `{"view": <node>}`.
- Then loop over stdin lines. Each line is an event:
  `{"action": "<string>", "values": {"<field name>": <value>, ...}, "edit": {...}}`
  (`edit` only for table edits). `values` always holds the current value of EVERY named
  field (input, toggle, select, table selection) in the view.
- For each event print ONE line: `{"view": <node>}` (replace everything) or
  `{"patch": [ ... ]}`. **Always `print(..., flush=True)`**, or byob waits forever for a
  reply stuck in Python's buffer. Never print anything else to stdout (use stderr for
  debugging: `print("debug", file=sys.stderr)`).

```python
import json, sys

count = 0

def view():
    return {"type": "card", "title": "Counter", "children": [
        {"type": "text", "text": f"Clicked {count} times"},
        {"type": "button", "label": "Click", "action": "click", "primary": True}]}

print(json.dumps({"view": view()}), flush=True)
for line in sys.stdin:
    ev = json.loads(line)
    if ev.get("action") == "click":
        count += 1
    print(json.dumps({"view": view()}), flush=True)
```

Sending the whole view after every event is always fine and the simplest thing to do: byob
works out what changed and sends only that to the window.

## Nodes

Every node is an object with a `"type"`. Any node may have a `"key"` (any string, unique in
the view): patches find the node by it, and byob keeps its scroll position and selection
under it. Unknown fields are ignored.

| type | fields |
|---|---|
| `column` | `children: [node]` — stacked top to bottom |
| `row` | `children: [node]` — side by side, wrapping; a row of `card`s or `column`s splits the width equally |
| `card` | `title?: str`, `children: [node]` — a framed box |
| `text` | `text: str`, `style?: "body" \| "heading" \| "strong" \| "small" \| "mono"`, `tone?` |
| `markdown` | `text: str` — bold, lists, links, code, tables |
| `chip` | `text: str`, `tone?` — a small status pill |
| `button` | `label: str`, `action: str` (sent as the event's action), `primary?: bool`, `danger?: bool` |
| `input` | `name: str`, `value?: str`, `placeholder?: str`, `submit?: str` (action sent when Enter is pressed) |
| `toggle` | `name: str`, `label?: str`, `value?: bool`, `action?: str` (sent when flipped) |
| `select` | `name: str`, `options: [str]`, `value?: str`, `action?: str` (sent when the choice changes) |
| `progress` | `value: 0..1`, `label?: str` |
| `table` | `name?: str`, `columns: [{"key": str, "label"?: str, "width"?: num, "editable"?: bool}]`, `rows: [[cell, ...]]`, `height?: num`, `select?: str` |
| `chart` | `spec`: a chart, the same JSON as `.chart.json` files: `{"version": 1, "type": "bar" \| "line" \| "area" \| "pie" \| "scatter", "title": str, "x": str, "series": [{"key": str, "label": str}], "data": [{...}]}` |
| `separator` | (no fields) — a line |
| `image` | `media?: str` (a workspace path) or `artifact?: str` (an artifact id), `alt?: str`, `width?: num`, `height?: num` |
| `canvas` | `width: num`, `height: num`, `shapes: [shape]`, `action?: str` — a drawing (see below) |

`tone` is one of `"normal"`, `"dim"`, `"accent"`, `"success"`, `"warning"`, `"danger"`.
These are colour *roles*: the theme picks the actual colours. There is no way to give a
colour directly, except on a canvas.

### Tables

`rows` are lists of cells in column order; a cell is a string, number, boolean or null. A row
may also be an object by column key (`{"sku": "A1", "qty": 3}`). Tables can hold 100,000
rows: only the rows on screen are drawn. `height` is the most the table takes before it
scrolls (300 by default). Columns without a `width` share the space left.

- Clicking a row selects it: `values[name]` is its index (or null), and the `select` action is
  sent if there is one.
- Double-clicking a cell of an `editable` column edits it. Enter (or clicking away) sends
  `{"action": "edit", "edit": {"table": name, "row": i, "column": key, "value": "text"}}`;
  Escape cancels. `value` is always text: convert it yourself. The new text shows until your
  reply arrives; reply with the corrected row (or the whole view).

### Canvas

A canvas draws shapes in a `width` × `height` box (scaled down if the window is narrower).
Each shape has a `"type"`:

| shape | fields |
|---|---|
| `rect` | `x, y, w, h`, `fill?`, `stroke?`, `radius?` |
| `circle` | `x, y, r`, `fill?`, `stroke?` |
| `line` | `points: [[x, y], ...]`, `stroke?`, `width?` (1) |
| `text` | `x, y, text`, `color?`, `size?` (14) — `x, y` is the top left |

Colours are theme role names (`"accent"`, `"text"`, `"text_dim"`, `"border"`, `"success"`,
`"danger"`, `"field_bg"`, …), `"chart1"` … `"chart8"`, or `"#rrggbb"`. A click on a canvas with
an `action` sends it with the point in `values._x` / `values._y` (canvas units).

## Events

```json
{"action": "save", "values": {"city": "Lisbon", "units": "metric", "alerts": true}}
```

- `action` is the button's `action`, an input's `submit`, a toggle's or select's `action`,
  a table's `select`, a canvas's `action`, or `"edit"` for a table cell edit.
- `values` holds every named field: input text (string), toggle (bool), select (string),
  table selection (row index or null). Typing sends nothing: the text arrives with the next
  event.
- To tell several similar buttons apart, put the index in the action: `"remove:3"`.

## Replies

```json
{"view": {...}}
{"patch": [...]}
{"view": {...}, "values": {"new": ""}}
```

- `view` replaces the whole view.
- `patch` changes parts of it (below).
- `values` sets fields to what you say, over what the user typed. A reply never changes text
  the user is typing otherwise — even a new `view` keeps it — unless the field's `value` in
  your view changed. So to clear an input after "Add", send `"values": {"new": ""}`.

## Patches

Each patch is an object with one operation. Most find their node by `key` (or by `path`, a
JSON pointer through `children` like `/children/0/children/2`):

| patch | does |
|---|---|
| `{"set": {"key": k, "fields": {"text": "3 left", "tone": "success"}}}` | change some fields of a node |
| `{"replace": {"key": k, "node": node}}` | replace a node |
| `{"append": {"key": k, "nodes": [node, ...]}}` | add children to the end of a column, row or card |
| `{"remove": {"key": k}}` | remove a node |
| `{"rows": {"key": k, "start": i, "rows": [[...], ...], "len": n}}` | write table rows from `start` (growing the table); `len` (optional) cuts or grows it to `n` rows |
| `{"put": {"path": "/children/0/text", "value": "hi"}}` | set what a JSON pointer into the view points at: a node, a field, a table row (`/rows/7`) or cell (`/rows/7/2`) |

Patches apply in order; one that can't apply (no such key, wrong type) is skipped and
reported, the rest still apply.

## Limits and mistakes

byob never rejects a view. It draws what it can and reports what it had to fix, as
`path: message` lines (the path is a JSON pointer into what you sent), e.g.
``/children/2: button: missing field `action` ``. When byob shows you such problems, fix them.

- A node byob doesn't know is left out, or shown as its `"fallback"` text if it has one
  (`{"type": "slider", "fallback": "Volume: 7"}`). A node missing a required field is
  treated the same way.
- At most 32 levels of nesting and 20,000 nodes; table rows (100,000), columns (64), select
  options (1,000) and canvas shapes (20,000) are capped; text longer than 65,536 characters
  is cut.
- Unknown tones and styles are the defaults.

## On steps

A tree can be shown on a step of a reply as a view of kind `byob.tree` (version 1; the tree
is the view's `data`, a markdown summary its fallback for clients that can't draw trees).
Other render sites (side pane, settings sections, status items) take the same trees.
