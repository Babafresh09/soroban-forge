//! Integration coverage for `soroban-forge verify`.
//!
//! These exercise only the environment-independent half — argument
//! validation, the local-build lookup and output shape — so they never need
//! a live network or the `stellar` CLI. The command resolves the local wasm
//! before it fetches anything, which is what makes that possible; the
//! hashing and comparison logic is unit-tested in `crates/verify`.

use std::process::Command;

const CONTRACT_ID: &str = "CA3D5KRYM6CB7OWQ6TWYRR3Z4T7GNZLKERYNZGGA5SOAOPIFY6YQGAXE";

#[test]
fn verify_is_a_registered_subcommand() {
    let output = Command::new(env!("CARGO_BIN_EXE_soroban-forge"))
        .arg("--list")
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("verify"), "{stdout}");
}

#[test]
fn malformed_contract_id_is_a_user_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_soroban-forge"))
        .args(["verify", "not-a-contract-id"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("not a valid contract ID"), "{stderr}");
}

#[test]
fn missing_local_build_points_at_stellar_contract_build() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_soroban-forge"))
        .args([
            "verify",
            CONTRACT_ID,
            "--path",
            temp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("stellar contract build"), "{stderr}");
}

#[test]
fn json_mode_reports_verify_failures_as_json() {
    let output = Command::new(env!("CARGO_BIN_EXE_soroban-forge"))
        .args(["--json", "verify", "not-a-contract-id"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");

    let stderr = String::from_utf8(output.stderr).unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(&stderr).expect("expected stderr to be valid JSON");
    assert_eq!(parsed["exit_code"], 1);
    assert!(parsed["error"]
        .as_str()
        .unwrap()
        .contains("not a valid contract ID"));
}

#[test]
fn verify_requires_a_contract_id() {
    let output = Command::new(env!("CARGO_BIN_EXE_soroban-forge"))
        .arg("verify")
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("CONTRACT_ID"), "{stderr}");
}
