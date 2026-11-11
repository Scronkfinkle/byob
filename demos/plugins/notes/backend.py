import byob_plugin as byob

notes = ["Water the basil", "Call Ana"]


@byob.tool
def add(a: int = 0, b: int = 0):
    return str(a + b)


@byob.tool
def show_notes():
    return {"text": "Notes: " + ", ".join(notes), "card": {"notes": notes}, "card_view": "notescard"}


@byob.on_web()
def notes_page(msg, event):
    if isinstance(msg, dict) and msg.get("add"):
        notes.append(str(msg["add"])[:200])
        byob.web_send("notes", {"notes": notes})
        return None
    return {"notes": notes}


byob.run()
