//! The panic hook: reports every panic with its message, location, thread
//! and backtrace, then runs the hook that was installed before.
//!
//! The hook runs on the panicking thread while the panic's scope is still
//! bound, so a panic in a request handler carries that request's data. A
//! panic inside a panic hook aborts the process, so everything here avoids
//! anything that could panic: locks recover from poisoning, and no app code
//! (`Display`, callbacks) runs here.

use std::backtrace::Backtrace;
use std::cell::Cell;
use std::sync::Once;

use serde_json::{Value, json};

use crate::client;
use crate::event::{EventId, ExceptionInfo, Level, Options, Site};
use crate::util::truncate;

thread_local! {
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
    static LAST_PANIC: Cell<Option<EventId>> = const { Cell::new(None) };
}

static INSTALL: Once = Once::new();

/// Installs the hook once per process, chaining the previous one.
pub(crate) fn install() {
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            report(info);
            previous(info);
        }));
    });
}

/// Whether this thread is running the panic hook.
pub(crate) fn in_hook() -> bool {
    IN_HOOK.try_with(Cell::get).unwrap_or(false)
}

/// The event id the hook reported for the last panic on this thread, taken
/// (so it is seen once). The axum layer uses it to avoid reporting a panic
/// twice.
pub(crate) fn take_last_panic() -> Option<EventId> {
    LAST_PANIC.try_with(Cell::take).ok().flatten()
}

fn report(info: &std::panic::PanicHookInfo<'_>) {
    if client::on_sdk_thread() || IN_HOOK.try_with(|flag| flag.replace(true)).unwrap_or(true) {
        return;
    }
    if let Some(client) = client::current_enabled() {
        let message = payload_message(info.payload());
        let site = info.location().map(Site::from);
        let level = if cfg!(panic = "abort") { Level::Fatal } else { Level::Error };
        let mut panic = json!({ "thread": std::thread::current().name().unwrap_or("unnamed") });
        if let Some(site) = &site {
            // Shown like a frame path: a panic inside a dependency or std
            // must not send the build machine's absolute path.
            let file = match &client.paths {
                Some(paths) => crate::backtrace::display_path_in_hook(paths, &site.file),
                None => site.file.clone(),
            };
            panic["location"] = Value::String(format!("{file}:{}:{}", site.line, site.column));
        }
        let info = ExceptionInfo { class: "panic".into(), message, causes: Vec::new() };
        let options = Options::new().level(level).context("panic", panic);
        let id = client.capture(info, options, Some(Backtrace::force_capture()), site);
        let _ = LAST_PANIC.try_with(|last| last.set(id));
        if cfg!(panic = "abort") {
            client.flush(client.config.shutdown_timeout);
        }
    }
    let _ = IN_HOOK.try_with(|flag| flag.set(false));
}

/// The panic's payload as text (`panic!` gives a `&str` or a `String`).
pub(crate) fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    let text = if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "Box<dyn Any>".to_string()
    };
    truncate(&text, 10_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_messages() {
        let boxed: Box<dyn std::any::Any + Send> = Box::new("static text");
        assert_eq!(payload_message(&*boxed), "static text");
        let boxed: Box<dyn std::any::Any + Send> = Box::new(format!("formatted {}", 42));
        assert_eq!(payload_message(&*boxed), "formatted 42");
        let boxed: Box<dyn std::any::Any + Send> = Box::new(7u8);
        assert_eq!(payload_message(&*boxed), "Box<dyn Any>");
    }
}
