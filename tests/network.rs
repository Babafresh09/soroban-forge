//! End-to-end CLI tests for `soroban-forge network`.
//!
//! These tests cover issues #404 (network remove), #405 (URL validation on
//! network add), and #406 (network show / current).
//!
//! All tests that mutate the network store use a temporary config directory via
//! the `XDG_CONFIG_HOME` environment variable so they never touch the real
//! user config.

use std::process::Command;

fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_soroban-forge"))
}

// ---------------------------------------------------------------------------
// #403 — spec --out (CLI-level checks)
// (unit-level tests live in tests/spec.rs; these are command-level)
// ---------------------------------------------------------------------------

// #405 — network add URL validation
// ---------------------------------------------------------------------------

/// `network add` with a missing scheme must fail with a clear error message.
#[test]
fn network_add_rejects_url_without_scheme() {
    let tmp = tempfile::tempdir().unwrap();
    let output = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args([
            "network",
            "add",
            "mynet",
            "--rpc-url",
            "soroban-testnet.stellar.org",
            "--passphrase",
            "My Network",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("scheme") || stderr.contains("well-formed URL"),
        "expected scheme-related error: {stderr}"
    );
}

/// `network add` with an empty host must fail with a clear error message.
#[test]
fn network_add_rejects_url_with_empty_host() {
    let tmp = tempfile::tempdir().unwrap();
    let output = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args([
            "network",
            "add",
            "mynet",
            "--rpc-url",
            "https://",
            "--passphrase",
            "My Network",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("host") || stderr.contains("well-formed URL"),
        "expected host-related error: {stderr}"
    );
}

/// `network add` with a valid URL succeeds.
#[test]
fn network_add_accepts_valid_url() {
    let tmp = tempfile::tempdir().unwrap();
    let output = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args([
            "network",
            "add",
            "mynet",
            "--rpc-url",
            "https://my.rpc.example.com",
            "--passphrase",
            "My Network",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected success with valid URL: {output:?}"
    );
}

// ---------------------------------------------------------------------------
// #404 — network remove
// ---------------------------------------------------------------------------

/// `network remove` deletes a previously added network.
#[test]
fn network_remove_deletes_a_configured_network() {
    let tmp = tempfile::tempdir().unwrap();

    // Add the network first
    let add = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args([
            "network",
            "add",
            "mynet",
            "--rpc-url",
            "https://my.rpc.example.com",
            "--passphrase",
            "My Network",
        ])
        .output()
        .unwrap();
    assert!(add.status.success(), "add failed: {add:?}");

    // Now remove it
    let remove = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["network", "remove", "mynet"])
        .output()
        .unwrap();
    assert!(remove.status.success(), "remove failed: {remove:?}");

    let stdout = String::from_utf8(remove.stdout).unwrap();
    assert!(stdout.contains("mynet"), "expected mention of removed name: {stdout}");
}

/// `network remove` on an unknown network fails with a clear error.
#[test]
fn network_remove_unknown_network_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let output = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["network", "remove", "does-not-exist"])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("does-not-exist") || stderr.contains("not found"),
        "expected not-found error: {stderr}"
    );
}

/// `network remove` cannot remove a built-in preset.
#[test]
fn network_remove_builtin_fails_with_clear_message() {
    let tmp = tempfile::tempdir().unwrap();
    let output = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["network", "remove", "testnet"])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("built-in") || stderr.contains("cannot be removed"),
        "expected built-in rejection message: {stderr}"
    );
}

/// `network rm` (alias) works the same as `network remove`.
#[test]
fn network_rm_alias_works() {
    let tmp = tempfile::tempdir().unwrap();

    // Add first
    forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args([
            "network",
            "add",
            "rmtest",
            "--rpc-url",
            "https://rmtest.example.com",
            "--passphrase",
            "Rm Test",
        ])
        .output()
        .unwrap();

    // Remove using alias
    let output = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["network", "rm", "rmtest"])
        .output()
        .unwrap();

    assert!(output.status.success(), "rm alias failed: {output:?}");
}

/// Removing the current default network clears the default, and the note is shown.
#[test]
fn network_remove_default_clears_default_and_notifies() {
    let tmp = tempfile::tempdir().unwrap();

    // Add and set as default (first add auto-sets default)
    forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args([
            "network",
            "add",
            "defnet",
            "--rpc-url",
            "https://def.rpc.example.com",
            "--passphrase",
            "Default Net",
        ])
        .output()
        .unwrap();

    let remove = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["network", "remove", "defnet"])
        .output()
        .unwrap();

    assert!(remove.status.success(), "{remove:?}");
    let stdout = String::from_utf8(remove.stdout).unwrap();
    // Should hint that no default is set after removal
    assert!(
        stdout.contains("no default") || stdout.contains("network use"),
        "expected hint about no default: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// #406 — network show / current
// ---------------------------------------------------------------------------

/// `network show` prints the active network's name, RPC URL and passphrase.
#[test]
fn network_show_prints_active_network_details() {
    let tmp = tempfile::tempdir().unwrap();

    // Add and auto-set as default
    forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args([
            "network",
            "add",
            "shownet",
            "--rpc-url",
            "https://show.rpc.example.com",
            "--passphrase",
            "Show Network Pass",
        ])
        .output()
        .unwrap();

    let show = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["network", "show"])
        .output()
        .unwrap();

    assert!(show.status.success(), "{show:?}");
    let stdout = String::from_utf8(show.stdout).unwrap();
    assert!(stdout.contains("shownet"), "should show name: {stdout}");
    assert!(stdout.contains("https://show.rpc.example.com"), "should show rpc url: {stdout}");
    assert!(stdout.contains("Show Network Pass"), "should show passphrase: {stdout}");
}

/// `network current` (alias of `network show`) works identically.
#[test]
fn network_current_alias_works_like_show() {
    let tmp = tempfile::tempdir().unwrap();

    forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args([
            "network",
            "add",
            "curnet",
            "--rpc-url",
            "https://cur.rpc.example.com",
            "--passphrase",
            "Current Net",
        ])
        .output()
        .unwrap();

    let current = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["network", "current"])
        .output()
        .unwrap();

    assert!(current.status.success(), "{current:?}");
    let stdout = String::from_utf8(current.stdout).unwrap();
    assert!(stdout.contains("curnet"), "alias should show name: {stdout}");
    assert!(stdout.contains("https://cur.rpc.example.com"), "alias should show rpc url: {stdout}");
}

/// `network show` errors clearly when no default is set (exit code 1).
#[test]
fn network_show_errors_when_no_default_set() {
    let tmp = tempfile::tempdir().unwrap();
    let output = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["network", "show"])
        .output()
        .unwrap();

    assert!(!output.status.success(), "expected failure when no default: {output:?}");
    assert_eq!(
        output.status.code(),
        Some(1),
        "exit code must be 1 (user error): {output:?}"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("no default") || stderr.contains("network use"),
        "expected helpful error: {stderr}"
    );
}

/// `network show --json` returns name, rpc_url, and network_passphrase as JSON.
#[test]
fn network_show_json_output_has_required_fields() {
    let tmp = tempfile::tempdir().unwrap();

    forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args([
            "network",
            "add",
            "jsonnet",
            "--rpc-url",
            "https://json.rpc.example.com",
            "--passphrase",
            "Json Network",
        ])
        .output()
        .unwrap();

    let show = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["--json", "network", "show"])
        .output()
        .unwrap();

    assert!(show.status.success(), "{show:?}");
    let stdout = String::from_utf8(show.stdout).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("output must be valid JSON: {stdout}");
    assert_eq!(json["name"], "jsonnet");
    assert_eq!(json["rpc_url"], "https://json.rpc.example.com");
    assert_eq!(json["network_passphrase"], "Json Network");
}

/// `network show --json` when no default is set emits a JSON error (not plain text).
#[test]
fn network_show_json_no_default_returns_json_error() {
    let tmp = tempfile::tempdir().unwrap();
    let output = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["--json", "network", "show"])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    // With --json, errors are emitted as JSON to stderr
    let parsed: serde_json::Value =
        serde_json::from_str(&stderr).expect("stderr must be JSON with --json flag: {stderr}");
    assert_eq!(parsed["exit_code"], 1);
}

// ---------------------------------------------------------------------------
// #411 — network list --check
// ---------------------------------------------------------------------------

/// Seed the store under `config_home` with one reachable and one unreachable
/// mock endpoint, and point every built-in preset at the unreachable one so
/// the probe never leaves the machine.  Returns the listener keeping the
/// reachable endpoint alive.
#[cfg(target_os = "linux")]
fn seed_mock_networks(config_home: &std::path::Path) -> std::net::TcpListener {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let up = format!(
        "http://127.0.0.1:{}/soroban/rpc",
        listener.local_addr().unwrap().port()
    );
    let down_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let down = format!("http://127.0.0.1:{down_port}/soroban/rpc");

    let mut networks = serde_json::Map::new();
    let entry = |url: &str| serde_json::json!({ "rpc_url": url, "network_passphrase": "Mock Network" });
    networks.insert("mock-up".into(), entry(&up));
    networks.insert("mock-down".into(), entry(&down));
    for builtin in ["testnet", "futurenet", "mainnet", "localnet"] {
        networks.insert(builtin.into(), entry(&down));
    }
    let store = serde_json::json!({ "networks": networks, "default": "mock-up" });

    let dir = config_home.join("soroban-forge");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("networks.json"), store.to_string()).unwrap();
    listener
}

#[cfg(target_os = "linux")]
#[test]
fn network_list_check_annotates_reachable_and_unreachable_endpoints() {
    let tmp = tempfile::tempdir().unwrap();
    let _listener = seed_mock_networks(tmp.path());

    let output = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["network", "list", "--check", "--timeout", "2"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let row = |name: &str| {
        stdout
            .lines()
            .find(|l| l.contains(name))
            .unwrap_or_else(|| panic!("no row for {name}: {stdout}"))
    };
    assert!(row("mock-up").contains("[reachable, "), "{stdout}");
    assert!(row("mock-down").contains("[unreachable: "), "{stdout}");
    assert!(row("testnet").contains("[unreachable: "), "{stdout}");
    assert!(stdout.contains("1 of 6 network(s) reachable"), "{stdout}");
}

#[cfg(target_os = "linux")]
#[test]
fn network_list_check_json_carries_reachability_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let _listener = seed_mock_networks(tmp.path());

    let output = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["--json", "network", "list", "--check", "--timeout", "2"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["networks"]["mock-up"]["reachable"], true, "{json}");
    assert!(json["networks"]["mock-up"]["latency_ms"].is_u64(), "{json}");
    assert_eq!(json["networks"]["mock-down"]["reachable"], false, "{json}");
    assert!(json["networks"]["mock-down"]["error"].is_string(), "{json}");
}

#[cfg(target_os = "linux")]
#[test]
fn network_list_without_check_is_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let _listener = seed_mock_networks(tmp.path());

    let text = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["network", "list"])
        .output()
        .unwrap();
    assert!(text.status.success(), "{text:?}");
    let stdout = String::from_utf8(text.stdout).unwrap();
    assert!(!stdout.contains("reachable"), "{stdout}");
    assert!(stdout.contains("* mock-up"), "{stdout}");

    let json = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["--json", "network", "list"])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert!(json["networks"]["mock-up"].get("reachable").is_none(), "{json}");
}

#[test]
fn network_list_check_refuses_offline_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let output = forge()
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["--offline", "network", "list", "--check"])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("offline"), "{stderr}");
}
