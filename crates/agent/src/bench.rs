//! Token throughput benchmark: the `bench` tool and the `/bench` command (bundled `bench`
//! plugin).
//!
//! A sweep sends batches of requests to the chat's own endpoint: for every prompt size and
//! concurrency (1, 2, 4, … up to the server's slots), `concurrency` requests at once, each a
//! fresh prompt of about that many tokens (a random first line, so no server's prompt cache
//! answers it) asking for `gen_tokens` tokens. Every request is timed from its stream:
//! time to the first token, prompt processing speed (prompt tokens over that time), decode
//! speed (the tokens after the first over the rest), and each batch's aggregate generation
//! throughput (all its tokens over its wall time). Token counts are the server's own (usage)
//! when it sends them.
//!
//! The model calls the tool and gets a table to interpret; `/bench` runs the same call
//! without the model (a scripted turn, see [`command`]) and the table is the reply. Both
//! draw the charts (`.chart.json`, as `make_chart` saves them) unless `charts` is false.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use byob_llm::{ChatMessage, Client, StreamEvent};
use byob_tools::{Access, FileChange, Tool, ToolContext, ToolOutput, ToolSpec};
use serde_json::{Value, json};

/// The tool's name (and the command's: `/bench`).
pub const TOOL: &str = "bench";
/// Set on the call `/bench` makes: the result is the reply, so it doesn't talk to a model.
pub const COMMAND_FLAG: &str = "_command";

/// Prompt sizes measured when the call gives none.
const DEFAULT_PROMPTS: [u64; 3] = [512, 4096, 16384];
/// Tokens generated per request when the call doesn't say.
const DEFAULT_GEN: u64 = 256;
/// Concurrency levels when the server doesn't say how many slots it has.
const UNKNOWN_SLOTS: u64 = 4;
/// Limits on what one call may ask for.
const MAX_CONCURRENCY: u64 = 64;
const MAX_LEVELS: usize = 12;
const MAX_PROMPT: u64 = 262_144;
const MAX_GEN: u64 = 4096;
const MAX_RUNS: u64 = 5;
/// Most requests one sweep sends.
const MAX_REQUESTS: u64 = 400;
/// Tokens kept free of the context window besides the prompt and the reply.
const CONTEXT_MARGIN: u64 = 256;

/// What the engine knows about the endpoint the chat uses.
#[derive(Debug, Clone, Default)]
pub struct Endpoint {
    /// Parallel slots the server reported (llama.cpp `total_slots`).
    pub slots: Option<u64>,
    /// Its context window, if known.
    pub context: Option<u64>,
    /// Server software ("llama.cpp", "vLLM", …); empty if unknown.
    pub server: String,
    /// The model id requests name.
    pub model: String,
}

impl Endpoint {
    /// The server takes `ignore_eos` (so every reply is exactly as long as asked).
    fn ignores_eos(&self) -> bool {
        let s = self.server.to_ascii_lowercase();
        s.contains("llama") || s.contains("vllm")
    }
}

/// The `bench` tool for one turn: the chat's endpoint.
pub struct BenchTool {
    pub llm: Client,
    pub endpoint: Endpoint,
}

/// What to measure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sweep {
    pub concurrency: Vec<u64>,
    pub prompts: Vec<u64>,
    pub gen_tokens: u64,
    pub runs: u64,
    pub charts: bool,
}

impl Sweep {
    /// The call's arguments, defaults filled in from the endpoint, sizes over its context
    /// left out. Errors are for the model.
    pub fn from_args(args: &Value, ep: &Endpoint) -> Result<(Self, Vec<String>), String> {
        let mut notes = Vec::new();
        let list = |key: &str| -> Result<Option<Vec<u64>>, String> {
            match args.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|v| as_count(v).ok_or_else(|| format!("{key} must be a list of positive whole numbers")))
                    .collect::<Result<Vec<_>, _>>()
                    .map(Some),
                Some(v) => as_count(v).map(|n| Some(vec![n])).ok_or_else(|| format!("{key} must be a list of positive whole numbers")),
            }
        };
        let one = |key: &str, default: u64, max: u64| -> Result<u64, String> {
            match args.get(key) {
                None | Some(Value::Null) => Ok(default),
                Some(v) => as_count(v).map(|n| n.min(max)).ok_or_else(|| format!("{key} must be a positive whole number")),
            }
        };
        let gen_tokens = one("gen_tokens", DEFAULT_GEN, MAX_GEN)?;
        let runs = one("runs", 1, MAX_RUNS)?;
        let charts = args.get("charts").and_then(Value::as_bool).unwrap_or(true);

        let mut concurrency = match list("concurrency")? {
            Some(c) => c,
            None => {
                if ep.slots.is_none() {
                    notes.push(format!("The server doesn't say how many requests it runs at once; tried up to {UNKNOWN_SLOTS}."));
                }
                doubling(ep.slots.unwrap_or(UNKNOWN_SLOTS))
            }
        };
        concurrency.iter_mut().for_each(|c| *c = (*c).min(MAX_CONCURRENCY));
        concurrency.sort_unstable();
        concurrency.dedup();
        concurrency.truncate(MAX_LEVELS);

        let mut prompts = list("prompt_tokens")?.unwrap_or_else(|| DEFAULT_PROMPTS.to_vec());
        prompts.iter_mut().for_each(|p| *p = (*p).clamp(16, MAX_PROMPT));
        prompts.sort_unstable();
        prompts.dedup();
        prompts.truncate(MAX_LEVELS);
        if let Some(ctx) = ep.context {
            let room = ctx.saturating_sub(gen_tokens).saturating_sub(CONTEXT_MARGIN);
            let (fit, over): (Vec<u64>, Vec<u64>) = prompts.iter().partition(|p| **p <= room);
            if !over.is_empty() {
                notes.push(format!("Skipped prompts over the {ctx}-token context: {}.", join(&over)));
            }
            prompts = fit;
        }
        if prompts.is_empty() || concurrency.is_empty() {
            return Err("nothing to measure: no prompt size fits the model's context window".into());
        }
        let requests = concurrency.iter().sum::<u64>().saturating_mul(u64::try_from(prompts.len()).unwrap_or(u64::MAX)).saturating_mul(runs);
        if requests > MAX_REQUESTS {
            return Err(format!("that sweep sends {requests} requests; the most is {MAX_REQUESTS}. Use fewer concurrency levels, prompt sizes or runs."));
        }
        Ok((Self { concurrency, prompts, gen_tokens, runs, charts }, notes))
    }
}

/// 1, 2, 4, … up to `max`, and `max` itself.
fn doubling(max: u64) -> Vec<u64> {
    let max = max.clamp(1, MAX_CONCURRENCY);
    let mut out = Vec::new();
    let mut c = 1u64;
    while c < max {
        out.push(c);
        c = c.saturating_mul(2);
    }
    out.push(max);
    out
}

fn as_count(v: &Value) -> Option<u64> {
    let n = v
        .as_u64()
        .or_else(|| v.as_f64().filter(|f| f.is_finite() && *f >= 1.0 && f.fract() == 0.0).map(|f| f as u64))
        .or_else(|| v.as_str().and_then(parse_size))?;
    (n >= 1).then_some(n)
}

/// "4096", "4k", "16K", "1m".
fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mul) = match s.strip_suffix('k') {
        Some(n) => (n, 1024),
        None => match s.strip_suffix('m') {
            Some(n) => (n, 1024 * 1024),
            None => (s.as_str(), 1),
        },
    };
    num.trim().parse::<u64>().ok().map(|n| n.saturating_mul(mul))
}

fn join(v: &[u64]) -> String {
    v.iter().map(|n| short(*n)).collect::<Vec<_>>().join(", ")
}

/// 4096 → "4k", 512 → "512".
fn short(n: u64) -> String {
    if n >= 1024 && n.is_multiple_of(1024) { format!("{}k", n / 1024) } else { n.to_string() }
}

/// One request's timings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Time to the first streamed token.
    pub ttft: Option<Duration>,
    pub total: Duration,
    pub error: Option<String>,
}

impl Sample {
    /// Prompt tokens per second (prompt over time to first token).
    fn prefill_tps(&self) -> Option<f64> {
        let t = self.ttft?.as_secs_f64();
        (t > 0.0 && self.prompt_tokens > 0).then(|| self.prompt_tokens as f64 / t)
    }

    /// Tokens per second after the first.
    fn decode_tps(&self) -> Option<f64> {
        let t = self.total.checked_sub(self.ttft?)?.as_secs_f64();
        (t > 0.0 && self.completion_tokens > 1).then(|| (self.completion_tokens - 1) as f64 / t)
    }
}

/// One batch: `concurrency` requests at once with prompts of about `prompt` tokens.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Batch {
    pub concurrency: u64,
    pub prompt: u64,
    pub samples: Vec<Sample>,
    pub wall: Duration,
}

impl Batch {
    fn ok(&self) -> impl Iterator<Item = &Sample> {
        self.samples.iter().filter(|s| s.error.is_none())
    }

    /// All generated tokens over the batch's wall time.
    pub fn aggregate_tps(&self) -> Option<f64> {
        let tokens: u64 = self.ok().map(|s| s.completion_tokens).sum();
        let t = self.wall.as_secs_f64();
        (t > 0.0 && tokens > 0).then(|| tokens as f64 / t)
    }

    pub fn decode_tps(&self) -> Option<f64> {
        mean(self.ok().filter_map(Sample::decode_tps))
    }

    pub fn prefill_tps(&self) -> Option<f64> {
        mean(self.ok().filter_map(Sample::prefill_tps))
    }

    pub fn ttft_ms(&self) -> Option<f64> {
        mean(self.ok().filter_map(|s| s.ttft.map(|t| t.as_secs_f64() * 1000.0)))
    }

    pub fn prompt_tokens(&self) -> Option<f64> {
        mean(self.ok().map(|s| s.prompt_tokens as f64).filter(|t| *t > 0.0))
    }

    fn errors(&self) -> usize {
        self.samples.iter().filter(|s| s.error.is_some()).count()
    }
}

fn mean(it: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = it.fold((0.0, 0u32), |(s, n), v| (s + v, n.saturating_add(1)));
    (n > 0).then(|| sum / f64::from(n))
}

/// Words the filler prompts are made of.
const WORDS: [&str; 32] = [
    "river", "lantern", "copper", "meadow", "signal", "harbor", "violet", "engine", "orchard", "glacier", "pencil", "thunder", "basket", "compass", "velvet",
    "canyon", "marble", "falcon", "garden", "silver", "window", "planet", "candle", "forest", "mirror", "rocket", "island", "ladder", "winter", "puzzle",
    "anchor", "saddle",
];

/// A prompt of about `tokens` tokens, unlike any other (its first line is `nonce`).
fn prompt_text(tokens: u64, nonce: u64) -> String {
    // About 1.3 tokens per word with the space; aim a little low, the instruction adds some.
    let words = usize::try_from(tokens.saturating_mul(10) / 13).unwrap_or(usize::MAX).saturating_sub(24).max(4);
    let mut out = format!("Benchmark run {nonce:x}.\n");
    let mut x = nonce | 1;
    for i in 0..words {
        // xorshift: cheap, different for every nonce.
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let word = WORDS.get(usize::try_from(x % 32).unwrap_or(0)).copied().unwrap_or("river");
        out.push_str(word);
        out.push(if i % 16 == 15 { '\n' } else { ' ' });
    }
    out.push_str("\n\nWrite a long story that uses the words above. Keep writing until you are stopped.");
    out
}

fn nonce() -> u64 {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    u64::try_from(t & u128::from(u64::MAX)).unwrap_or(0) ^ 0x9e37_79b9_7f4a_7c15
}

/// Send one request and time it.
fn one(llm: &Client, prompt: &str, cancel: &AtomicBool) -> Sample {
    let messages = [ChatMessage::user(prompt.to_owned())];
    let start = Instant::now();
    let mut first: Option<Duration> = None;
    let mut chars = 0usize;
    let result = llm.stream_chat(&messages, &[], cancel, |ev| {
        let t = match ev {
            StreamEvent::Content(t) | StreamEvent::Reasoning(t) => t,
        };
        if !t.is_empty() {
            first.get_or_insert_with(|| start.elapsed());
            chars = chars.saturating_add(t.chars().count());
        }
    });
    let total = start.elapsed();
    match result {
        Ok(out) if out.cancelled => Sample { ttft: first, total, error: Some("stopped".into()), ..Sample::default() },
        Ok(out) => {
            let (prompt_tokens, completion_tokens) = match out.usage {
                Some(u) if u.completion_tokens > 0 => (u.prompt_tokens, u.completion_tokens),
                // No usage from the server: estimate (about 4 characters a token).
                _ => (byob_llm::estimate_tokens(prompt) as u64, (chars / 4).max(1) as u64),
            };
            Sample { prompt_tokens, completion_tokens, ttft: first, total, error: None }
        }
        Err(e) => Sample { ttft: first, total, error: Some(e.to_string()), ..Sample::default() },
    }
}

/// Run `concurrency` requests at once.
fn batch(llm: &Client, concurrency: u64, prompt: u64, cancel: &AtomicBool) -> Batch {
    let prompts: Vec<String> = (0..concurrency).map(|i| prompt_text(prompt, nonce().wrapping_add(i.wrapping_mul(0x2545_f491)))).collect();
    let start = Instant::now();
    let samples = std::thread::scope(|scope| {
        let handles: Vec<_> = prompts.iter().map(|p| scope.spawn(move || one(llm, p, cancel))).collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Sample { error: Some("the request's thread crashed".into()), ..Sample::default() })).collect()
    });
    Batch { concurrency, prompt, samples, wall: start.elapsed() }
}

/// The client the sweep uses: the chat's endpoint, asking for exactly `gen` tokens.
fn bench_client(llm: &Client, ep: &Endpoint, gen_tokens: u64) -> Result<Client, String> {
    let mut config = llm.config().clone();
    config.params.max_tokens = Some(u32::try_from(gen_tokens).unwrap_or(u32::MAX));
    if ep.ignores_eos() {
        config.params.extra.insert("ignore_eos".into(), json!(true));
    }
    Client::new(config).map_err(|e| format!("can't reach the model: {e}"))
}

/// Run the sweep, in order: prompt sizes, then concurrency. Stops early when cancelled.
pub fn run_sweep(llm: &Client, ep: &Endpoint, sweep: &Sweep, cancel: &AtomicBool) -> Result<Vec<Batch>, String> {
    let client = bench_client(llm, ep, sweep.gen_tokens)?;
    let mut out = Vec::new();
    // A short request first, so a model that is still loading doesn't spoil the first batch.
    let warm = one(&client, &prompt_text(64, nonce()), cancel);
    if let Some(e) = warm.error {
        return Err(format!("the model didn't answer a first request: {e}"));
    }
    for &prompt in &sweep.prompts {
        for &c in &sweep.concurrency {
            for _ in 0..sweep.runs {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(out);
                }
                out.push(batch(&client, c, prompt, cancel));
            }
        }
    }
    Ok(out)
}

/// The runs of each (prompt, concurrency), merged.
fn merged(batches: &[Batch]) -> Vec<Batch> {
    let mut out: Vec<Batch> = Vec::new();
    for b in batches {
        match out.iter_mut().find(|m| m.prompt == b.prompt && m.concurrency == b.concurrency) {
            Some(m) => {
                m.samples.extend(b.samples.iter().cloned());
                m.wall = m.wall.saturating_add(b.wall);
            }
            None => out.push(b.clone()),
        }
    }
    out
}

fn num(v: Option<f64>, digits: usize) -> String {
    v.map_or_else(|| "–".into(), |v| format!("{v:.digits$}"))
}

/// The results as a Markdown table, with what was run.
pub fn report(ep: &Endpoint, sweep: &Sweep, batches: &[Batch], notes: &[String], elapsed: Duration) -> String {
    let rows = merged(batches);
    let mut t = String::new();
    let server = if ep.server.is_empty() { String::new() } else { format!(" on {}", ep.server) };
    let slots = ep.slots.map_or_else(String::new, |s| format!(", {s} slots"));
    let _ = writeln!(
        t,
        "**Throughput of {}**{server}{slots}: {} tokens generated per request, {} run(s) each, {:.0} s in all.\n",
        ep.model,
        sweep.gen_tokens,
        sweep.runs,
        elapsed.as_secs_f64()
    );
    t.push_str("| Prompt | Concurrent | Total gen tok/s | Per-request gen tok/s | Prompt tok/s | First token (ms) | Errors |\n");
    t.push_str("|---:|---:|---:|---:|---:|---:|---:|\n");
    for b in &rows {
        let prompt = b.prompt_tokens().map_or_else(|| short(b.prompt), |p| format!("{p:.0}"));
        let _ = writeln!(
            t,
            "| {prompt} | {} | {} | {} | {} | {} | {} |",
            b.concurrency,
            num(b.aggregate_tps(), 1),
            num(b.decode_tps(), 1),
            num(b.prefill_tps(), 0),
            num(b.ttft_ms(), 0),
            b.errors()
        );
    }
    t.push_str(
        "\nTotal: every token the batch generated over its wall time, prompt processing included. Per-request: one request's \
         speed after its first token. Prompt: prompt tokens over the time to the first token.\n",
    );
    if let Some(e) = rows.iter().flat_map(|b| b.samples.iter()).find_map(|s| s.error.as_deref()) {
        let _ = write!(t, "\nFirst error: {}\n", crate::limits::one_line(e, 300));
    }
    for n in notes {
        let _ = write!(t, "\n{n}");
    }
    if !ep.ignores_eos() {
        t.push_str("\nThis server may end replies early, so generation lengths can vary.");
    }
    t
}

/// The charts' rows: one per concurrency, a column per prompt size.
fn by_concurrency(rows: &[Batch], sweep: &Sweep, value: impl Fn(&Batch) -> Option<f64>) -> (Vec<Value>, Vec<String>) {
    let series: Vec<String> = sweep.prompts.iter().map(|p| format!("{} prompt", short(*p))).collect();
    let data = sweep
        .concurrency
        .iter()
        .map(|c| {
            let mut row = serde_json::Map::new();
            row.insert("concurrency".into(), json!(c.to_string()));
            for (p, name) in sweep.prompts.iter().zip(&series) {
                let v = rows.iter().find(|b| b.concurrency == *c && b.prompt == *p).and_then(&value);
                row.insert(name.clone(), v.map_or(Value::Null, |v| json!((v * 10.0).round() / 10.0)));
            }
            Value::Object(row)
        })
        .collect();
    (data, series)
}

/// Draw the charts with `make_chart`; their files, or why one failed.
fn charts(model: &str, sweep: &Sweep, batches: &[Batch], ctx: &ToolContext<'_>) -> (Vec<FileChange>, Vec<String>) {
    let rows = merged(batches);
    let mut specs = Vec::new();
    let line = |title: String, unit: &str, (data, series): (Vec<Value>, Vec<String>)| json!({"type": "line", "title": title, "x": "concurrency", "series": series, "data": data, "unit": unit});
    specs.push(line(format!("{model}: total generation throughput"), "tok/s", by_concurrency(&rows, sweep, Batch::aggregate_tps)));
    specs.push(line(format!("{model}: generation speed per request"), "tok/s", by_concurrency(&rows, sweep, Batch::decode_tps)));
    specs.push(line(format!("{model}: time to first token"), "ms", by_concurrency(&rows, sweep, Batch::ttft_ms)));
    let lowest = sweep.concurrency.first().copied().unwrap_or(1);
    let prefill: Vec<Value> = sweep
        .prompts
        .iter()
        .filter_map(|p| rows.iter().find(|b| b.prompt == *p && b.concurrency == lowest))
        .map(|b| json!({"prompt": format!("{} tokens", short(b.prompt)), "tok/s": b.prefill_tps().map(f64::round)}))
        .collect();
    specs.push(json!({"type": "bar", "title": format!("{model}: prompt processing speed ({lowest} at a time)"), "x": "prompt", "series": ["tok/s"], "data": prefill, "unit": "tok/s"}));
    let mut changes = Vec::new();
    let mut failed = Vec::new();
    for spec in specs {
        let out = byob_tools::MakeChart.run(&spec, ctx);
        if out.failed {
            failed.push(out.text);
        } else {
            changes.extend(out.changes);
        }
    }
    (changes, failed)
}

impl BenchTool {
    /// The endpoint as the engine knows it, or as a quick probe finds it when the engine
    /// doesn't know yet (its own probe hadn't finished when the turn started).
    fn endpoint(&self) -> Endpoint {
        if !self.endpoint.server.is_empty() || self.endpoint.slots.is_some() {
            return self.endpoint.clone();
        }
        let caps = self.llm.probe_with(byob_llm::ProbeOptions { test_tools: false, budget: Duration::from_secs(10), ..Default::default() });
        Endpoint {
            slots: caps.slots,
            context: self.endpoint.context.or(caps.context_length),
            server: crate::adapt::server_name(caps.server).into(),
            model: self.endpoint.model.clone(),
        }
    }
}

impl Tool for BenchTool {
    fn spec(&self) -> ToolSpec {
        let slots = self.endpoint.slots.map_or_else(|| format!("unknown; default up to {UNKNOWN_SLOTS}"), |s| s.to_string());
        ToolSpec {
            name: TOOL.into(),
            description: format!(
                "Benchmark the token throughput of the model this chat runs on (its server's slots: {slots}). Sends batches of concurrent requests and \
                 measures aggregate and per-request generation tok/s, prompt processing tok/s and time to first token, for each prompt size and concurrency, \
                 and draws charts. Takes minutes and loads the server: only when the user asks for a benchmark. Defaults: concurrency 1, 2, 4… up to the \
                 slots, prompts of 512, 4k and 16k tokens, {DEFAULT_GEN} tokens generated."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "concurrency": {"type": "array", "items": {"type": "integer"}, "description": "Requests at once, e.g. [1, 2, 4] (default: doubling up to the server's slots)"},
                    "prompt_tokens": {"type": "array", "items": {"type": "integer"}, "description": "Prompt sizes in tokens, e.g. [512, 4096] (default [512, 4096, 16384])"},
                    "gen_tokens": {"type": "integer", "description": "Tokens to generate per request (default 256)"},
                    "runs": {"type": "integer", "description": "Times to repeat each batch (default 1, at most 5)"},
                    "charts": {"type": "boolean", "description": "Draw the charts (default true)"}
                },
                "required": []
            }),
        }
    }

    fn title(&self, _args: &Value) -> String {
        format!("Benchmarked {}", self.endpoint.model)
    }

    fn pending_title(&self, _args: &Value) -> String {
        format!("Benchmarking {}", self.endpoint.model)
    }

    fn access(&self, _args: &Value) -> Access {
        // It talks to the chat's own model server and only creates new chart files, like
        // make_chart; asking every time would be noise.
        Access::Read
    }

    fn concurrent(&self) -> bool {
        true
    }

    fn repeatable(&self, _args: &Value) -> bool {
        true
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let endpoint = self.endpoint();
        let (sweep, notes) = match Sweep::from_args(args, &endpoint) {
            Ok(s) => s,
            Err(e) => return ToolOutput::err(e),
        };
        let started = Instant::now();
        let batches = match run_sweep(&self.llm, &endpoint, &sweep, ctx.cancel_flag()) {
            Ok(b) => b,
            Err(e) => return ToolOutput::err(e),
        };
        if ctx.cancelled() {
            return ToolOutput::err("stopped");
        }
        let mut text = report(&endpoint, &sweep, &batches, &notes, started.elapsed());
        let mut changes = Vec::new();
        if sweep.charts {
            let (made, failed) = charts(&endpoint.model, &sweep, &batches, ctx);
            changes = made;
            for f in failed {
                let _ = write!(text, "\nA chart failed: {f}");
            }
        }
        if args.get(COMMAND_FLAG).and_then(Value::as_bool) != Some(true) {
            text.push_str(
                "\n\nThe charts show in the chat. The numbers are measured averages (a batch's requests queue for prompt processing, so its last one \
                 starts later than the average): don't recompute them. Briefly explain what they say: how throughput scales with concurrency, where it \
                 saturates, how prompt size affects speed and time to first token, and anything odd.",
            );
        }
        ToolOutput { text, failed: false, changes, images: Vec::new() }
    }
}

/// `/bench [concurrency=1,2,4] [prompt=512,4k] [gen=256] [runs=1] [charts=off]`: the
/// call's arguments, or `None` if `text` isn't the command.
pub fn command(text: &str) -> Option<Value> {
    let rest = text.trim().strip_prefix("/bench")?;
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    let mut args = serde_json::Map::new();
    args.insert(COMMAND_FLAG.into(), json!(true));
    for word in rest.split_whitespace() {
        let Some((key, value)) = word.split_once('=') else { continue };
        let numbers = || Value::Array(value.split(',').filter_map(parse_size).map(|n| json!(n)).collect());
        match key.trim().to_ascii_lowercase().as_str() {
            "c" | "concurrency" => args.insert("concurrency".into(), numbers()),
            "p" | "prompt" | "prompts" | "prompt_tokens" => args.insert("prompt_tokens".into(), numbers()),
            "g" | "gen" | "gen_tokens" => parse_size(value).and_then(|n| args.insert("gen_tokens".into(), json!(n))),
            "runs" | "r" => parse_size(value).and_then(|n| args.insert("runs".into(), json!(n))),
            "charts" => args.insert("charts".into(), json!(!matches!(value, "off" | "no" | "false" | "0"))),
            _ => None,
        };
    }
    Some(Value::Object(args))
}

#[cfg(test)]
#[path = "bench_tests.rs"]
mod tests;
