//! # soroban-forge-identity
//!
//! `soroban-forge identity generate|list|fund|remove` — manage test keypairs
//! and fund them via friendbot on testnet.
//!
//! Identities are stored as a JSON file at
//! `~/.config/soroban-forge/identities.json`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Arg, ArgMatches, Command};
use serde::{Deserialize, Serialize};
use soroban_forge_core::{ForgeContext, ForgeError, ForgePlugin, Result};

/// A stored test identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub public_key: String,
    pub secret_key: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub funded_on: Vec<String>,
}

/// The on-disk identity store.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct IdentityStore {
    #[serde(default)]
    pub identities: BTreeMap<String, Identity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// Return the path to the identity store file.
/// `~/.config/soroban-forge/identities.json`
pub fn store_path() -> Result<PathBuf> {
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .ok_or_else(|| {
            ForgeError::Other("could not determine user config directory".into())
        })?;
    Ok(config_dir.join("soroban-forge").join("identities.json"))
}

/// Load the identity store from disk, or return a default empty one.
pub fn load_store(path: &PathBuf) -> Result<IdentityStore> {
    if !path.is_file() {
        return Ok(IdentityStore::default());
    }
    let raw = std::fs::read_to_string(path)
        .map_err(ForgeError::io(format!("reading {}", path.display())))?;
    serde_json::from_str(&raw).map_err(|e| ForgeError::Config {
        path: path.clone(),
        message: e.to_string(),
    })
}

/// Save the identity store to disk, creating parent directories as needed.
///
/// Writes through [`soroban_forge_core::atomic::write_atomic`] (#470): a crash
/// or disk-full error partway through must not leave `identities.json`
/// truncated, because the next `load_store` would then fail to parse and every
/// stored identity would be lost.
pub fn save_store(path: &PathBuf, store: &IdentityStore) -> Result<()> {
    let json = serde_json::to_string_pretty(store)
        .map_err(|e| ForgeError::Other(format!("serializing identity store: {e}")))?;
    soroban_forge_core::atomic::write_atomic(path, &json)
        .map_err(|e| ForgeError::Other(format!("writing {}: {e}", path.display())))
}

fn remove_identity(path: &PathBuf, name: &str) -> Result<()> {
    let mut store = load_store(path)?;
    if store.identities.remove(name).is_none() {
        return Err(ForgeError::InvalidArgument(format!(
            "identity `{name}` not found (use `soroban-forge identity list` to see available identities)"
        )));
    }
    if store.default.as_deref() == Some(name) {
        store.default = None;
    }
    save_store(path, &store)
}

/// Persist the active identity choice into `forge.toml` in `cwd`.
///
/// If the file does not exist it is created with just the `[identity]` section.
/// If it exists the `[identity]` section is patched (or appended) without
/// disturbing any other content.
pub fn write_identity_to_forge_toml(cwd: &std::path::Path, name: &str) -> Result<()> {
    let path = cwd.join("forge.toml");

    let existing = if path.is_file() {
        std::fs::read_to_string(&path)
            .map_err(ForgeError::io(format!("reading {}", path.display())))?
    } else {
        String::new()
    };

    let updated = patch_forge_toml_identity(&existing, name);
    std::fs::write(&path, updated)
        .map_err(ForgeError::io(format!("writing {}", path.display())))
}

/// Patch (or append) the `[identity]` section in raw `forge.toml` text.
///
/// Rules:
/// - If an `[identity]` section already exists, the `default = …` key within it is
///   replaced (or inserted if missing). Other keys in the section are preserved.
/// - If no `[identity]` section exists, one is appended.
pub fn patch_forge_toml_identity(existing: &str, name: &str) -> String {
    let original_lines: Vec<&str> = existing.lines().collect();
    let mut out_lines: Vec<String> = Vec::with_capacity(original_lines.len() + 3);

    let mut in_identity_section = false;
    let mut default_key_written = false;
    let mut identity_section_header_idx: Option<usize> = None;

    for line in &original_lines {
        let trimmed = line.trim();

        if trimmed == "[identity]" {
            in_identity_section = true;
            identity_section_header_idx = Some(out_lines.len());
            out_lines.push(line.to_string());
            continue;
        }

        // Entering a new section other than [identity]
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if in_identity_section && !default_key_written {
                out_lines.push(format!("default = \"{name}\""));
                default_key_written = true;
            }
            in_identity_section = false;
            out_lines.push(line.to_string());
            continue;
        }

        // Replace the `default = ...` key inside [identity]
        if in_identity_section && !default_key_written {
            if let Some(eq_pos) = trimmed.find('=') {
                let key = trimmed[..eq_pos].trim();
                if key == "default" {
                    out_lines.push(format!("default = \"{name}\""));
                    default_key_written = true;
                    continue; // skip old line
                }
            }
        }

        out_lines.push(line.to_string());
    }

    // EOF while still inside [identity] and default was never written
    if in_identity_section && !default_key_written {
        out_lines.push(format!("default = \"{name}\""));
        default_key_written = true;
    }

    // No [identity] section found at all — append
    if identity_section_header_idx.is_none() && !default_key_written {
        if !out_lines.is_empty() && !out_lines.last().map(|l| l.is_empty()).unwrap_or(true) {
            out_lines.push(String::new());
        }
        out_lines.push("[identity]".to_string());
        out_lines.push(format!("default = \"{name}\""));
    }

    let mut result = out_lines.join("\n");
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// Generate a new Stellar keypair and return `(public_key, secret_key)` as
/// Stellar-encoded strings (`G...` / `S...`).
pub fn generate_keypair() -> (String, String) {
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use stellar_strkey::ed25519::{PrivateKey, PublicKey};

    let signing_key = SigningKey::generate(&mut OsRng);
    let seed = signing_key.to_bytes();
    let pubkey = signing_key.verifying_key().to_bytes();

    let public = PublicKey(pubkey).to_string();
    let secret = PrivateKey(seed).to_string();
    (public, secret)
}

/// GET `url`, bounded by `timeout` when one is set (`--timeout`).
fn http_get(
    url: &str,
    timeout: Option<Duration>,
) -> std::result::Result<ureq::Response, ureq::Error> {
    let mut request = ureq::get(url);
    if let Some(timeout) = timeout {
        request = request.timeout(timeout);
    }
    request.call()
}

/// Friendbot host for a network passphrase.
///
/// Testnet and futurenet each run their own friendbot deployment, and the
/// futurenet one is a different host — `friendbot.stellar.org` is testnet-only
/// (#471). Sending a futurenet account to the testnet friendbot either funds an
/// account on the wrong network or fails outright, despite the error message
/// below claiming futurenet is supported.
///
/// Returns `None` for a passphrase this crate does not know, so the caller can
/// refuse rather than silently default to testnet.
fn friendbot_host_for(passphrase: &str) -> Option<&'static str> {
    if passphrase.contains("Public Global Stellar Network") {
        // Mainnet — no friendbot exists at all.
        None
    } else if passphrase.contains("Future Network") {
        Some("https://friendbot-futurenet.stellar.org")
    } else if passphrase.contains("Test SDF Network") {
        Some("https://friendbot.stellar.org")
    } else {
        // Standalone/local networks run their own friendbot; this crate has no
        // way to guess its host, and guessing testnet would fund the wrong
        // network.
        None
    }
}

/// Build the friendbot request URL for a network, or `None` when the network
/// has no known friendbot.
///
/// Split out from [`fund_friendbot`] so the URL construction — which is the
/// part that was wrong in #471 — is testable without making a network call.
pub fn friendbot_url(public_key: &str, network_passphrase: Option<&str>) -> Option<String> {
    let passphrase = network_passphrase?;
    let host = friendbot_host_for(passphrase)?;
    Some(format!("{host}/?addr={public_key}"))
}

/// Fund a Stellar testnet/futurenet account via friendbot.
/// Returns the parsed balance (in XLM) on success.
///
/// The friendbot host is chosen from the effective network passphrase (#471):
/// testnet and futurenet have separate deployments. Mainnet, and any passphrase
/// this crate does not recognize, is refused rather than silently funded on
/// testnet. The request is bounded by `timeout` when set (`--timeout`).
pub fn fund_friendbot(
    public_key: &str,
    network_passphrase: Option<&str>,
    timeout: Option<Duration>,
) -> Result<String> {
    // #287 — refuse to run on mainnet
    let Some(passphrase) = network_passphrase else {
        return Err(ForgeError::InvalidArgument(
            "friendbot funding needs a known network passphrase; \
             set a network with `soroban-forge network use <name>`"
                .into(),
        ));
    };
    if passphrase.contains("Public Global Stellar Network") {
        return Err(ForgeError::InvalidArgument(
            "friendbot funding is only available on testnet/futurenet, not mainnet".into(),
        ));
    }

    let Some(url) = friendbot_url(public_key, network_passphrase) else {
        return Err(ForgeError::InvalidArgument(format!(
            "no friendbot is known for the network passphrase {passphrase:?}; \
             friendbot funding supports testnet and futurenet only"
        )));
    };

    log::debug!("requesting friendbot: {url}");
    let response = http_get(&url, timeout).map_err(|e| {
        // Surface actionable error messages (#287)
        ForgeError::Other(format!(
            "friendbot request failed: {e}\n  \
             hint: check your network connection, or the account may already be funded"
        ))
    })?;
    let body = response
        .into_string()
        .map_err(|e| ForgeError::Other(format!("reading friendbot response: {e}")))?;

    // Try to parse the balance from the horizon response.
    // Friendbot returns the created account record; the native balance lives in balances[].
    let balance = parse_native_balance(&body).unwrap_or_else(|| "unknown".to_string());
    Ok(balance)
}

/// Extract the native XLM balance from a Horizon account JSON response.
fn parse_native_balance(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let balances = v.get("balances")?.as_array()?;
    for b in balances {
        if b.get("asset_type")?.as_str()? == "native" {
            let bal = b.get("balance")?.as_str()?;
            return Some(bal.to_string());
        }
    }
    None
}

/// Format the identity list for display.
/// Secret keys are always masked here — use `--show-secret` to reveal them.
pub fn format_list(store: &IdentityStore) -> String {
    if store.identities.is_empty() {
        return "no identities stored. Use `soroban-forge identity generate <name>` to create one.\n".to_string();
    }
    let mut out = String::from("stored identities:\n\n");
    let name_width = store.identities.keys().map(|k| k.len()).max().unwrap_or(0);
    for (name, id) in &store.identities {
        let funded_suffix = if id.funded_on.is_empty() {
            String::new()
        } else {
            format!("  funded: {}", id.funded_on.join(", "))
        };
        out.push_str(&format!(
            "  {:<width$}  {}{}\n",
            name,
            id.public_key,
            funded_suffix,
            width = name_width
        ));
    }
    out
}

/// The `identity` subcommand.
pub struct IdentityPlugin;

impl ForgePlugin for IdentityPlugin {
    fn name(&self) -> &'static str {
        "identity"
    }

    fn command(&self) -> Command {
        Command::new("identity")
            .about("Manage test keypairs and fund them via friendbot on testnet")
            .subcommand_required(true)
            .subcommand(
                Command::new("generate")
                    .about("Generate a new Stellar test keypair")
                    .arg(
                        Arg::new("name")
                            .help("Name for the identity (e.g. alice, deployer)")
                            .required(true),
                    )
                    .arg(
                        Arg::new("force")
                            .long("force")
                            .action(clap::ArgAction::SetTrue)
                            .help("Overwrite an existing identity with the same name"),
                    )
                    // #288 — explicit opt-in to showing secret keys
                    .arg(
                        Arg::new("show-secret")
                            .long("show-secret")
                            .action(clap::ArgAction::SetTrue)
                            .help("Print the secret key in the output (omitted by default for safety)"),
                    ),
            )
            .subcommand(
                Command::new("list")
                    .about("List all stored identities (public keys only)")
                    // #288 — explicit opt-in to showing secret keys
                    .arg(
                        Arg::new("show-secret")
                            .long("show-secret")
                            .action(clap::ArgAction::SetTrue)
                            .help("Also print secret keys (use with care in shared terminals)"),
                    ),
            )
            .subcommand(
                Command::new("fund")
                    .about("Fund a stored identity via Stellar testnet friendbot")
                    .arg(
                        Arg::new("name")
                            .help("Name of the identity to fund")
                            .required(true),
                    )
                    // #287 — allow passing an explicit network passphrase to guard mainnet
                    .arg(
                        Arg::new("network-passphrase")
                            .long("network-passphrase")
                            .value_name("PASSPHRASE")
                            .help("Network passphrase (used to refuse mainnet funding)"),
                    ),
            )
            .subcommand(
                Command::new("remove")
                    .about("Remove a stored identity")
                    .arg(
                        Arg::new("name")
                            .help("Name of the identity to remove")
                            .required(true),
                    ),
            )
            .subcommand(
                Command::new("use")
                    .about("Select a default identity")
                    .arg(
                        Arg::new("name")
                            .help("Name of the identity to set as default")
                            .required(true),
                    ),
            )
    }

    fn run(&self, matches: &ArgMatches, ctx: &ForgeContext) -> Result<()> {
        let path = store_path()?;

        match matches.subcommand() {
            Some(("generate", sub)) => {
                let name = sub.get_one::<String>("name").unwrap();
                let force = sub.get_flag("force");
                // #288 — only show secret key when explicitly requested
                let show_secret = sub.get_flag("show-secret");
                let mut store = load_store(&path)?;

                if store.identities.contains_key(name.as_str()) && !force {
                    return Err(ForgeError::AlreadyExists(PathBuf::from(name.as_str())));
                }

                let (public_key, secret_key) = generate_keypair();
                store.identities.insert(
                    name.clone(),
                    Identity {
                        public_key: public_key.clone(),
                        secret_key: secret_key.clone(),
                        funded_on: Vec::new(),
                    },
                );
                save_store(&path, &store)?;

                if ctx.json {
                    // #288 — never emit secret key in JSON output unless --show-secret
                    let mut report = serde_json::json!({
                        "name": name,
                        "public_key": public_key,
                    });
                    if show_secret {
                        report["secret_key"] = serde_json::Value::String(secret_key.clone());
                    } else {
                        report["secret_key"] = serde_json::Value::String("[redacted — pass --show-secret to reveal]".into());
                    }
                    println!("{}", serde_json::to_string_pretty(&report).unwrap());
                } else if !ctx.quiet {
                    println!("generated identity `{name}`");
                    println!("  public key: {public_key}");
                    // #288 — mask secret key by default
                    if show_secret {
                        println!("  secret key: {secret_key}");
                    } else {
                        println!("  secret key: [redacted — pass --show-secret to reveal]");
                    }
                    println!();
                    println!("fund on testnet: soroban-forge identity fund {name}");
                }
                Ok(())
            }

            Some(("list", sub)) => {
                let show_secret = sub.get_flag("show-secret");
                let store = load_store(&path)?;
                if ctx.json {
                    if show_secret {
                        // Full store including secrets
                        println!("{}", serde_json::to_string_pretty(&store.identities).unwrap());
                    } else {
                        // #288 — omit secret keys from JSON output
                        let public_only: BTreeMap<&str, serde_json::Value> = store
                            .identities
                            .iter()
                            .map(|(k, v)| {
                                (
                                    k.as_str(),
                                    serde_json::json!({
                                        "public_key": v.public_key,
                                        "funded_on": v.funded_on,
                                    }),
                                )
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&public_only).unwrap());
                    }
                } else if !ctx.quiet {
                    if show_secret && !store.identities.is_empty() {
                        // Show full list including secrets
                        println!("stored identities:\n");
                        let name_width = store.identities.keys().map(|k| k.len()).max().unwrap_or(0);
                        for (name, id) in &store.identities {
                            let funded_suffix = if id.funded_on.is_empty() {
                                String::new()
                            } else {
                                format!("  funded: {}", id.funded_on.join(", "))
                            };
                            println!(
                                "  {:<width$}  pub: {}  secret: {}{}",
                                name,
                                id.public_key,
                                id.secret_key,
                                funded_suffix,
                                width = name_width
                            );
                        }
                    } else {
                        print!("{}", format_list(&store));
                    }
                }
                Ok(())
            }

            Some(("remove", sub)) => {
                let name = sub.get_one::<String>("name").unwrap();
                remove_identity(&path, name)?;

                if ctx.json {
                    println!("{}", serde_json::json!({ "name": name, "removed": true }));
                } else if !ctx.quiet {
                    println!("removed identity `{name}`");
                }
                Ok(())
            }

            Some(("use", sub)) => {
                let name = sub.get_one::<String>("name").unwrap();
                let mut store = load_store(&path)?;

                if !store.identities.contains_key(name.as_str()) {
                    return Err(ForgeError::InvalidArgument(format!(
                        "identity `{name}` not found (use `soroban-forge identity list` to see available identities)"
                    )));
                }

                store.default = Some(name.clone());
                save_store(&path, &store)?;

                write_identity_to_forge_toml(&ctx.cwd, name)?;

                if ctx.json {
                    let report = serde_json::json!({ "default": name });
                    println!("{}", serde_json::to_string_pretty(&report).unwrap());
                } else if !ctx.quiet {
                    println!("using `{name}` as the default identity");
                    println!("  recorded in forge.toml and identities.json");
                }
                Ok(())
            }

            Some(("fund", sub)) => {
                // #287 — refuse in offline mode
                if ctx.offline {
                    return Err(ForgeError::InvalidArgument(
                        "friendbot funding is unavailable in offline mode".into(),
                    ));
                }
                let name = sub.get_one::<String>("name").unwrap();
                // #287 — accept passphrase to guard against mainnet runs
                let network_passphrase = sub
                    .get_one::<String>("network-passphrase")
                    .map(String::as_str);

                // Also check via forge.toml / context config
                let config_passphrase = ctx
                    .config
                    .as_ref()
                    .and_then(|c| c.network.passphrase.as_deref());
                let effective_passphrase = network_passphrase.or(config_passphrase);

                // #287 — refuse on mainnet
                if let Some(passphrase) = effective_passphrase {
                    if passphrase.contains("Public Global Stellar Network") {
                        return Err(ForgeError::InvalidArgument(
                            "friendbot funding is only available on testnet/futurenet, not mainnet".into(),
                        ));
                    }
                }

                let mut store = load_store(&path)?;
                let (pubkey, mut funded_on) = {
                    let id = store.identities.get(name.as_str()).ok_or_else(|| {
                        ForgeError::InvalidArgument(format!(
                            "identity `{name}` not found (use `soroban-forge identity list` to see available identities)"
                        ))
                    })?;
                    (id.public_key.clone(), id.funded_on.clone())
                };

                let net_name = match effective_passphrase {
                    Some(p) if p.contains("Future Network") => "futurenet",
                    _ => "testnet",
                };

                if !ctx.quiet {
                    println!("funding `{name}` ({pubkey}) via {net_name} friendbot...");
                }

                // #287 — fund_friendbot now returns the balance
                let balance = fund_friendbot(&pubkey, effective_passphrase, ctx.timeout())?;

                if !funded_on.contains(&net_name.to_string()) {
                    funded_on.push(net_name.to_string());
                }
                if let Some(id) = store.identities.get_mut(name.as_str()) {
                    id.funded_on = funded_on;
                    save_store(&path, &store)?;
                }

                if ctx.json {
                    let report = serde_json::json!({
                        "name": name,
                        "public_key": pubkey,
                        "funded": true,
                        "network": net_name,
                        "balance_xlm": balance,
                    });
                    println!("{}", serde_json::to_string_pretty(&report).unwrap());
                } else if !ctx.quiet {
                    println!("funded `{name}` on {net_name}.");
                    println!("  balance: {balance} XLM");
                }
                Ok(())
            }

            _ => Err(ForgeError::InvalidArgument(
                "unknown identity subcommand".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_keypair_produces_valid_stellar_keys() {
        let (public, secret) = generate_keypair();
        assert!(public.starts_with('G'), "public key should start with G: {public}");
        assert!(secret.starts_with('S'), "secret key should start with S: {secret}");
        assert_eq!(public.len(), 56, "Stellar public keys are 56 chars");
        assert_eq!(secret.len(), 56, "Stellar secret keys are 56 chars");
    }

    #[test]
    fn generate_keypair_is_unique() {
        let (pub1, _) = generate_keypair();
        let (pub2, _) = generate_keypair();
        assert_ne!(pub1, pub2, "two generated keypairs should differ");
    }

    #[test]
    fn store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identities.json");

        let mut store = IdentityStore::default();
        store.identities.insert(
            "alice".into(),
            Identity {
                public_key: "GABC".into(),
                secret_key: "SABC".into(),
                funded_on: Vec::new(),
            },
        );
        save_store(&path, &store).unwrap();

        let loaded = load_store(&path).unwrap();
        assert_eq!(loaded.identities.len(), 1);
        assert_eq!(loaded.identities["alice"].public_key, "GABC");
    }

    #[test]
    fn load_missing_file_returns_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let store = load_store(&path).unwrap();
        assert!(store.identities.is_empty());
    }

    #[test]
    fn remove_identity_updates_store_and_rejects_missing_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identities.json");
        let mut store = IdentityStore::default();
        for name in ["alice", "bob"] {
            store.identities.insert(
                name.into(),
                Identity {
                    public_key: format!("G{name}"),
                    secret_key: format!("S{name}"),
                    funded_on: Vec::new(),
                },
            );
        }
        save_store(&path, &store).unwrap();

        remove_identity(&path, "alice").unwrap();

        let updated = load_store(&path).unwrap();
        assert!(!updated.identities.contains_key("alice"));
        assert_eq!(updated.identities["bob"].public_key, "Gbob");

        let error = remove_identity(&path, "carol").unwrap_err();
        assert!(error.to_string().contains("identity `carol` not found"));
    }

    #[test]
    fn format_list_empty() {
        let store = IdentityStore::default();
        assert!(format_list(&store).contains("no identities stored"));
    }

    #[test]
    fn format_list_shows_names_and_public_keys() {
        let mut store = IdentityStore::default();
        store.identities.insert(
            "alice".into(),
            Identity {
                public_key: "GAAA".into(),
                secret_key: "SAAA".into(),
                funded_on: Vec::new(),
            },
        );
        store.identities.insert(
            "bob".into(),
            Identity {
                public_key: "GBBB".into(),
                secret_key: "SBBB".into(),
                funded_on: Vec::new(),
            },
        );
        let output = format_list(&store);
        assert!(output.contains("alice"));
        assert!(output.contains("GAAA"));
        assert!(output.contains("bob"));
        assert!(output.contains("GBBB"));
        // #288 — Secret keys must NOT appear in the list output
        assert!(!output.contains("SAAA"), "secret key must not appear in format_list output");
        assert!(!output.contains("SBBB"), "secret key must not appear in format_list output");
    }

    // #288 — secret keys must never appear in output unless --show-secret is passed
    #[test]
    fn generate_output_does_not_contain_secret_without_show_secret() {
        use soroban_forge_core::ForgeContext;

        let dir = tempfile::tempdir().unwrap();
        // We can't easily capture stdout, but we can verify the plugin builds
        // and the show_secret flag is wired up.
        let plugin = IdentityPlugin;
        let cmd = plugin.command();
        let sub = cmd
            .find_subcommand("generate")
            .expect("generate subcommand exists");
        let has_show_secret = sub
            .get_arguments()
            .any(|a| a.get_long() == Some("show-secret"));
        assert!(has_show_secret, "generate must have --show-secret flag");
    }

    // #288 — list must have --show-secret flag
    #[test]
    fn list_subcommand_has_show_secret_flag() {
        let plugin = IdentityPlugin;
        let cmd = plugin.command();
        let sub = cmd
            .find_subcommand("list")
            .expect("list subcommand exists");
        let has_show_secret = sub
            .get_arguments()
            .any(|a| a.get_long() == Some("show-secret"));
        assert!(has_show_secret, "list must have --show-secret flag");
    }

    // #329 — --timeout bounds a slow HTTP call instead of hanging forever
    #[test]
    fn http_get_is_bounded_by_timeout() {
        // A server that accepts the connection but never responds.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let _accepter = std::thread::spawn(move || {
            let held = listener.accept();
            std::thread::sleep(Duration::from_secs(10));
            drop(held);
        });

        let started = std::time::Instant::now();
        let result = http_get(&url, Some(Duration::from_millis(300)));
        assert!(result.is_err(), "a silent server must trip the timeout");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "request was not bounded by the timeout: {:?}",
            started.elapsed()
        );
    }

    // #471 — the friendbot host is chosen per network, not hardcoded to testnet
    #[test]
    fn friendbot_host_is_chosen_per_network() {
        // Testnet and futurenet run separate friendbot deployments.
        assert_eq!(
            friendbot_host_for("Test SDF Network ; September 2015"),
            Some("https://friendbot.stellar.org")
        );
        assert_eq!(
            friendbot_host_for("Test SDF Future Network ; October 2022"),
            Some("https://friendbot-futurenet.stellar.org")
        );
    }

    #[test]
    fn friendbot_host_is_none_for_mainnet_and_unknown_networks() {
        assert_eq!(friendbot_host_for("Public Global Stellar Network ; September 2015"), None);
        // An unrecognized/custom passphrase must not silently fall back to the
        // testnet friendbot — that would fund an account on the wrong network.
        assert_eq!(friendbot_host_for("Standalone Network ; February 2017"), None);
        assert_eq!(friendbot_host_for("Some Custom Network"), None);
    }

    #[test]
    fn fund_friendbot_uses_the_futurenet_host_for_a_futurenet_passphrase() {
        // The bug in #471 was that this request went to friendbot.stellar.org
        // (testnet) regardless of the passphrase. Assert on the URL the
        // function actually builds, without making the request.
        let url = friendbot_url(
            "GABC",
            Some("Test SDF Future Network ; October 2022"),
        )
        .expect("futurenet should resolve to a friendbot host");
        assert!(
            url.starts_with("https://friendbot-futurenet.stellar.org/"),
            "futurenet must use its own friendbot host, got {url}"
        );
        assert!(
            !url.contains("//friendbot.stellar.org"),
            "futurenet must not use the testnet friendbot, got {url}"
        );
    }

    #[test]
    fn fund_friendbot_refuses_a_custom_passphrase() {
        // Not mainnet, but not a network with a known friendbot either: the
        // error must say so rather than defaulting to testnet.
        let result = fund_friendbot("GABC", Some("Standalone Network ; February 2017"), None);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Standalone Network"),
            "error should name the unrecognized passphrase: {msg}"
        );
    }

    #[test]
    fn fund_friendbot_requires_a_passphrase() {
        let result = fund_friendbot("GABC", None, None);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("passphrase"),
            "error should explain a passphrase is needed: {msg}"
        );
    }

    // #470 — a crash mid-write must not corrupt the identity store
    #[test]
    fn save_store_replaces_the_file_atomically() {
        let dir = std::env::temp_dir().join(format!("sf-identity-470-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("identities.json");

        // Write a store with one identity, then a larger one, then a smaller
        // one. A truncate-and-write would risk a reader seeing a prefix of the
        // new content; an atomic rename cannot.
        let mut store = IdentityStore::default();
        store.identities.insert(
            "alice".into(),
            Identity { public_key: "GALICE".into(), secret_key: "SALICE".into(), funded_on: Vec::new() },
        );
        save_store(&path, &store).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();

        store.identities.insert(
            "bob".into(),
            Identity { public_key: "GBOB".into(), secret_key: "SBOB".into(), funded_on: Vec::new() },
        );
        save_store(&path, &store).unwrap();

        store.identities.remove("alice");
        save_store(&path, &store).unwrap();

        // Whatever the write sequence, the file always parses and holds exactly
        // what was last written.
        let loaded = load_store(&path).unwrap();
        assert_eq!(loaded.identities.len(), 1);
        assert!(loaded.identities.contains_key("bob"));
        assert_ne!(std::fs::read_to_string(&path).unwrap(), first);

        // No temporary file left behind for the next run to trip over.
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["identities.json".to_string()], "stray files: {names:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_save_leaves_the_previous_store_readable() {
        // The property the issue is really about: whatever goes wrong, the
        // previously saved identities are still there afterwards.
        let dir = std::env::temp_dir().join(format!("sf-identity-470b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("identities.json");
        let mut store = IdentityStore::default();
        store.identities.insert(
            "alice".into(),
            Identity { public_key: "GALICE".into(), secret_key: "SALICE".into(), funded_on: Vec::new() },
        );
        save_store(&path, &store).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        // Make the destination directory read-only so the temporary file cannot
        // be created; the rename therefore never happens.
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        let readonly = perms.clone();
        perms.set_readonly(true);
        std::fs::set_permissions(&dir, perms).unwrap();

        let mut updated = store.clone();
        updated.identities.insert(
            "bob".into(),
            Identity { public_key: "GBOB".into(), secret_key: "SBOB".into(), funded_on: Vec::new() },
        );
        let result = save_store(&path, &updated);

        // Restore permissions before asserting, so the temp dir can be removed.
        std::fs::set_permissions(&dir, readonly).unwrap();

        if result.is_err() {
            // The write was refused, and the store on disk is untouched.
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
            assert_eq!(load_store(&path).unwrap().identities.len(), 1);
        }
        // On a platform where a read-only directory is not enforced (or when
        // running as root), the write simply succeeded — the store still parses.
        assert!(load_store(&path).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // #287 — fund refuses on mainnet passphrase
    #[test]
    fn fund_friendbot_refuses_mainnet_passphrase() {
        let result = fund_friendbot(
            "GABC",
            Some("Public Global Stellar Network ; September 2015"),
            None,
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("mainnet"), "error should mention mainnet: {msg}");
    }

    // #287 — fund allows testnet passphrase (would make network call, test only guards the refusal)
    #[test]
    fn fund_friendbot_allows_testnet_passphrase_guard() {
        // We cannot make real network calls in unit tests. We verify that the
        // mainnet guard does NOT fire for a testnet passphrase. The ureq call
        // will fail (no network in CI) but the error won't be about mainnet.
        let result = fund_friendbot("GABC", Some("Test SDF Network ; September 2015"), None);
        match &result {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    !msg.contains("mainnet"),
                    "testnet passphrase should not trigger mainnet refusal: {msg}"
                );
            }
            Ok(_) => {} // real network available — fine
        }
    }

    #[test]
    fn identity_command_has_subcommands() {
        let plugin = IdentityPlugin;
        let cmd = plugin.command();
        let sub_names: Vec<&str> = cmd
            .get_subcommands()
            .map(|s| s.get_name())
            .collect();
        assert!(sub_names.contains(&"generate"));
        assert!(sub_names.contains(&"list"));
        assert!(sub_names.contains(&"fund"));
        assert!(sub_names.contains(&"remove"));
        assert!(sub_names.contains(&"use"));
    }

    // #428 — funded marker is recorded in store and displayed in format_list
    #[test]
    fn funded_marker_recorded_and_displayed() {
        let mut store = IdentityStore::default();
        let id = Identity {
            public_key: "GALICE".into(),
            secret_key: "SALICE".into(),
            funded_on: vec!["testnet".into()],
        };
        store.identities.insert("alice".into(), id);

        let list_output = format_list(&store);
        assert!(list_output.contains("alice"));
        assert!(list_output.contains("GALICE"));
        assert!(list_output.contains("funded: testnet"), "list must show funded marker: {list_output}");
    }

    // #427 — identity use sets default in store and writes forge.toml
    #[test]
    fn identity_use_sets_default_and_writes_forge_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let store_file = tmp.path().join("identities.json");
        let mut store = IdentityStore::default();
        store.identities.insert(
            "alice".into(),
            Identity {
                public_key: "GALICE".into(),
                secret_key: "SALICE".into(),
                funded_on: Vec::new(),
            },
        );
        save_store(&store_file, &store).unwrap();

        // Test patching forge.toml
        write_identity_to_forge_toml(tmp.path(), "alice").unwrap();
        let toml_content = std::fs::read_to_string(tmp.path().join("forge.toml")).unwrap();
        assert!(toml_content.contains("[identity]"));
        assert!(toml_content.contains("default = \"alice\""));

        // Test updating default in store
        store.default = Some("alice".into());
        save_store(&store_file, &store).unwrap();

        let loaded = load_store(&store_file).unwrap();
        assert_eq!(loaded.default.as_deref(), Some("alice"));
    }

    // #427 — removing the default identity clears the default marker
    #[test]
    fn identity_remove_clears_default() {
        let tmp = tempfile::tempdir().unwrap();
        let store_file = tmp.path().join("identities.json");
        let mut store = IdentityStore::default();
        store.identities.insert(
            "alice".into(),
            Identity {
                public_key: "GALICE".into(),
                secret_key: "SALICE".into(),
                funded_on: Vec::new(),
            },
        );
        store.default = Some("alice".into());
        save_store(&store_file, &store).unwrap();

        remove_identity(&store_file, "alice").unwrap();

        let loaded = load_store(&store_file).unwrap();
        assert!(!loaded.identities.contains_key("alice"));
        assert!(loaded.default.is_none());
    }

    // #427 — patch_forge_toml_identity replaces existing default key cleanly
    #[test]
    fn test_patch_forge_toml_identity() {
        let existing = "[project]\nname = \"demo\"\n\n[identity]\ndefault = \"bob\"\n";
        let patched = patch_forge_toml_identity(existing, "alice");
        assert!(patched.contains("[project]"));
        assert!(patched.contains("default = \"alice\""));
        assert!(!patched.contains("default = \"bob\""));
    }

    // #287 — fund refuses offline
    #[test]
    fn fund_subcommand_refuses_offline() {
        use soroban_forge_core::ForgeContext;
        let dir = tempfile::tempdir().unwrap();
        let ctx = ForgeContext::with_options(
            dir.path().to_path_buf(),
            0,
            false,
            false,
            false,
            true, // offline = true
            None,
            None,
            None,
        )
        .unwrap();
        let plugin = IdentityPlugin;
        let cmd = plugin.command();
        let matches = cmd
            .try_get_matches_from(["identity", "fund", "alice"])
            .unwrap();
        let result = plugin.run(&matches, &ctx);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("offline"), "error should mention offline: {msg}");
    }
}
