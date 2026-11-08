//! Key filtering and the server's size limits, applied before anything
//! leaves the process.

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::util::truncate_in_place;

pub(crate) const FILTERED: &str = "[FILTERED]";
pub(crate) const TRUNCATED: &str = "[truncated]";

/// Parts of key names that are always filtered by default. They mirror the
/// Ruby gem's defaults (Rails' generated `filter_parameters` plus the
/// server's scrub list), written as plain substrings of the normalised key
/// (lowercase, `-` read as `_`).
pub(crate) const DEFAULT_FILTER_PARTS: &[&str] = &[
    "passw",
    "secret",
    "token",
    "_key",
    "apikey",
    "api_key",
    "crypt",
    "salt",
    "certificate",
    "otp",
    "ssn",
    "cvv",
    "cvc",
    "authorization",
    "cookie",
    "csrf",
    "xsrf",
    "sessionid",
    "session_id",
    "creditcard",
    "credit_card",
    "cardnumber",
    "card_number",
    "signature",
    "email",
];

/// Masks values under sensitive keys, at any depth. Names match a whole
/// key; parts match anywhere in it. Both ignore case and treat `-` as `_`.
#[derive(Debug, Clone, Default)]
pub(crate) struct KeyFilter {
    names: HashSet<String>,
    parts: Vec<String>,
}

impl KeyFilter {
    pub(crate) fn new(names: &[String], parts: &[String]) -> Self {
        KeyFilter {
            names: names.iter().map(|name| normalize(name)).collect(),
            parts: parts.iter().map(|part| normalize(part)).filter(|part| !part.is_empty()).collect(),
        }
    }

    pub(crate) fn is_sensitive(&self, key: &str) -> bool {
        let key = normalize(key);
        self.names.contains(&key) || self.parts.iter().any(|part| key.contains(part.as_str()))
    }

    /// Filters `value` in place: objects keep their keys, and the values of
    /// sensitive keys become `"[FILTERED]"`.
    pub(crate) fn filter(&self, value: &mut Value) {
        self.filter_at(value, 0);
    }

    fn filter_at(&self, value: &mut Value, depth: usize) {
        if depth > 16 {
            *value = Value::String(TRUNCATED.to_string());
            return;
        }
        match value {
            Value::Object(map) => {
                for (key, item) in map.iter_mut() {
                    if self.is_sensitive(key) {
                        *item = Value::String(FILTERED.to_string());
                    } else {
                        self.filter_at(item, depth + 1);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| self.filter_at(item, depth + 1)),
            _ => {}
        }
    }

    /// Filters a flat string map (tags, headers).
    pub(crate) fn filter_map(&self, map: &mut Map<String, Value>) {
        for (key, item) in map.iter_mut() {
            if self.is_sensitive(key) {
                *item = Value::String(FILTERED.to_string());
            } else {
                self.filter_at(item, 1);
            }
        }
    }
}

fn normalize(key: &str) -> String {
    key.trim().to_ascii_lowercase().replace('-', "_")
}

/// The server's limits for free-form sections (`request`, `params`,
/// `headers`, `context`, …): nesting depth ≤ 6, arrays ≤ 50 items, objects
/// ≤ 100 keys, strings ≤ 4,096 characters. Deeper values become
/// `"[truncated]"`.
pub(crate) fn bound(value: &mut Value) {
    bound_at(value, 1);
}

fn bound_at(value: &mut Value, depth: usize) {
    match value {
        Value::Object(map) => {
            if depth > 6 {
                *value = Value::String(TRUNCATED.to_string());
                return;
            }
            if map.len() > 100 {
                let keep: Vec<String> = map.keys().take(100).cloned().collect();
                map.retain(|key, _| keep.contains(key));
            }
            map.values_mut().for_each(|item| bound_at(item, depth + 1));
        }
        Value::Array(items) => {
            if depth > 6 {
                *value = Value::String(TRUNCATED.to_string());
                return;
            }
            items.truncate(50);
            items.iter_mut().for_each(|item| bound_at(item, depth + 1));
        }
        Value::String(text) => truncate_in_place(text, 4_096),
        _ => {}
    }
}

/// Parses a query string into a JSON object, decoding `+` and `%XX`.
/// Repeated keys keep the last value.
pub(crate) fn query_params(query: &str) -> Map<String, Value> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), Value::String(percent_decode(value)))
        })
        .collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
                match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        index += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn defaults() -> KeyFilter {
        let parts: Vec<String> = DEFAULT_FILTER_PARTS.iter().map(|p| p.to_string()).collect();
        KeyFilter::new(&["card_token".to_string()], &parts)
    }

    #[test]
    fn filters_default_keys_at_any_depth() {
        let mut value = json!({
            "user_password": "hunter2",
            "nested": { "X-Access-Key": "k", "list": [{ "api-key": "x", "name": "ok" }] },
            "Authorization": "Bearer abc",
            "session-id": "s",
            "email_address": "a@example.com",
            "card_token": "tok",
            "cart_id": 88123
        });
        defaults().filter(&mut value);
        assert_eq!(value["user_password"], FILTERED);
        assert_eq!(value["nested"]["X-Access-Key"], FILTERED);
        assert_eq!(value["nested"]["list"][0]["api-key"], FILTERED);
        assert_eq!(value["nested"]["list"][0]["name"], "ok");
        assert_eq!(value["Authorization"], FILTERED);
        assert_eq!(value["session-id"], FILTERED);
        assert_eq!(value["email_address"], FILTERED);
        assert_eq!(value["card_token"], FILTERED);
        assert_eq!(value["cart_id"], 88123);
    }

    #[test]
    fn names_match_whole_keys_only() {
        let filter = KeyFilter::new(&["Secret-Code".to_string()], &[]);
        assert!(filter.is_sensitive("secret_code"));
        assert!(!filter.is_sensitive("my_secret_code"));
    }

    #[test]
    fn bounds_free_form_values() {
        let mut deep = json!("leaf");
        for _ in 0..8 {
            deep = json!({ "k": deep });
        }
        bound(&mut deep);
        assert_eq!(deep["k"]["k"]["k"]["k"]["k"]["k"], TRUNCATED);

        let mut value = json!({ "list": (0..80).collect::<Vec<_>>(), "text": "x".repeat(5_000) });
        bound(&mut value);
        assert_eq!(value["list"].as_array().unwrap().len(), 50);
        assert_eq!(value["text"].as_str().unwrap().len(), 4_096);
    }

    #[test]
    fn parses_query_strings() {
        let params = query_params("a=1&b=two+words&c=%41%zz&flag");
        assert_eq!(params["a"], "1");
        assert_eq!(params["b"], "two words");
        assert_eq!(params["c"], "A%zz");
        assert_eq!(params["flag"], "");
    }
}
