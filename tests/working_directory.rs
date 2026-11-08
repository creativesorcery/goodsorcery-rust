//! Backtraces when the process runs from a parent of the build root (`/`
//! is the default in Docker images without `WORKDIR` and under systemd).
//! Its own test binary: it changes the process-wide working directory.

mod support;

use support::{FakeServer, config, serial};

#[test]
fn frames_stay_app_relative_when_started_in_the_root_directory() {
    let _lock = serial();
    std::env::set_current_dir("/").unwrap();
    let server = FakeServer::start();
    let guard = goodsorcery::init(config(&server));
    goodsorcery::capture_error(&std::io::Error::other("disk full"));
    drop(guard);

    let event = &server.events()[0];
    let frames = event["exception"]["backtrace"].as_array().unwrap();
    let top = &frames[0];
    assert_eq!(top["file"], "tests/working_directory.rs", "{top}");
    assert_eq!(top["in_app"], true);
    let home = std::env::var("HOME").unwrap_or_default();
    for frame in frames {
        let file = frame["file"].as_str().unwrap_or("");
        assert!(!file.starts_with("Users/") && !file.starts_with("home/"), "{frame}");
        assert!(home.len() <= 1 || !file.contains(home.trim_start_matches('/')), "{frame}");
    }
}
