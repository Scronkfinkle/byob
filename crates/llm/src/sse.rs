//! Server-sent events, the subset OpenAI-style servers use: `data:` lines and `[DONE]`.

pub enum Line<'a> {
    Data(&'a str),
    Done,
    Skip,
}

pub fn parse_line(line: &str) -> Line<'_> {
    let line = line.trim_end_matches(['\r', '\n']);
    let Some(data) = line.strip_prefix("data:") else { return Line::Skip };
    let data = data.trim_start();
    if data == "[DONE]" {
        Line::Done
    } else if data.is_empty() {
        Line::Skip
    } else {
        Line::Data(data)
    }
}
