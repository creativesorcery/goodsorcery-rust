//! Per-minute aggregation: counts, failures, total duration and per-bucket
//! layer sums for each transaction, keyed by the UTC minute in which the
//! unit of work finished.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::histogram;
use super::recorder::Kind;
use crate::util::{iso8601_seconds, log, round3};

pub(crate) const MAX_TRANSACTIONS: usize = 500;
pub(crate) const MAX_PENDING_MINUTES: usize = 5;
pub(crate) const OTHER: &str = "(other)";
const RESERVED: [&str; 2] = ["All web requests", "All background jobs"];

/// One finished unit of work's numbers.
#[derive(Debug, Clone)]
pub(crate) struct Measurement {
    pub(crate) kind: Kind,
    pub(crate) name: String,
    pub(crate) method: Option<String>,
    pub(crate) route: Option<String>,
    pub(crate) adapter: Option<String>,
    pub(crate) queue: Option<String>,
    pub(crate) minute: u64,
    pub(crate) duration_ms: f64,
    pub(crate) db_ms: f64,
    pub(crate) http_ms: f64,
    pub(crate) view_ms: f64,
    pub(crate) app_ms: f64,
    pub(crate) failed: bool,
    pub(crate) queue_wait_ms: Option<f64>,
}

#[derive(Debug, Clone)]
pub(crate) struct TransactionStats {
    kind: Kind,
    name: String,
    method: Option<String>,
    route: Option<String>,
    adapter: Option<String>,
    queue: Option<String>,
    count: u64,
    failed: u64,
    duration_ms: f64,
    /// bucket index → [count, db_ms, http_ms, view_ms, app_ms]
    buckets: BTreeMap<usize, [f64; 5]>,
    queue_wait_ms: f64,
    queue_wait: BTreeMap<usize, u64>,
}

impl TransactionStats {
    fn new(kind: Kind, name: String) -> Self {
        TransactionStats {
            kind,
            name,
            method: None,
            route: None,
            adapter: None,
            queue: None,
            count: 0,
            failed: 0,
            duration_ms: 0.0,
            buckets: BTreeMap::new(),
            queue_wait_ms: 0.0,
            queue_wait: BTreeMap::new(),
        }
    }

    fn add(&mut self, m: &Measurement, labels: bool) {
        if labels {
            self.method = m.method.clone().or(self.method.take());
            self.route = m.route.clone().or(self.route.take());
            self.adapter = m.adapter.clone().or(self.adapter.take());
            self.queue = m.queue.clone().or(self.queue.take());
        }
        self.count += 1;
        self.failed += u64::from(m.failed);
        self.duration_ms += m.duration_ms;
        let bucket = self.buckets.entry(histogram::index(m.duration_ms)).or_insert([0.0; 5]);
        bucket[0] += 1.0;
        bucket[1] += m.db_ms;
        bucket[2] += m.http_ms;
        bucket[3] += m.view_ms;
        bucket[4] += m.app_ms;
        if let Some(wait) = m.queue_wait_ms {
            self.queue_wait_ms += wait;
            *self.queue_wait.entry(histogram::index(wait)).or_insert(0) += 1;
        }
    }

    /// The transaction's entry in a metrics payload.
    pub(crate) fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("kind".into(), Value::String(self.kind.name().into()));
        map.insert("name".into(), Value::String(self.name.clone()));
        let labels: [(&str, &Option<String>); 4] =
            [("method", &self.method), ("route", &self.route), ("adapter", &self.adapter), ("queue", &self.queue)];
        for (key, value) in labels {
            let allowed = match self.kind {
                Kind::Web => matches!(key, "method" | "route"),
                Kind::Job => matches!(key, "adapter" | "queue"),
            };
            if let (true, Some(value)) = (allowed, value) {
                map.insert(key.into(), Value::String(value.clone()));
            }
        }
        map.insert("count".into(), Value::from(self.count));
        map.insert("failed".into(), Value::from(self.failed));
        map.insert("duration_ms".into(), json!(round3(self.duration_ms)));
        let buckets: Vec<Value> = self
            .buckets
            .iter()
            .map(|(index, sums)| {
                json!([*index, sums[0] as u64, round3(sums[1]), round3(sums[2]), round3(sums[3]), round3(sums[4])])
            })
            .collect();
        map.insert("buckets".into(), Value::Array(buckets));
        if self.kind == Kind::Job && !self.queue_wait.is_empty() {
            map.insert("queue_wait_ms".into(), json!(round3(self.queue_wait_ms)));
            let waits: Vec<Value> = self.queue_wait.iter().map(|(index, count)| json!([*index, *count])).collect();
            map.insert("queue_wait".into(), Value::Array(waits));
        }
        Value::Object(map)
    }
}

#[derive(Debug, Default)]
struct MinuteStats {
    transactions: HashMap<(Kind, String), TransactionStats>,
    web_names: usize,
    job_names: usize,
}

/// Every minute not yet flushed (the open one plus any unsent).
#[derive(Debug, Default)]
pub(crate) struct Aggregator {
    minutes: BTreeMap<u64, MinuteStats>,
    dropped_minutes: u64,
}

impl Aggregator {
    pub(crate) fn add(&mut self, m: &Measurement) {
        let stats = self.minutes.entry(m.minute).or_default();
        let name = if RESERVED.contains(&m.name.as_str()) { OTHER.to_string() } else { m.name.clone() };
        let key = (m.kind, name);
        let (key, labels) = if stats.transactions.contains_key(&key) {
            let labels = key_has_labels(&key.1);
            (key, labels)
        } else {
            let names = match m.kind {
                Kind::Web => &mut stats.web_names,
                Kind::Job => &mut stats.job_names,
            };
            if *names >= MAX_TRANSACTIONS || key.1 == OTHER {
                ((m.kind, OTHER.to_string()), false)
            } else {
                *names += 1;
                (key, true)
            }
        };
        stats
            .transactions
            .entry(key.clone())
            .or_insert_with(|| TransactionStats::new(key.0, key.1.clone()))
            .add(m, labels);
        while self.minutes.len() > MAX_PENDING_MINUTES {
            self.minutes.pop_first();
            self.dropped_minutes += 1;
            log::warn_every(
                "apm_minutes_dropped",
                Duration::from_secs(60),
                format!(
                    "performance data for {} minute(s) could not be sent in time and was dropped",
                    self.dropped_minutes
                ),
            );
        }
    }

    /// Takes the minutes before `current` (or every minute with
    /// `include_open`), oldest first.
    pub(crate) fn take(&mut self, current: u64, include_open: bool) -> Vec<(u64, Vec<TransactionStats>)> {
        let keep = if include_open { BTreeMap::new() } else { self.minutes.split_off(&current) };
        let taken = std::mem::replace(&mut self.minutes, keep);
        taken
            .into_iter()
            .map(|(minute, stats)| {
                let mut list: Vec<TransactionStats> = stats.transactions.into_values().collect();
                list.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
                (minute, list)
            })
            .collect()
    }
}

fn key_has_labels(name: &str) -> bool {
    name != OTHER
}

/// The minute's ISO form.
pub(crate) fn minute_iso(minute: u64) -> String {
    iso8601_seconds(minute)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measurement(name: &str, minute: u64, duration_ms: f64) -> Measurement {
        Measurement {
            kind: Kind::Web,
            name: name.into(),
            method: Some("GET".into()),
            route: Some("/users/{id}".into()),
            adapter: None,
            queue: None,
            minute,
            duration_ms,
            db_ms: duration_ms / 2.0,
            http_ms: 0.0,
            view_ms: 0.0,
            app_ms: duration_ms / 2.0,
            failed: duration_ms > 1_000.0,
            queue_wait_ms: None,
        }
    }

    #[test]
    fn sums_buckets_and_layers_per_minute() {
        let mut aggregator = Aggregator::default();
        aggregator.add(&measurement("GET /users/{id}", 60, 250.0));
        aggregator.add(&measurement("GET /users/{id}", 60, 260.0));
        aggregator.add(&measurement("GET /users/{id}", 60, 2_000.0));
        let minutes = aggregator.take(120, false);
        assert_eq!(minutes.len(), 1);
        let json = minutes[0].1[0].to_json();
        assert_eq!(json["count"], 3);
        assert_eq!(json["failed"], 1);
        assert_eq!(json["duration_ms"], 2_510.0);
        assert_eq!(json["route"], "/users/{id}");
        assert_eq!(
            json["buckets"],
            json!([
                [15, 1, 125.0, 0.0, 0.0, 125.0],
                [16, 1, 130.0, 0.0, 0.0, 130.0],
                [21, 1, 1000.0, 0.0, 0.0, 1000.0]
            ])
        );
        assert!(json.get("adapter").is_none());
    }

    #[test]
    fn keeps_the_open_minute_unless_asked() {
        let mut aggregator = Aggregator::default();
        aggregator.add(&measurement("a", 60, 1.0));
        aggregator.add(&measurement("a", 120, 1.0));
        assert_eq!(aggregator.take(120, false).len(), 1);
        assert_eq!(aggregator.take(120, true).len(), 1);
        assert!(aggregator.take(0, true).is_empty());
    }

    #[test]
    fn folds_names_past_the_cap_into_other() {
        let mut aggregator = Aggregator::default();
        for n in 0..(MAX_TRANSACTIONS + 10) {
            aggregator.add(&measurement(&format!("GET /{n}"), 60, 1.0));
        }
        aggregator.add(&measurement("All web requests", 60, 1.0));
        let list = &aggregator.take(120, false)[0].1;
        assert_eq!(list.len(), MAX_TRANSACTIONS + 1);
        let other = list.iter().find(|stats| stats.name == OTHER).unwrap().to_json();
        assert_eq!(other["count"], 11);
        assert!(other.get("route").is_none());
    }

    #[test]
    fn holds_at_most_five_minutes() {
        let mut aggregator = Aggregator::default();
        for minute in 0..8u64 {
            aggregator.add(&measurement("a", minute * 60, 1.0));
        }
        let minutes = aggregator.take(10_000, false);
        assert_eq!(minutes.len(), MAX_PENDING_MINUTES);
        assert_eq!(minutes[0].0, 180);
    }

    #[test]
    fn job_queue_waits() {
        let mut aggregator = Aggregator::default();
        let mut job = measurement("SendReceipt", 60, 10.0);
        job.kind = Kind::Job;
        job.adapter = Some("apalis".into());
        job.queue = Some("mailers".into());
        job.queue_wait_ms = Some(30.0);
        aggregator.add(&job);
        let json = aggregator.take(120, false)[0].1[0].to_json();
        assert_eq!(json["kind"], "job");
        assert_eq!(json["adapter"], "apalis");
        assert!(json.get("method").is_none());
        assert_eq!(json["queue_wait_ms"], 30.0);
        assert_eq!(json["queue_wait"], json!([[10, 1]]));
    }
}
