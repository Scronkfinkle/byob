"""Guard: a demo of byob's turn hooks.

- before_tool_call: delete_file is always denied; write_file always needs a person
  (an approval card even with --auto-approve, because `ask` can't be answered by policy).
- before_prompt: one line added to the system prompt.
- after_tool_call: a small card on the step saying what the guard saw.
- a header chip counting its decisions.
"""

import byob_plugin as byob

tally = {"asked": 0, "denied": 0, "checked": 0}


@byob.hook("before_tool_call")
def guard(call):
    if call["tool"] == "delete_file":
        tally["denied"] += 1
        byob.refresh("tally")
        return byob.deny("Deleting files is turned off by the guard plugin. Tell the user instead.")
    tally["asked"] += 1
    byob.refresh("tally")
    path = (call.get("args") or {}).get("path", "?")
    return byob.ask(f"The guard plugin wants a person to confirm this write to {path} (even with auto-approve).")


@byob.hook("before_prompt")
def prompt(p):
    return byob.modify(add=["Files in this workspace are guarded: writes need the user's confirmation and deleting files is not allowed."])


@byob.hook("after_tool_call")
def checked(call):
    tally["checked"] += 1
    byob.refresh("tally")
    result = call.get("result") or ""
    if not isinstance(result, str):
        result = str(result)
    args = call.get("args") or {}
    what = args.get("path") or args.get("command") or args.get("dir") or ""
    return byob.modify(card={"type": "card", "title": "Guard", "children": [
        {"type": "row", "children": [
            {"type": "chip", "text": "failed" if call.get("failed") else "checked", "tone": "danger" if call.get("failed") else "success"},
            {"type": "text", "text": f"{call['tool']} {what}".strip(), "style": "mono"},
        ]},
        {"type": "text", "text": f"{len(result)} characters of result went back to the model.", "style": "small", "tone": "dim"},
    ]})


@byob.view("tally")
def tally_chip():
    return {"type": "chip", "text": f"guard: {tally['asked']} asked · {tally['denied']} denied · {tally['checked']} checked", "tone": "warning" if tally["denied"] else "accent"}


byob.run()
