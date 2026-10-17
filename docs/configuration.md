# Configuration

## Global Flags

| Flag | Env Variable | Default | Description |
|------|-------------|---------|-------------|
| `--verbose` | `SOROBAN_FORGE_VERBOSE` | `false` | Debug logging |
| `--quiet` | `SOROBAN_FORGE_QUIET` | `false` | Suppress output |
| `--json` | `SOROBAN_FORGE_JSON` | `false` | JSON output |
| `--yes`/`-y` | `SOROBAN_FORGE_YES` | `false` | Auto-confirm |
| `--cwd`/`-C` | — | cwd | Run from DIR |
| `--offline` | `SOROBAN_FORGE_OFFLINE` | `false` | No network |

For configurable command values, precedence is **CLI flag > project
`forge.toml` > user `config.toml` > built-in default**. Environment variables
continue to provide the documented values for global flags; they do not
override project or user configuration values. `--config PATH` selects an
explicit project-config file instead of discovering `forge.toml`, while the
user config remains the fallback for values it omits.

## User defaults

Optional user-wide defaults live at
`~/.config/soroban-forge/config.toml` (or
`$XDG_CONFIG_HOME/soroban-forge/config.toml` when `XDG_CONFIG_HOME` is set).
Missing files are ignored; an existing but unreadable or invalid TOML file is
reported as an error. The file
accepts the same network and template sections as `forge.toml`, plus a default
source identity:

```toml
[network]
name = "testnet"
# rpc_url = "https://soroban-testnet.stellar.org"
# passphrase = "Test SDF Network ; September 2015"

[identity]
default = "deployer"

[scaffold]
default_template = "hello-world"
```

`--network`, `--rpc-url`, `--network-passphrase`, `--source`, and
`--template` override configured values for that invocation. Project values
override user values independently, so a project can override only its
network while still inheriting the user's identity and template.

## Identity and network stores

Two more files live next to the user config, written by the `identity` and
`network` subcommands rather than edited by hand:

| File | Written by |
|------|------------|
| `~/.config/soroban-forge/identities.json` | `identity generate`, `identity import`, `identity use`, `identity fund`, `identity remove` |
| `~/.config/soroban-forge/networks.json` | `network add`, `network use`, `network remove` |

Both honour `XDG_CONFIG_HOME` the same way `config.toml` does.

Every command that changes either file reads it, edits it and writes it back.
That sequence is serialized with an advisory lock so parallel invocations
cannot lose each other's writes — without it, two `identity generate` runs
started at the same time would both read the same store, each add their
identity, and the second write would silently discard the first, even though
both commands printed a public key and exited `0`. The same protection covers
`networks.json`, which is written the same way.

The lock is a `identities.json.lock` / `networks.json.lock` file alongside the
store, held only for the read-edit-write itself — never across a network call,
so `identity fund` does not block other commands while friendbot answers. A
command that cannot take the lock within 10 seconds fails rather than
overwriting, and names the lock file in its error. A lock file left behind by
a killed process is ignored (and removed) after 60 seconds, so a crash cannot
wedge later commands. The lock is advisory: it protects soroban-forge
invocations from each other, not against another program, or you, editing the
JSON by hand while a command runs.

The writes themselves are atomic — the store is written to a temporary file in
the same directory and renamed into place — so an interrupted command leaves
either the old file or the new one, never a half-written store.

## `forge.toml` reference

Every key any command actually reads from the optional `forge.toml` project
file — one row per key, generated from an audit of `ctx.config` usage across
every crate (not just `soroban-forge-core::config::ForgeConfig`; `optimize`
parses its own `[optimize]` section directly). `crates/core/src/config.rs`
tests (`unknown_keys`) flag anything else in the file as an unrecognized key.

| Key | Type | Default | Read by |
|-----|------|---------|---------|
| `[project] name` | string | none (falls back to `Cargo.toml`'s `[package] name`, or the directory name) | `ci-init` (default project name for generated workflow templates); also shown by `config` |
| `[project] authors` | array of strings | `[]` | `new` (first entry seeds the generated `Cargo.toml`'s author, via `ForgeConfig::author()`; falls back to the local git identity, then a placeholder, when unset); also shown by `config` |
| `[scaffold] default_template` | string | `"hello-world"` | `new` (default `--template` when the flag is omitted); also shown by `config` |
| `[defaults] timeout_secs` | integer (seconds) | none | **none** — parsed and printed by `config`, but no command ever applies it; the timeout commands actually honor (`ctx.timeout()`) comes only from the `--timeout` flag / `SOROBAN_FORGE_TIMEOUT` |
| `[defaults] max_size` | integer (bytes) | none | **none** — parsed and printed by `config`, but no command applies it. This is *not* the wasm-size budget `optimize --check` enforces; that's the separate `[optimize] max-size` key below |
| `[defaults] update_check` | boolean | `true` | Persistent opt-out for the release-version check. Set to `false` to disable the background check on every invocation. Can also be suppressed per-session with `SOROBAN_FORGE_NO_UPDATE_CHECK=1` or `--offline` |
| `[defaults.ci-init] max_size` (TOML key `ci-init`, alias `ci_init`) | integer (bytes) | `65536` | `ci-init` (default for `--max-size`, used in the generated `contract-size`/Buildkite pipeline's size check) |
| `[network] name` | string | none (falls back to `"testnet"`, [`DEFAULT_NETWORK`](../crates/verify/src/lib.rs)) | `network use` (writes this key); `deploy`, `invoke`, `verify` (default `--network` via `NetworkArgs::resolve`, CLI flags win); `doctor` (health-check target) |
| `[network] rpc_url` | string | none | `deploy`, `invoke`, `verify` (default `--rpc-url` via `NetworkArgs::resolve`); `doctor` (health-check endpoint) |
| `[network] passphrase` | string | none | `deploy`, `invoke`, `verify` (default `--network-passphrase` via `NetworkArgs::resolve`); `identity fund` (refuses to run against a passphrase containing `"Public Global Stellar Network"`, i.e. mainnet) |
| `[identity] default` | string | none | `deploy`, `invoke` (default `--source` identity; CLI `--source` wins) |
| `[optimize] max-size` (alias `max_size`) | integer (bytes) | none | `optimize --check` (fails when the built wasm exceeds this). Read directly from `forge.toml` by `soroban-forge-optimize`, independent of `ForgeConfig` above — so it is *not* covered by `config`'s unknown-key warning or its resolved-config printout |
| `[bindings.ts] output` | string (path) | `"bindings/typescript"` | `bindings ts` (default output directory for the generated TypeScript package, relative to the contract project directory; overridden by `--out-dir` / `--output` on the command line) |

`[defaults] timeout_secs` and `[defaults] max_size` are parsed and echoed
by `soroban-forge config` even though nothing currently consults them —
they're dead configuration, kept here for accuracy rather than silently
dropped from the table.
