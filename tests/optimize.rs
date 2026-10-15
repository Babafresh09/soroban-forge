//! End-to-end coverage for `soroban-forge optimize` against a stub `stellar`.
//!
//! The stub is a tiny shell script placed first on `PATH`: `--version`
//! reports a supported release, and `contract optimize --wasm <file>` writes
//! `<file>.optimized.wasm` holding the first `FAKE_STELLAR_KEEP` bytes of the
//! input (default 6).  With `FAKE_STELLAR_FAIL` set it prints that message to
//! stderr and exits non-zero instead.  Nothing here needs a real toolchain.
//!
//! Covers issues #412 (`--all`), #417 (CLI stderr in failure messages) and
//! #418 (`--compare-to`).

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const STUB: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "stellar 22.0.0"
  exit 0
fi
if [ -n "$FAKE_STELLAR_FAIL" ]; then
  echo "stub progress output"
  echo "error: $FAKE_STELLAR_FAIL" >&2
  exit 3
fi
wasm="$4"
out="${wasm%.wasm}.optimized.wasm"
head -c "${FAKE_STELLAR_KEEP:-6}" "$wasm" > "$out"
"#;

/// 20-byte stand-in for a release wasm.
const WASM: &[u8] = b"\0asm\x01\0\0\0padding-bytes";

struct Fixture {
    _tmp: tempfile::TempDir,
    bin: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let stub = bin.join("stellar");
        std::fs::write(&stub, STUB).unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let root = tmp.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        Fixture {
            _tmp: tmp,
            bin,
            root,
        }
    }

    fn write(&self, rel: &str, contents: &[u8]) -> PathBuf {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn forge(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_soroban-forge"));
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
        );
        cmd.env("PATH", path)
            .env_remove("FAKE_STELLAR_FAIL")
            .env_remove("FAKE_STELLAR_KEEP")
            .current_dir(&self.root);
        cmd
    }
}

fn contract_manifest(name: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [lib]\ncrate-type = [\"lib\", \"cdylib\"]\n"
    )
}

/// A single-contract project with a release build in place.
fn single_project() -> Fixture {
    let fixture = Fixture::new();
    fixture.write("Cargo.toml", contract_manifest("demo").as_bytes());
    fixture.write("target/wasm32v1-none/release/demo.wasm", WASM);
    fixture
}

fn stdout_json(output: &Output) -> serde_json::Value {
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {output:?}"))
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// ---- #417 ----

#[test]
fn failure_message_includes_the_clis_stderr() {
    let fixture = single_project();
    let output = fixture
        .forge()
        .env("FAKE_STELLAR_FAIL", "wasm parse error at offset 8")
        .args(["optimize"])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("stellar contract optimize failed"), "{stderr}");
    assert!(stderr.contains("wasm parse error at offset 8"), "{stderr}");
    // Without --verbose the CLI's own output is captured, not echoed.
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("stub progress output"),
        "{output:?}"
    );
}

#[test]
fn verbose_streams_cli_output_live_and_still_captures_stderr() {
    let fixture = single_project();
    let output = fixture
        .forge()
        .env("FAKE_STELLAR_FAIL", "boom from the stub")
        .args(["-v", "optimize"])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("stub progress output"),
        "stdout should be streamed under --verbose: {output:?}"
    );
    let stderr = stderr(&output);
    // Once streamed live, once inside the final error message.
    assert!(
        stderr.matches("boom from the stub").count() >= 2,
        "expected live + captured stderr: {stderr}"
    );
    assert!(stderr.contains("stellar contract optimize failed"), "{stderr}");
}

// ---- #412 ----

fn workspace_project() -> Fixture {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        b"[workspace]\nresolver = \"2\"\nmembers = [\"contracts/*\"]\n",
    );
    fixture.write("contracts/token/Cargo.toml", contract_manifest("my-token").as_bytes());
    fixture.write("contracts/pool/Cargo.toml", contract_manifest("pool").as_bytes());
    // Cargo workspaces share one target/ at the root.
    fixture.write("target/wasm32v1-none/release/my_token.wasm", WASM);
    fixture.write("target/wasm32v1-none/release/pool.wasm", WASM);
    fixture
}

#[test]
fn all_optimizes_every_contract_in_one_invocation() {
    let fixture = workspace_project();
    let output = fixture
        .forge()
        .args(["--json", "optimize", "--all"])
        .output()
        .unwrap();

    let json = stdout_json(&output);
    let reports = json.as_array().expect("--all --json emits an array");
    assert_eq!(reports.len(), 2, "{json}");

    let contracts: Vec<_> = reports.iter().map(|r| r["contract"].as_str().unwrap()).collect();
    assert_eq!(contracts, vec!["pool", "my_token"]);
    for report in reports {
        assert_eq!(report["before_bytes"], WASM.len() as u64, "{report}");
        assert_eq!(report["after_bytes"], 6, "{report}");
        assert_eq!(report["status"], "optimized", "{report}");
    }

    for name in ["my_token", "pool"] {
        let optimized = fixture
            .root
            .join(format!("target/wasm32v1-none/release/{name}.optimized.wasm"));
        assert!(optimized.is_file(), "missing {}", optimized.display());
    }
}

#[test]
fn all_human_output_has_one_block_per_contract() {
    let fixture = workspace_project();
    let output = fixture.forge().args(["optimize", "--all"]).output().unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[pool] optimized"), "{stdout}");
    assert!(stdout.contains("[my_token] optimized"), "{stdout}");
    assert!(stdout.contains("2 contract(s): 2 optimized, 0 failed"), "{stdout}");
}

#[test]
fn all_reports_a_missing_build_and_fails_after_trying_every_contract() {
    let fixture = workspace_project();
    std::fs::remove_file(fixture.root.join("target/wasm32v1-none/release/pool.wasm")).unwrap();

    let output = fixture
        .forge()
        .args(["--json", "optimize", "--all"])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let reports = json.as_array().unwrap();
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0]["contract"], "pool");
    assert!(reports[0]["error"].as_str().unwrap().contains("no release build"));
    assert_eq!(reports[1]["contract"], "my_token");
    assert_eq!(reports[1]["after_bytes"], 6);
    assert!(stderr(&output).contains("1 of 2 contract(s) failed: pool"), "{output:?}");
}

// ---- #418 ----

fn save_report(fixture: &Fixture, args: &[&str], keep: &str, file: &str) -> PathBuf {
    let output = fixture
        .forge()
        .env("FAKE_STELLAR_KEEP", keep)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    // Equivalent of `optimize --json > report.json`.
    fixture.write(file, &output.stdout)
}

fn assert_delta(report: &serde_json::Value, baseline_after: u64, delta: i64) {
    let comparison = &report["comparison"];
    assert_eq!(comparison["baseline_after_bytes"], baseline_after, "{report}");
    assert_eq!(comparison["delta_bytes"], delta, "{report}");
    let expected = delta as f64 / baseline_after as f64 * 100.0;
    let percent = comparison["delta_percent"].as_f64().unwrap();
    assert!((percent - expected).abs() < 1e-9, "{percent} vs {expected}");
}

#[test]
fn compare_to_reports_the_delta_against_a_saved_report() {
    let fixture = single_project();
    save_report(&fixture, &["--json", "optimize"], "10", "baseline.json");

    let output = fixture
        .forge()
        .env("FAKE_STELLAR_KEEP", "8")
        .args(["--json", "optimize", "--compare-to", "baseline.json"])
        .output()
        .unwrap();
    let report = stdout_json(&output);
    assert_eq!(report["after_bytes"], 8);
    assert_delta(&report, 10, -2);

    let text = fixture
        .forge()
        .env("FAKE_STELLAR_KEEP", "12")
        .args(["optimize", "--compare-to", "baseline.json"])
        .output()
        .unwrap();
    assert!(text.status.success(), "{text:?}");
    let stdout = String::from_utf8_lossy(&text.stdout);
    assert!(stdout.contains("vs baseline 10 bytes: +2 bytes (+20.0%, larger)"), "{stdout}");
}

#[test]
fn compare_to_works_per_contract_with_an_all_baseline() {
    let fixture = workspace_project();
    save_report(&fixture, &["--json", "optimize", "--all"], "10", "all.json");

    let output = fixture
        .forge()
        .env("FAKE_STELLAR_KEEP", "5")
        .args(["--json", "optimize", "--all", "--compare-to", "all.json"])
        .output()
        .unwrap();
    let json = stdout_json(&output);
    for report in json.as_array().unwrap() {
        assert_delta(report, 10, -5);
    }
}

#[test]
fn compare_to_rejects_a_file_that_is_not_a_report() {
    let fixture = single_project();
    fixture.write("bogus.json", b"{\"hello\": \"world\"}");

    let output = fixture
        .forge()
        .args(["optimize", "--compare-to", "bogus.json"])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("bogus.json"), "{stderr}");
    // The bad baseline is caught before the wasm is touched.
    assert!(!optimized_exists(&fixture.root, "demo"), "{stderr}");
}

fn optimized_exists(root: &Path, name: &str) -> bool {
    root.join(format!("target/wasm32v1-none/release/{name}.optimized.wasm"))
        .is_file()
}
