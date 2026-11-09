//! Prompt-based tool calling for servers without native `tools` support.
//!
//! [`tool_prompt`] renders a system-prompt section that teaches the model a Hermes-like
//! `<function_call>{json}</function_call>` format; [`parse_tool_calls`] pulls calls back out of
//! the reply text. The parser is deliberately forgiving: it also accepts Hermes/Qwen
//! `<tool_call>` tags, ```json fences, bare JSON, OpenAI-shaped objects, arrays of calls and
//! Qwen3-Coder's XML-ish `<function=name><parameter=k>v</parameter></function>` form. Model
//! output is hostile input: sizes and counts are capped and nothing here panics.
//!
//! Why not ask for `<tool_call>`: servers with a tool-call parser (llama-server `--jinja`, and
//! proxies in front of it) rewrite or reject `<tool_call>` text in replies even when the
//! request has no `tools` — seen live on 2026-11-09, where a JSON body inside `<tool_call>`
//! aborted the stream with "malformed tool call". `<function_call>` passes through untouched.

use serde_json::Value;

use crate::{ChatMessage, StreamEvent, ToolCall, ToolSpec};

/// Most content bytes we scan for calls.
const MAX_SCAN: usize = 1024 * 1024;
/// Most calls extracted from one reply.
const MAX_CALLS: usize = 64;
/// Bare-JSON scanning limits, so hostile `{{{{…` input stays cheap.
const MAX_BARE_ATTEMPTS: usize = 256;
const MAX_BARE_SPAN: usize = 64 * 1024;

/// The tags we ask for.
const OPEN: &str = "<function_call>";
const CLOSE: &str = "</function_call>";
/// Every recognised (open, close) tag pair; the second is what Hermes/Qwen models are trained on.
const TAGS: [(&str, &str); 2] = [(OPEN, CLOSE), ("<tool_call>", "</tool_call>")];

/// System-prompt section describing `tools` and the `<function_call>` reply format.
/// Returns an empty string when there are no tools.
pub fn tool_prompt(tools: &[ToolSpec]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "# Tools\n\nYou can call the tools below. Each is described by a JSON object with its name, \
         a description, and a JSON Schema for its arguments:\n\n<tools>\n",
    );
    for t in tools {
        let spec = serde_json::json!({"name": t.name, "description": t.description, "parameters": t.parameters});
        out.push_str(&spec.to_string());
        out.push('\n');
    }
    out.push_str(
        "</tools>\n\nTo call a tool, write a block in exactly this format:\n\n\
         <function_call>\n{\"name\": \"<tool name>\", \"arguments\": {<arguments as a JSON object>}}\n</function_call>\n\n\
         You may write several <function_call> blocks in a row. After your calls, stop and wait: \
         the results come back in a <tool_response> message. Only call tools listed above. \
         If no tool is needed, answer normally without any <function_call> block.",
    );
    out
}

/// An assistant turn for the transcript, with its calls rendered back as `<function_call>` text
/// (for servers that reject the `tool_calls` field).
pub fn fallback_assistant_message(text: &str, calls: &[ToolCall]) -> ChatMessage {
    let mut content = text.trim_end().to_owned();
    for c in calls {
        let args: Value = serde_json::from_str(&c.arguments).unwrap_or_else(|_| Value::String(c.arguments.clone()));
        let call = serde_json::json!({"name": c.name, "arguments": args});
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&format!("{OPEN}\n{call}\n{CLOSE}"));
    }
    ChatMessage::assistant(content)
}

/// A tool result as a plain `user` message (for servers without the `tool` role).
pub fn fallback_tool_result(call: &ToolCall, result: &str) -> ChatMessage {
    ChatMessage::user(format!("{RESPONSE_OPEN} name=\"{}\">\n{result}\n</tool_response>", call.name))
}

const RESPONSE_OPEN: &str = "<tool_response";

/// A tool result in either form: a `tool` message, or a [`fallback_tool_result`] `user` message.
pub fn is_tool_result(m: &ChatMessage) -> bool {
    m.role == "tool" || m.role == "user" && m.content.as_deref().is_some_and(|c| c.starts_with(RESPONSE_OPEN))
}

/// Rewrite native tool-calling messages (assistant `tool_calls`, `tool` results) into the
/// text-only fallback form, e.g. when switching a running turn to prompt-based tool calling.
pub fn to_fallback(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut names = std::collections::BTreeMap::new();
    messages
        .iter()
        .map(|m| {
            if m.role == "assistant" && !m.tool_calls.is_empty() {
                let calls: Vec<ToolCall> = m
                    .tool_calls
                    .iter()
                    .map(|c| ToolCall { id: c.id.clone(), name: c.function.name.clone(), arguments: c.function.arguments.clone() })
                    .collect();
                for c in &calls {
                    names.insert(c.id.clone(), c.name.clone());
                }
                fallback_assistant_message(m.content.as_deref().unwrap_or_default(), &calls)
            } else if m.role == "tool" {
                let id = m.tool_call_id.clone().unwrap_or_default();
                let name = names.get(&id).cloned().unwrap_or_else(|| "tool".into());
                fallback_tool_result(&ToolCall { id, name, arguments: String::new() }, m.content.as_deref().unwrap_or_default())
            } else {
                m.clone()
            }
        })
        .collect()
}

/// Visible text plus the calls extracted from a reply.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedToolCalls {
    /// The reply with all recognised call markup removed, trimmed.
    pub text: String,
    pub calls: Vec<ToolCall>,
}

/// Extract tool calls written as text. `tools` (may be empty) is used to reject bare JSON
/// that does not name a known tool and to type Qwen-XML parameters; calls inside explicit
/// `<tool_call>` tags are accepted whatever their name, so the agent can report the mistake.
pub fn parse_tool_calls(content: &str, tools: &[ToolSpec]) -> ParsedToolCalls {
    let content = prefix_at_boundary(content, MAX_SCAN);
    let mut calls = Vec::new();
    let text = extract_tagged(content, tools, &mut calls);
    let text = extract_fenced(&text, tools, &mut calls);
    let text = extract_bare(&text, tools, &mut calls);
    calls.truncate(MAX_CALLS);
    for (i, c) in calls.iter_mut().enumerate() {
        c.id = format!("call_text_{i}");
    }
    ParsedToolCalls { text: text.trim().to_owned(), calls }
}

fn prefix_at_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    s.get(..end).unwrap_or_default()
}

/// `<tool_call>…</tool_call>` spans (an unterminated last one runs to the end).
fn extract_tagged(content: &str, tools: &[ToolSpec], calls: &mut Vec<ToolCall>) -> String {
    let mut text = String::new();
    let mut rest = content;
    while let Some((start, open, close)) = first_open(rest) {
        text.push_str(rest.get(..start).unwrap_or_default());
        let after = rest.get(start + open.len()..).unwrap_or_default();
        let (inner, next) = match after.find(close) {
            Some(end) => (after.get(..end).unwrap_or_default(), after.get(end + close.len()..).unwrap_or_default()),
            None => (after, ""),
        };
        let inner = strip_fence(inner.trim());
        let before = calls.len();
        calls_from_json_text(inner, None, calls);
        if calls.len() == before {
            calls_from_qwen_xml(inner, tools, calls);
        }
        rest = next;
    }
    text.push_str(rest);
    text
}

/// The earliest opening call tag in `s`: (position, open tag, matching close tag).
fn first_open(s: &str) -> Option<(usize, &'static str, &'static str)> {
    TAGS.iter().filter_map(|&(open, close)| s.find(open).map(|p| (p, open, close))).min_by_key(|&(p, _, _)| p)
}

/// ```lang … ``` fences whose body is a call to a known tool.
fn extract_fenced(content: &str, tools: &[ToolSpec], calls: &mut Vec<ToolCall>) -> String {
    let mut text = String::new();
    let mut rest = content;
    while let Some(start) = rest.find("```") {
        let after = rest.get(start + 3..).unwrap_or_default();
        let Some(end) = after.find("```") else { break };
        let block = after.get(..end).unwrap_or_default();
        // Drop the language tag line, if any.
        let body = match block.find('\n') {
            Some(nl) if !block.get(..nl).unwrap_or_default().trim_start().starts_with(['{', '[']) => block.get(nl..).unwrap_or_default(),
            _ => block,
        };
        let before = calls.len();
        calls_from_json_text(body.trim(), Some(tools), calls);
        let span_end = start + 3 + end + 3;
        if calls.len() == before {
            text.push_str(rest.get(..span_end).unwrap_or_default());
        } else {
            text.push_str(rest.get(..start).unwrap_or_default());
        }
        rest = rest.get(span_end..).unwrap_or_default();
    }
    text.push_str(rest);
    text
}

/// Balanced `{…}` / `[…]` JSON embedded in prose that names a known tool.
fn extract_bare(content: &str, tools: &[ToolSpec], calls: &mut Vec<ToolCall>) -> String {
    let mut text = String::new();
    let mut pos = 0;
    let bytes = content.as_bytes();
    let mut i = 0;
    let mut attempts = 0;
    while i < bytes.len() && calls.len() < MAX_CALLS && attempts < MAX_BARE_ATTEMPTS {
        let b = bytes.get(i).copied().unwrap_or(0);
        if b == b'{' || b == b'[' {
            attempts += 1;
            if let Some(end) = balanced_end(content, i) {
                let candidate = content.get(i..end).unwrap_or_default();
                let before = calls.len();
                calls_from_json_text(candidate, Some(tools), calls);
                if calls.len() > before {
                    text.push_str(content.get(pos..i).unwrap_or_default());
                    pos = end;
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    text.push_str(content.get(pos..).unwrap_or_default());
    text
}

/// Byte index just past the bracket matching the one at `start`, honouring JSON strings.
fn balanced_end(s: &str, start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (off, b) in s.as_bytes().get(start..)?.iter().take(MAX_BARE_SPAN).enumerate() {
        if in_str {
            match (escaped, b) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(start + off + 1);
                }
            }
            _ => {}
        }
    }
    None
}

fn strip_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else { return s };
    let rest = rest.trim_end().strip_suffix("```").unwrap_or(rest);
    match rest.find('\n') {
        Some(nl) if !rest.get(..nl).unwrap_or_default().trim_start().starts_with(['{', '[']) => rest.get(nl..).unwrap_or_default().trim(),
        _ => rest.trim(),
    }
}

/// Parse `text` as one JSON value or a run of JSON values and collect the calls in it.
/// With `known = Some(tools)`, only calls to a listed tool count (any name if `tools` is empty,
/// but then an arguments field is required).
fn calls_from_json_text(text: &str, known: Option<&[ToolSpec]>, calls: &mut Vec<ToolCall>) {
    let found: Vec<(String, String, bool)> =
        serde_json::Deserializer::from_str(text).into_iter::<Value>().map_while(Result::ok).flat_map(|v| calls_from_value(&v)).collect();
    // Under a known-names check every object must qualify, so prose JSON is never half-eaten.
    if let Some(tools) = known {
        let ok = |(name, _, has_args): &(String, String, bool)| {
            if tools.is_empty() { *has_args } else { tools.iter().any(|t| &t.name == name) }
        };
        if found.is_empty() || !found.iter().all(ok) {
            return;
        }
    }
    for (name, arguments, _) in found {
        if calls.len() >= MAX_CALLS {
            return;
        }
        calls.push(ToolCall { id: String::new(), name, arguments });
    }
}

/// (name, arguments JSON, had an explicit arguments field) for each call in `v`.
fn calls_from_value(v: &Value) -> Vec<(String, String, bool)> {
    match v {
        Value::Array(items) => items.iter().take(MAX_CALLS).filter_map(call_from_object).collect(),
        Value::Object(_) => call_from_object(v).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn call_from_object(v: &Value) -> Option<(String, String, bool)> {
    let obj = v.as_object()?;
    if let Some(f) = obj.get("function").filter(|f| f.is_object()) {
        return call_from_object(f);
    }
    let name = ["name", "tool", "tool_name", "function"].iter().find_map(|k| obj.get(*k).and_then(Value::as_str))?.trim();
    if name.is_empty() || name.len() > 256 {
        return None;
    }
    let args = ["arguments", "parameters", "args", "input"].iter().find_map(|k| obj.get(*k));
    let arguments = match args {
        Some(Value::String(s)) => s.clone(),
        Some(v @ Value::Object(_)) => v.to_string(),
        Some(Value::Null) | None => "{}".to_owned(),
        Some(_) => return None,
    };
    Some((name.to_owned(), arguments, args.is_some()))
}

/// Qwen3-Coder style: `<function=name><parameter=key>value</parameter>…</function>`.
fn calls_from_qwen_xml(text: &str, tools: &[ToolSpec], calls: &mut Vec<ToolCall>) {
    let mut rest = text;
    while let Some(start) = rest.find("<function=") {
        if calls.len() >= MAX_CALLS {
            return;
        }
        let after = rest.get(start + "<function=".len()..).unwrap_or_default();
        let Some(gt) = after.find('>') else { return };
        let name = after.get(..gt).unwrap_or_default().trim().trim_matches(['"', '\'']).to_owned();
        let body_all = after.get(gt + 1..).unwrap_or_default();
        let (body, next) = match body_all.find("</function>") {
            Some(end) => (body_all.get(..end).unwrap_or_default(), body_all.get(end + "</function>".len()..).unwrap_or_default()),
            None => (body_all, ""),
        };
        if !name.is_empty() {
            let schema = tools.iter().find(|t| t.name == name).map(|t| &t.parameters);
            let args = qwen_params(body, schema);
            calls.push(ToolCall { id: String::new(), name, arguments: Value::Object(args).to_string() });
        }
        rest = next;
    }
}

fn qwen_params(body: &str, schema: Option<&Value>) -> serde_json::Map<String, Value> {
    let mut args = serde_json::Map::new();
    let mut rest = body;
    while let Some(start) = rest.find("<parameter=") {
        let after = rest.get(start + "<parameter=".len()..).unwrap_or_default();
        let Some(gt) = after.find('>') else { break };
        let key = after.get(..gt).unwrap_or_default().trim().trim_matches(['"', '\'']).to_owned();
        let val_all = after.get(gt + 1..).unwrap_or_default();
        let (raw, next) = match val_all.find("</parameter>") {
            Some(end) => (val_all.get(..end).unwrap_or_default(), val_all.get(end + "</parameter>".len()..).unwrap_or_default()),
            None => (val_all.split("<parameter=").next().unwrap_or_default(), val_all.find("<parameter=").and_then(|p| val_all.get(p..)).unwrap_or("")),
        };
        let raw = raw.strip_prefix('\n').unwrap_or(raw);
        let raw = raw.strip_suffix('\n').unwrap_or(raw);
        let is_string = schema.and_then(|s| s.pointer(&format!("/properties/{key}/type"))).and_then(Value::as_str) == Some("string");
        let value = if is_string { Value::String(raw.to_owned()) } else { serde_json::from_str(raw.trim()).unwrap_or_else(|_| Value::String(raw.to_owned())) };
        if !key.is_empty() {
            args.insert(key, value);
        }
        rest = next;
    }
    args
}

/// Streaming display filter: passes text through but hides `<function_call>…</function_call>`
/// and `<tool_call>…</tool_call>` spans, holding back any suffix that could be the start of a
/// tag. Feed the *unfiltered* content to [`parse_tool_calls`] once the reply is complete.
/// Fenced/bare JSON is not hidden.
#[derive(Debug, Default)]
pub struct ToolCallFilter {
    /// Inside a call: the close tag we are waiting for.
    close: Option<&'static str>,
    held: String,
}

/// Length of the longest proper prefix of `tag` that `s` ends with.
fn partial_tag(s: &str, tag: &str) -> usize {
    (1..tag.len()).rev().find(|&k| s.is_char_boundary(s.len().saturating_sub(k)) && s.ends_with(tag.get(..k).unwrap_or_default())).unwrap_or(0)
}

impl ToolCallFilter {
    /// Visible text from the next chunk (possibly empty).
    pub fn push(&mut self, chunk: &str) -> String {
        self.held.push_str(chunk);
        let mut out = String::new();
        loop {
            if let Some(close) = self.close {
                if let Some(pos) = self.held.find(close) {
                    self.held.drain(..pos + close.len());
                    self.close = None;
                    continue;
                }
                let split = self.held.len().saturating_sub(partial_tag(&self.held, close));
                self.held.drain(..split);
                return out;
            }
            if let Some((pos, open, close)) = first_open(&self.held) {
                out.extend(self.held.drain(..pos));
                self.held.drain(..open.len());
                self.close = Some(close);
                continue;
            }
            let keep = TAGS.iter().map(|(open, _)| partial_tag(&self.held, open)).max().unwrap_or(0);
            let split = self.held.len().saturating_sub(keep);
            out.extend(self.held.drain(..split));
            return out;
        }
    }

    /// Flush what is left at the end of the stream.
    pub fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.held);
        if self.close.is_some() { String::new() } else { rest }
    }

    /// Convenience for [`StreamEvent`]s: reasoning passes through, content is filtered.
    pub fn filter_event(&mut self, ev: StreamEvent) -> Option<StreamEvent> {
        match ev {
            StreamEvent::Content(t) => Some(self.push(&t)).filter(|s| !s.is_empty()).map(StreamEvent::Content),
            other => Some(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "d".into(),
            parameters: serde_json::json!({"type":"object","properties":{"path":{"type":"string"},"n":{"type":"integer"}}}),
        }
    }

    fn names(p: &ParsedToolCalls) -> Vec<&str> {
        p.calls.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn prompt_lists_tools_and_format() {
        let p = tool_prompt(&[tool("read_file")]);
        assert!(p.contains(r#""name":"read_file""#));
        assert!(p.contains("<function_call>") && !p.contains("<tool_call>"));
        assert!(tool_prompt(&[]).is_empty());
    }

    #[test]
    fn function_call_tags_and_mixed_tags_parse_in_order() {
        let s = "A <function_call>{\"name\":\"a\",\"arguments\":{}}</function_call> B <tool_call>{\"name\":\"b\",\"arguments\":{}}</tool_call> \
                 <function_call>{\"name\":\"c\",\"arguments\":{}}</function_call>";
        let p = parse_tool_calls(s, &[]);
        assert_eq!(names(&p), ["a", "b", "c"]);
        assert_eq!(p.text, "A  B");
        let mut f = ToolCallFilter::default();
        let mut shown = String::new();
        for c in s.as_bytes().chunks(3) {
            shown.push_str(&f.push(std::str::from_utf8(c).unwrap()));
        }
        shown.push_str(&f.finish());
        assert_eq!(shown, "A  B  ");
        // A close tag of the other kind does not end a call.
        let mut f = ToolCallFilter::default();
        assert_eq!(f.push("x<function_call>{</tool_call>}</function_call>y"), "xy");
        assert_eq!(f.finish(), "");
    }

    #[test]
    fn hermes_tags_single_and_multiple() {
        let s = "Let me look.\n<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.txt\"}}\n</tool_call>\n<tool_call>{\"name\":\"ls\",\"arguments\":{}}</tool_call>";
        let p = parse_tool_calls(s, &[tool("read_file")]);
        assert_eq!(names(&p), ["read_file", "ls"]);
        assert_eq!(p.calls[0].arguments, r#"{"path":"a.txt"}"#);
        assert_eq!(p.calls[1].id, "call_text_1");
        assert_eq!(p.text, "Let me look.");
    }

    #[test]
    fn unterminated_tag_still_parses() {
        let p = parse_tool_calls("<tool_call>{\"name\":\"ls\",\"arguments\":{\"n\":1}}", &[]);
        assert_eq!(names(&p), ["ls"]);
        assert!(p.text.is_empty());
    }

    #[test]
    fn json_fence_with_known_tool() {
        let s = "Sure:\n```json\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"x\"}}\n```\nDone.";
        let p = parse_tool_calls(s, &[tool("read_file")]);
        assert_eq!(names(&p), ["read_file"]);
        assert_eq!(p.text, "Sure:\n\nDone.");
    }

    #[test]
    fn fence_with_unknown_name_is_left_alone() {
        let s = "Example config:\n```json\n{\"name\": \"bob\", \"age\": 3}\n```";
        let p = parse_tool_calls(s, &[tool("read_file")]);
        assert!(p.calls.is_empty());
        assert_eq!(p.text, s);
    }

    #[test]
    fn openai_shape_and_arrays_and_string_args() {
        let s = r#"[{"type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a\"}"}},{"name":"read_file","parameters":{"path":"b"}}]"#;
        let p = parse_tool_calls(s, &[tool("read_file")]);
        assert_eq!(p.calls.len(), 2);
        assert_eq!(p.calls[0].arguments, r#"{"path":"a"}"#);
        assert_eq!(p.calls[1].arguments, r#"{"path":"b"}"#);
    }

    #[test]
    fn bare_json_in_prose() {
        let s = "I will call {\"name\": \"read_file\", \"arguments\": {\"path\": \"}{\"}} now";
        let p = parse_tool_calls(s, &[tool("read_file")]);
        assert_eq!(names(&p), ["read_file"]);
        assert_eq!(p.calls[0].arguments, r#"{"path":"}{"}"#);
        assert_eq!(p.text, "I will call  now");
    }

    #[test]
    fn qwen_xml_inside_tags() {
        let s = "<tool_call>\n<function=read_file>\n<parameter=path>\n007\n</parameter>\n<parameter=n>\n5\n</parameter>\n</function>\n</tool_call>";
        let p = parse_tool_calls(s, &[tool("read_file")]);
        assert_eq!(names(&p), ["read_file"]);
        let args: Value = serde_json::from_str(&p.calls[0].arguments).unwrap();
        assert_eq!(args, serde_json::json!({"path": "007", "n": 5}));
    }

    #[test]
    fn garbage_does_not_panic_or_match() {
        for junk in [
            "",
            "<tool_call>",
            "<tool_call></tool_call>",
            "<tool_call>{not json</tool_call>",
            "```",
            "```json\n{",
            "{{{{[[[[",
            "]]]}}}",
            "<function=",
            "<tool_call><function=",
            "{\"name\": 5}",
            "{\"name\": \"\", \"arguments\": {}}",
            "é{ü\"ß",
        ] {
            let p = parse_tool_calls(junk, &[tool("read_file")]);
            assert!(p.calls.is_empty(), "{junk:?} -> {p:?}");
        }
        let many = "<tool_call>{\"name\":\"a\",\"arguments\":{}}</tool_call>".repeat(200);
        assert_eq!(parse_tool_calls(&many, &[]).calls.len(), MAX_CALLS);
        let hostile = "{".repeat(MAX_SCAN);
        assert!(parse_tool_calls(&hostile, &[tool("read_file")]).calls.is_empty());
    }

    #[test]
    fn plain_prose_is_untouched() {
        let s = "The set {1, 2} and the list [3] are fine.";
        assert_eq!(parse_tool_calls(s, &[tool("read_file")]).text, s);
    }

    #[test]
    fn filter_hides_tags_split_across_chunks() {
        let mut f = ToolCallFilter::default();
        let mut shown = String::new();
        for c in ["Hi <tool", "_call>{\"name\":", "\"x\"}</tool_", "call> bye"] {
            shown.push_str(&f.push(c));
        }
        shown.push_str(&f.finish());
        assert_eq!(shown, "Hi  bye");
        let mut f = ToolCallFilter::default();
        assert_eq!(f.push("a<tool_call>{"), "a");
        assert_eq!(f.finish(), "");
    }

    #[test]
    fn fallback_messages_round_trip() {
        let call = ToolCall { id: "1".into(), name: "read_file".into(), arguments: r#"{"path":"a"}"#.into() };
        let m = fallback_assistant_message("ok", std::slice::from_ref(&call));
        let p = parse_tool_calls(m.content.as_deref().unwrap(), &[tool("read_file")]);
        assert_eq!(p.calls[0].arguments, call.arguments);
        assert_eq!(p.text, "ok");
        assert_eq!(fallback_tool_result(&call, "data").role, "user");
        assert!(is_tool_result(&fallback_tool_result(&call, "data")));
        assert!(!is_tool_result(&ChatMessage::user("hi")));
    }

    #[test]
    fn native_history_converts_to_fallback() {
        let call = ToolCall { id: "c1".into(), name: "read_file".into(), arguments: r#"{"path":"a"}"#.into() };
        let h = vec![
            ChatMessage::system("s"),
            ChatMessage::user("u"),
            ChatMessage::assistant_tool_calls(Some("looking".into()), std::slice::from_ref(&call)),
            ChatMessage::tool_result("c1", "data"),
        ];
        let out = to_fallback(&h);
        assert_eq!(out[..2], h[..2]);
        assert!(out[2].tool_calls.is_empty());
        assert_eq!(parse_tool_calls(out[2].content.as_deref().unwrap(), &[]).calls[0].name, "read_file");
        assert_eq!(out[3].role, "user");
        assert!(out[3].content.as_deref().unwrap().contains("name=\"read_file\""));
        assert!(out.iter().all(|m| m.role != "tool"));
    }
}
