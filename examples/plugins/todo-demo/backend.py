"""To-do demo: a pane, a header chip, a settings section and step cards, all live views."""

import byob_plugin as byob

todos = []  # {"id": int, "text": str, "done": bool}
prefs = {"hide_done": False}
next_id = [1]


def find(item_id):
    for t in todos:
        if str(t["id"]) == str(item_id):
            return t
    return None


def add(text):
    item = {"id": next_id[0], "text": text, "done": False}
    next_id[0] += 1
    todos.append(item)
    return item


@byob.view("list")
def todo_list():
    rows = []
    for t in todos:
        if prefs["hide_done"] and t["done"]:
            continue
        rows.append({"type": "row", "key": f"item-{t['id']}", "children": [
            {"type": "toggle", "name": f"done-{t['id']}", "label": t["text"], "value": t["done"], "action": f"toggle:{t['id']}"},
            {"type": "button", "label": "Remove", "action": f"remove:{t['id']}", "danger": True},
        ]})
    if not rows:
        rows = [{"type": "text", "text": "Nothing to do.", "tone": "dim"}]
    left = sum(1 for t in todos if not t["done"])
    return {"type": "column", "children": [
        {"type": "row", "children": [
            {"type": "input", "name": "new", "placeholder": "Add an item…", "submit": "add"},
            {"type": "button", "label": "Add", "action": "add", "primary": True},
        ]},
        {"type": "column", "key": "items", "children": rows},
        {"type": "text", "text": f"{left} left of {len(todos)}", "style": "small", "tone": "dim"},
    ]}


@byob.on_event("list")
def list_event(event):
    action, _, arg = event["action"].partition(":")
    if action == "add":
        text = (event["values"].get("new") or "").strip()
        if text:
            add(text)
        return {"values": {"new": ""}}
    item = find(arg)
    if item and action == "toggle":
        item["done"] = not item["done"]
    elif item and action == "remove":
        todos.remove(item)


@byob.view("count")
def count():
    left = sum(1 for t in todos if not t["done"])
    if not todos:
        return {"type": "chip", "text": "No to-dos", "tone": "dim"}
    return {"type": "chip", "text": f"{left} to do", "tone": "accent" if left else "success"}


@byob.view("prefs")
def settings():
    return {"type": "column", "children": [
        {"type": "toggle", "name": "hide_done", "label": "Hide finished items in the pane", "value": prefs["hide_done"], "action": "hide"},
        {"type": "row", "children": [
            {"type": "button", "label": "Clear finished", "action": "clear"},
            {"type": "text", "text": f"{sum(1 for t in todos if t['done'])} finished", "style": "small", "tone": "dim"},
        ]},
    ]}


@byob.on_event("prefs")
def prefs_event(event):
    if event["action"] == "hide":
        prefs["hide_done"] = bool(event["values"].get("hide_done"))
    elif event["action"] == "clear":
        todos[:] = [t for t in todos if not t["done"]]


@byob.view("item")
def item_card(key):
    t = find(key)
    if t is None:
        return {"type": "text", "text": "This item was removed.", "tone": "dim"}
    return {"type": "card", "title": "To-do", "children": [
        {"type": "row", "children": [
            {"type": "chip", "text": "done" if t["done"] else "open", "tone": "success" if t["done"] else "accent"},
            {"type": "text", "text": t["text"], "style": "strong"},
        ]},
        {"type": "button", "label": "Mark open" if t["done"] else "Mark done", "action": "toggle"},
    ]}


@byob.on_event("item")
def item_event(event):
    t = find(event["key"])
    if t and event["action"] == "toggle":
        t["done"] = not t["done"]


@byob.tool
def todo_add(text: str):
    item = add(text)
    return byob.card(f"Added {text!r} (item {item['id']}); {len(todos)} on the list", "item", key=str(item["id"]))


byob.run()
