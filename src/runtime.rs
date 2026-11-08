//! What every event and trace says about the process: the runtimes, host
//! and process label, and the notifier.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Map, Value, json};

use crate::config::Config;
use crate::util::truncate;

/// The client library's name, as sent in `notifier.name` and `User-Agent`.
pub const NOTIFIER_NAME: &str = "goodsorcery-rust";

/// The crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The version of the compiler that built the app (from the build script).
const RUSTC_VERSION: Option<&str> = option_env!("GOODSORCERY_RUSTC_VERSION");

/// Set once the axum layer is built, so the runtime label says "axum 0.8".
static AXUM_IN_USE: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "axum")]
pub(crate) fn note_axum() {
    AXUM_IN_USE.store(true, Ordering::Relaxed);
}

pub(crate) fn notifier() -> Value {
    json!({ "name": NOTIFIER_NAME, "version": VERSION })
}

/// Process details, computed once at `init`.
#[derive(Debug, Clone)]
pub(crate) struct RuntimeInfo {
    pub(crate) host: Option<String>,
    pub(crate) program: String,
    extra: Vec<(String, String)>,
}

impl RuntimeInfo {
    pub(crate) fn new(config: &Config) -> Self {
        RuntimeInfo {
            host: config.server_name.clone().or_else(hostname),
            program: program_name(),
            extra: config.runtimes.clone(),
        }
    }

    /// `[{ "name": "Rust", "version": "1.82.0" }, { "name": "axum", … }]`
    /// (protocol revision 1.1).
    pub(crate) fn runtime_list(&self) -> Value {
        let mut list = vec![json!({ "name": "Rust", "version": RUSTC_VERSION.unwrap_or("unknown") })];
        if AXUM_IN_USE.load(Ordering::Relaxed) && !self.extra.iter().any(|(name, _)| name == "axum") {
            list.push(json!({ "name": "axum", "version": "0.8" }));
        }
        for (name, version) in self.extra.iter().take(8) {
            list.push(json!({ "name": truncate(name, 32), "version": truncate(version, 64) }));
        }
        Value::Array(list)
    }

    /// A process label: the program's name, pid and the current thread.
    pub(crate) fn process_label(&self) -> String {
        let thread = std::thread::current();
        let thread = thread.name().map_or_else(|| format!("{:?}", thread.id()), str::to_string);
        truncate(&format!("{} · pid {} · thread {thread}", self.program, std::process::id()), 255)
    }

    /// The program's name and pid, for traces.
    pub(crate) fn trace_process(&self) -> String {
        truncate(&format!("{} · pid {}", self.program, std::process::id()), 255)
    }

    /// The `context` section's runtime fields.
    pub(crate) fn context(&self) -> Map<String, Value> {
        let mut context = Map::new();
        context.insert("runtime".into(), self.runtime_list());
        if let Some(host) = &self.host {
            context.insert("host".into(), Value::String(host.clone()));
        }
        context.insert("process".into(), Value::String(self.process_label()));
        context
    }
}

/// The host name: `HOSTNAME`, else the kernel's (Linux), else
/// `/etc/hostname`.
fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| truncate(name.trim(), 255))
        .filter(|name| !name.is_empty())
}

/// The executable's file name, without arguments (they can carry secrets).
fn program_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.file_name().map(|name| name.to_string_lossy().into_owned()))
        .or_else(|| {
            std::env::args_os()
                .next()
                .map(|arg| arg.to_string_lossy().rsplit(['/', '\\']).next().unwrap_or("").to_string())
        })
        .map(|name| truncate(&name, 128))
        .unwrap_or_else(|| "rust".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_list_starts_with_rust() {
        let info = RuntimeInfo::new(&Config::new().runtime("tokio", "1.41.0").server_name("api-3"));
        let list = info.runtime_list();
        assert_eq!(list[0]["name"], "Rust");
        assert!(list[0]["version"].as_str().unwrap().starts_with("1."));
        assert!(list.as_array().unwrap().iter().any(|entry| entry["name"] == "tokio"));
        let context = info.context();
        assert_eq!(context["host"], "api-3");
        assert!(context["process"].as_str().unwrap().contains("pid"));
    }
}
