//! The backend host with a native backend: same lifecycle as a process (init first, queued
//! messages, replies, sink messages, a panic is a crash with a fault and a fresh start, a
//! stop is final).

use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use byob_plugins::backend::{Backend, Out, Phase, Sink, Start};
use byob_plugins::native::{NativeBackend, Outbox};
use serde_json::{Value, json};

struct Counter {
    init: Value,
    n: u32,
}

impl NativeBackend for Counter {
    fn init(&mut self, init: &Value, out: &Outbox) {
        self.init = init.clone();
        out.subscribe(&["turn_finished", "nope"]);
    }

    fn event(&mut self, ev: &Value, out: &Outbox) {
        out.notify(json!({"got": ev["event"]}), None);
    }

    fn request(&mut self, method: &str, params: &Value, _: &Outbox) -> Result<Value, String> {
        if params["panic"] == true {
            panic!("on purpose");
        }
        self.n += 1;
        Ok(json!({"method": method, "n": self.n, "restart": self.init["restart"], "plugin": self.init["plugin"]}))
    }
}

#[test]
fn native_backends_live_like_processes() {
    let (tx, rx) = channel();
    let tx = Mutex::new(tx);
    let sink: Sink = Arc::new(move |o| {
        let _ = tx.lock().unwrap().send(o);
    });
    let made = Arc::new(Mutex::new(0));
    let m = Arc::clone(&made);
    let prepare = Box::new(move || {
        *m.lock().unwrap() += 1;
        Ok(Start::Native(Box::new(Counter { init: Value::Null, n: 0 })))
    });
    let b = Backend::new("counter", json!({"type": "init", "plugin": "counter"}), &[], prepare, sink);
    assert_eq!(b.status().phase, Phase::Idle);
    let t = Duration::from_secs(10);
    // an event before it runs is queued and arrives after init
    assert!(b.event(json!({"type": "event", "event": "client"})));
    assert_eq!(b.request("tool", json!({}), t, &|| false).unwrap(), json!({"method": "tool", "n": 1, "restart": 0, "plugin": "counter"}));
    assert_eq!(b.status().phase, Phase::Running);
    assert_eq!(b.status().pid, None);
    let mut notified = false;
    while let Ok(o) = rx.recv_timeout(Duration::from_millis(200)) {
        if let Out::Msg(m) = o
            && m["type"] == "notify"
        {
            notified = m["msg"]["got"] == "client";
        }
    }
    assert!(notified);
    // subscribe works (unknown events are a fault)
    assert!(b.wants("turn_finished"));
    assert!(b.status().faults.iter().any(|f| f.contains("no such engine events")));

    // a panic: the call fails, a fault, Crashed; the next use starts a fresh one
    let e = b.request("tool", json!({"panic": true}), t, &|| false).unwrap_err();
    assert!(e.contains("it panicked: on purpose"), "{e}");
    let st = b.status();
    assert!(st.faults.iter().any(|f| f == "the native backend panicked: on purpose"), "{:?}", st.faults);
    assert!(st.stderr.contains("(panic) on purpose"));
    assert_eq!(b.request("tool", json!({}), t, &|| false).unwrap()["restart"], 1);
    assert_eq!(*made.lock().unwrap(), 2);

    // stopped for good
    b.stop("switched off");
    assert_eq!(b.status().phase, Phase::Stopped);
    assert!(b.request("tool", json!({}), t, &|| false).unwrap_err().contains("stopped"));
}
