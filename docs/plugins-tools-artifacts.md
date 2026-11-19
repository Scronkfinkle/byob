# Plugins, tools and artifacts in byob

byob has three building blocks, and each shows up differently in the interface:

| | What it is | Grammar | How you meet it |
|---|---|---|---|
| **Plugin** | A package of abilities you install or switch on | Setting | Switches and settings |
| **Tool** | One action the model can take | Verb | Steps in a reply |
| **Artifact** | A thing the model makes for you to keep | Noun | Cards, tabs, versions |

**Plugins** bring the **tools**, and some tools make **artifacts**.

Analogy: plugins are the appliances in a kitchen, tools are what the cook can do with them (bake, blend), and artifacts are the dishes that come out, which you can keep, change and take home.

---

## Plugins: the packages

A plugin is a bundle that adds a capability to byob. Most of byob is built this way: it ships with 22 plugins, including its file tools.

A plugin can bring any mix of:
- **Tools** for the model (the Web plugin brings web search and fetch).
- **Guidance** in the model's instructions (how to write HTML apps).
- **Settings and permissions** (Mail: your account, and "confirm every send").
- **Kinds of artifact** and how they're displayed (Charts, Markdown, Math).
- **Interface pieces**: panes, views, header items, `/commands`, suggested prompts ("Benchmark this model").
- **Behind-the-scenes behavior**, such as summarizing long chats (Compaction).

Some plugins have no tools at all. Math only teaches byob to typeset formulas.

**In the interface:**
- Settings → Plugins lists every plugin with a switch, its settings and its status.
- A plugin can add items to the header or open a pane beside the chat.
- A few plugins are locked on because byob can't work without them (the file tools, artifacts).

**Turning a plugin off** removes everything it brought: tools, guidance, commands, views. Things it already made, like artifacts, stay.

**Where plugins come from:** bundled with byob, installed from a folder, or written by the model on request.

---

## Tools: the actions

A tool is one thing the model can do while it works on a reply: read a file, search the web, make a chart, change the app's look, run a benchmark.

Each tool has:
- **A name and description**, which tell the model when to use it.
- **An access level.** Reading is free. Writing or anything risky asks you first, unless you've said "always allow" or turned on YOLO mode.
- **A result** the model reads before deciding what to do next.

**In the interface:**
- Each tool call is a **step** in the reply, written in plain language: "Read notes.md", "Searched the web", "Made Loan calculator".
- Steps show a status: running, done, failed, or **waiting for your approval** (shown as a card with Allow / Deny).
- A step that changed files says so ("1 file"), and **Undo changes** under the reply reverses them.

**A tool only exists while its plugin is on.** Someone who never sees a "send email" step might simply have the Mail plugin off.

---

## Artifacts: the things made

An artifact is something the model makes for you to keep, instead of text in the chat: a report, an interactive app, a chart, a spreadsheet, an image, a data file.

**What makes it an artifact:**
- **Versions.** Every change adds a version (v1, v2, v3…), and old versions are never overwritten. You can go back to any of them.
- **A type**, which decides how it's displayed:

  | Type | Shown as |
  |---|---|
  | Document (Markdown) | Formatted text |
  | HTML app | Running and interactive; offline unless given internet access |
  | Chart | A drawn chart; can stay linked to a spreadsheet and redraw |
  | Spreadsheet | An editable grid |
  | Image | The picture |
  | CSV, JSON, code | Source text |

- **A home.** It belongs to its chat and is stored as files in that chat's workspace folder.

**In the interface:**
- **A card in the conversation**, where the reply made it: a type badge ("APP", "DOCUMENT"), the title and version, and a live preview for apps.
- **The artifact pane beside the chat**: tabs, a version picker, Open / Export / Copy, and controls specific to the type, such as an app's **Internet** switch or a spreadsheet's **Charts**. The pane can collapse to a rail or fill the window.
- **You can change some artifacts yourself.** Editing a spreadsheet's cells makes a new version, just as a change by the model does.

---

## How they connect: one request, start to finish

*"Make me a loan calculator."*

1. The **Artifacts** and **HTML apps** plugins are on, so the model has the `create_artifact` **tool** and knows how to write an app.
2. The model calls the tool. A **step** appears: "Made Loan calculator".
3. The tool produces an **artifact**: a card appears in the reply, and the app opens running in the pane as v1.
4. "Make the button blue" calls `update_artifact`, another tool and another step, and the same artifact gets **v2**.

Most steps never produce an artifact: reading files, searching the web and answering questions leave only text. Every artifact, though, was made by a tool, and every tool comes from a plugin.

---

## Rules of thumb for design

- **Plugins are configuration**, so they belong in settings. Show them as switches, with status ("needs setup", "has trouble") and settings behind them.
- **Tools are activity**, so they belong in the reply's flow. Show them as plain-language steps, with progress, results and approval when needed.
- **Artifacts are objects**, so they belong in places with persistence. Show them as cards and tabs, with versions, preview and export.
- **Lifetimes differ:**
  - A tool call is over when the reply ends.
  - An artifact lasts as long as its chat, and gains versions over time.
  - A plugin lasts until it's switched off.
- **Removing one doesn't remove the others.** Turning off a plugin hides its tools but keeps the artifacts they made. Deleting a chat deletes its artifacts.
