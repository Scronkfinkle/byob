//! Replies that don't arrive whole: a stream that breaks off and is tried again, a reply cut
//! off at the model's output limit (mid-call, or while still thinking), a failed reply
//! followed by another message.

use byob_types::config::ToolMode;

use super::*;

/// byob talking to a model without native tools (prompt-based tool calling).
fn prompt_mode() -> World {
    World::with(|c| c.adapt = crate::AdaptOptions { tool_mode: ToolMode::Prompt, probe: false })
}

/// Regression: a stream that broke off after some text (llama-server's "Failed to parse tool
/// call" comes after the text before the call) was tried again, and the text of both tries
/// stayed in the reply: shown twice, saved twice, sent back to the model twice.
#[test]
fn a_reply_that_breaks_off_and_is_tried_again_reads_once() {
    let mut w = World::new();
    w.model.answers(|_| Answer::text("Let me look at the notes. ").failing("Failed to parse tool call arguments"));
    w.model.says("Let me look at the notes. They're fine.");
    w.model.says("You're welcome.");
    let turn = w.send("check my notes");
    assert_eq!(turn.error, None);
    assert_eq!(turn.text(), "Let me look at the notes. They're fine.");
    // What was saved reads the same, and is what the model is reminded of.
    w.restart();
    let reply = w.seen.entries.iter().find(|e| e.id == turn.reply.id).unwrap();
    assert_eq!(reply.content, "Let me look at the notes. They're fine.");
    w.send("thanks");
    let sent = w.model.last();
    let assistant: Vec<&Value> = sent.messages().iter().filter(|m| m["role"] == "assistant").collect();
    assert_eq!(assistant.len(), 1);
    assert_eq!(assistant[0]["content"], "Let me look at the notes. They're fine.");
}

/// Regression: a model without native tools hit its output limit while writing a file in
/// Claude's `<invoke>` format; the unfinished call ran and wrote half the file. It isn't
/// run, and the model is told why.
#[test]
fn a_file_cut_off_at_the_output_limit_is_not_written() {
    let mut w = prompt_mode();
    w.model.answers(|_| {
        Answer::text(
            "Writing it.\n<function_calls>\n<invoke name=\"write_file\">\n<parameter name=\"path\">notes.md</parameter>\n<parameter name=\"content\"># Notes\n\nFirst half of",
        )
        .cut()
    });
    w.model.answers(|r| {
        let told = r.user_texts().last().cloned().unwrap_or_default();
        assert!(told.contains("output limit"), "{told}");
        Answer::text("It was too long; I'll write it in parts.")
    });
    let turn = w.send("write my notes to notes.md");
    assert_eq!(turn.error, None);
    assert_eq!(w.read("notes.md"), None, "half a file was written");
    let step = turn.step("write_file");
    assert!(step.failed().contains("output limit"), "{}", step.detail);
    assert!(turn.text().ends_with("It was too long; I'll write it in parts."), "{}", turn.text());
}

/// A call that ends before the limit still runs, closing tag or not (models stop there).
#[test]
fn a_finished_call_in_a_reply_cut_off_later_runs() {
    let mut w = prompt_mode();
    w.model.answers(|_| Answer::text("<invoke name=\"write_file\">\n<parameter name=\"path\">a.md</parameter>\n<parameter name=\"content\">hi</parameter>\n</invoke>\nAnd now a long expl").cut());
    w.model.says("Done.");
    let turn = w.send("write a.md");
    turn.step("write_file").done();
    assert_eq!(w.read("a.md").as_deref(), Some("hi"));
}

/// Native tools: the arguments are cut short, so they aren't JSON; the model is told it ran
/// out of room, not only that its JSON is wrong (or it writes the same call again).
#[test]
fn a_native_call_cut_off_at_the_output_limit_tells_the_model_why() {
    let mut w = World::new();
    w.model.answers(|_| Answer::raw_call("write_file", r##"{"path": "notes.md", "content": "# Notes\n\nFirst half"##).cut());
    w.model.says("I'll write it in parts.");
    let turn = w.send("write my notes");
    assert_eq!(w.read("notes.md"), None);
    let result = turn.result("write_file");
    assert!(result.contains("output limit"), "{result}");
}

/// Regression: a reply cut off while the model was still thinking (no text yet) was asked
/// for again with the "you said nothing" nudge, which only spends the same output limit a
/// second time. It says it was cut off.
#[test]
fn thinking_cut_off_at_the_limit_is_not_asked_again() {
    let mut w = World::new();
    w.model.answers(|_| Answer::text("<think>First I should consider every angle of this. Then").cut());
    let turn = w.send("plan my week");
    assert_eq!(turn.error, None);
    assert!(turn.text().contains("The reply was cut off"), "{}", turn.text());
    assert_eq!(w.model.requests().len(), 1, "asked once");
}

/// Regression: after a reply that failed (or was stopped) the next request had two user
/// messages in a row, which chat templates that insist on alternating roles (Gemma,
/// Mistral) refuse; from then on every message in the chat failed.
#[test]
fn after_a_failed_reply_the_roles_still_alternate() {
    let mut w = World::new();
    for _ in 0..3 {
        w.model.answers(|_| Answer::text("").failing("Failed to parse input"));
    }
    let failed = w.send("first");
    assert!(failed.error.is_some());
    w.model.answers(|r| {
        let roles: Vec<&str> = r.messages().iter().map(|m| m["role"].as_str().unwrap_or("")).collect();
        assert_eq!(roles, ["system", "user", "assistant", "user"]);
        assert_eq!(r.user_texts(), ["first", "second"]);
        Answer::text("Hello.")
    });
    let turn = w.send("second");
    assert_eq!(turn.error, None);
    assert_eq!(turn.text(), "Hello.");
}

/// Regression: a model without native tools wrote its call, then went on with the result
/// it imagined and more calls built on it (seen with Claude via Claude Code, sometimes for
/// tens of thousands of tokens). The reply ends at the made-up result: the real call runs,
/// the imagined ones don't, and none of it shows.
#[test]
fn a_model_that_makes_up_its_tool_results_is_stopped_there() {
    let mut w = prompt_mode();
    w.file("notes.md", "buy milk");
    w.model.answers(|_| {
        Answer::text(
            "Let me read it.\n<function_calls>\n<invoke name=\"read_file\">\n<parameter name=\"path\">notes.md</parameter>\n</invoke>\n</function_calls>\n<function_results>\nbuy eggs\n</function_results>\n<function_calls>\n<invoke name=\"write_file\">\n<parameter name=\"path\">x.md</parameter>\n<parameter name=\"content\">eggs</parameter>\n</invoke>\n</function_calls>\nDone, you need eggs.",
        )
    });
    w.model.answers(|r| {
        let result = r.user_texts().last().cloned().unwrap_or_default();
        assert!(result.contains("buy milk"), "{result}");
        let sent = r.messages().iter().rev().find(|m| m["role"] == "assistant").map(content_text).unwrap_or_default();
        assert!(!sent.contains("eggs"), "the made-up part went back to the model: {sent}");
        Answer::text("You need milk.")
    });
    let turn = w.send("what's in my notes?");
    assert_eq!(turn.tools(), ["read_file"]);
    turn.step("read_file").done();
    assert_eq!(w.read("x.md"), None);
    assert!(!turn.text().contains("eggs"), "{}", turn.text());
    assert!(turn.text().ends_with("You need milk."), "{}", turn.text());
}

/// Regression: a model without native tools that reached the turn's tool-call limit (tools
/// off, and no tool prompt) wrote one more call anyway, and its markup was the answer the
/// user read and the chat kept. The call is still ignored, and its markup hidden.
#[test]
fn a_call_past_the_limit_is_ignored_and_not_shown() {
    let mut w = World::with(|c| {
        c.adapt = crate::AdaptOptions { tool_mode: ToolMode::Prompt, probe: false };
        c.defaults.max_tool_calls = Some(2);
    });
    w.file("a.md", "one");
    w.file("b.md", "two");
    w.file("c.md", "three");
    for f in ["a.md", "b.md"] {
        w.model.answers(move |_| Answer::text(format!("<function_call>\n{{\"name\": \"read_file\", \"arguments\": {{\"path\": \"{f}\"}}}}\n</function_call>")));
    }
    w.model.answers(|r| {
        assert!(r.tools().is_empty() && !r.system().contains("<function_call>"), "tools are off");
        Answer::text("They say one and two. Let me read the last one.\n<function_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"c.md\"}}\n</function_call>")
    });
    let turn = w.send("read my notes");
    assert_eq!(turn.steps("read_file").len(), 2);
    assert_eq!(turn.text().trim_end(), "They say one and two. Let me read the last one.");
    w.restart();
    let saved = w.seen.entries.iter().find(|e| e.id == turn.reply.id).unwrap();
    assert!(!saved.content.contains("function_call"), "{}", saved.content);
}
