<!-- Generated from docs/mail.md by `cargo xtask help-docs`. Edit docs/mail.md, not this copy. -->
# Mail

The bundled `mail` plugin gives the model one email account over IMAP (reading, drafts,
flags, folders) and SMTP (sending). Its backend is in-process Rust: `crates/mail`
(`byob-mail`, L4), registered by the app under native id `mail` (default feature
`native-mail`). It ships switched off: turn it on under Settings → Plugins → Mail and fill in
the account.

## Settings

| Key | | |
|---|---|---|
| `PROVIDER` | text | `fastmail`, `icloud`, `gmail` or `custom` (default). Fills in the servers; anything set below wins. |
| `USERNAME` | required | Usually the full address. |
| `PASSWORD` | required, **secret** | An app password where the provider has them (Gmail and iCloud require one). Stored in `secrets.json`, sent to the backend in `init.secrets`, never to clients. |
| `FROM_ADDRESS`, `FROM_NAME` | | `From:`; the address defaults to the username when it is one. |
| `IMAP_HOST`, `IMAP_PORT`, `IMAP_SECURITY` | | `tls` (default, port 993), `starttls` (143), `none` (local test servers only). |
| `SMTP_HOST`, `SMTP_PORT`, `SMTP_SECURITY` | | `tls` (default, 465), `starttls` (587), `none`. |
| `DRAFTS_FOLDER`, `SENT_FOLDER` | | Default: the folder with the server's `\Drafts` / `\Sent` special-use flag, else a common name. |
| `SAVE_SENT` | | `auto` (default): append sent mail to Sent unless the SMTP host is Gmail or Outlook (they file it themselves); `always`; `never`. |

STARTTLS is required when chosen (no fallback to plain). Certificates are checked against
the webpki roots (the same rustls + ring stack as inference).

## Tools

Ids are `Folder:UID`, e.g. `INBOX:42` (the last `:` splits, so folders may contain one).

- `mail_folders` (read): folders with message and unread counts and special use.
- `mail_search` (read): `folder` (default INBOX), `from`, `to`, `subject`, `text`, `since`,
  `before` (YYYY-MM-DD), `unread`, `flagged`, `limit` (10, ≤ 50). Newest first (highest
  UID); per message: id, date, from, subject, flags (unread, flagged, replied, draft),
  attachment count and a ≤ 160-character snippet (quoted lines skipped). Non-ASCII terms go as
  `CHARSET UTF-8` quoted strings (strict servers would want literals).
- `mail_read` (read): headers, flags, attachments (name, type, size; never opened), the body
  as text: text/plain preferred, else HTML converted to text (scripts, styles and images
  dropped, links kept as their text). A window of `max_chars` (12000, 1000–40000) from
  `start`, ending with the `start=` to read on.
- `mail_draft` (write): appends a plain-text message to Drafts (`\Draft \Seen`). With
  `in_reply_to` it sets `In-Reply-To` and `References` from the original, and `to` / `subject`
  default to its Reply-To or From and `Re: …`.
- `mail_send` (network, must confirm): sends over SMTP exactly what the card shows (`to`,
  `cc`, `bcc`, `subject`, `body`), threads it with `in_reply_to` (and flags the original
  `\Answered`), removes `draft` once sent, and appends a copy to Sent per `SAVE_SENT`. Bcc
  goes in the envelope, not the headers. After the SMTP server took the message, later
  problems (no Sent folder, a draft that won't go) are notes in the result, not errors, so
  the model doesn't send twice.
- `mail_mark` (write): `ids`, `action` = read, unread, flag, unflag or move (`folder`). Moves
  use `UID MOVE`, else copy + `\Deleted` + `UID EXPUNGE` (UIDPLUS; without it the originals
  stay marked deleted, and the result says so).

### Sending always asks

`mail_send` declares `"confirm"` in the manifest (see `docs/plugins-backend.md`): its calls
are must-confirm (`Tool::always_ask`), so the approval card (warning: "Sends this email from
your account. It can't be unsent.", preview: the arguments in full, body as a block, scrolling) waits for a
person even under "Work freely", after an "always allow" answer, with `--auto-approve`,
and when a `before_tool_call` hook says allow. Only switching network tools off refuses it.
`crates/agent/src/native_backend_tests.rs`
(`mail_send_always_waits_for_a_person_even_when_everything_is_allowed`) holds this.

## Hostile input

Everything a server sends is untrusted. Every connection has a 15 s connect and 30 s
read/write deadline and reads at most 48 MB; `mail_search` reads at most 24 KB of each
message, `mail_read` 8 MB (bigger messages are cut, and the result says so). Header values,
names and bodies are cleaned (control characters, bidi overrides and zero-width characters
removed) and capped. Results of `mail_search` and `mail_read` start with a line saying the
text is from outside and is data, not instructions; the plugin's prompt says the same.
Nothing a message points to is fetched. Reads use `EXAMINE` and `BODY.PEEK`, so looking
never marks mail read. Folder names the model gives are refused if they hold quotes,
backslashes or control characters; names are shown decoded from IMAP's modified UTF-7.

Each call opens its own session (connect, log in, work, log out); calls run one at a time on
the backend's thread. A stopped turn abandons the call (the engine stops waiting); the
backend finishes it within its deadlines.

## Not yet

OAuth (Gmail/Outlook without app passwords), more than one account, an inbox pane, push
(IDLE), sending attachments, HTML mail, saving attachments to the workspace.

## Gmail

Use `PROVIDER` `gmail` and an **app password** (Google Account → Security → App passwords; it
needs 2-Step Verification on). Google no longer accepts the account password over IMAP, and a
Workspace admin can turn IMAP or app passwords off. Gmail shows labels as folders
(`[Gmail]/All Mail`, `[Gmail]/Sent Mail`, one per label), so one message can turn up in several
folders; moving a message adds a label, and moving it out of `INBOX` archives it. Gmail files
sent mail itself, so byob doesn't append a copy (`SAVE_SENT` `auto`). Searches use standard IMAP
`SEARCH`, not Gmail's own syntax.

## Not supported yet

Outlook.com and Microsoft 365: Microsoft has turned off password sign-in for IMAP and SMTP, and
app passwords don't bring it back; they need OAuth, which byob doesn't do yet.
