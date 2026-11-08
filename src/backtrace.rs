//! Turns a `std::backtrace::Backtrace` into the payload's frames: app-relative
//! file, line, column, method, module, package and `in_app`, with source
//! lines for the top in-app frames when the files can be read.
//!
//! A backtrace is captured unresolved (cheap) where the error happens and
//! resolved on the sender thread, so symbolisation never runs in the
//! request. Resolution goes through the standard library's `Display`
//! output, the only stable access to symbols and locations.

use std::backtrace::Backtrace;
use std::collections::HashMap;
use std::panic::Location;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Map, Value};

use crate::util::{lock, truncate};

const MAX_FRAMES: usize = 250;
const CONTEXT_FRAMES: usize = 10;
const MAX_SOURCE_FILES: usize = 32;
const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;

/// Crates whose frames are never the app's own code, used when a frame has
/// no path that tells.
const RUNTIME_CRATES: &[&str] = &[
    "std",
    "core",
    "alloc",
    "proc_macro",
    "test",
    "backtrace",
    "goodsorcery",
    "tokio",
    "tokio_util",
    "futures",
    "futures_util",
    "futures_core",
    "futures_executor",
    "hyper",
    "hyper_util",
    "h2",
    "http",
    "axum",
    "axum_core",
    "tower",
    "tower_http",
    "tower_service",
    "tracing",
    "tracing_core",
    "tracing_subscriber",
    "sqlx",
    "sqlx_core",
    "reqwest",
    "__rustc",
];

/// One frame as the standard library prints it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RawFrame {
    pub(crate) symbol: Option<String>,
    pub(crate) file: Option<String>,
    pub(crate) line: Option<u32>,
    pub(crate) column: Option<u32>,
}

/// Parses `Backtrace`'s (short) `Display` output: an indexed symbol line per
/// frame, optionally followed by an `at file:line:col` line.
///
/// The short output prints a path under the current directory as
/// `./<rest>`. Call it right after formatting the backtrace, so the
/// directory it reads is the one std stripped.
pub(crate) fn parse(text: &str) -> Vec<RawFrame> {
    let cwd = std::env::current_dir().ok().and_then(|dir| dir.to_str().map(str::to_string));
    parse_in(text, cwd.as_deref())
}

/// [`parse`] with the directory std made paths relative to: `./<rest>` is
/// turned back into `<cwd>/<rest>`, so every path is classified (app root,
/// `target/`, the standard library, the home directory) as it was built.
/// With the process started in `/` (Docker without `WORKDIR`, systemd)
/// every absolute path comes out that way.
pub(crate) fn parse_in(text: &str, cwd: Option<&str>) -> Vec<RawFrame> {
    let mut frames: Vec<RawFrame> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(location) = trimmed.strip_prefix("at ") {
            if let Some(frame) = frames.last_mut() {
                let (file, line, column) = split_location(location);
                frame.file = Some(absolute(file, cwd));
                frame.line = line;
                frame.column = column;
            }
            continue;
        }
        if let Some((index, symbol)) = trimmed.split_once(": ") {
            if !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()) {
                let symbol = symbol.trim();
                frames.push(RawFrame {
                    symbol: (symbol != "<unknown>" && !symbol.is_empty()).then(|| strip_hash(symbol).to_string()),
                    ..RawFrame::default()
                });
            }
        }
    }
    frames
}

/// `./rest` (or `.\\rest`) under `cwd` → `cwd/rest`; other paths as they are.
fn absolute(path: String, cwd: Option<&str>) -> String {
    let (Some(cwd), Some(rest)) = (cwd, path.strip_prefix("./").or_else(|| path.strip_prefix(".\\"))) else {
        return path;
    };
    let separator = if cwd.contains('\\') && !cwd.contains('/') { '\\' } else { '/' };
    format!("{}{separator}{rest}", cwd.trim_end_matches(['/', '\\']))
}

/// `path:line:col` → (path, line, col), reading numbers from the right.
fn split_location(location: &str) -> (String, Option<u32>, Option<u32>) {
    let mut path = location.trim();
    let mut numbers = Vec::new();
    while numbers.len() < 2 {
        match path.rsplit_once(':') {
            Some((rest, number)) if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) => {
                numbers.push(number.parse::<u32>().ok());
                path = rest;
            }
            _ => break,
        }
    }
    match numbers.as_slice() {
        [column, line] => (path.to_string(), *line, *column),
        [line] => (path.to_string(), *line, None),
        _ => (path.to_string(), None, None),
    }
}

/// Removes a legacy-mangling hash suffix (`::h0123456789abcdef`).
fn strip_hash(symbol: &str) -> &str {
    match symbol.rsplit_once("::h") {
        Some((rest, hash)) if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => rest,
        _ => symbol,
    }
}

/// Drops the frames of the backtrace machinery, the panic machinery and this
/// client at the top, and the thread or `main` start-up below
/// `__rust_begin_short_backtrace` at the bottom.
pub(crate) fn trim(frames: Vec<RawFrame>) -> Vec<RawFrame> {
    let end = frames
        .iter()
        .position(|frame| frame.symbol.as_deref().is_some_and(|s| s.contains("__rust_begin_short_backtrace")))
        .unwrap_or(frames.len());
    let start = frames[..end].iter().position(|frame| !is_machinery(frame)).unwrap_or(end);
    frames[start..end].to_vec()
}

fn is_machinery(frame: &RawFrame) -> bool {
    let Some(symbol) = frame.symbol.as_deref() else { return true };
    match crate_of(symbol) {
        Some(name) => matches!(name, "std" | "core" | "alloc" | "backtrace" | "goodsorcery" | "__rustc"),
        None => true,
    }
}

/// The crate a symbol belongs to: its first path segment, or the first
/// segment of the type in `<Type as Trait>::method`.
pub(crate) fn crate_of(symbol: &str) -> Option<&str> {
    let text = symbol.trim_start_matches('<').trim_start_matches('&').trim_start_matches("mut ");
    let text = text.strip_prefix("dyn ").unwrap_or(text);
    let end = text.find("::")?;
    let name = &text[..end];
    let valid = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    valid.then_some(name)
}

/// Splits a symbol into (module, method): `shop::cart::line_total` →
/// (`shop::cart`, `line_total`). Closure and shim segments stay with the
/// method (`create::{{closure}}`).
pub(crate) fn split_symbol(symbol: &str) -> (Option<String>, String) {
    let segments = path_segments(symbol);
    if segments.len() < 2 {
        return (None, symbol.to_string());
    }
    let mut method_start = segments.len() - 1;
    while method_start > 0 && is_suffix_segment(segments[method_start]) {
        method_start -= 1;
    }
    if method_start == 0 {
        return (None, symbol.to_string());
    }
    (Some(segments[..method_start].join("::")), segments[method_start..].join("::"))
}

fn is_suffix_segment(segment: &str) -> bool {
    segment.starts_with("{{") || segment.starts_with('{') || segment.starts_with('<')
}

/// Splits on `::` outside `<…>`, `(…)` and `[…]`.
fn path_segments(symbol: &str) -> Vec<&str> {
    let bytes = symbol.as_bytes();
    let mut depth = 0i32;
    let mut segments = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' | b')' | b']' => depth -= 1,
            b':' if depth == 0 && bytes.get(index + 1) == Some(&b':') => {
                segments.push(&symbol[start..index]);
                index += 2;
                start = index;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    segments.push(&symbol[start..]);
    segments
}

/// What the app was built from, learned once from where `init` was called.
#[derive(Debug, Default, Clone)]
struct Learned {
    root: Option<String>,
    app_crate: Option<String>,
}

/// Classifies and rewrites frame paths.
pub(crate) struct Paths {
    configured_root: Option<String>,
    cwd: Option<String>,
    home: Option<String>,
    include: Vec<String>,
    exclude: Vec<String>,
    init_location: Option<&'static Location<'static>>,
    init_trace: Mutex<Option<Backtrace>>,
    learned: OnceLock<Learned>,
}

impl Paths {
    pub(crate) fn new(
        configured_root: Option<String>,
        include: Vec<String>,
        exclude: Vec<String>,
        init_location: Option<&'static Location<'static>>,
        init_trace: Option<Backtrace>,
    ) -> Self {
        Paths {
            configured_root: configured_root.map(with_slash),
            cwd: std::env::current_dir().ok().and_then(|dir| dir.to_str().map(with_slash)),
            home: std::env::var("HOME").ok().filter(|home| home.len() > 1).map(with_slash),
            include,
            exclude,
            init_location,
            init_trace: Mutex::new(init_trace),
            learned: OnceLock::new(),
        }
    }

    /// Learns the build root and the app's crate from the backtrace taken in
    /// `init`: the caller's frame has an absolute path ending in the
    /// caller's (relative) `Location`, and a symbol in the app's crate.
    fn learned(&self) -> &Learned {
        self.learned.get_or_init(|| {
            let Some(trace) = lock(&self.init_trace).take() else { return Learned::default() };
            let frames = parse(&trace.to_string());
            let caller = frames
                .iter()
                .skip_while(|frame| !frame.symbol.as_deref().is_some_and(|s| s.starts_with("goodsorcery::")))
                .find(|frame| !frame.symbol.as_deref().is_some_and(|s| s.starts_with("goodsorcery::")));
            let Some(caller) = caller else { return Learned::default() };
            let app_crate = caller
                .symbol
                .as_deref()
                .and_then(crate_of)
                .filter(|name| !RUNTIME_CRATES.contains(name))
                .map(str::to_string);
            let root = match (self.init_location, caller.file.as_deref()) {
                (Some(location), Some(file)) if !location.file().starts_with('/') && file.starts_with('/') => {
                    file.strip_suffix(location.file()).filter(|root| root.ends_with('/')).map(str::to_string)
                }
                _ => None,
            };
            Learned { root, app_crate }
        })
    }

    fn roots_with<'a>(&'a self, learned: Option<&'a str>) -> impl Iterator<Item = &'a str> {
        self.configured_root.as_deref().into_iter().chain(learned).chain(self.cwd.as_deref().filter(|cwd| *cwd != "/"))
    }

    /// Classifies one frame: its display path, whether it is the app's own
    /// code and the package it belongs to.
    pub(crate) fn classify(&self, file: Option<&str>, symbol: Option<&str>) -> FrameClass {
        let crate_name = symbol.and_then(crate_of);
        if let Some(name) = crate_name {
            if self.exclude.iter().any(|excluded| excluded == name) {
                return self.class_for(file, false, crate_name);
            }
            if self.include.iter().any(|included| included == name) {
                return self.class_for(file, true, crate_name);
            }
        }
        if crate_name == Some("goodsorcery") && self.learned().app_crate.as_deref() != Some("goodsorcery") {
            // This client is never the app's code, even when it is built
            // from a path inside the app (a workspace member or a vendored copy).
            return self.class_for(file, false, crate_name);
        }
        let in_app = match file {
            Some(path) => self.path_in_app(path, crate_name),
            None => crate_name.is_some_and(|name| self.learned().app_crate.as_deref() == Some(name)),
        };
        self.class_for(file, in_app, crate_name)
    }

    fn class_for(&self, file: Option<&str>, in_app: bool, crate_name: Option<&str>) -> FrameClass {
        let (display, package) = match file {
            Some(path) => self.display(path),
            None => (None, None),
        };
        let package = if in_app { None } else { package.or_else(|| crate_name.map(str::to_string)) };
        FrameClass { file: display, in_app, package }
    }

    fn path_in_app(&self, path: &str, crate_name: Option<&str>) -> bool {
        self.path_in_app_with(path, crate_name, self.learned().root.as_deref())
    }

    /// Whether a source file named in tracing metadata (where a span was
    /// declared) is the app's own code. Cheap enough for every span: it
    /// never resolves the backtrace taken in `init`.
    pub(crate) fn declared_in_app(&self, file: &str) -> bool {
        let learned = self.learned.get().and_then(|learned| learned.root.as_deref());
        self.path_in_app_with(&file.replace('\\', "/"), None, learned)
    }

    fn path_in_app_with(&self, path: &str, crate_name: Option<&str>, learned: Option<&str>) -> bool {
        if is_dependency_path(path) {
            return false;
        }
        if let Some(rest) = self.roots_with(learned).find_map(|root| path.strip_prefix(root)) {
            return !(rest.starts_with("target/") || rest.starts_with("vendor/"));
        }
        if !path.starts_with('/') && !is_windows_absolute(path) {
            return !path.starts_with("<");
        }
        !crate_name.is_some_and(|name| RUNTIME_CRATES.contains(&name))
    }

    /// The path to send and, for dependency paths, the package name.
    fn display(&self, path: &str) -> (Option<String>, Option<String>) {
        self.display_with(path, true)
    }

    /// [`display`](Self::display); with `learn` false it never resolves the
    /// backtrace taken in `init` (too slow for the panic hook), using the
    /// learned root only if it is already known.
    fn display_with(&self, path: &str, learn: bool) -> (Option<String>, Option<String>) {
        let learned =
            if learn { self.learned().root.as_deref() } else { self.learned.get().and_then(|l| l.root.as_deref()) };
        let path = path.replace('\\', "/");
        if let Some((rest, package)) = registry_path(&path) {
            return (Some(truncate(&rest, 1_024)), Some(package));
        }
        if let Some((rest, package)) = git_path(&path) {
            return (Some(truncate(&rest, 1_024)), Some(package));
        }
        if let Some((rest, package)) = std_path(&path) {
            return (Some(truncate(&rest, 1_024)), Some(package));
        }
        if let Some(relative) = path.strip_prefix("./") {
            return (Some(truncate(relative, 1_024)), None);
        }
        if let Some(rest) = self.roots_with(learned).find_map(|root| path.strip_prefix(root)) {
            return (Some(truncate(rest, 1_024)), None);
        }
        if let Some(rest) = self.home.as_deref().and_then(|home| path.strip_prefix(home)) {
            return (Some(truncate(&format!("~/{rest}"), 1_024)), None);
        }
        (Some(truncate(&path, 1_024)), None)
    }

    /// A path that can be opened to read source lines, if any.
    fn readable_path(&self, path: &str) -> String {
        path.to_string()
    }
}

/// How one frame is reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrameClass {
    pub(crate) file: Option<String>,
    pub(crate) in_app: bool,
    pub(crate) package: Option<String>,
}

fn with_slash(path: impl AsRef<str>) -> String {
    let path = path.as_ref().replace('\\', "/");
    if path.ends_with('/') { path } else { format!("{path}/") }
}

fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() > 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn is_dependency_path(path: &str) -> bool {
    let path = path.replace('\\', "/");
    path.contains("/registry/src/")
        || path.contains("/.cargo/git/checkouts/")
        || path.starts_with("/rustc/")
        || path.contains("/lib/rustlib/")
        || path.contains("/vendor/")
}

/// `…/registry/src/<index>/<name>-<version>/rest` → `crates/<name>-<version>/rest`.
fn registry_path(path: &str) -> Option<(String, String)> {
    let (_, after) = path.split_once("/registry/src/")?;
    let (_index, rest) = after.split_once('/')?;
    let (dir, file) = rest.split_once('/')?;
    Some((format!("crates/{dir}/{file}"), package_name(dir)))
}

/// `…/.cargo/git/checkouts/<repo>-<hash>/<rev>/rest` → `crates/<repo>/rest`.
fn git_path(path: &str) -> Option<(String, String)> {
    let (_, after) = path.split_once("/.cargo/git/checkouts/")?;
    let (dir, rest) = after.split_once('/')?;
    let (_rev, file) = rest.split_once('/')?;
    let repo = dir.rsplit_once('-').map_or(dir, |(name, _)| name);
    Some((format!("crates/{repo}/{file}"), repo.to_string()))
}

/// The standard library: `/rustc/<hash>/library/std/…` or
/// `…/lib/rustlib/src/rust/library/std/…` → `rust/library/std/…`.
fn std_path(path: &str) -> Option<(String, String)> {
    let (_, after) = path.split_once("/library/")?;
    if !(path.starts_with("/rustc/") || path.contains("/lib/rustlib/")) {
        return None;
    }
    let package = after.split('/').next().unwrap_or("std").to_string();
    Some((format!("rust/library/{after}"), package))
}

/// `tokio-util-0.7.11` → `tokio-util`: everything before the first `-`
/// followed by a digit.
fn package_name(dir: &str) -> String {
    let bytes = dir.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'-' && bytes.get(index + 1).is_some_and(|next| next.is_ascii_digit()) {
            return dir[..index].to_string();
        }
    }
    dir.to_string()
}

/// Source lines for code context, cached per file.
pub(crate) struct SourceCache {
    files: Mutex<HashMap<String, Option<Arc<Vec<String>>>>>,
}

impl SourceCache {
    pub(crate) fn new() -> Self {
        SourceCache { files: Mutex::new(HashMap::new()) }
    }

    fn lines(&self, path: &str) -> Option<Arc<Vec<String>>> {
        if let Some(cached) = lock(&self.files).get(path) {
            return cached.clone();
        }
        let loaded = std::fs::metadata(path)
            .ok()
            .filter(|meta| meta.is_file() && meta.len() <= MAX_SOURCE_BYTES)
            .and_then(|_| std::fs::read(path).ok())
            .map(|bytes| Arc::new(String::from_utf8_lossy(&bytes).lines().map(str::to_string).collect::<Vec<_>>()));
        let mut files = lock(&self.files);
        if files.len() < MAX_SOURCE_FILES {
            files.insert(path.to_string(), loaded.clone());
        }
        loaded
    }

    /// `{start_line, lines}` around `line` (1-based), or `None` when the
    /// file can't be read or is shorter.
    pub(crate) fn context(&self, path: &str, line: u32, before: u32, after: u32, max_len: usize) -> Option<Value> {
        let lines = self.lines(path)?;
        let line = line as usize;
        if line == 0 || line > lines.len() {
            return None;
        }
        let start = line.saturating_sub(before as usize).max(1);
        let end = (line + after as usize).min(lines.len());
        let picked: Vec<Value> =
            lines[start - 1..end].iter().map(|text| Value::String(truncate(text, max_len))).collect();
        let mut map = Map::new();
        map.insert("start_line".into(), Value::from(start));
        map.insert("lines".into(), Value::Array(picked));
        Some(Value::Object(map))
    }
}

/// Builds the payload's frames from a captured backtrace.
pub(crate) fn frames(trace: &Backtrace, paths: &Paths, source: Option<&SourceCache>) -> Vec<Value> {
    frames_from(trim(parse(&trace.to_string())), paths, source)
}

pub(crate) fn frames_from(raw: Vec<RawFrame>, paths: &Paths, source: Option<&SourceCache>) -> Vec<Value> {
    let mut context_left = if source.is_some() { CONTEXT_FRAMES } else { 0 };
    raw.into_iter()
        .take(MAX_FRAMES)
        .map(|frame| {
            let mut json = frame_json(&frame, paths);
            if context_left > 0 && json.get("in_app") == Some(&Value::Bool(true)) {
                if let (Some(source), Some(file), Some(line)) = (source, frame.file.as_deref(), frame.line) {
                    if let Some(context) = source.context(&paths.readable_path(file), line, 5, 5, 500) {
                        json.insert("context".into(), context);
                    }
                }
                context_left -= 1;
            }
            Value::Object(json)
        })
        .collect()
}

/// One frame's JSON (without context).
pub(crate) fn frame_json(frame: &RawFrame, paths: &Paths) -> Map<String, Value> {
    let class = paths.classify(frame.file.as_deref(), frame.symbol.as_deref());
    let mut map = Map::new();
    if let Some(file) = class.file {
        map.insert("file".into(), Value::String(file));
    }
    if let Some(line) = frame.line {
        map.insert("line".into(), Value::from(line));
    }
    if let Some(column) = frame.column {
        map.insert("column".into(), Value::from(column));
    }
    if let Some(symbol) = frame.symbol.as_deref() {
        let (module, method) = split_symbol(symbol);
        map.insert("method".into(), Value::String(truncate(&method, 255)));
        if let Some(module) = module {
            map.insert("module".into(), Value::String(truncate(&module, 255)));
        }
    }
    map.insert("in_app".into(), Value::Bool(class.in_app));
    if let Some(package) = class.package {
        map.insert("package".into(), Value::String(truncate(&package, 255)));
    }
    map
}

/// The first in-app frame of a backtrace, for an N+1 query's call site.
pub(crate) fn first_app_frame(trace: &Backtrace, paths: &Paths) -> Option<RawFrame> {
    trim(parse(&trace.to_string()))
        .into_iter()
        .find(|frame| frame.file.is_some() && paths.classify(frame.file.as_deref(), frame.symbol.as_deref()).in_app)
}

/// The display path of an app file (for a `#[track_caller]` location).
pub(crate) fn display_path(paths: &Paths, file: &str) -> String {
    relative_std_location(file).unwrap_or_else(|| paths.display(file).0.unwrap_or_else(|| file.to_string()))
}

/// [`display_path`] for the panic hook: never resolves a backtrace.
pub(crate) fn display_path_in_hook(paths: &Paths, file: &str) -> String {
    relative_std_location(file).unwrap_or_else(|| paths.display_with(file, false).0.unwrap_or_else(|| file.to_string()))
}

/// Standard library crates, as named under `library/`.
const STD_CRATES: [&str; 6] = ["std", "core", "alloc", "proc_macro", "test", "panic_unwind"];

/// Some toolchains (Linux builds of 1.85, for one) give `Location::file()`
/// for std code as `library/std/src/time.rs` rather than
/// `/rustc/<hash>/library/…`: shown as `rust/library/std/src/time.rs`, like
/// std frames. Only for locations, never backtrace paths.
fn relative_std_location(file: &str) -> Option<String> {
    let rest = file.strip_prefix("library/")?;
    let krate = rest.split('/').next()?;
    STD_CRATES.contains(&krate).then(|| format!("rust/{file}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "   0: <std::backtrace::Backtrace>::create
   1: goodsorcery::panic::hook
             at /Users/me/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/goodsorcery-0.1.0/src/panic.rs:40:18
   2: std::panicking::panic_with_hook
   3: __rustc::rust_begin_unwind
   4: core::panicking::panic_fmt
   5: core::option::Option<T>::unwrap
             at /rustc/90b35a6239c3d8bdabc530a6a0816f7ff89a0aaf/library/core/src/option.rs:1016:21
   6: shop::cart::line_total
             at /build/src/cart.rs:42:17
   7: shop::handlers::checkout::create::{{closure}}
             at ./src/handlers/checkout.rs:18:5
   8: <axum::routing::route::Route as tower_service::Service>::call
             at /Users/me/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/axum-0.8.1/src/routing/route.rs:140:9
   9: tokio::runtime::task::harness::poll
             at /opt/homebrew/Cellar/rust/1.96.0/lib/rustlib/src/rust/library/std/src/thread.rs:12:1
  10: std::sys::backtrace::__rust_begin_short_backtrace
             at /rustc/90b35a6239c3d8bdabc530a6a0816f7ff89a0aaf/library/std/src/sys/backtrace.rs:166:18
  11: std::rt::lang_start
  12: _main
";

    fn paths() -> Paths {
        Paths::new(Some("/build".into()), vec![], vec![], None, None)
    }

    #[test]
    fn parses_and_trims_frames() {
        let frames = trim(parse(SAMPLE));
        let symbols: Vec<_> = frames.iter().map(|f| f.symbol.clone().unwrap()).collect();
        assert_eq!(symbols.first().unwrap(), "shop::cart::line_total");
        assert_eq!(symbols.last().unwrap(), "tokio::runtime::task::harness::poll");
        assert_eq!(frames[0].file.as_deref(), Some("/build/src/cart.rs"));
        assert_eq!(frames[0].line, Some(42));
        assert_eq!(frames[0].column, Some(17));
    }

    #[test]
    fn classifies_frames() {
        let paths = paths();
        let json: Vec<_> = trim(parse(SAMPLE)).iter().map(|f| frame_json(f, &paths)).collect();
        assert_eq!(json[0]["file"], "src/cart.rs");
        assert_eq!(json[0]["method"], "line_total");
        assert_eq!(json[0]["module"], "shop::cart");
        assert_eq!(json[0]["in_app"], true);
        assert!(json[0].get("package").is_none());
        assert_eq!(json[1]["file"], "src/handlers/checkout.rs");
        assert_eq!(json[1]["method"], "create::{{closure}}");
        assert_eq!(json[1]["in_app"], true);
        assert_eq!(json[2]["file"], "crates/axum-0.8.1/src/routing/route.rs");
        assert_eq!(json[2]["in_app"], false);
        assert_eq!(json[2]["package"], "axum");
        assert_eq!(json[2]["module"], "<axum::routing::route::Route as tower_service::Service>");
        assert_eq!(json[3]["file"], "rust/library/std/src/thread.rs");
        assert_eq!(json[3]["package"], "std");
        assert_eq!(json[3]["in_app"], false);
    }

    #[test]
    fn paths_relative_to_the_working_directory_are_made_absolute() {
        // Started in `/`: std prints every absolute path as `./…`.
        let text = "   0: shop::cart::line_total
             at ./build/src/cart.rs:42:17
   1: core::option::Option<T>::unwrap
             at ./rustc/90b35a6239c3d8bdabc530a6a0816f7ff89a0aaf/library/core/src/option.rs:1016:21
   2: shop::build_script
             at ./build/target/debug/build/shop-1/out/gen.rs:3:1
   3: other::thing
             at ./home/mara/src/other/lib.rs:1:1
";
        let frames = parse_in(text, Some("/"));
        assert_eq!(frames[0].file.as_deref(), Some("/build/src/cart.rs"));
        let mut paths = paths();
        paths.cwd = Some("/".into());
        paths.home = Some("/home/mara/".into());
        let json: Vec<_> = frames.iter().map(|f| frame_json(f, &paths)).collect();
        assert_eq!(json[0]["file"], "src/cart.rs");
        assert_eq!(json[0]["in_app"], true);
        assert_eq!(json[1]["file"], "rust/library/core/src/option.rs");
        assert_eq!(json[1]["in_app"], false);
        assert_eq!(json[2]["in_app"], false, "build output under target/ is not app code");
        assert_eq!(json[3]["file"], "~/src/other/lib.rs");
        // Started in the build root, as before.
        let frames = parse_in("   0: shop::run\n             at ./src/main.rs:1:1\n", Some("/build"));
        assert_eq!(frames[0].file.as_deref(), Some("/build/src/main.rs"));
        assert_eq!(frame_json(&frames[0], &paths)["file"], "src/main.rs");
        // A Windows working directory.
        let frames = parse_in("   0: shop::run\n             at .\\src\\main.rs:1:1\n", Some("C:\\build"));
        assert_eq!(frames[0].file.as_deref(), Some("C:\\build\\src\\main.rs"));
    }

    #[test]
    fn panic_locations_lose_build_machine_paths() {
        let mut paths = paths();
        paths.home = Some("/Users/mara/".into());
        let registry = "/Users/mara/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/hyper-1.4.1/src/proto/h1.rs";
        assert_eq!(display_path_in_hook(&paths, registry), "crates/hyper-1.4.1/src/proto/h1.rs");
        let std = "/rustc/90b35a6239c3d8bdabc530a6a0816f7ff89a0aaf/library/core/src/option.rs";
        assert_eq!(display_path_in_hook(&paths, std), "rust/library/core/src/option.rs");
        assert_eq!(display_path_in_hook(&paths, "library/std/src/time.rs"), "rust/library/std/src/time.rs");
        assert_eq!(display_path(&paths, "library/core/src/option.rs"), "rust/library/core/src/option.rs");
        assert_eq!(display_path_in_hook(&paths, "library/mine/src/lib.rs"), "library/mine/src/lib.rs");
        assert_eq!(display_path_in_hook(&paths, "/Users/mara/elsewhere/lib.rs"), "~/elsewhere/lib.rs");
        assert_eq!(display_path_in_hook(&paths, "/build/src/main.rs"), "src/main.rs");
        assert_eq!(display_path_in_hook(&paths, "src/main.rs"), "src/main.rs");
    }

    #[test]
    fn declared_files_in_dependencies_are_not_app_code() {
        let paths = paths();
        assert!(paths.declared_in_app("src/db.rs"));
        assert!(paths.declared_in_app("/build/src/db.rs"));
        assert!(!paths.declared_in_app("/build/target/debug/build/shop-1/out/gen.rs"));
        assert!(!paths.declared_in_app(
            "/Users/me/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/sea-orm-1.1.0/src/executor/query.rs"
        ));
        assert!(!paths.declared_in_app("/home/me/.cargo/git/checkouts/mylib-1a2b3c/abc1234/src/lib.rs"));
    }

    #[test]
    fn crate_names_and_packages() {
        assert_eq!(crate_of("shop::cart::line_total"), Some("shop"));
        assert_eq!(crate_of("<alloc::boxed::Box<F> as core::ops::Fn>::call"), Some("alloc"));
        assert_eq!(crate_of("_main"), None);
        assert_eq!(package_name("tokio-util-0.7.11"), "tokio-util");
        assert_eq!(package_name("h2-0.4.5"), "h2");
        assert_eq!(package_name("foo-1.0.0-beta.1"), "foo");
    }

    #[test]
    fn splits_symbols() {
        assert_eq!(split_symbol("line_total"), (None, "line_total".into()));
        assert_eq!(
            split_symbol("core::option::Option<T>::unwrap"),
            (Some("core::option::Option<T>".into()), "unwrap".into())
        );
        assert_eq!(split_symbol("shop::main::{closure#0}"), (Some("shop".into()), "main::{closure#0}".into()));
    }

    #[test]
    fn strips_hashes_and_unknown_symbols() {
        let frames = parse("   0: shop::run::h0123456789abcdef\n   1: <unknown>\n");
        assert_eq!(frames[0].symbol.as_deref(), Some("shop::run"));
        assert_eq!(frames[1].symbol, None);
    }

    #[test]
    fn locations_without_columns() {
        assert_eq!(split_location("C:\\src\\main.rs:12"), ("C:\\src\\main.rs".into(), Some(12), None));
        assert_eq!(split_location("src/a.rs:1:2"), ("src/a.rs".into(), Some(1), Some(2)));
    }

    #[test]
    fn git_and_home_paths() {
        let paths = paths();
        let class =
            paths.classify(Some("/home/me/.cargo/git/checkouts/mylib-1a2b3c/abc1234/src/lib.rs"), Some("mylib::run"));
        assert_eq!(class.file.as_deref(), Some("crates/mylib/src/lib.rs"));
        assert!(!class.in_app);
        assert_eq!(class.package.as_deref(), Some("mylib"));
        let unknown = paths.classify(None, Some("tokio::spawn"));
        assert!(!unknown.in_app);
        assert_eq!(unknown.package.as_deref(), Some("tokio"));
    }

    #[test]
    fn this_client_is_never_app_code() {
        let paths = paths();
        let symbol = "<goodsorcery::axum::ResponseFuture<F> as core::future::future::Future>::poll";
        let class = paths.classify(Some("/build/src/axum.rs"), Some(symbol));
        assert!(!class.in_app, "a path dependency on the client is still not the app");
        assert_eq!(class.package.as_deref(), Some("goodsorcery"));
        assert!(paths.classify(Some("/build/src/axum.rs"), Some("shop::axum::handler")).in_app);
        let included = Paths::new(Some("/build".into()), vec!["goodsorcery".into()], vec![], None, None);
        assert!(included.classify(Some("/build/src/axum.rs"), Some(symbol)).in_app);
    }

    #[test]
    fn include_and_exclude_override() {
        let paths = Paths::new(None, vec!["vendored_app".into()], vec!["shop_macros".into()], None, None);
        assert!(paths.classify(Some("/x/vendor/vendored_app/src/a.rs"), Some("vendored_app::a")).in_app);
        assert!(!paths.classify(Some("src/m.rs"), Some("shop_macros::m")).in_app);
    }

    #[test]
    fn reads_code_context() {
        let cache = SourceCache::new();
        let file = file!();
        let context = cache.context(file, 3, 2, 2, 500).expect("this file is readable from the crate root");
        assert_eq!(context["start_line"], 1);
        assert_eq!(context["lines"].as_array().unwrap().len(), 5);
        assert!(cache.context("/no/such/file.rs", 1, 2, 2, 500).is_none());
    }
}
