//! The client's log lines must never panic the app when stderr is gone (a
//! pipe whose reader died gives EPIPE, and `eprintln!` panics on that).
//! The test re-runs itself as a child process whose stderr is a closed pipe.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

const CHILD: &str = "GOODSORCERY_TEST_BROKEN_STDERR_CHILD";

#[test]
fn logging_to_a_broken_stderr_never_panics() {
    if std::env::var_os(CHILD).is_some() {
        return child();
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "logging_to_a_broken_stderr_never_panics", "--test-threads=1", "--nocapture"])
        .env(CHILD, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Close the read end before the child writes anything.
    drop(child.stderr.take());
    let mut stdout = String::new();
    child.stdout.take().unwrap().read_to_string(&mut stdout).unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "the child failed: {status}\n{stdout}");
    assert!(stdout.contains("1 passed"), "{stdout}");
}

fn child() {
    // Give the parent time to close its end of the pipe.
    std::thread::sleep(Duration::from_millis(300));
    let guard = goodsorcery::init(
        goodsorcery::Config::new()
            .api_key("gs_test_0123456789abcdef")
            // Nothing listens here: delivery fails and is logged.
            .endpoint("http://127.0.0.1:9")
            .max_queue_size(1)
            .max_retries(0)
            .flush_interval(Duration::from_secs(60))
            .shutdown_timeout(Duration::from_millis(500))
            .debug(true),
    );
    assert!(guard.is_enabled());
    for n in 0..5 {
        // The queue holds one event: the rest are dropped with a warning.
        goodsorcery::capture_message(format!("event {n}"), goodsorcery::Level::Error);
    }
    drop(guard);
}
