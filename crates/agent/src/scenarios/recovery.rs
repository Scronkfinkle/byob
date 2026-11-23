//! Things going wrong mid-turn and long turns: failed tool calls the model recovers from,
//! and a turn that outgrows the context window.

use byob_protocol::StepStatus;
use serde_json::json;

use super::{Answer, SUMMARY, World};

/// A turn where the model's first tries fail (a misspelled file, arguments that aren't
/// JSON, a tool that doesn't exist): each failure's text reaches the model in its next
/// request, it corrects itself, and the turn ends with its answer, not an error.
#[test]
fn the_model_reads_its_tool_errors_and_recovers() {
    let mut w = World::new();
    w.file("harvest.csv", "crop,kg\nbeans,40\ntomatoes,90\n");
    w.model.calls("read_file", json!({"path": "harvest.cvs"}));
    w.model.answers(|sent| {
        let error = sent.last_result();
        assert!(error.contains("harvest.cvs"), "the error names the path: {error}");
        Answer::raw_call("read_file", r#"{"path": "harvest.csv""#).with_text("Let me try that again.")
    });
    w.model.answers(|sent| {
        let error = sent.last_result();
        assert!(error.to_lowercase().contains("json") || error.contains("arguments"), "it says the arguments were broken: {error}");
        Answer::call("read_spreadsheet", json!({"path": "harvest.csv"}))
    });
    w.model.answers(|sent| {
        let error = sent.last_result();
        assert!(error.contains("read_spreadsheet"), "it names the unknown tool: {error}");
        Answer::call("read_file", json!({"path": "harvest.csv"}))
    });
    w.model.answers(|sent| {
        assert!(sent.last_result().contains("tomatoes,90"), "{}", sent.last_result());
        Answer::text("Tomatoes: 90 kg.")
    });
    let turn = w.send("How many kg of tomatoes?");
    assert_eq!(turn.error, None);
    let statuses: Vec<_> = turn.reply.steps.iter().map(|s| (s.tool.as_str(), s.status)).collect();
    assert_eq!(
        statuses,
        [("read_file", StepStatus::Failed), ("read_file", StepStatus::Failed), ("read_spreadsheet", StepStatus::Failed), ("read_file", StepStatus::Done)]
    );
    assert!(turn.text().ends_with("Tomatoes: 90 kg."), "{}", turn.text());
    assert_eq!(w.model.unused(), 0);
}

/// A long turn: the model reads one big log after another until the conversation outgrows
/// the context window. byob summarizes mid-turn and the turn goes on from the summary to an
/// answer; after a restart the chat still continues from that summary.
#[test]
fn a_turn_that_outgrows_the_context_is_compacted_and_carries_on() {
    let mut w = World::new();
    const LOGS: usize = 8;
    for i in 1..=LOGS {
        let lines: String = (0..1000).map(|n| format!("log {i} line {n}: watered bed {}\n", n % 7)).collect();
        w.file(&format!("logs/{i}.txt"), &lines);
    }
    for i in 1..=LOGS {
        w.model.calls("read_file", json!({"path": format!("logs/{i}.txt")}));
    }
    w.model.answers(|sent| {
        assert!(sent.user_texts().iter().any(|t| t.contains(SUMMARY)), "the request goes on from the summary");
        Answer::text("Bed 3 got the most water.")
    });
    let turn = w.send("Read every watering log and tell me which bed got the most");
    assert_eq!(turn.error, None);
    assert_eq!(turn.text(), "Bed 3 got the most water.");
    assert_eq!(turn.steps("read_file").len(), LOGS);
    let summaries = w.model.requests().iter().filter(|r| r.system() == crate::compact::SYSTEM).count();
    assert!(summaries >= 1, "it was summarized");
    assert!(!w.seen.compactions.is_empty() && !w.seen.compactions[0].manual, "{:?}", w.seen.compactions);

    w.restart();
    assert!(!w.seen.compactions.is_empty(), "the chat opens with its summary");
    w.model.answers(|sent| {
        let users = sent.user_texts();
        assert!(users.first().is_some_and(|t| t.contains(SUMMARY)), "rebuilt from the saved summary: {users:?}");
        Answer::text("Still bed 3.")
    });
    assert_eq!(w.send("Which bed again?").text(), "Still bed 3.");
}

/// Regression: a server whose context window is smaller than byob thought (LM Studio loads
/// models with 4096 tokens by default; byob assumes 32k when it can't tell) refused every
/// request once the chat grew, and the chat was stuck. The request is fitted into less and
/// sent again, and the user is told why.
#[test]
fn a_request_too_long_for_the_server_is_shortened_and_sent_again() {
    let mut w = World::new();
    for i in 0..3 {
        let long = format!("Part {i}: {}", "the beans need water every morning. ".repeat(500));
        w.model.answers(move |_| Answer::text(&long));
        w.send(&format!("tell me part {i}"));
    }
    let sizes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = std::sync::Arc::clone(&sizes);
    w.model.answers(move |r| {
        seen.lock().unwrap().push(r.0.to_string().len());
        Answer::http(400, "the request exceeds the available context size, try increasing it")
    });
    let seen = std::sync::Arc::clone(&sizes);
    w.model.answers(move |r| {
        seen.lock().unwrap().push(r.0.to_string().len());
        Answer::text("Water them every morning.")
    });
    let turn = w.send("so what do the beans need?");
    assert_eq!(turn.error, None);
    assert_eq!(turn.text(), "Water them every morning.");
    let sizes = sizes.lock().unwrap();
    assert!(sizes[1] < sizes[0] * 3 / 4, "the second request is shorter: {sizes:?}");
    assert!(w.seen.notices.iter().any(|(_, t)| t.contains("too long for the model's context window")), "{:?}", w.seen.notices);
}
