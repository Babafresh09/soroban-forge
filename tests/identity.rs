//! End-to-end CLI tests for `soroban-forge identity import` (#421) and
//! `identity show --reveal-secret` (#422).
//!
//! The identity store is redirected with `XDG_CONFIG_HOME`, which the `dirs`
//! crate honours on Linux only, so these tests are Linux-gated to never touch
//! a developer's real identities. The logic itself is unit-tested in
//! `crates/identity` on every platform.

#![cfg(target_os = "linux")]

use std::process::{Command, Output};

fn forge(config_home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_soroban-forge"))
        .env("XDG_CONFIG_HOME", config_home)
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A fresh, valid secret key: generate one under a scratch name and read it
/// back. Avoids hard-coding key material in the repository.
fn fresh_keypair(config_home: &std::path::Path) -> (String, String) {
    let out = forge(config_home, &["--json", "identity", "generate", "scratch", "--show-secret", "--force"]);
    assert!(out.status.success(), "{out:?}");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    (
        json["public_key"].as_str().unwrap().to_string(),
        json["secret_key"].as_str().unwrap().to_string(),
    )
}

fn imported_public_key(config_home: &std::path::Path) -> String {
    let out = forge(config_home, &["--json", "identity", "show", "ci"]);
    assert!(out.status.success(), "{out:?}");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    json["public_key"].as_str().unwrap().to_string()
}

#[test]
fn import_stores_a_valid_secret_and_derives_its_public_key() {
    let tmp = tempfile::tempdir().unwrap();
    let (expected_public, secret) = fresh_keypair(tmp.path());
    let out = forge(tmp.path(), &["identity", "import", "ci", "--secret", &secret]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(text.contains("imported identity `ci`"), "{text}");
    assert!(!text.contains(&secret), "import must not echo the secret: {text}");

    assert_eq!(imported_public_key(tmp.path()), expected_public);

    // A second import under the same name needs --force, like generate.
    let again = forge(tmp.path(), &["identity", "import", "ci", "--secret", &secret]);
    assert!(!again.status.success(), "{again:?}");
    let forced = forge(tmp.path(), &["identity", "import", "ci", "--secret", &secret, "--force"]);
    assert!(forced.status.success(), "{forced:?}");
}

#[test]
fn import_rejects_a_malformed_secret_and_stores_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let out = forge(tmp.path(), &["identity", "import", "ci", "--secret", "SNOTAREALKEY"]);
    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not a valid Stellar secret key"), "{stderr}");
    assert!(!stderr.contains("SNOTAREALKEY"), "error must not echo the input: {stderr}");

    let list = forge(tmp.path(), &["identity", "list"]);
    assert!(stdout(&list).contains("no identities stored"), "{list:?}");
}

#[test]
fn show_prints_the_secret_only_with_reveal_secret() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, secret) = fresh_keypair(tmp.path());
    assert!(forge(tmp.path(), &["identity", "import", "ci", "--secret", &secret])
        .status
        .success());

    let hidden = forge(tmp.path(), &["identity", "show", "ci"]);
    assert!(hidden.status.success(), "{hidden:?}");
    assert!(!stdout(&hidden).contains(&secret), "secret printed without the flag");

    let hidden_json = forge(tmp.path(), &["--json", "identity", "show", "ci"]);
    assert!(!stdout(&hidden_json).contains(&secret), "secret in JSON without the flag");

    let list = forge(tmp.path(), &["identity", "list"]);
    assert!(!stdout(&list).contains(&secret), "list must never print secrets by default");

    let revealed = forge(tmp.path(), &["identity", "show", "ci", "--reveal-secret"]);
    assert!(revealed.status.success(), "{revealed:?}");
    assert!(stdout(&revealed).contains(&secret), "{revealed:?}");

    let revealed_json = forge(tmp.path(), &["--json", "identity", "show", "ci", "--reveal-secret"]);
    let json: serde_json::Value = serde_json::from_slice(&revealed_json.stdout).unwrap();
    assert_eq!(json["secret_key"], secret.as_str());
}

#[test]
fn show_reports_unknown_identities() {
    let tmp = tempfile::tempdir().unwrap();
    let out = forge(tmp.path(), &["identity", "show", "nobody"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
}
