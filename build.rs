//! Records the compiler version for the runtime label ("Rust 1.82").

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=RUSTC");
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let version = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.split_whitespace().nth(1).map(str::to_string));
    if let Some(version) = version {
        println!("cargo:rustc-env=GOODSORCERY_RUSTC_VERSION={version}");
    }
}
