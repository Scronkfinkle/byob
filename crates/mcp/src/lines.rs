//! Bounded line reading and a blocking Server-Sent Events parser. Input is hostile:
//! every buffer is capped and oversized lines/events are skipped, never fatal.

use std::io::{self, BufRead};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LineRead {
    /// A complete line (without `\n` / `\r\n`) is in the buffer.
    Line,
    /// The line was longer than the cap; it was consumed and dropped.
    TooLong,
    /// End of input.
    Eof,
}

/// Read one line into `buf` (cleared first), holding at most `max` bytes.
/// A final line without a trailing newline still counts as a line.
pub(crate) fn read_line(r: &mut impl BufRead, buf: &mut Vec<u8>, max: usize) -> io::Result<LineRead> {
    buf.clear();
    let mut overflow = false;
    let mut any = false;
    loop {
        let avail = match r.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if avail.is_empty() {
            return Ok(match (overflow, any) {
                (true, _) => LineRead::TooLong,
                (false, true) => {
                    trim_cr(buf);
                    LineRead::Line
                }
                (false, false) => LineRead::Eof,
            });
        }
        any = true;
        let newline = avail.iter().position(|&b| b == b'\n');
        let (chunk, used) = match newline {
            Some(i) => (avail.split_at(i).0, i.saturating_add(1)),
            None => (avail, avail.len()),
        };
        if !overflow {
            if buf.len().saturating_add(chunk.len()) > max {
                overflow = true;
                buf.clear();
                buf.shrink_to(64 * 1024);
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        r.consume(used);
        if newline.is_some() {
            if overflow {
                return Ok(LineRead::TooLong);
            }
            trim_cr(buf);
            return Ok(LineRead::Line);
        }
    }
}

fn trim_cr(buf: &mut Vec<u8>) {
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
}

/// One dispatched SSE event.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// `event:` field, `"message"` when absent.
    pub event: String,
    pub data: String,
}

/// Pulls events out of a `text/event-stream` body
/// (<https://html.spec.whatwg.org/multipage/server-sent-events.html>).
pub(crate) struct SseReader<R> {
    reader: R,
    line: Vec<u8>,
    /// Cap on one event's total data.
    max: usize,
}

impl<R: BufRead> SseReader<R> {
    pub fn new(reader: R, max: usize) -> Self {
        Self { reader, line: Vec::new(), max }
    }

    /// Next event, or `None` at end of stream. Oversized events are skipped.
    pub fn next_event(&mut self) -> io::Result<Option<SseEvent>> {
        let mut event = String::new();
        let mut data = String::new();
        let mut has_data = false;
        let mut oversized = false;
        loop {
            let read = read_line(&mut self.reader, &mut self.line, self.max)?;
            match read {
                LineRead::Eof => {
                    // An unterminated final event is dropped, per the spec.
                    return Ok(None);
                }
                LineRead::TooLong => {
                    oversized = true;
                    continue;
                }
                LineRead::Line => {}
            }
            if self.line.is_empty() {
                if has_data && !oversized {
                    let event = if event.is_empty() { "message".to_owned() } else { event };
                    return Ok(Some(SseEvent { event, data }));
                }
                if oversized {
                    log::warn!("mcp: skipped an oversized SSE event");
                }
                event.clear();
                data.clear();
                has_data = false;
                oversized = false;
                continue;
            }
            let line = String::from_utf8_lossy(&self.line);
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (line.as_ref(), ""),
            };
            match field {
                "event" => event = value.to_owned(),
                "data" if !oversized => {
                    if data.len().saturating_add(value.len()).saturating_add(1) > self.max {
                        oversized = true;
                        data.clear();
                    } else {
                        if has_data {
                            data.push('\n');
                        }
                        data.push_str(value);
                        has_data = true;
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &[u8], max: usize) -> Vec<Result<String, LineRead>> {
        let mut r = io::BufReader::with_capacity(4, input);
        let mut buf = Vec::new();
        let mut out = Vec::new();
        loop {
            match read_line(&mut r, &mut buf, max).unwrap() {
                LineRead::Line => out.push(Ok(String::from_utf8(buf.clone()).unwrap())),
                LineRead::Eof => return out,
                other => out.push(Err(other)),
            }
        }
    }

    #[test]
    fn splits_lines_and_strips_crlf() {
        assert_eq!(lines(b"ab\r\ncd\n\nlast", 100), vec![Ok("ab".into()), Ok("cd".into()), Ok(String::new()), Ok("last".into())]);
    }

    #[test]
    fn oversized_line_is_skipped_not_fatal() {
        assert_eq!(lines(b"short\nwaaaaaaaaaaay too long\nok\n", 8), vec![Ok("short".into()), Err(LineRead::TooLong), Ok("ok".into())]);
        assert_eq!(lines(b"ok\nwaaaaaaaaaaay too long", 8), vec![Ok("ok".into()), Err(LineRead::TooLong)]);
    }

    fn events(input: &str, max: usize) -> Vec<SseEvent> {
        let mut r = SseReader::new(io::BufReader::with_capacity(3, input.as_bytes()), max);
        let mut out = Vec::new();
        while let Some(e) = r.next_event().unwrap() {
            out.push(e);
        }
        out
    }

    #[test]
    fn parses_sse_events() {
        let got = events(": hi\nevent: endpoint\ndata: /x\n\ndata:a\ndata: b\r\n\r\nid: 3\n\ndata: tail-without-blank", 100);
        assert_eq!(got, vec![SseEvent { event: "endpoint".into(), data: "/x".into() }, SseEvent { event: "message".into(), data: "a\nb".into() },]);
    }

    #[test]
    fn oversized_sse_event_is_skipped() {
        let got = events("data: 0123456789012345\n\ndata: ok\n\n", 10);
        assert_eq!(got, vec![SseEvent { event: "message".into(), data: "ok".into() }]);
        let got = events("data: 01234\ndata: 56789\n\ndata: ok\n\n", 10);
        assert_eq!(got, vec![SseEvent { event: "message".into(), data: "ok".into() }]);
    }
}
