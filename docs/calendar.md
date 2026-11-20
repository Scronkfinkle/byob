# Calendar (bundled `calendar` plugin)

The model reads and changes one CalDAV account: lists calendars, reads events in a range,
finds free time, and adds, moves or deletes events. Backend: `crates/calendar`
(`byob-calendar`, L4, native backend `calendar`, registered by the app), blocking `ureq` like
inference, on the backend's own thread. It ships **off** (it needs an account): switch it on
under Settings → Plugins → Calendar and fill in the settings.

## Settings

| key | | |
|---|---|---|
| `CALDAV_URL` | | the server; discovery starts here (unset: the tools say so) |
| `USERNAME` | | usually the email address |
| `PASSWORD` | secret | in secrets.json; reaches only the backend's `init`, never a client |
| `DEFAULT_CALENDAR` | | id or name new events go to (default: the first writable one) |
| `TIME_ZONE` | | IANA zone times are shown and read in (default: `$TZ`, else the system's) |

Servers: Fastmail `https://caldav.fastmail.com/` and iCloud `https://caldav.icloud.com/`
(both with an app-specific password), Nextcloud `https://<host>/remote.php/dav/` (an app
password), Radicale `http://localhost:5232/`. **Google Calendar isn't supported**: its CalDAV
needs OAuth. Basic auth over HTTPS only; plain `http://` is refused except to this machine
(localhost, 127.0.0.1, ::1), so a password never crosses a network unencrypted.

## CalDAV

- **Discovery**: PROPFIND (Depth 0) on the URL for `current-user-principal`,
  `calendar-home-set`, `calendar-user-address-set`; if that fails, the same on
  `/.well-known/caldav`. The principal gives the home set (and the user's address, used as
  ORGANIZER); PROPFIND Depth 1 on the home lists calendars: `displayname`, Apple
  `calendar-color`, `current-user-privilege-set` (no `write`/`write-content`/`bind`/`all`:
  read-only), `supported-calendar-component-set` (collections without VEVENT, i.e. task
  lists, are skipped). A URL that is a calendar itself is used as the only one. Done once per
  backend start (`calendar_list` does it again).
- Redirects are followed by hand (same method, at most 5); the Authorization header only goes
  to the configured origin, and multistatus hrefs on another origin are dropped.
- **Reading**: `REPORT calendar-query` (Depth 1) with a VEVENT `time-range`, per calendar;
  recurrence is expanded by byob, not the server (`<c:expand>` support varies).
- **Writing**: `PUT <calendar>/<uid>.ics` with `If-None-Match: *` for new events; updates GET
  the object, change only the fields given (unknown properties, alarms and overridden
  instances are kept; SEQUENCE, DTSTAMP, LAST-MODIFIED bumped) and PUT with `If-Match: <etag>`;
  DELETE also sends `If-Match`. A 412 tells the model the event changed meanwhile.
- Caps: 8 MiB per answer, 1 MiB per calendar object, 30 s per request, XML depth 64 and
  200,000 elements, iCalendar nesting 8, 2000 components, 500 properties each; a range is at
  most 400 days.

## Tools

| tool | access | |
|---|---|---|
| `calendar_list` | read | calendars: name, id, color, read-only, default |
| `calendar_events(from?, to?, calendar?, query?)` | read | occurrences, sorted, at most 80 |
| `calendar_free_busy(from, to, min_minutes?, hours?, calendar?)` | read | free slots (≤ 40) |
| `calendar_create(title, start, end?, duration_minutes?, all_day?, calendar?, location?, description?, attendees?)` | write | returns the id |
| `calendar_update(id, …fields)` | write | only the fields given change |
| `calendar_delete(id)` | write | always confirmed |

An event id is `<calendar id>/<object name>` (`work/1234.ics`), the calendar id being the
collection's last path segment. A line of `calendar_events`:

```
- Thu 2026-11-19 09:30–09:45 +01:00 · "Standup" · id: work/standup.ics · 2 attendees: "Ann (yes), me@example.com" · organizer "Boss" · repeats weekly on Mon, Thu
  notes: "…"
```

**Event text is outside content**: titles, places, notes, attendee names come from the
calendar and other people's invitations. They are quoted (JSON string escapes, control
characters removed, one line; 200 characters, notes 600), the list starts with a line saying
they are data, not instructions, and the plugin's prompt says the same.

**Free/busy** is computed from the events (no scheduling server): busy are events not
cancelled and not `TRANSP:TRANSPARENT`; all-day events count only when marked `OPAQUE` (most
are birthdays, holidays, reminders). `hours: "09:00-17:00"` limits each day.

**Attendees** are stored on the event (`ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;
RSVP=TRUE`, with an ORGANIZER). byob sends no iTIP/iMIP email itself, but servers with
scheduling (iCloud, Fastmail, Nextcloud) usually email invitations and updates: the reason
for the confirmations below.

## Always asked

Even under "Work freely" (or `--auto-approve`), a person confirms:

- `calendar_create` and `calendar_update` **with attendees** and every `calendar_delete`: the
  manifest's `confirm` rules (`"confirm": {"when": ["attendees"], "warning": …}`, see
  `docs/plugins-backend.md`). They make the tool's `always_ask`, so no policy, grant or other
  plugin's `allow` skips the card.
- `calendar_update` of an event that **already has attendees**: the plugin's own
  `before_tool_call` hook (priority 50, `fail: "closed"`) reads the event and answers `ask`,
  with what the call will do as the card's text (also for the cases above). If the event
  can't be read it asks anyway. Known gap: a hook from another plugin that answers `allow`
  and runs earlier (priority < 50) skips this one (tested in `native_backend_tests`).

## Times

Shown in the user's zone with the offset (`Thu 2026-11-19 15:00 +01:00`); all-day events as
dates (`Fri 2026-11-20 (all day)`, the last day inclusive). The model writes local times
(`2026-11-19T15:00`), times with `Z` or an offset, or dates; a date as `to` covers that day.
Values read: UTC, `TZID` (IANA names, also inside vendor prefixes like
`/mozilla.org/…/Europe/Berlin`, common Windows names like `W. Europe Standard Time`, else the
VTIMEZONE's standard offset), floating (the user's zone) and `DATE`. DST gaps move a time on
an hour; ambiguous times take the earlier. New events are written in UTC (all-day as DATE);
an update keeps an event's IANA `TZID`.

## Recurrence

Expanded on the event's own wall clock (a weekly 09:00 in Berlin stays 09:00 across DST):
FREQ=DAILY/WEEKLY/MONTHLY/YEARLY, INTERVAL, COUNT, UNTIL, BYDAY (with ordinals: `2TU`,
`-1FR`), BYMONTHDAY (negative too), BYMONTH, BYSETPOS, WKST; EXDATE, RDATE, and overridden
instances (RECURRENCE-ID; `STATUS:CANCELLED` ones are marked). At most 40,000 periods per rule
and 500 occurrences per event per call. **Not covered**: SECONDLY/MINUTELY/HOURLY, BYYEARDAY,
BYWEEKNO (the event shows once, at its start, saying the rule can't be expanded);
BYHOUR/BYMINUTE/BYSECOND are ignored (each occurrence keeps DTSTART's time). Updates and
deletes act on the whole series (no single-occurrence edits).

## Out of scope

OAuth (Google), more than one account, an agenda pane, sending invitations (iTIP/iMIP),
sync or caching (every call asks the server), editing one occurrence of a series, VTODO.

## Testing

- Unit tests (`cargo test -p byob-calendar`): the iCalendar reader/writer, times and zones,
  recurrence, and the backend against `fake.rs`, a scripted CalDAV server on a local
  `TcpListener` (well-known redirect, principal, home, a read-only calendar, a task list,
  REPORT, GET/PUT/DELETE with ETags, Basic auth). With the `fake-server` feature the engine's
  tests use it too (`crates/agent/src/native_backend_tests.rs`: the confirmations under
  "Work freely").
- Live, against a throwaway local Radicale (never a real account; the test refuses non-local
  URLs):

  ```sh
  d=$(mktemp -d); nix shell nixpkgs#radicale -c radicale --server-hosts 127.0.0.1:5232 \
    --storage-filesystem-folder "$d/collections" --auth-type none &
  BYOB_LIVE_CALDAV=http://127.0.0.1:5232/ BYOB_LIVE_CALDAV_USER=test \
    cargo test -p byob-calendar live -- --ignored --nocapture
  ```

  It makes a calendar (MKCALENDAR), seeds a recurring event, then lists, finds free time,
  creates, moves and deletes an event, and removes the calendar. With `--auth-type htpasswd`
  set `BYOB_LIVE_CALDAV_PASSWORD` too.
