//! The bench tool against a fake streaming server: the sweep's shape, what each request
//! asks for, the numbers and the charts.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use byob_types::InferenceConfig;
use serde_json::{Value, json};

use super::*;
use crate::loop_tests::read_request;

/// Bodies the fake server got, and the most requests it had at once.
#[derive(Default)]
struct Seen {
    bodies: Mutex<Vec<Value>>,
    now: AtomicUsize,
    most: AtomicUsize,
}

/// A fake server streaming `max_tokens` one-token chunks (5 ms apart), with usage.
fn server() -> (String, Arc<Seen>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Seen> = Arc::default();
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let log = Arc::clone(&log);
            std::thread::spawn(move || {
                let body = read_request(&mut s);
                let now = log.now.fetch_add(1, Ordering::SeqCst) + 1;
                log.most.fetch_max(now, Ordering::SeqCst);
                let n = body["max_tokens"].as_u64().unwrap_or(8);
                let prompt = body["messages"][0]["content"].as_str().unwrap_or_default().split_whitespace().count() as u64;
                log.bodies.lock().unwrap().push(body);
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n");
                std::thread::sleep(Duration::from_millis(20));
                for _ in 0..n {
                    let _ = write!(s, "data: {}\n\n", json!({"choices":[{"delta":{"content": "tok "}}]}));
                    let _ = s.flush();
                    std::thread::sleep(Duration::from_millis(5));
                }
                let usage = json!({"choices":[{"delta":{},"finish_reason":"length"}],"usage":{"prompt_tokens": prompt, "completion_tokens": n}});
                let _ = write!(s, "data: {usage}\n\ndata: [DONE]\n\n");
                log.now.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    (format!("http://{addr}/v1"), seen)
}

fn client(url: &str) -> Client {
    Client::new(InferenceConfig { base_url: url.into(), model: "m".into(), ..InferenceConfig::default() }).unwrap()
}

fn ep(slots: Option<u64>, context: Option<u64>) -> Endpoint {
    Endpoint { slots, context, server: "llama.cpp".into(), model: "m".into() }
}

#[test]
fn defaults_double_up_to_the_slots_and_skip_prompts_over_the_context() {
    let (s, notes) = Sweep::from_args(&json!({}), &ep(Some(6), None)).unwrap();
    assert_eq!((s.concurrency, s.prompts, s.gen_tokens, s.runs, s.charts), (vec![1, 2, 4, 6], vec![512, 4096, 16384], 256, 1, true));
    assert!(notes.is_empty());
    // No slots reported: up to 4, and it says so.
    let (s, notes) = Sweep::from_args(&json!({}), &ep(None, Some(8192))).unwrap();
    assert_eq!((s.concurrency, s.prompts), (vec![1, 2, 4], vec![512, 4096]));
    assert!(notes.iter().any(|n| n.contains("doesn't say")) && notes.iter().any(|n| n.contains("16k")), "{notes:?}");
    // Given values: sorted, deduplicated, "4k" understood, capped.
    let (s, _) = Sweep::from_args(
        &json!({"concurrency": [4, 1, 4, 16], "prompt_tokens": ["4k", 100], "gen_tokens": 99999, "runs": 9, "charts": false}),
        &ep(Some(4), None),
    )
    .unwrap();
    assert_eq!((s.concurrency, s.prompts, s.gen_tokens, s.runs, s.charts), (vec![1, 4, 16], vec![100, 4096], 4096, 5, false));
    // Nonsense and oversized sweeps are refused in words.
    assert!(Sweep::from_args(&json!({"concurrency": [-1]}), &ep(None, None)).unwrap_err().contains("concurrency"));
    assert!(Sweep::from_args(&json!({"prompt_tokens": [100000]}), &ep(None, Some(4096))).unwrap_err().contains("context"));
    assert!(Sweep::from_args(&json!({"concurrency": [64, 63, 62, 61, 60, 59], "runs": 5}), &ep(None, None)).unwrap_err().contains("requests"));
}

#[test]
fn the_command_reads_its_options() {
    assert_eq!(command("/benchmark"), None);
    assert_eq!(command("hello /bench"), None);
    assert_eq!(command("/bench"), Some(json!({COMMAND_FLAG: true})));
    assert_eq!(
        command("/bench c=1,2,8 p=512,4k gen=64 runs=2 charts=off junk"),
        Some(json!({COMMAND_FLAG: true, "concurrency": [1, 2, 8], "prompt_tokens": [512, 4096], "gen_tokens": 64, "runs": 2, "charts": false}))
    );
}

#[test]
fn prompts_are_about_the_size_asked_and_never_repeat() {
    let a = prompt_text(4096, 1);
    let b = prompt_text(4096, 2);
    assert_ne!(a.lines().next(), b.lines().next());
    // About a word a token or a bit more (the live run measures the real count).
    let words = a.split_whitespace().count();
    assert!((2000..4096).contains(&words), "{words}");
    assert!(prompt_text(16, 3).split_whitespace().count() >= 4);
}

#[test]
fn a_sweep_runs_each_batch_at_its_concurrency_and_times_it() {
    let (url, seen) = server();
    let sweep = Sweep { concurrency: vec![1, 3], prompts: vec![64, 256], gen_tokens: 10, runs: 1, charts: true };
    let cancel = AtomicBool::new(false);
    let batches = run_sweep(&client(&url), &ep(Some(3), None), &sweep, &cancel).unwrap();
    assert_eq!(batches.iter().map(|b| (b.prompt, b.concurrency, b.samples.len())).collect::<Vec<_>>(), [(64, 1, 1), (64, 3, 3), (256, 1, 1), (256, 3, 3)]);
    assert_eq!(seen.most.load(Ordering::SeqCst), 3);
    let bodies = seen.bodies.lock().unwrap();
    // A warm-up, then 8 requests; each asks for exactly 10 tokens and starts differently.
    assert_eq!(bodies.len(), 9);
    assert!(bodies.iter().all(|b| b["max_tokens"] == json!(10) && b["ignore_eos"] == json!(true)), "{bodies:?}");
    let firsts: std::collections::BTreeSet<_> =
        bodies.iter().map(|b| b["messages"][0]["content"].as_str().unwrap().lines().next().unwrap().to_owned()).collect();
    assert_eq!(firsts.len(), 9);
    for b in &batches {
        assert!(b.samples.iter().all(|s| s.error.is_none() && s.completion_tokens == 10 && s.ttft.is_some()), "{b:?}");
        let (agg, per) = (b.aggregate_tps().unwrap(), b.decode_tps().unwrap());
        // About 200 tok/s per request (5 ms a token); several at once add up.
        assert!((50.0..400.0).contains(&per), "{per}");
        if b.concurrency == 3 {
            assert!(agg > per * 1.5, "{agg} vs {per}");
        }
        assert!(b.prefill_tps().is_some() && b.ttft_ms().unwrap() >= 15.0);
    }
    let text = report(&ep(Some(3), None), &sweep, &batches, &[], Duration::from_secs(2));
    assert!(text.contains("| Prompt | Concurrent |") && text.lines().filter(|l| l.starts_with("| ") && !l.starts_with("| Prompt")).count() == 4, "{text}");
}

#[test]
fn the_tool_draws_four_charts_and_a_command_run_doesnt_ask_for_analysis() {
    let (url, _) = server();
    let ws = tempfile::tempdir().unwrap();
    let tool = BenchTool { llm: client(&url), endpoint: ep(Some(2), None) };
    let cancel = AtomicBool::new(false);
    let ctx = ToolContext::new(ws.path(), &cancel);
    let args = json!({"prompt_tokens": [64], "gen_tokens": 4});
    let out = tool.run(&args, &ctx);
    assert!(!out.failed, "{}", out.text);
    assert_eq!(out.changes.len(), 4, "{}", out.text);
    assert!(out.text.contains("don't recompute"));
    let files: Vec<_> = std::fs::read_dir(ws.path().join("charts")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert!(files.iter().all(|f| f.ends_with(".chart.json")) && files.len() == 4, "{files:?}");
    let quiet = tool.run(&json!({"prompt_tokens": [64], "gen_tokens": 4, "charts": false, COMMAND_FLAG: true}), &ctx);
    assert!(!quiet.failed && quiet.changes.is_empty() && !quiet.text.contains("recompute"), "{}", quiet.text);
}

#[test]
fn a_server_that_is_down_fails_in_words() {
    let tool = BenchTool { llm: client("http://127.0.0.1:9/v1"), endpoint: ep(Some(1), None) };
    let ws = tempfile::tempdir().unwrap();
    let cancel = AtomicBool::new(false);
    let out = tool.run(&json!({}), &ToolContext::new(ws.path(), &cancel));
    assert!(out.failed && out.text.contains("didn't answer"), "{}", out.text);
}

#[test]
fn a_stopped_sweep_ends_between_batches() {
    let (url, seen) = server();
    let cancel = AtomicBool::new(true);
    let sweep = Sweep { concurrency: vec![1, 2], prompts: vec![64], gen_tokens: 4, runs: 1, charts: false };
    let batches = run_sweep(&client(&url), &ep(Some(2), None), &sweep, &cancel);
    // The warm-up is stopped too; nothing else is sent.
    assert!(batches.is_err() || batches.unwrap().is_empty());
    assert!(seen.bodies.lock().unwrap().len() <= 1);
}

mod engine {
    use std::sync::Arc;
    use std::time::Instant;

    use byob_protocol::{ClientMsg, Connection, ServerMsg};

    use super::*;
    use crate::{EngineConfig, EngineHandle, LocalConnection};

    fn recv(c: &mut LocalConnection) -> ServerMsg {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(m) = c.try_recv() {
                return m;
            }
            assert!(Instant::now() < deadline, "timed out waiting for the engine");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Send `text`; everything the engine said until its reply finished, and the error.
    fn send(url: &str, text: &str) -> (Vec<String>, Option<String>, tempfile::TempDir) {
        let ws = tempfile::tempdir().unwrap();
        let home = ws.path().join(".home");
        let mut config = EngineConfig::new(InferenceConfig { base_url: url.into(), model: "m".into(), ..InferenceConfig::default() });
        config.workspace = ws.path().to_path_buf();
        config.auto_title = false;
        // Only the bundled `bench` plugin: it offers the tool and `/bench`.
        let mut bundled = crate::natives::bundled_plugins();
        bundled.retain(|e| e.id == "bench");
        config.plugins = Some(crate::extensions::PluginsConfig {
            bundled_cache: Some(home.join("bundled-plugins")),
            scratch_root: Some(home.join("chats")),
            bundled,
            natives: crate::natives::Natives::builtin(),
            ..Default::default()
        });
        let e = EngineHandle::spawn_with(config).unwrap();
        let mut c = e.connect(Arc::new(|| {}));
        c.send(ClientMsg::hello("test"));
        while !matches!(recv(&mut c), ServerMsg::Artifacts { .. }) {}
        c.send(ClientMsg::SendMessage { text: text.into(), attachments: vec![] });
        let mut said = Vec::new();
        loop {
            let m = recv(&mut c);
            said.push(format!("{m:?}"));
            if let ServerMsg::EntryFinished { error, .. } = m {
                return (said, error, ws);
            }
        }
    }

    #[test]
    fn bench_command_runs_the_sweep_without_asking_the_model() {
        let (url, seen) = server();
        let (said, error, ws) = send(&url, "/bench c=1,2 p=64 gen=4");
        assert_eq!(error, None);
        let all = said.join("\n");
        assert!(all.contains("Total gen tok/s") && !all.contains("don't recompute"), "{all}");
        // Only the sweep's requests (a warm-up, then 1 + 2): no model request with byob's prompt.
        let bodies = seen.bodies.lock().unwrap();
        let chats: Vec<_> = bodies.iter().filter(|b| b.get("messages").is_some()).collect();
        assert_eq!(chats.len(), 4, "{chats:?}");
        assert!(chats.iter().all(|b| b["messages"][0]["content"].as_str().unwrap().starts_with("Benchmark run")), "{chats:?}");
        assert_eq!(std::fs::read_dir(ws.path().join("charts")).unwrap().count(), 4);
    }

    #[test]
    fn a_message_that_only_mentions_bench_goes_to_the_model() {
        let (url, seen) = server();
        let (_, error, _ws) = send(&url, "/benchmark this please");
        assert_eq!(error, None);
        let bodies = seen.bodies.lock().unwrap();
        // The model got it, with the bench tool offered.
        assert!(bodies.iter().any(|b| b["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["function"]["name"] == "bench"))), "{bodies:?}");
    }
}

#[test]
fn an_endpoint_not_probed_yet_is_probed_first() {
    let (url, seen) = server();
    let ws = tempfile::tempdir().unwrap();
    let tool = BenchTool { llm: client(&url), endpoint: Endpoint { model: "m".into(), ..Endpoint::default() } };
    let cancel = AtomicBool::new(false);
    let out = tool.run(&json!({"concurrency": [1], "prompt_tokens": [64], "gen_tokens": 4, "charts": false}), &ToolContext::new(ws.path(), &cancel));
    // This fake server says nothing useful: the sweep still runs, without ignore_eos.
    assert!(!out.failed, "{}", out.text);
    assert!(out.text.contains("may end replies early"), "{}", out.text);
    assert!(seen.bodies.lock().unwrap().iter().any(|b| b.get("messages").is_none()), "the probe's requests");
}
