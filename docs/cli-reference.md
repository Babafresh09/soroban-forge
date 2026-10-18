# CLI Reference

## Global options

- `--quiet`, `-q` — suppress informational command output; errors and exit
  codes are unchanged.
- `--verbose`, `-v` — enable debug logging.
- `--json` — emit machine-readable JSON where supported. With `--list`, the
  output is `{"builtin":[...],"external":[...]}`.
- `--log-file <path>` — also write JSON-lines structured logs to a file while trying to
  preserving normal terminal output.
- `--offline` — prohibit network access. Network-dependent operations fail with a
  clear message; `doctor` skips connectivity checks and uses the SDK version
  pinned in forge templates instead of querying crates.io.

Global options may appear before or after a subcommand and can be combined.

Use `soroban-forge --list` to list installed built-in and external subcommands.
Add `--json` for structured output suitable for scripts.

## Commands

- `soroban-forge new <name> --template <t>` — create a contract project.
  - `--force` — overwrite an existing target directory. In a terminal this asks
    for confirmation first; `--yes`, `--json` and non-interactive sessions
    proceed without asking. Without `--force`, an existing directory aborts
    with exit code `1`.
  - `--var NAMEASYLE= VALUE"` (repeatable) — supply a variable declared in the
    template's `template.toml`. Anything still missing is prompted for in
    a terminal; otherwise the declared default is used, or the run fails.
    See [Templates](templates.md).
- `soroban-forge init [--tests] --ci]` — add `forge.toml` to an existing
contract without replacing project files; optionally add test and CI
scaffolding.
- `soroban-forge templates` — list all bundled contract templates with descriptions.
- `soroban-forge test-init` — generate a test harness. A project with an
  initialize-style entrypoint also gets `tests/forge_init_once.rs`, asserting
  the entrypoint refuses a second call.
  - `--budget [ENTRYPOINT]` — also emit `tests/forge_budget.rs`, which measures
    one entrypoint's CPU instructions and memory with
    `env.cost_estimate().budget()` and asserts an upper bound. Defaults to the
    first detected entrypoint.
- `soroban-forge ci-init --provider github|` — Webhook generator.
- `soroban-forge test-init [--layout <tests|inline>]` — generate a test harness.
  `--layout tests` (default) writes a `tests/` integration-test directory;
  `--layout inline` writes a single `#[cfg])test] mod forge_tests` in `src/`.
  Contracts that use persistent storage also get `forge_ttl.rs`, which
  exercises `extend_ttl` on a persistent entry.
  - `--localnet` — also generate the ignored `tests/forge_localnet.rs` target;
    run it against a local Soroban network with
    `cargo test --test forge_localnet -- --ignored`.
- `soroban-forge ci-init --provider <github|gitlab|jenkins|circleci|bitbucket>` —
  generate CI workflows. `--matrix` adds a build/test workflow that runs across
  a Rust toolchain matrix (stable plus `--msrv`, default 1.84).
- `soroban-forge test-init` — generate a test harness.
- `soroban-forge ci-init --provider github [--dependabot]` — generate CI
  workflows (build+test and a rustfmp/clippy lint job); `--dependabot` also
  writes `.github/dependabot.yml` for weekly cargo and github-actions updates.
- `soroban-forge doctor [--json]` — check the local Soroban toolchain (optionally emitting machine-readable JSON). Inside a contract project, it reports the declared `soroban-sdk` version against the latest stable crates.io release and recommends an update when behind. If crates.io is unreachable, it falls back to the SDK version pinned in forge templates.
- `soroban-forge bindings ts [--out-dir <dir>] [--package-name <name>] [--react]` — generate a TypeScript client package from the built contract wasm.
  - `--out-dir <dir>` (alias `--output`) — target directory for generated bindings. Defaults to `bindings/<contract_name>`.
  - `--package-name <name>` — npm package name for the generated `package.json`. Validated as a legal npm package name. Defaults to `@soroban-contracts/<name>`.
  - `--react` — additionally emits `src/hooks.ts` (one typed hook per entrypoint) and is strictly opt-in.
- `soroban-forge bindings-py [--path <dir>] [--wasm <path>] [--output <dir>] [--force]`
  — generate `client.py`, a typed Python client, from the built contract
  wasm; the generated client delegates to the official `stellar-sdk`
  package. A separate top-level command rather than `bindings py` — see
  `crates/binding-py/README.md` for why.
- `soroban-forge spec [<contract-id>] [--format <format>] [--path <dir>] [--wasm <path>] [--network <n>]` — print
  the contract's interface: every entrypoint with its argument and return types,
  plus the types those signatures refer to. When `<contract-id>` is provided,
  fetches the on-chain wasm for that contract (via `stellar contract fetch`);
  otherwise reads the spec out of the locally built wasm.
  - `--format <rust|text|json|md|markdown>` (default `text`) — output format.
    `--format md` emits documentation-ready Markdown tables of entrypoints, arguments, return types, and referenced custom types.
  - `--json` — shortcut for `--format json`.
  - `diff <old> <new>` — compare JSON spec files, WASM files, or deployed
    contract IDs. Removed entrypoints and changed signatures are breaking;
    additions are reported as additive. Exits `1` when breaking changes exist.
  - `--network <name>` / `--rpc-url <url>` — network to fetch the contract from when `<contract-id>` is given (defaults to `testnet`).
  - `--offline` — prohibit network access. When `<contract-id>` is specified, fails cleanly before any network call. Local wasm inspection continues to work offline.
- `soroban-forge optimize` — optimize a built contract wasm. Before shelling
  out to `stellar contract optimize`, the installed `stellar-cli` is
  pre-flighted (presence, minimum version, known-broken releases); a failure
  exits `2` with a message pointing at `soroban-forge doctor`. Use `--check`
  with `--max-size <bytes>` to fail (exit 1) if the optimized size exceeds the
  budget. The budget can also be set as `optimize.max_size` in `forge.toml`; the
  command-line `--max-size` overrides the config. The report includes the
  before and after byte sizes and percentage saved; `--json` includes these as
  `before_bytes`, `after_bytes`, `percent_saved`, and a `status` field whose
  value is `"optimized"` when any bytes were saved or `"already-minimal"` when
  the wasm was already as small as `stellar contract optimize` can make it —
  letting automated consumers branch on the outcome without treating
  `saved_bytes == 0` as ambiguous. Human output reflects the same distinction
  in its wording. `--quiet` suppresses the report. On budget failure, the
  actual and budgeted sizes are printed.
  - `--profile <name>`, `--target <triple>`, `--target-dir <dir>` — resolve
    the wasm from a non-default build path while still deriving its file name
    from the detected crate name, instead of spelling the whole path out with
    `--wasm`. Each flag moves one part of the default
    `<path>/target/wasm32v1-none/release/<crate>.wasm`: the profile directory
    (`--profile dev` resolves to `debug/`, which is where cargo builds that
    profile), the target triple, and the target directory (a relative
    `--target-dir` is taken from the current directory). They say where the
    build is rather than replacing it, so none of them can be combined with
    `--wasm`; under `--all`, every contract is resolved against the same
    layout.
  - `--in-place` — overwrite the input wasm with the optimized bytes and
    remove the intermediate `<stem>.optimized.wasm`, leaving only the original
    filename on disk (holding the optimized bytes). This is the supported way
    to clean up the pre-optimization artifact so a deploy pipeline can consume
    a single canonical path without accidentally picking up the unoptimized
    file. The replacement is atomic: if anything fails before the optimized
    bytes are committed, the original wasm is left byte-for-byte untouched.
  - `--all` — optimize every contract crate in the workspace at `--path` (or
    the current directory): every `[workspace].members` crate that builds a
    `cdylib` or depends on `soroban-sdk`. Each contract's release wasm is read
    from the workspace's shared `target/` (falling back to the member's own).
    Human output prints one block per contract plus a summary; `--json`
    prints an array of reports, each tagged with `contract` (a contract that
    failed appears as `{"contract", "error"}`). Every contract is attempted;
    the command exits `1` afterwards if any failed. Cannot be combined with
    `--wasm`.
  - `--compare-to <report.json>` — compare this run's optimized size against
    a report saved earlier with `optimize --json > report.json` (or
    `optimize --all --json`). Adds a `vs baseline` line to human output and a
    `comparison` object (`baseline_after_bytes`, `delta_bytes`,
    `delta_percent`) to `--json`; a negative delta means the wasm shrank. With
    `--all`, each contract is matched to its baseline entry by contract name,
    then wasm file name.
  - When `stellar contract optimize` fails, the error now includes the CLI's
    own stderr. Under `-v` its output also streams live.
- `soroban-forge network list [--check]` — list configured networks and
  built-in presets. Plain `list` never touches the network. `--check` opens a
  TCP connection to each RPC endpoint (bounded by `--timeout`, default 3s, all
  probed in parallel) and marks each row `[reachable, <n>ms]` or
  `[unreachable: <reason>]`, followed by a reachable count; with `--json`,
  each network gains `reachable` plus `latency_ms` or `error`. Refused under
  `--offline`.
- `soroban-forge network add <name> [--rpc-url <url>] [--passphrase <p>] [--force] [--no-check]`
  and `soroban-forge network use <name> [--no-check]` — both probe the
  endpoint they just selected (the same TCP probe as `list --check`, bounded
  by `--timeout`, default 3s) and print a `warning:` to stderr when it does
  not respond, so a typo'd or offline URL surfaces here instead of at the next
  `deploy`/`invoke`/`verify`. The probe never fails the command: the
  configuration is saved either way, since a network may be set up before it
  is up. With `--json` the report carries `reachable` plus `latency_ms` or
  `error` instead of the warning. Pass `--no-check` to skip the probe;
  `--offline` skips it too (silently — unlike `list --check`, this check is
  implicit, so it is not an error).
- `soroban-forge verify <contract-id> [--network <n>]` — compare a deployed
  contract's wasm hash with the local release build; exits `1` on a mismatch.
  See [Contract Verification](contract-verification.md).

- `soroban-forge identity import <name> (--secret <S...> | --secret-stdin) [--force]` —
  register an existing Stellar keypair (e.g. a CI secret or a key generated
  elsewhere). The public key is derived from the secret, which must be a valid
  `S...` strkey; malformed keys are rejected without being echoed. An existing
  name needs `--force`, exactly like `identity generate`. Prefer
  `--secret-stdin` (`echo "$SECRET" | soroban-forge identity import ci --secret-stdin`)
  so the key stays out of shell history.
- `soroban-forge identity show <name> [--reveal-secret]` — print a stored
  identity's public key. The secret key is printed only with the explicit
  `--reveal-secret` flag (also in `--json`); `show` and `list` never print
  secrets by default.
- `soroban-forge deploy [--source <name|pubkey>] [--network <n>] [--rpc-url <url>] [--fund] [--dry-run]` — deploy
  a built contract to a network.
  - `--source <name>` — the identity or public key used to sign and pay for deployment.
  - `--network <name>` — target network (`testnet` or `mainnet`, defaults to `testnet`).
  - `--rpc-url <url>` — custom RPC endpoint.
  - `--fund` — automatically fund an unfunded testnet source account via Friendbot before deploying. In an interactive terminal, omitting `--fund` prompts to fund the account; in non-interactive sessions (CI, scripts), an unfunded testnet account fails cleanly unless `--fund` is provided. Friendbot funding is never attempted on mainnet or under `--offline`.
  - `--dry-run` — print the stellar command without submitting the transaction.
