//! Repo gate tasks — vendored per Rust repo as `xtask/`.
//!
//! Enable with `.cargo/config.toml`:
//!   [alias]
//!   xtask = "run --manifest-path xtask/Cargo.toml --"
//!
//! Tasks: fmt, fmt-fix, clippy, check, test, doc, gate (all of the above).

use std::process::Command;

fn cargo(args: &[&str]) -> bool {
    println!("$ cargo {}", args.join(" "));
    Command::new("cargo")
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// The `handl-term` feature gates `impl IrModule for handl_calculus::Term`;
// run the Rust gates with it enabled so the feature code is actually
// linted/tested/documented (the crate stays dep-free by default — the
// feature only adds the optional `handl-calculus` dep).
const FEATURES: &[&str] = &["--features", "handl-term"];

fn gate() -> bool {
    for (name, args) in [
        ("fmt", &[
            "fmt",
            "--all",
            "--",
            "--check",
        ][..]),
        (
            "clippy",
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                FEATURES[0],
                FEATURES[1],
                "--",
                "-D",
                "warnings",
            ][..],
        ),
        ("test", &["test", "--workspace", FEATURES[0], FEATURES[1]][..]),
        (
            "doc",
            &["doc", "--workspace", "--no-deps", FEATURES[0], FEATURES[1]][..],
        ),
    ] {
        if !cargo(args) {
            eprintln!("xtask gate: {name} failed");
            return false;
        }
    }
    true
}

fn main() {
    let task = std::env::args().nth(1).unwrap_or_else(|| "gate".into());
    let ok = match task.as_str() {
        "fmt" => cargo(&["fmt", "--all", "--", "--check"]),
        "fmt-fix" => cargo(&["fmt", "--all"]),
        "clippy" => cargo(&[
            "clippy",
            "--workspace",
            "--all-targets",
            FEATURES[0],
            FEATURES[1],
            "--",
            "-D",
            "warnings",
        ]),
        "check" => cargo(&["check", "--workspace", "--all-targets"]),
        "test" => cargo(&["test", "--workspace", FEATURES[0], FEATURES[1]]),
        "doc" => cargo(&["doc", "--workspace", "--no-deps", FEATURES[0], FEATURES[1]]),
        "gate" => gate(),
        other => {
            eprintln!("usage: cargo xtask [fmt|fmt-fix|clippy|check|test|doc|gate] (got {other:?})");
            false
        }
    };
    std::process::exit(if ok { 0 } else { 1 });
}
