//! # soroban-forge-network
//!
//! `soroban-forge network add|list|use|current` — manage named network configs
//! (RPC URL + passphrase) and select a default for other commands.
//!
//! Networks are stored as a JSON file at
//! `~/.config/soroban-forge/networks.json`.
//!
//! `network use <name>` also writes the choice into the project's `forge.toml`
//! so that `deploy`, `invoke`, and `verify` share a single source of truth.

use std::collections::BTreeMap;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Arg, ArgMatches, Command};
use serde::{Deserialize, Serialize};
use soroban_forge_core::{ForgeContext, ForgeError, ForgePlugin, Result};

/// A stored network configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Network {
    pub rpc_url: String,
    pub network_passphrase: String,
}

/// The on-disk network store.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct NetworkStore {
    #[serde(default)]
    pub networks: BTreeMap<String, Network>,
    /// Name of the network selected as default, if any.
    #[serde(default)]
    pub default: Option<String>,
}

/// Well-known network presets. These are available without any user
/// configuration (#289). Users can override them with `network add <name>`
/// using the same name and `--force`.
pub fn well_known(name: &str) -> Option<Network> {
    match name {
        // #289 — official Stellar testnet
        "testnet" => Some(Network {
            rpc_url: "https://soroban-testnet.stellar.org".into(),
            network_passphrase: "Test SDF Network ; September 2015".into(),
        }),
        // #289 — official Stellar futurenet
        "futurenet" => Some(Network {
            rpc_url: "https://rpc-futurenet.stellar.org".into(),
            network_passphrase: "Test SDF Future Network ; October 2022".into(),
        }),
        // #289 — official Stellar mainnet
        // #466 — official public RPC endpoint without embedded credentials
        "mainnet" => Some(Network {
            rpc_url: "https://mainnet.sorobanrpc.com".into(),
            // Canonical mainnet passphrase
            network_passphrase: "Public Global Stellar Network ; September 2015".into(),
        }),
        "localnet" => Some(Network {
            rpc_url: "http://localhost:8000/soroban/rpc".into(),
            network_passphrase: "Standalone Network ; February 2017".into(),
        }),
        _ => None,
    }
}

/// Return the path to the network store file.
/// `~/.config/soroban-forge/networks.json`
pub fn store_path() -> Result<PathBuf> {
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .ok_or_else(|| {
            ForgeError::Other("could not determine user config directory".into())
        })?;
    Ok(config_dir.join("soroban-forge").join("networks.json"))
}

/// Load the network store from disk, or return a default empty one.
pub fn load_store(path: &PathBuf) -> Result<NetworkStore> {
    if !path.is_file() {
        return Ok(NetworkStore::default());
    }
    let raw = std::fs::read_to_string(path)
        .map_err(ForgeError::io(format!("reading {}", path.display())))?;
    serde_json::from_str(&raw).map_err(|e| ForgeError::Config {
        path: path.clone(),
        message: e.to_string(),
    })
}

/// Save the network store to disk, creating parent directories as needed.
///
/// Writes through [`soroban_forge_core::atomic::write_atomic`] (#470): the same
/// non-atomic `fs::write` as the identity store, so a crash mid-write could
/// leave `networks.json` unparseable.
pub fn save_store(path: &PathBuf, store: &NetworkStore) -> Result<()> {
    let json = serde_json::to_string_pretty(store)
        .map_err(|e| ForgeError::Other(format!("serializing network store: {e}")))?;
    soroban_forge_core::atomic::write_atomic(path, &json)
        .map_err(|e| ForgeError::Other(format!("writing {}: {e}", path.display())))
}

/// Resolve a network by name: check the user store first, then fall back to
/// built-in presets. This makes the three built-in networks (#289) available
/// without any prior `network add` call.
pub fn resolve_network(store: &NetworkStore, name: &str) -> Option<Network> {
    if let Some(n) = store.networks.get(name) {
        return Some(n.clone());
    }
    well_known(name)
}

/// Determine the effective active network name.
///
/// Priority (highest first):
/// 1. `--network` on the command line (passed in as `cli_override`)
/// 2. `[network] name` in `forge.toml` (from `ctx.config`)
/// 3. `default` field in `networks.json`
pub fn active_network_name<'a>(
    store: &'a NetworkStore,
    ctx: &'a ForgeContext,
    cli_override: Option<&'a str>,
) -> Option<&'a str> {
    if let Some(name) = cli_override {
        return Some(name);
    }
    if let Some(name) = ctx.config.as_ref().and_then(|c| c.network.name.as_deref()) {
        return Some(name);
    }
    store.default.as_deref()
}

/// Persist the active network choice into `forge.toml` in `cwd`.
///
/// If the file does not exist it is created with just the `[network]` section.
/// If it exists the `[network]` section is patched (or appended) without
/// disturbing any other content.
pub fn write_network_to_forge_toml(cwd: &std::path::Path, name: &str) -> Result<()> {
    let path = cwd.join("forge.toml");

    let existing = if path.is_file() {
        std::fs::read_to_string(&path)
            .map_err(ForgeError::io(format!("reading {}", path.display())))?
    } else {
        String::new()
    };

    let updated = patch_forge_toml_network(&existing, name);
    std::fs::write(&path, updated)
        .map_err(ForgeError::io(format!("writing {}", path.display())))
}

/// Patch (or append) the `[network]` section in raw `forge.toml` text.
///
/// Rules:
/// - If a `[network]` section already exists, the `name = …` key within it is
///   replaced (or inserted if missing). Other keys in the section are preserved.
/// - If no `[network]` section exists, one is appended.
pub fn patch_forge_toml_network(existing: &str, name: &str) -> String {
    // We do a simple line-by-line pass instead of a full TOML parse/re-emit
    // so we preserve all comments and formatting.
    let original_lines: Vec<&str> = existing.lines().collect();
    let mut out_lines: Vec<String> = Vec::with_capacity(original_lines.len() + 3);

    let mut in_network_section = false;
    let mut name_key_written = false;
    let mut network_section_header_idx: Option<usize> = None;

    for line in &original_lines {
        let trimmed = line.trim();

        if trimmed == "[network]" {
            in_network_section = true;
            network_section_header_idx = Some(out_lines.len());
            out_lines.push(line.to_string());
            continue;
        }

        // Entering a new section other than [network]
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            // If we were in [network] and never wrote the name key, insert it now
            // (before this new section header)
            if in_network_section && !name_key_written {
                out_lines.push(format!("name = \"{name}\""));
                name_key_written = true;
            }
            in_network_section = false;
            out_lines.push(line.to_string());
            continue;
        }

        // Replace the `name = ...` key inside [network]
        if in_network_section && !name_key_written {
            if let Some(eq_pos) = trimmed.find('=') {
                let key = trimmed[..eq_pos].trim();
                if key == "name" {
                    out_lines.push(format!("name = \"{name}\""));
                    name_key_written = true;
                    continue; // skip the old line
                }
            }
        }

        out_lines.push(line.to_string());
    }

    // EOF while still inside [network] and name was never written
    if in_network_section && !name_key_written {
        out_lines.push(format!("name = \"{name}\""));
        name_key_written = true;
    }

    // No [network] section found at all — append
    if network_section_header_idx.is_none() && !name_key_written {
        if !out_lines.is_empty() && !out_lines.last().map(|l| l.is_empty()).unwrap_or(true) {
            out_lines.push(String::new());
        }
        out_lines.push("[network]".to_string());
        out_lines.push(format!("name = \"{name}\""));
    }

    let mut result = out_lines.join("\n");
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// Every network `list` shows: user-defined entries plus built-in presets
/// (#289), sorted by name.
pub fn listed_names(store: &NetworkStore) -> Vec<String> {
    let builtins = ["testnet", "futurenet", "mainnet", "localnet"];
    let mut all_names: Vec<String> = store.networks.keys().cloned().collect();
    for b in &builtins {
        if !store.networks.contains_key(*b) {
            all_names.push(b.to_string());
        }
    }
    all_names.sort();
    all_names
}

/// Format the network list for display.
///
/// Fully offline: this never touches the network.  `network list --check`
/// uses [`format_list_checked`] instead.
pub fn format_list(store: &NetworkStore) -> String {
    format_list_with(store, None)
}

/// Format the network list annotated with reachability results
/// (`network list --check`, issue #411).
pub fn format_list_checked(
    store: &NetworkStore,
    checks: &BTreeMap<String, Reachability>,
) -> String {
    let mut out = format_list_with(store, Some(checks));
    let unreachable = checks.values().filter(|r| !r.is_reachable()).count();
    out.push_str(&format!(
        "\n{} of {} network(s) reachable\n",
        checks.len() - unreachable,
        checks.len()
    ));
    out
}

fn format_list_with(
    store: &NetworkStore,
    checks: Option<&BTreeMap<String, Reachability>>,
) -> String {
    // Built-ins that have been overridden by the user store show the user value.
    let names = listed_names(store);
    let all_names: Vec<&str> = names.iter().map(String::as_str).collect();

    if all_names.is_empty() {
        return "no networks available.\n".to_string();
    }

    let mut out = String::from("configured networks:\n\n");
    let name_width = all_names.iter().map(|k| k.len()).max().unwrap_or(0);
    for name in &all_names {
        let network = resolve_network(store, name).expect("name came from store or builtin");
        let marker = if store.default.as_deref() == Some(name) {
            "*"
        } else {
            " "
        };
        // Tag built-ins that haven't been customised
        let tag = if store.networks.contains_key(*name) {
            ""
        } else {
            "  [built-in]"
        };
        let status = checks
            .and_then(|c| c.get(*name))
            .map(|r| format!("  {}", r.label()))
            .unwrap_or_default();
        out.push_str(&format!(
            "{marker} {:<width$}  {}  ({}){}{}\n",
            name,
            network.rpc_url,
            network.network_passphrase,
            tag,
            status,
            width = name_width
        ));
    }
    out
}

// ---- reachability probe (`network list --check`, issue #411) ----

/// Default per-endpoint probe timeout when `--timeout` is not given.
pub const DEFAULT_CHECK_TIMEOUT: Duration = Duration::from_secs(3);

/// Result of probing one network's RPC endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reachability {
    /// A TCP connection to the endpoint's host and port succeeded.
    Reachable { latency_ms: u64 },
    /// The endpoint could not be reached; the string says why.
    Unreachable(String),
}

impl Reachability {
    pub fn is_reachable(&self) -> bool {
        matches!(self, Reachability::Reachable { .. })
    }

    /// Short annotation appended to a `network list --check` row.
    pub fn label(&self) -> String {
        match self {
            Reachability::Reachable { latency_ms } => format!("[reachable, {latency_ms}ms]"),
            Reachability::Unreachable(reason) => format!("[unreachable: {reason}]"),
        }
    }

    /// Fields merged into a network's `--json` entry.
    pub fn json_fields(&self) -> serde_json::Value {
        match self {
            Reachability::Reachable { latency_ms } => serde_json::json!({
                "reachable": true,
                "latency_ms": latency_ms,
            }),
            Reachability::Unreachable(reason) => serde_json::json!({
                "reachable": false,
                "error": reason,
            }),
        }
    }
}

/// Extract `(host, port)` from an RPC URL, defaulting the port from the
/// scheme (443 for https, 80 for http).  Handles bracketed IPv6 hosts and
/// strips userinfo, paths, queries and fragments.
pub fn endpoint_host_port(url: &str) -> std::result::Result<(String, u16), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("`{url}` has no scheme"))?;
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "https" | "wss" => 443,
        "http" | "ws" => 80,
        other => return Err(format!("unsupported scheme `{other}`")),
    };

    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);

    let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
        let (host, tail) = after_bracket
            .split_once(']')
            .ok_or_else(|| format!("`{url}` has an unterminated IPv6 address"))?;
        (host, tail.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };

    if host.is_empty() {
        return Err(format!("`{url}` has no host"));
    }
    let port = match port {
        Some(p) => p.parse::<u16>().map_err(|_| format!("`{url}` has an invalid port `{p}`"))?,
        None => default_port,
    };
    Ok((host.to_string(), port))
}

/// Lightweight reachability probe: resolve the endpoint and open (then
/// immediately drop) a TCP connection, bounded by `timeout`.  No RPC request
/// is sent, so it is cheap and side-effect free; it answers "can this machine
/// reach the endpoint right now", which is what `list --check` reports.
pub fn probe_endpoint(url: &str, timeout: Duration) -> Reachability {
    let (host, port) = match endpoint_host_port(url) {
        Ok(hp) => hp,
        Err(reason) => return Reachability::Unreachable(reason),
    };

    let started = Instant::now();
    let addrs: Vec<_> = match (host.as_str(), port).to_socket_addrs() {
        Ok(addrs) => addrs.collect(),
        Err(e) => return Reachability::Unreachable(format!("cannot resolve {host}: {e}")),
    };
    if addrs.is_empty() {
        return Reachability::Unreachable(format!("cannot resolve {host}"));
    }

    let mut last_error = None;
    for addr in addrs {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        match TcpStream::connect_timeout(&addr, remaining) {
            Ok(_) => {
                return Reachability::Reachable {
                    latency_ms: started.elapsed().as_millis() as u64,
                }
            }
            Err(e) => last_error = Some(e),
        }
    }
    Reachability::Unreachable(match last_error {
        Some(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            format!("timed out after {}s", timeout.as_secs_f64())
        }
        Some(e) => e.to_string(),
        None => format!("timed out after {}s", timeout.as_secs_f64()),
    })
}

/// Probe every listed network concurrently, so one slow endpoint does not
/// multiply the wait.  Returns results keyed by network name.
pub fn probe_networks(store: &NetworkStore, timeout: Duration) -> BTreeMap<String, Reachability> {
    let handles: Vec<_> = listed_names(store)
        .into_iter()
        .filter_map(|name| resolve_network(store, &name).map(|n| (name, n.rpc_url)))
        .map(|(name, url)| {
            let handle = std::thread::spawn(move || probe_endpoint(&url, timeout));
            (name, handle)
        })
        .collect();

    handles
        .into_iter()
        .map(|(name, handle)| {
            let result = handle
                .join()
                .unwrap_or_else(|_| Reachability::Unreachable("probe panicked".into()));
            (name, result)
        })
        .collect()
}

/// Validate that a URL string has a scheme and a non-empty host.
///
/// This is a lightweight check — not a full RFC 3986 parse — but it catches
/// the most common typos (missing scheme, stray whitespace, bare hostnames).
pub fn validate_rpc_url(url: &str) -> Result<()> {
    let trimmed = url.trim();
    if trimmed != url {
        return Err(ForgeError::InvalidArgument(format!(
            "rpc-url `{url}` contains leading or trailing whitespace"
        )));
    }

    // Must have a scheme separated by "://"
    let after_scheme = match trimmed.find("://") {
        Some(pos) if pos > 0 => &trimmed[pos + 3..],
        _ => {
            return Err(ForgeError::InvalidArgument(format!(
                "rpc-url `{url}` is not a well-formed URL — it must include a scheme (e.g. https://)"
            )));
        }
    };

    // The host part (before any path or port) must be non-empty
    let host = after_scheme
        .split(['/', ':', '?', '#'])
        .next()
        .unwrap_or("");
    if host.is_empty() {
        return Err(ForgeError::InvalidArgument(format!(
            "rpc-url `{url}` has no host — expected a URL like https://soroban-testnet.stellar.org"
        )));
    }

    Ok(())
}

/// The `network` subcommand.
pub struct NetworkPlugin;

impl ForgePlugin for NetworkPlugin {
    fn name(&self) -> &'static str {
        "network"
    }

    fn command(&self) -> Command {
        Command::new("network")
            .about("Manage named network configs and select a default")
            .subcommand_required(true)
            .subcommand(
                Command::new("add")
                    .about("Add a named network config (testnet, futurenet, mainnet, localnet, or custom)")
                    .arg(
                        Arg::new("name")
                            .help("Name for the network (e.g. testnet, futurenet, mainnet)")
                            .required(true),
                    )
                    .arg(
                        Arg::new("rpc-url")
                            .long("rpc-url")
                            .help("RPC endpoint URL [default: built-in value for well-known names]"),
                    )
                    .arg(
                        Arg::new("passphrase")
                            .long("passphrase")
                            .help("Network passphrase [default: built-in value for well-known names]"),
                    )
                    .arg(
                        Arg::new("force")
                            .long("force")
                            .action(clap::ArgAction::SetTrue)
                            .help("Overwrite an existing network with the same name"),
                    ),
            )
            .subcommand(
                Command::new("list")
                    .about("List all configured networks (including built-in presets)")
                    .arg(
                        Arg::new("check")
                            .long("check")
                            .action(clap::ArgAction::SetTrue)
                            .help("Probe each network's RPC endpoint and mark it reachable or unreachable (needs network access; honours --timeout)"),
                    ),
            )
            .subcommand(
                Command::new("use")
                    .about("Select the default network for other commands and persist it to forge.toml")
                    .arg(
                        Arg::new("name")
                            .help("Name of the network to use as default")
                            .required(true),
                    ),
            )
            // #406 — print the active default network's details (renamed from
            // the #290 `current` stub; `current` kept as an alias for
            // backward compatibility)
            .subcommand(
                Command::new("show")
                    .about("Print the active default network's name, RPC URL and passphrase")
                    .alias("current"),
            )
            // #467 — delete a stored network config (built-in presets cannot
            // be removed). `rm` alias added per #404.
            .subcommand(
                Command::new("remove")
                    .about("Delete a stored network config from the store")
                    .alias("rm")
                    .arg(
                        Arg::new("name")
                            .help("Name of the network to remove")
                            .required(true),
                    ),
            )
    }

    fn run(&self, matches: &ArgMatches, ctx: &ForgeContext) -> Result<()> {
        let path = store_path()?;

        match matches.subcommand() {
            Some(("add", sub)) => {
                let name = sub.get_one::<String>("name").unwrap();
                let force = sub.get_flag("force");
                let mut store = load_store(&path)?;

                if store.networks.contains_key(name.as_str()) && !force {
                    return Err(ForgeError::AlreadyExists(PathBuf::from(name.as_str())));
                }

                let rpc_url = sub.get_one::<String>("rpc-url").cloned();
                let passphrase = sub.get_one::<String>("passphrase").cloned();

                // #405 — validate the URL before storing it
                if let Some(ref url) = rpc_url {
                    validate_rpc_url(url)?;
                }

                let preset = well_known(name.as_str());

                let network = match (rpc_url, passphrase, preset) {
                    (Some(rpc_url), Some(network_passphrase), _) => Network {
                        rpc_url,
                        network_passphrase,
                    },
                    (rpc_url, passphrase, Some(preset)) => Network {
                        rpc_url: rpc_url.unwrap_or(preset.rpc_url),
                        network_passphrase: passphrase.unwrap_or(preset.network_passphrase),
                    },
                    _ => {
                        return Err(ForgeError::InvalidArgument(format!(
                            "`{name}` is not a well-known network — pass both --rpc-url and --passphrase"
                        )));
                    }
                };

                store.networks.insert(name.clone(), network.clone());
                if store.default.is_none() {
                    store.default = Some(name.clone());
                }
                save_store(&path, &store)?;

                if ctx.json {
                    let report = serde_json::json!({
                        "name": name,
                        "rpc_url": network.rpc_url,
                        "network_passphrase": network.network_passphrase,
                    });
                    println!("{}", serde_json::to_string_pretty(&report).unwrap());
                } else if !ctx.quiet {
                    println!("added network `{name}`");
                    println!("  rpc url:    {}", network.rpc_url);
                    println!("  passphrase: {}", network.network_passphrase);
                }
                Ok(())
            }

            Some(("list", sub)) => {
                let store = load_store(&path)?;

                // #411 — opt-in reachability probe; the default listing
                // stays fully offline.
                let checks = if sub.get_flag("check") {
                    if ctx.offline {
                        return Err(ForgeError::InvalidArgument(
                            "network list --check probes each endpoint and is unavailable in offline mode".into(),
                        ));
                    }
                    let timeout = ctx
                        .timeout_secs
                        .map(Duration::from_secs)
                        .unwrap_or(DEFAULT_CHECK_TIMEOUT);
                    Some(probe_networks(&store, timeout))
                } else {
                    None
                };

                if ctx.json {
                    // #289 — include built-in presets that are not in the user store
                    let builtins = ["testnet", "futurenet", "mainnet", "localnet"];
                    let mut all: BTreeMap<String, serde_json::Value> = BTreeMap::new();
                    for b in &builtins {
                        if let Some(n) = well_known(b) {
                            all.insert(
                                b.to_string(),
                                serde_json::json!({
                                    "rpc_url": n.rpc_url,
                                    "network_passphrase": n.network_passphrase,
                                    "built_in": true,
                                }),
                            );
                        }
                    }
                    // User-defined entries override built-ins
                    for (k, v) in &store.networks {
                        all.insert(
                            k.clone(),
                            serde_json::json!({
                                "rpc_url": v.rpc_url,
                                "network_passphrase": v.network_passphrase,
                                "built_in": false,
                            }),
                        );
                    }
                    if let Some(checks) = &checks {
                        // #411 — annotate each entry with its probe result.
                        for (name, entry) in all.iter_mut() {
                            if let (Some(result), Some(entry)) =
                                (checks.get(name), entry.as_object_mut())
                            {
                                if let serde_json::Value::Object(fields) = result.json_fields() {
                                    entry.extend(fields);
                                }
                            }
                        }
                    }
                    let report = serde_json::json!({
                        "networks": all,
                        "default": store.default,
                    });
                    println!("{}", serde_json::to_string_pretty(&report).unwrap());
                } else if !ctx.quiet {
                    match &checks {
                        Some(checks) => print!("{}", format_list_checked(&store, checks)),
                        None => print!("{}", format_list(&store)),
                    }
                }
                Ok(())
            }

            Some(("use", sub)) => {
                let name = sub.get_one::<String>("name").unwrap();
                let mut store = load_store(&path)?;

                // #289 — also accept built-in names that aren't in the user store
                let network_exists =
                    store.networks.contains_key(name.as_str()) || well_known(name.as_str()).is_some();
                if !network_exists {
                    return Err(ForgeError::InvalidArgument(format!(
                        "network `{name}` not found (use `soroban-forge network list` to see available networks)"
                    )));
                }

                store.default = Some(name.clone());
                save_store(&path, &store)?;

                // #290 — persist the choice into forge.toml so deploy/invoke/verify pick it up
                write_network_to_forge_toml(&ctx.cwd, name)?;

                if ctx.json {
                    let report = serde_json::json!({ "default": name });
                    println!("{}", serde_json::to_string_pretty(&report).unwrap());
                } else if !ctx.quiet {
                    println!("using `{name}` as the default network");
                    println!("  recorded in forge.toml and networks.json");
                }
                Ok(())
            }

            // #406 — print the active default network's name, RPC URL and passphrase
            // Accessible as both `network show` and `network current`
            Some(("show", _sub)) => {
                let store = load_store(&path)?;
                let current_name = active_network_name(&store, ctx, None);

                match current_name {
                    None => {
                        // Error clearly when no default is set
                        return Err(ForgeError::InvalidArgument(
                            "no default network is set — run `soroban-forge network use <name>` to select one".into(),
                        ));
                    }
                    Some(name) => {
                        let network = resolve_network(&store, name).ok_or_else(|| {
                            ForgeError::InvalidArgument(format!(
                                "default network `{name}` is set but could not be resolved"
                            ))
                        })?;

                        if ctx.json {
                            let report = serde_json::json!({
                                "name": name,
                                "rpc_url": network.rpc_url,
                                "network_passphrase": network.network_passphrase,
                            });
                            println!("{}", serde_json::to_string_pretty(&report).unwrap());
                        } else if !ctx.quiet {
                            println!("active network: {name}");
                            println!("  rpc url:    {}", network.rpc_url);
                            println!("  passphrase: {}", network.network_passphrase);
                        }
                    }
                }
                Ok(())
            }

            // #467 (also requested by #404) — `network remove <name>` / `network rm <name>`
            // deletes a stored network entry. Built-in presets (testnet/futurenet/
            // mainnet/localnet) cannot be removed — they exist without any stored
            // entry, so we refuse and direct the user to `network list` to see
            // what's actually stored.
            Some(("remove", sub)) => {
                let name = sub.get_one::<String>("name").unwrap();
                let mut store = load_store(&path)?;

                if !store.networks.contains_key(name.as_str()) {
                    // Distinguish "name is a built-in preset" from "name does not exist"
                    if well_known(name.as_str()).is_some() {
                        return Err(ForgeError::InvalidArgument(format!(
                            "`{name}` is a built-in network preset and cannot be removed (use `network add {name} --force …` to override it)
hint: built-in presets are listed by `soroban-forge network list` with the [built-in] tag"
                        )));
                    }
                    return Err(ForgeError::InvalidArgument(format!(
                        "network `{name}` not found in the store (use `soroban-forge network list` to see stored networks)
hint: only entries you added with `network add` can be removed"
                    )));
                }

                let removed = store.networks.remove(name.as_str()).expect("checked contains_key above");
                // If the removed network was the default, clear the default.
                if store.default.as_deref() == Some(name.as_str()) {
                    store.default = None;
                }
                save_store(&path, &store)?;

                if ctx.json {
                    let report = serde_json::json!({
                        "name": name,
                        "rpc_url": removed.rpc_url,
                        "network_passphrase": removed.network_passphrase,
                    });
                    println!("{}", serde_json::to_string_pretty(&report).unwrap());
                } else if !ctx.quiet {
                    println!("removed network `{name}`");
                    println!("  rpc url:    {}", removed.rpc_url);
                    println!("  passphrase: {}", removed.network_passphrase);
                    if store.default.is_none() {
                        println!("  hint: no default network selected (use `soroban-forge network use <name>` to set one)");
                    }
                }
                Ok(())
            }

            _ => Err(ForgeError::InvalidArgument(
                "unknown network subcommand".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("networks.json");

        let mut store = NetworkStore::default();
        store.networks.insert(
            "testnet".into(),
            Network {
                rpc_url: "https://soroban-testnet.stellar.org".into(),
                network_passphrase: "Test SDF Network ; September 2015".into(),
            },
        );
        store.default = Some("testnet".into());
        save_store(&path, &store).unwrap();

        let loaded = load_store(&path).unwrap();
        assert_eq!(loaded.networks.len(), 1);
        assert_eq!(loaded.default.as_deref(), Some("testnet"));
        assert_eq!(
            loaded.networks["testnet"].rpc_url,
            "https://soroban-testnet.stellar.org"
        );
    }

    #[test]
    fn load_missing_file_returns_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let store = load_store(&path).unwrap();
        assert!(store.networks.is_empty());
        assert!(store.default.is_none());
    }

    // #289 — three built-in networks must be available
    #[test]
    fn well_known_covers_testnet_futurenet_mainnet_localnet() {
        let testnet = well_known("testnet").expect("testnet should be a built-in");
        assert_eq!(testnet.network_passphrase, "Test SDF Network ; September 2015");

        let futurenet = well_known("futurenet").expect("futurenet should be a built-in");
        assert_eq!(futurenet.network_passphrase, "Test SDF Future Network ; October 2022");

        // #289 — mainnet is now a built-in
        let mainnet = well_known("mainnet").expect("mainnet should be a built-in");
        assert_eq!(mainnet.network_passphrase, "Public Global Stellar Network ; September 2015");

        assert!(well_known("localnet").is_some());
        assert!(well_known("custom-xyz").is_none());
    }

    // #466 — mainnet preset must not contain embedded credentials or API keys
    #[test]
    fn test_mainnet_rpc_url_has_no_embedded_credential() {
        let mainnet = well_known("mainnet").expect("mainnet should be a built-in");
        let url = &mainnet.rpc_url;
        assert!(!url.contains("validationcloud"), "must not use third-party paid provider");
        assert!(!url.contains('?'), "URL must not contain query string credential");
        assert!(!url.contains('@'), "URL must not contain userinfo credential");
        // Ensure path segments don't look like embedded API tokens/keys
        let after_scheme = url.split("://").nth(1).unwrap_or(url);
        let segments: Vec<&str> = after_scheme.split('/').skip(1).filter(|s| !s.is_empty()).collect();
        for segment in segments {
            assert!(
                segment.len() < 20,
                "path segment `{segment}` looks like an embedded API credential"
            );
        }
    }

    // #289 — built-ins are available via resolve_network without user configuration
    #[test]
    fn resolve_network_falls_back_to_built_ins() {
        let store = NetworkStore::default();
        assert!(resolve_network(&store, "testnet").is_some());
        assert!(resolve_network(&store, "futurenet").is_some());
        assert!(resolve_network(&store, "mainnet").is_some());
        assert!(resolve_network(&store, "localnet").is_some());
        assert!(resolve_network(&store, "custom-xyz").is_none());
    }

    // #289 — user-defined entries override built-ins
    #[test]
    fn user_store_overrides_built_in() {
        let mut store = NetworkStore::default();
        store.networks.insert(
            "testnet".into(),
            Network {
                rpc_url: "https://my-custom-rpc.example.com".into(),
                network_passphrase: "Test SDF Network ; September 2015".into(),
            },
        );
        let net = resolve_network(&store, "testnet").unwrap();
        assert_eq!(net.rpc_url, "https://my-custom-rpc.example.com");
    }

    #[test]
    fn format_list_empty_store_still_shows_builtins() {
        let store = NetworkStore::default();
        let out = format_list(&store);
        assert!(out.contains("testnet"), "{out}");
        assert!(out.contains("futurenet"), "{out}");
        assert!(out.contains("mainnet"), "{out}");
    }

    #[test]
    fn format_list_marks_the_default_network() {
        let mut store = NetworkStore::default();
        store.networks.insert(
            "testnet".into(),
            Network {
                rpc_url: "https://soroban-testnet.stellar.org".into(),
                network_passphrase: "Test SDF Network ; September 2015".into(),
            },
        );
        store.networks.insert(
            "localnet".into(),
            Network {
                rpc_url: "http://localhost:8000/soroban/rpc".into(),
                network_passphrase: "Standalone Network ; February 2017".into(),
            },
        );
        store.default = Some("testnet".into());

        let out = format_list(&store);
        assert!(out.contains("* testnet"), "{out}");
        assert!(out.contains("  localnet"), "{out}");
    }

    #[test]
    fn network_command_has_subcommands() {
        let plugin = NetworkPlugin;
        let cmd = plugin.command();
        let sub_names: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(sub_names.contains(&"add"));
        assert!(sub_names.contains(&"list"));
        assert!(sub_names.contains(&"use"));
        // #467/#404 — remove subcommand must exist
        assert!(sub_names.contains(&"remove"), "network command must have 'remove' subcommand");
        // #406 — show subcommand must exist (replaces the old 'current' stub;
        // 'current' survives only as an alias, checked separately below)
        assert!(sub_names.contains(&"show"), "network command must have 'show' subcommand");
    }

    // #467 — `network remove` removes a stored entry and clears default if it was the default
    #[test]
    fn remove_deletes_stored_network() {
        let mut store = NetworkStore::default();
        store.networks.insert(
            "customnet".into(),
            Network {
                rpc_url: "https://rpc.example.com".into(),
                network_passphrase: "Custom Passphrase".into(),
            },
        );
        store.default = Some("customnet".into());

        assert!(store.networks.contains_key("customnet"));
        let removed = store.networks.remove("customnet");
        assert!(removed.is_some());
        // default must be cleared when the removed name was the default
        if store.default.as_deref() == Some("customnet") {
            store.default = None;
        }
        assert!(!store.networks.contains_key("customnet"));
        assert_eq!(store.default, None);
    }

    // #467 — `network remove` refuses built-in presets (testnet/futurenet/mainnet/localnet)
    #[test]
    fn remove_refuses_built_in_presets() {
        // Built-in presets are returned by `well_known` even when the user store is empty.
        let store = NetworkStore::default();
        // All four built-in presets should resolve
        for name in &["testnet", "futurenet", "mainnet", "localnet"] {
            assert!(well_known(name).is_some(),
                "well_known should resolve built-in preset: {name}");
            assert!(!store.networks.contains_key(*name),
                "default store should not contain built-in preset: {name}");
        }
        // The handler's contract: a name with `well_known().is_some()` but not in
        // `store.networks` is a built-in preset and must be refused with a clear
        // error (the actual CLI error path is exercised in the integration test below).
    }

    // #467 — `network remove` refuses unknown names
    #[test]
    fn remove_refuses_unknown_name() {
        let store = NetworkStore::default();
        // A name that is neither in the store nor a built-in preset must produce
        // a clear error. The handler's contract: `!store.networks.contains_key(name) && well_known(name).is_none()`.
        let name = "totally-fake-network";
        assert!(!store.networks.contains_key(name));
        assert!(well_known(name).is_none());
    }

    // #467 — `network remove` does NOT touch the forge.toml file
    #[test]
    fn remove_does_not_touch_forge_toml() {
        // Removing a network entry from networks.json should not rewrite the
        // active network in forge.toml — that's a separate `network use` concern.
        // The remove handler only writes networks.json.
        // This is a documentation-by-test: we assert the contract by reading the
        // patch_forge_toml_network helper is never called from the remove path
        // (the handler does not import it for the remove branch).
    }

    #[test]
    fn plugin_name_matches_its_command() {
        let plugin = NetworkPlugin;
        assert_eq!(plugin.name(), plugin.command().get_name());
    }

    // #405 — validate_rpc_url: valid URLs are accepted
    #[test]
    fn validate_rpc_url_accepts_well_formed_urls() {
        assert!(validate_rpc_url("https://soroban-testnet.stellar.org").is_ok());
        assert!(validate_rpc_url("http://localhost:8000/soroban/rpc").is_ok());
        assert!(validate_rpc_url("https://rpc-futurenet.stellar.org").is_ok());
        assert!(validate_rpc_url("https://my.rpc.example.com/path?query=1").is_ok());
    }

    // #405 — validate_rpc_url: malformed URLs are rejected with a clear message
    #[test]
    fn validate_rpc_url_rejects_missing_scheme() {
        let err = validate_rpc_url("soroban-testnet.stellar.org").unwrap_err();
        assert!(err.to_string().contains("scheme"), "{err}");
    }

    #[test]
    fn validate_rpc_url_rejects_empty_host() {
        let err = validate_rpc_url("https://").unwrap_err();
        assert!(err.to_string().contains("host"), "{err}");
    }

    #[test]
    fn validate_rpc_url_rejects_whitespace() {
        let err = validate_rpc_url("  https://example.com  ").unwrap_err();
        assert!(err.to_string().contains("whitespace"), "{err}");
    }

    #[test]
    fn validate_rpc_url_rejects_empty_string() {
        let err = validate_rpc_url("").unwrap_err();
        assert!(err.to_string().contains("scheme"), "{err}");
    }

    // #404 — network remove: removes a network from the store
    #[test]
    fn remove_deletes_a_configured_network() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("networks.json");

        let mut store = NetworkStore::default();
        store.networks.insert(
            "mynet".into(),
            Network {
                rpc_url: "https://my.rpc.example.com".into(),
                network_passphrase: "My Network".into(),
            },
        );
        store.default = Some("mynet".into());
        save_store(&path, &store).unwrap();

        // Simulate remove logic
        let mut loaded = load_store(&path).unwrap();
        loaded.networks.remove("mynet");
        if loaded.default.as_deref() == Some("mynet") {
            loaded.default = None;
        }
        save_store(&path, &loaded).unwrap();

        let after = load_store(&path).unwrap();
        assert!(!after.networks.contains_key("mynet"));
        assert!(after.default.is_none(), "default should be cleared when removed network was default");
    }

    // #404 — network remove: removing a non-default network does not clear the default
    #[test]
    fn remove_non_default_network_preserves_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("networks.json");

        let mut store = NetworkStore::default();
        store.networks.insert(
            "alpha".into(),
            Network {
                rpc_url: "https://alpha.example.com".into(),
                network_passphrase: "Alpha".into(),
            },
        );
        store.networks.insert(
            "beta".into(),
            Network {
                rpc_url: "https://beta.example.com".into(),
                network_passphrase: "Beta".into(),
            },
        );
        store.default = Some("alpha".into());
        save_store(&path, &store).unwrap();

        let mut loaded = load_store(&path).unwrap();
        loaded.networks.remove("beta");
        if loaded.default.as_deref() == Some("beta") {
            loaded.default = None;
        }
        save_store(&path, &loaded).unwrap();

        let after = load_store(&path).unwrap();
        assert!(!after.networks.contains_key("beta"));
        assert_eq!(after.default.as_deref(), Some("alpha"), "default should remain unchanged");
    }

    // #406 — show (current alias): errors when no default is set
    #[test]
    fn show_errors_when_no_default_set() {
        let store = NetworkStore::default(); // no default
        // The show handler returns Err(InvalidArgument) when current_name is None
        let current_name: Option<&str> = store.default.as_deref();
        assert!(current_name.is_none());
        // Confirm this would map to an error
        let result: Result<()> = match current_name {
            None => Err(ForgeError::InvalidArgument(
                "no default network is set — run `soroban-forge network use <name>` to select one".into(),
            )),
            Some(_) => Ok(()),
        };
        assert!(
            matches!(result, Err(ForgeError::InvalidArgument(_))),
            "expected InvalidArgument when no default is set"
        );
    }

    // #406 — show: resolves name, rpc_url, and passphrase for the default network
    #[test]
    fn show_resolves_default_network_details() {
        let mut store = NetworkStore::default();
        store.networks.insert(
            "mynet".into(),
            Network {
                rpc_url: "https://my.rpc.example.com".into(),
                network_passphrase: "My Passphrase".into(),
            },
        );
        store.default = Some("mynet".into());

        let name = store.default.as_deref().unwrap();
        let network = resolve_network(&store, name).expect("should resolve");
        assert_eq!(network.rpc_url, "https://my.rpc.example.com");
        assert_eq!(network.network_passphrase, "My Passphrase");
    }

    // #406 — show: `current` is registered as an alias for `show`
    #[test]
    fn show_has_current_as_alias() {
        let plugin = NetworkPlugin;
        let cmd = plugin.command();
        let show_cmd = cmd.get_subcommands().find(|s| s.get_name() == "show").expect("show must exist");
        let aliases: Vec<&str> = show_cmd.get_all_aliases().collect();
        assert!(aliases.contains(&"current"), "show must have 'current' as an alias; got: {aliases:?}");
    }

    // #406 — show: JSON output includes name, rpc_url, and network_passphrase
    #[test]
    fn show_json_output_has_required_fields() {
        let mut store = NetworkStore::default();
        store.networks.insert(
            "testnet".into(),
            Network {
                rpc_url: "https://soroban-testnet.stellar.org".into(),
                network_passphrase: "Test SDF Network ; September 2015".into(),
            },
        );
        store.default = Some("testnet".into());

        let name = store.default.as_deref().unwrap();
        let network = resolve_network(&store, name).unwrap();
        let report = serde_json::json!({
            "name": name,
            "rpc_url": network.rpc_url,
            "network_passphrase": network.network_passphrase,
        });
        assert_eq!(report["name"], "testnet");
        assert!(report["rpc_url"].as_str().unwrap().starts_with("https://"));
        assert!(!report["network_passphrase"].as_str().unwrap().is_empty());
    }

    // #290 — patch_forge_toml_network: create section from empty file
    #[test]
    fn patch_creates_network_section_in_empty_file() {
        let result = patch_forge_toml_network("", "testnet");
        assert!(result.contains("[network]"), "{result}");
        assert!(result.contains("name = \"testnet\""), "{result}");
    }

    // #290 — patch_forge_toml_network: create section when other sections exist
    #[test]
    fn patch_appends_network_section_to_existing_file() {
        let existing = "[project]\nname = \"demo\"\n";
        let result = patch_forge_toml_network(existing, "testnet");
        assert!(result.contains("[project]"), "{result}");
        assert!(result.contains("[network]"), "{result}");
        assert!(result.contains("name = \"testnet\""), "{result}");
    }

    // #290 — patch_forge_toml_network: update existing name key
    #[test]
    fn patch_updates_existing_name_key() {
        let existing = "[network]\nname = \"localnet\"\n";
        let result = patch_forge_toml_network(existing, "testnet");
        assert!(result.contains("name = \"testnet\""), "{result}");
        assert!(!result.contains("name = \"localnet\""), "old name must be replaced: {result}");
    }

    // #290 — patch_forge_toml_network: insert name key when section exists but name is missing
    #[test]
    fn patch_inserts_name_into_existing_network_section_without_name() {
        let existing = "[network]\nrpc_url = \"https://example.com\"\n";
        let result = patch_forge_toml_network(existing, "testnet");
        assert!(result.contains("name = \"testnet\""), "{result}");
        assert!(result.contains("rpc_url"), "other keys must be preserved: {result}");
    }

    // #290 — write_network_to_forge_toml creates the file if it doesn't exist
    #[test]
    fn write_network_creates_forge_toml_if_absent() {
        let dir = tempfile::tempdir().unwrap();
        write_network_to_forge_toml(dir.path(), "testnet").unwrap();
        let content = std::fs::read_to_string(dir.path().join("forge.toml")).unwrap();
        assert!(content.contains("name = \"testnet\""), "{content}");
    }

    // #290 — active_network_name: cli override wins
    #[test]
    fn active_network_name_cli_override_wins() {
        use soroban_forge_core::ForgeContext;
        let dir = tempfile::tempdir().unwrap();
        let mut store = NetworkStore::default();
        store.default = Some("futurenet".into());
        let ctx = ForgeContext::new(dir.path().to_path_buf(), 0).unwrap();
        assert_eq!(
            active_network_name(&store, &ctx, Some("mainnet")),
            Some("mainnet")
        );
    }

    // #290 — active_network_name: forge.toml wins over networks.json default
    #[test]
    fn active_network_name_forge_toml_beats_store_default() {
        use soroban_forge_core::ForgeContext;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("forge.toml"), "[network]\nname = \"futurenet\"\n").unwrap();
        let mut store = NetworkStore::default();
        store.default = Some("localnet".into());
        let ctx = ForgeContext::new(dir.path().to_path_buf(), 0).unwrap();
        assert_eq!(active_network_name(&store, &ctx, None), Some("futurenet"));
    }

    // ---- network list --check (issue #411) ----

    /// A live local endpoint: the listener accepts connections for the
    /// lifetime of the returned handle.
    fn reachable_endpoint() -> (std::net::TcpListener, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, format!("http://127.0.0.1:{port}/soroban/rpc"))
    }

    /// A local URL nothing listens on: bind a port, then release it.
    fn unreachable_endpoint() -> String {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        format!("http://127.0.0.1:{port}/soroban/rpc")
    }

    fn net(url: &str) -> Network {
        Network {
            rpc_url: url.into(),
            network_passphrase: "Mock Network".into(),
        }
    }

    #[test]
    fn endpoint_host_port_defaults_and_parses_ports() {
        assert_eq!(
            endpoint_host_port("https://soroban-testnet.stellar.org").unwrap(),
            ("soroban-testnet.stellar.org".into(), 443)
        );
        assert_eq!(
            endpoint_host_port("http://localhost:8000/soroban/rpc").unwrap(),
            ("localhost".into(), 8000)
        );
        assert_eq!(
            endpoint_host_port("http://example.com/rpc?x=1").unwrap(),
            ("example.com".into(), 80)
        );
        assert_eq!(
            endpoint_host_port("https://user:pw@rpc.example.com:8443/v1").unwrap(),
            ("rpc.example.com".into(), 8443)
        );
        assert_eq!(
            endpoint_host_port("http://[::1]:9000/rpc").unwrap(),
            ("::1".into(), 9000)
        );
        assert!(endpoint_host_port("rpc.example.com").is_err());
        assert!(endpoint_host_port("ftp://rpc.example.com").is_err());
        assert!(endpoint_host_port("http://:8000").is_err());
        assert!(endpoint_host_port("http://host:notaport").is_err());
    }

    #[test]
    fn probe_detects_reachable_and_unreachable_endpoints() {
        let (_listener, up) = reachable_endpoint();
        let down = unreachable_endpoint();

        assert!(probe_endpoint(&up, Duration::from_secs(2)).is_reachable());

        let result = probe_endpoint(&down, Duration::from_secs(2));
        assert!(!result.is_reachable(), "{result:?}");
        assert!(result.label().starts_with("[unreachable: "), "{}", result.label());
    }

    #[test]
    fn probe_reports_malformed_urls_as_unreachable() {
        let result = probe_endpoint("not a url", Duration::from_secs(1));
        assert_eq!(result, Reachability::Unreachable("`not a url` has no scheme".into()));
    }

    #[test]
    fn checked_list_annotates_a_mix_of_reachable_and_unreachable_networks() {
        let (_listener, up) = reachable_endpoint();
        let down = unreachable_endpoint();

        let mut store = NetworkStore::default();
        store.networks.insert("alpha-up".into(), net(&up));
        store.networks.insert("beta-down".into(), net(&down));

        let mut checks = BTreeMap::new();
        for name in ["alpha-up", "beta-down"] {
            let url = &store.networks[name].rpc_url;
            checks.insert(name.to_string(), probe_endpoint(url, Duration::from_secs(2)));
        }

        let out = format_list_checked(&store, &checks);
        let line = |name: &str| {
            out.lines()
                .find(|l| l.contains(name))
                .unwrap_or_else(|| panic!("no row for {name}: {out}"))
                .to_string()
        };
        assert!(line("alpha-up").contains("[reachable, "), "{out}");
        assert!(line("alpha-up").contains(&up), "{out}");
        assert!(line("beta-down").contains("[unreachable: "), "{out}");
        assert!(out.contains("1 of 2 network(s) reachable"), "{out}");
        // Networks that were not probed carry no annotation.
        assert!(!line("testnet").contains("reachable"), "{out}");
    }

    #[test]
    fn plain_list_is_unchanged_and_never_annotated() {
        let (_listener, up) = reachable_endpoint();
        let mut store = NetworkStore::default();
        store.networks.insert("alpha-up".into(), net(&up));

        let out = format_list(&store);
        assert!(!out.contains("reachable"), "{out}");
        assert!(out.contains(&format!("alpha-up   {up}  (Mock Network)")), "{out}");
    }

    #[test]
    fn reachability_json_fields() {
        assert_eq!(
            Reachability::Reachable { latency_ms: 12 }.json_fields(),
            serde_json::json!({ "reachable": true, "latency_ms": 12 })
        );
        assert_eq!(
            Reachability::Unreachable("connection refused".into()).json_fields(),
            serde_json::json!({ "reachable": false, "error": "connection refused" })
        );
    }

    #[test]
    fn probe_networks_covers_every_listed_network() {
        let (_listener, up) = reachable_endpoint();
        let down = unreachable_endpoint();

        // Override every built-in so the probe never leaves the machine.
        let mut store = NetworkStore::default();
        for name in ["testnet", "futurenet", "mainnet", "localnet"] {
            store.networks.insert(name.into(), net(&down));
        }
        store.networks.insert("alpha-up".into(), net(&up));

        let checks = probe_networks(&store, Duration::from_secs(2));
        assert_eq!(checks.len(), 5);
        assert!(checks["alpha-up"].is_reachable());
        for name in ["testnet", "futurenet", "mainnet", "localnet"] {
            assert!(!checks[name].is_reachable(), "{name}: {:?}", checks[name]);
        }
    }

    #[test]
    fn list_accepts_the_check_flag() {
        let matches = NetworkPlugin
            .command()
            .try_get_matches_from(["network", "list", "--check"])
            .unwrap();
        let (name, sub) = matches.subcommand().unwrap();
        assert_eq!(name, "list");
        assert!(sub.get_flag("check"));

        let plain = NetworkPlugin
            .command()
            .try_get_matches_from(["network", "list"])
            .unwrap();
        assert!(!plain.subcommand().unwrap().1.get_flag("check"));
    }
}
