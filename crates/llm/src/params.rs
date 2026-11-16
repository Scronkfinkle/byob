//! A profile's request options ([`byob_types::profile::Params`]) on a request body.
//!
//! - OpenAI: `temperature`, `top_p` and `max_tokens` when set.
//! - Anthropic: `max_tokens` replaces byob's [`crate::anthropic::MAX_TOKENS`];
//!   `temperature`/`top_p` only when set (current Claude models reject them);
//!   `effort` becomes `output_config.effort`.
//! - Then `extra` is merged in, for both: objects key by key, anything else replaces, so it
//!   may override the fields above. The fields byob sets itself
//!   ([`byob_types::profile::PROTECTED_FIELDS`]: model, messages, tools, stream, the slot,
//!   ...) are skipped (the engine refuses profiles that set them; this is the second line).
//!
//! Only chat requests ([`crate::Client::stream_chat`] and friends) get them; the probe
//! builds its own bodies.

use byob_types::profile::{PROTECTED_FIELDS, Params};
use serde_json::{Map, Value, json};

/// `params` on an OpenAI chat-completions body.
pub fn apply_openai(body: &mut Value, params: &Params) {
    let Value::Object(map) = body else { return };
    set_common(map, params);
    merge_extra(map, &params.extra);
}

/// `params` on an Anthropic Messages body.
pub fn apply_anthropic(body: &mut Value, params: &Params) {
    let Value::Object(map) = body else { return };
    set_common(map, params);
    if let Some(e) = params.effort {
        merge(map, "output_config", json!({"effort": e.as_str()}));
    }
    merge_extra(map, &params.extra);
}

fn set_common(map: &mut Map<String, Value>, params: &Params) {
    let number = |v: f64| serde_json::Number::from_f64(v).map(Value::Number);
    if let Some(v) = params.temperature.and_then(number) {
        map.insert("temperature".into(), v);
    }
    if let Some(v) = params.top_p.and_then(number) {
        map.insert("top_p".into(), v);
    }
    if let Some(n) = params.max_tokens.filter(|n| *n > 0) {
        map.insert("max_tokens".into(), json!(n));
    }
}

fn merge_extra(map: &mut Map<String, Value>, extra: &Map<String, Value>) {
    for (k, v) in extra {
        if PROTECTED_FIELDS.contains(&k.as_str()) {
            log::warn!("ignored extra request field {k:?}: byob sets it");
            continue;
        }
        merge(map, k, v.clone());
    }
}

/// Put `value` at `key`: into an object already there key by key (recursively), else in
/// its place.
fn merge(map: &mut Map<String, Value>, key: &str, value: Value) {
    match (map.get_mut(key), value) {
        (Some(Value::Object(have)), Value::Object(add)) => {
            for (k, v) in add {
                merge(have, &k, v);
            }
        }
        (_, value) => {
            map.insert(key.to_owned(), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use byob_types::profile::Effort;

    #[test]
    fn extra_merges_deeply_and_never_touches_byobs_fields() {
        let mut body = json!({"model": "m", "messages": [], "stream": true, "id_slot": 1, "thinking": {"type": "adaptive", "display": "summarized"}});
        let extra = json!({
            "model": "evil", "messages": [1], "tools": [], "stream": false, "id_slot": 9, "system": "x", "stream_options": {},
            "top_k": 20, "thinking": {"display": "omitted"}, "temperature": 0.1,
        });
        let extra = extra.as_object().cloned().unwrap();
        let params = Params { temperature: Some(0.7), max_tokens: Some(100), effort: Some(Effort::Low), extra, ..Params::default() };
        apply_anthropic(&mut body, &params);
        assert_eq!(
            body,
            json!({
                "model": "m", "messages": [], "stream": true, "id_slot": 1,
                "thinking": {"type": "adaptive", "display": "omitted"},
                "temperature": 0.1, "max_tokens": 100, "output_config": {"effort": "low"}, "top_k": 20,
            })
        );
    }

    #[test]
    fn nothing_set_changes_nothing() {
        let before = json!({"model": "m", "max_tokens": 64000});
        let mut body = before.clone();
        apply_anthropic(&mut body, &Params::default());
        apply_openai(&mut body, &Params::default());
        assert_eq!(body, before);
    }
}
