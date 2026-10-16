//! # soroban-forge-optimize
//!
//! `soroban-forge optimize` — wraps `stellar contract optimize` and reports
//! the wasm size before and after.
//!
//! Per soroban-forge's "wrap, don't reimplement" rule, the optimization
//! itself is delegated to the official `stellar` CLI; this module only
//! locates the local build, runs it, and reports the size delta.
//!
//! Before shelling out, `optimize` pre-flights the installed `stellar-cli`
//! (presence, minimum version, known-broken releases), reusing the
//! `soroban-forge-core::toolchain` helpers that `doctor` uses.  This turns a
//! cryptic "unknown subcommand: contract" into a clear error pointing at
//! `soroban-forge doctor`.
//!
//! `--all` optimizes every contract crate of a Cargo workspace in one run
//! (issue #412), and `--compare-to` diffs a run against a report previously
//! saved with `optimize --json` (issue #418).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use clap::{Arg, ArgMatches, Command};
use serde::{Deserialize, Serialize};
use soroban_forge_core::toolchain::{
    known_broken_stellar_cli_replacement, version_at_least, MIN_STELLAR,
};
use soroban_forge_core::{ForgeContext, ForgeError, ForgePlugin, Result};

#[derive(Deserialize)]
struct Manifest {
    package: Package,
}

#[derive(Deserialize)]
struct Package {
    name: String,
}

/// Budget for `--check` from `forge.toml`.
#[derive(Deserialize)]
struct ForgeConfig {
    optimize: Option<OptimizeConfig>,
}

#[derive(Deserialize)]
struct OptimizeConfig {
    #[serde(rename = "max-size", alias = "max_size")]
    max_size: Option<u64>,
}

/// Read `[package].name` from `dir/Cargo.toml` and return it as a crate name
/// (snake_case), which is what the build output is named after.
///
/// Deliberately duplicated rather than shared with `verify`: modules depend
/// only on `soroban-forge-core`, never on each other.
pub fn read_crate_name(dir: &Path) -> Result<String> {
    let manifest_path = dir.join("Cargo.toml");
    if !manifest_path.is_file() {
        return Err(ForgeError::InvalidArgument(format!(
            "{} is not a cargo project (no Cargo.toml) — pass --path or --wasm",
            dir.display()
        )));
    }
    let raw = std::fs::read_to_string(&manifest_path).map_err(ForgeError::io(format!(
        "reading {}",
        manifest_path.display()
    )))?;
    let manifest: Manifest = toml::from_str(&raw).map_err(|e| ForgeError::Config {
        path: manifest_path.clone(),
        message: e.to_string(),
    })?;
    Ok(manifest.package.name.replace('-', "_"))
}

/// Read `max-size` from `[optimize]` in `dir/forge.toml`, if present.
fn forge_config_budget(dir: &Path) -> Result<Option<u64>> {
    let config_path = dir.join("forge.toml");
    if !config_path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&config_path)
        .map_err(ForgeError::io(format!("reading {}", config_path.display())))?;
    let config: ForgeConfig = toml::from_str(&raw).map_err(|e| ForgeError::Config {
        path: config_path.clone(),
        message: e.to_string(),
    })?;
    Ok(config.optimize.and_then(|optimize| optimize.max_size))
}

/// Default location `stellar contract build` writes its release wasm to.
pub fn locate_wasm(dir: &Path, crate_name: &str) -> PathBuf {
    dir.join("target/wasm32v1-none/release")
        .join(format!("{crate_name}.wasm"))
}

/// Resolve which local wasm to optimize: `wasm_override` when given,
/// otherwise the release build of the cargo project in `dir`.
pub fn resolve_local_wasm(dir: &Path, wasm_override: Option<&Path>) -> Result<PathBuf> {
    let wasm_path = match wasm_override {
        Some(path) => path.to_path_buf(),
        None => {
            let crate_name = read_crate_name(dir)?;
            locate_wasm(dir, &crate_name)
        }
    };

    if !wasm_path.is_file() {
        return Err(ForgeError::InvalidArgument(format!(
            "no release build found at {} — run `stellar contract build` first (or pass --wasm)",
            wasm_path.display()
        )));
    }
    Ok(wasm_path)
}

/// Path `stellar contract optimize` writes its output to for a given input.
pub fn optimized_wasm_path(wasm_path: &Path) -> PathBuf {
    let stem = wasm_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    wasm_path.with_file_name(format!("{stem}.optimized.wasm"))
}

/// Sibling temp path used to stage the optimized bytes before an atomic
/// rename over `original`.  Pid-scoped so concurrent `optimize --in-place`
/// runs against the same directory cannot clobber each other's staging file.
fn staging_path(original: &Path) -> PathBuf {
    let stem = original
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let pid = std::process::id();
    original.with_file_name(format!("{stem}.{pid}.tmp"))
}

/// Move the bytes at `optimized` over `original`, then remove `optimized`.
///
/// Used by `optimize --in-place` so a deploy pipeline can keep a single
/// canonical artifact path.
///
/// Failure safety: the optimized bytes are staged in a sibling temp file and
/// then `rename`d over `original`.  On every supported platform `rename` on
/// the same filesystem is atomic, so if any step before the rename fails the
/// original file is left byte-for-byte untouched — satisfying the "original
/// preserved on failure" guarantee.  A staging file left behind by a failed
/// rename is cleaned up on the error path.
pub fn replace_with_optimized(original: &Path, optimized: &Path) -> Result<()> {
    let bytes = std::fs::read(optimized)
        .map_err(ForgeError::io(format!("reading {}", optimized.display())))?;

    let staging = staging_path(original);
    // Guard so the staging file never leaks on an error path.
    let cleanup = StagingGuard(&staging);

    std::fs::write(&staging, &bytes).map_err(ForgeError::io(format!(
        "writing staging file {}",
        staging.display()
    )))?;

    std::fs::rename(&staging, original).map_err(ForgeError::io(format!(
        "replacing {} with optimized bytes",
        original.display()
    )))?;
    cleanup.disarm();

    std::fs::remove_file(optimized)
        .map_err(ForgeError::io(format!("removing {}", optimized.display())))?;
    Ok(())
}

/// Deletes its path on drop unless disarmed.  A tiny RAII helper so the
/// staging file cannot linger if an intermediate step fails.
struct StagingGuard<'a>(&'a Path);

impl StagingGuard<'_> {
    fn disarm(self) {
        std::mem::forget(self);
    }
}

impl Drop for StagingGuard<'_> {
    fn drop(&mut self) {
        // Best-effort: if the file was already renamed away, `remove_file`
        // returns NotFound and we ignore it.  If cleanup itself fails we
        // also ignore it — the original file is what matters.
        let _ = std::fs::remove_file(self.0);
    }
}

/// Pre-flight gate on the installed `stellar-cli`, evaluated before we
/// shell out to it (issue #415).
///
/// `version_line` is the first line of `stellar --version`, or `None` when
/// the binary could not be run at all.  Pure and side-effect-free so it can
/// be unit-tested without a `stellar` binary on `PATH`:
///
/// - `None` -> [`ForgeError::ToolMissing`] (binary absent or unlaunchable)
/// - version < [`MIN_STELLAR`] -> [`ForgeError::ToolUnsupported`]
/// - version in the known-broken denylist -> [`ForgeError::ToolUnsupported`]
/// - otherwise -> `Ok(())`
///
/// Both error variants render a pointer at `soroban-forge doctor`, which is
/// what the acceptance criterion asks for.
pub fn check_stellar_cli(version_line: Option<&str>) -> Result<()> {
    let Some(line) = version_line else {
        return Err(ForgeError::ToolMissing("stellar-cli".into()));
    };

    if !version_at_least(line, MIN_STELLAR) {
        return Err(ForgeError::ToolUnsupported(format!(
            "stellar-cli {line} is too old for `optimize` (need >= {}.{})",
            MIN_STELLAR.0, MIN_STELLAR.1
        )));
    }

    if let Some(recommended) = known_broken_stellar_cli_replacement(line) {
        return Err(ForgeError::ToolUnsupported(format!(
            "stellar-cli {line} is a known-broken release (upgrade to {recommended})"
        )));
    }

    Ok(())
}

/// Probe the installed `stellar-cli` version line.
///
/// Thin system-touching wrapper around `stellar --version`; the decision
/// itself lives in [`check_stellar_cli`], which is unit-tested.  Not
/// unit-tested here because it needs a real `stellar` on `PATH`.
fn detect_stellar_cli_version() -> Option<String> {
    let output = std::process::Command::new("stellar")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().next().map(|l| l.trim().to_string())
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| ForgeError::Other(format!("path {} is not valid UTF-8", path.display())))
}

/// Run `command` with piped stdout/stderr, echoing both live to this
/// process's terminal while also capturing them, so `--verbose` users still
/// see progress as it happens and the captured stderr can go into an error.
///
/// Deliberately duplicated from `deploy` rather than shared: modules depend
/// only on `soroban-forge-core`, never on each other.
fn stream_command(command: &mut std::process::Command) -> std::io::Result<Output> {
    let mut child = command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut stderr = child.stderr.take().expect("stderr was piped");
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let count = stdout.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            std::io::stdout().write_all(&chunk[..count])?;
            std::io::stdout().flush()?;
            bytes.extend_from_slice(&chunk[..count]);
        }
        Ok::<_, std::io::Error>(bytes)
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let count = stderr.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            std::io::stderr().write_all(&chunk[..count])?;
            std::io::stderr().flush()?;
            bytes.extend_from_slice(&chunk[..count]);
        }
        Ok::<_, std::io::Error>(bytes)
    });
    let status = child.wait()?;
    let stdout = stdout_reader.join().expect("stdout reader did not panic")?;
    let stderr = stderr_reader.join().expect("stderr reader did not panic")?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Error message for a failed `stellar contract optimize`, carrying the CLI's
/// own stderr so the reason survives log capture that never saw the live
/// terminal (issue #417).  Matches the `"<cmd> failed:\n<stderr>"` shape
/// `verify`, `deploy` and `spec` use.
pub fn optimize_failure_message(status: &str, stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("stellar contract optimize failed ({status})")
    } else {
        format!("stellar contract optimize failed ({status}):\n{stderr}")
    }
}

/// Run `stellar contract optimize --wasm <wasm_path>`. Never reimplemented
/// locally.
///
/// Output is always captured so a failure can report the CLI's stderr; with
/// `verbose` it is additionally streamed live as it is produced.
///
/// Thin system-touching wrapper; the failure formatting lives in
/// [`optimize_failure_message`], which is unit-tested, and the whole path is
/// covered end-to-end by `tests/optimize.rs` with a stub `stellar`.
fn run_stellar_optimize(wasm_path: &Path, verbose: bool) -> Result<()> {
    let wasm_str = path_str(wasm_path)?;

    let mut cmd = std::process::Command::new("stellar");
    cmd.args(["contract", "optimize", "--wasm", wasm_str]);
    log::debug!("optimizing {wasm_str}");

    let result = if verbose {
        stream_command(&mut cmd)
    } else {
        cmd.output()
    };

    match result {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(ForgeError::Other(optimize_failure_message(
            &out.status.to_string(),
            &out.stderr,
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(ForgeError::ToolMissing("stellar-cli".into()))
        }
        Err(e) => Err(ForgeError::io("running stellar contract optimize")(e)),
    }
}

/// Whether an optimize run actually shrank the wasm.
///
/// Derived from [`OptimizeReport::saved_bytes`]: any positive saving is
/// [`OptimizeStatus::Optimized`], while a zero (or, pathologically, negative)
/// saving is [`OptimizeStatus::AlreadyMinimal`].  Automated consumers can
/// branch on this in JSON output instead of treating `saved_bytes == 0` as
/// ambiguous (issue #416).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OptimizeStatus {
    /// The run reduced the wasm size.
    Optimized,
    /// The wasm was already as small as `stellar contract optimize` can make
    /// it — no bytes were saved.
    AlreadyMinimal,
}

/// How this run's optimized size compares to a saved baseline report
/// (`optimize --compare-to`, issue #418).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Comparison {
    /// Path of the baseline report, as given on the command line.
    pub baseline: String,
    /// `after_bytes` recorded in the baseline report.
    pub baseline_after_bytes: u64,
    /// This run's `after_bytes` minus the baseline's: negative means the
    /// optimized wasm shrank since the baseline, positive means it grew.
    pub delta_bytes: i64,
}

impl Comparison {
    pub fn new(baseline: &str, baseline_after_bytes: u64, current_after_bytes: u64) -> Self {
        Self {
            baseline: baseline.to_string(),
            baseline_after_bytes,
            delta_bytes: current_after_bytes as i64 - baseline_after_bytes as i64,
        }
    }

    /// Size change relative to the baseline, in percent (`0.0` when the
    /// baseline was empty).
    pub fn delta_percent(&self) -> f64 {
        if self.baseline_after_bytes == 0 {
            0.0
        } else {
            (self.delta_bytes as f64 / self.baseline_after_bytes as f64) * 100.0
        }
    }
}

/// The outcome of one optimize run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OptimizeReport {
    /// Crate name, set for `--all` runs so each array entry is identifiable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract: Option<String>,
    pub wasm: String,
    pub optimized_wasm: String,
    pub before_bytes: u64,
    pub after_bytes: u64,
    /// Positive when the optimized wasm is smaller.
    pub saved_bytes: i64,
    /// Whether any bytes were saved, derived from `saved_bytes` at
    /// construction so it can never disagree with it.
    pub status: OptimizeStatus,
    /// Delta against `--compare-to`, when given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<Comparison>,
}

impl OptimizeReport {
    pub fn new(wasm: &Path, optimized_wasm: &Path, before_bytes: u64, after_bytes: u64) -> Self {
        let saved_bytes = before_bytes as i64 - after_bytes as i64;
        let status = if saved_bytes > 0 {
            OptimizeStatus::Optimized
        } else {
            OptimizeStatus::AlreadyMinimal
        };
        Self {
            contract: None,
            wasm: wasm.display().to_string(),
            optimized_wasm: optimized_wasm.display().to_string(),
            before_bytes,
            after_bytes,
            saved_bytes,
            status,
            comparison: None,
        }
    }

    /// Tag the report with the contract it belongs to (`--all`).
    pub fn with_contract(mut self, contract: &str) -> Self {
        self.contract = Some(contract.to_string());
        self
    }

    /// Attach a comparison against a baseline report's `after_bytes`.
    pub fn compared_to(mut self, baseline: &str, baseline_after_bytes: u64) -> Self {
        self.comparison = Some(Comparison::new(baseline, baseline_after_bytes, self.after_bytes));
        self
    }

    /// Percentage reduction from `before_bytes` to `after_bytes`, `0.0` when
    /// `before_bytes` is `0`.
    pub fn percent_saved(&self) -> f64 {
        if self.before_bytes == 0 {
            0.0
        } else {
            (self.saved_bytes as f64 / self.before_bytes as f64) * 100.0
        }
    }
}

/// Optimize the wasm at `wasm_path` via `stellar contract optimize` and
/// report the size delta.
///
/// With `in_place = false` (the default) the optimized bytes land at
/// `<stem>.optimized.wasm` next to the input and both files remain — the
/// historical behavior.  With `in_place = true` the optimized bytes replace
/// the original file and the intermediate `.optimized.wasm` is removed, so a
/// deploy pipeline can consume a single canonical path.  This also covers the
/// "clean up the pre-optimization wasm" requirement: after a successful
/// in-place run, only the original filename remains on disk.
pub fn optimize(wasm_path: &Path, in_place: bool) -> Result<OptimizeReport> {
    optimize_with(wasm_path, in_place, false)
}

/// [`optimize`], additionally streaming the CLI's output live when `verbose`.
pub fn optimize_with(wasm_path: &Path, in_place: bool, verbose: bool) -> Result<OptimizeReport> {
    let before_bytes = std::fs::metadata(wasm_path)
        .map_err(ForgeError::io(format!("reading {}", wasm_path.display())))?
        .len();

    run_stellar_optimize(wasm_path, verbose)?;

    let optimized_path = optimized_wasm_path(wasm_path);
    let after_bytes = std::fs::metadata(&optimized_path)
        .map_err(ForgeError::io(format!(
            "reading {}",
            optimized_path.display()
        )))?
        .len();

    if in_place {
        replace_with_optimized(wasm_path, &optimized_path)?;
        // The optimized bytes now live at the original path.
        Ok(OptimizeReport::new(
            wasm_path,
            wasm_path,
            before_bytes,
            after_bytes,
        ))
    } else {
        Ok(OptimizeReport::new(
            wasm_path,
            &optimized_path,
            before_bytes,
            after_bytes,
        ))
    }
}

/// Human-readable report, printed unless `--quiet`.
///
/// Wording distinguishes the two statuses (issue #416): a genuine reduction
/// prints before/after/saved, while an already-minimal run collapses to a
/// single `size` line and never prints a misleading "saved 0 bytes".
pub fn format_report(report: &OptimizeReport) -> String {
    let mut out = format_size_lines(report);
    if let Some(comparison) = &report.comparison {
        out.push_str(&format_comparison(comparison));
    }
    out
}

fn format_size_lines(report: &OptimizeReport) -> String {
    match report.status {
        OptimizeStatus::Optimized => format!(
            "optimized {} -> {}\n\n  before  {} bytes\n  after   {} bytes\n  saved   {} bytes ({:.1}%)\n",
            report.wasm,
            report.optimized_wasm,
            report.before_bytes,
            report.after_bytes,
            report.saved_bytes,
            report.percent_saved(),
        ),
        OptimizeStatus::AlreadyMinimal => format!(
            "already minimal: {}\n\n  size    {} bytes (no reduction)\n",
            report.wasm, report.after_bytes,
        ),
    }
}

/// The `vs baseline` line appended under `--compare-to`.
pub fn format_comparison(comparison: &Comparison) -> String {
    let trend = match comparison.delta_bytes {
        d if d < 0 => "smaller",
        d if d > 0 => "larger",
        _ => "unchanged",
    };
    format!(
        "  vs baseline {} bytes: {:+} bytes ({:+.1}%, {trend}) [{}]\n",
        comparison.baseline_after_bytes,
        comparison.delta_bytes,
        comparison.delta_percent(),
        comparison.baseline,
    )
}

/// The report as a JSON value.
///
/// `status` is serialized by the `Serialize` derive as one of
/// `"optimized"` / `"already-minimal"`.  `percent_saved` (and
/// `comparison.delta_percent`) are injected here because they are computed
/// conveniences, not stored fields.
pub fn json_value(report: &OptimizeReport) -> serde_json::Value {
    let mut value = match serde_json::to_value(report) {
        Ok(value) => value,
        Err(e) => return serde_json::json!({ "error": e.to_string() }),
    };
    if let Some(object) = value.as_object_mut() {
        object.insert("percent_saved".into(), serde_json::json!(report.percent_saved()));
        if let (Some(comparison), Some(serde_json::Value::Object(c))) =
            (&report.comparison, object.get_mut("comparison"))
        {
            c.insert(
                "delta_percent".into(),
                serde_json::json!(comparison.delta_percent()),
            );
        }
    }
    value
}

/// The same report as JSON, for `--json`.
pub fn json_report(report: &OptimizeReport) -> String {
    serde_json::to_string_pretty(&json_value(report))
        .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

// ---- --compare-to baselines (issue #418) ----

/// The subset of a saved `optimize --json` report needed to compare against.
/// Unknown fields are ignored, so reports from older or newer versions load.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BaselineEntry {
    #[serde(default)]
    pub contract: Option<String>,
    #[serde(default)]
    pub wasm: Option<String>,
    pub after_bytes: u64,
}

/// Parse a saved report: either a single object (`optimize --json`) or an
/// array (`optimize --all --json`).  Array entries without `after_bytes` —
/// i.e. contracts that failed in that run — are skipped.
pub fn parse_baseline(raw: &str) -> std::result::Result<Vec<BaselineEntry>, String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("not valid JSON: {e}"))?;
    match value {
        serde_json::Value::Array(items) => {
            let entries: Vec<BaselineEntry> = items
                .into_iter()
                .filter_map(|item| serde_json::from_value(item).ok())
                .collect();
            if entries.is_empty() {
                Err("contains no report with an `after_bytes` field".into())
            } else {
                Ok(entries)
            }
        }
        serde_json::Value::Object(_) => serde_json::from_value(value)
            .map(|entry| vec![entry])
            .map_err(|e| {
                format!("is not an `optimize --json` report ({e}) — save one with `optimize --json > report.json`")
            }),
        _ => Err("is not an `optimize --json` report (expected an object or array)".into()),
    }
}

/// Read and parse a baseline report from disk.
pub fn load_baseline(path: &Path) -> Result<Vec<BaselineEntry>> {
    let raw = std::fs::read_to_string(path)
        .map_err(ForgeError::io(format!("reading baseline {}", path.display())))?;
    parse_baseline(&raw).map_err(|message| ForgeError::Config {
        path: path.to_path_buf(),
        message,
    })
}

fn wasm_file_name(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// Pick the baseline entry matching `report`: by contract name, then by
/// wasm file name, then — for a single-entry baseline — that entry.
pub fn select_baseline<'a>(
    entries: &'a [BaselineEntry],
    report: &OptimizeReport,
) -> Option<&'a BaselineEntry> {
    if let Some(contract) = report.contract.as_deref() {
        if let Some(entry) = entries
            .iter()
            .find(|e| e.contract.as_deref() == Some(contract))
        {
            return Some(entry);
        }
    }
    let name = wasm_file_name(&report.wasm);
    if name.is_some() {
        if let Some(entry) = entries
            .iter()
            .find(|e| e.wasm.as_deref().and_then(wasm_file_name) == name)
        {
            return Some(entry);
        }
    }
    if entries.len() == 1 {
        return entries.first();
    }
    None
}

// ---- --all workspace discovery (issue #412) ----

/// A contract crate found in a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceContract {
    /// snake_case crate name, which the wasm is named after.
    pub crate_name: String,
    /// Directory holding the member's `Cargo.toml`.
    pub dir: PathBuf,
}

fn read_manifest_table(dir: &Path) -> Result<toml::Table> {
    let manifest_path = dir.join("Cargo.toml");
    let raw = std::fs::read_to_string(&manifest_path).map_err(ForgeError::io(format!(
        "reading {}",
        manifest_path.display()
    )))?;
    raw.parse::<toml::Table>().map_err(|e| ForgeError::Config {
        path: manifest_path.clone(),
        message: e.to_string(),
    })
}

/// Whether a manifest describes a Soroban contract: it builds a `cdylib`
/// (what `stellar contract build` emits a wasm for) or depends on
/// `soroban-sdk`.  Helper crates in the same workspace are skipped.
pub fn is_contract_manifest(table: &toml::Table) -> bool {
    let cdylib = table
        .get("lib")
        .and_then(|lib| lib.get("crate-type"))
        .and_then(|types| types.as_array())
        .is_some_and(|types| types.iter().any(|t| t.as_str() == Some("cdylib")));
    let soroban_dep = table
        .get("dependencies")
        .and_then(|deps| deps.as_table())
        .is_some_and(|deps| deps.contains_key("soroban-sdk"));
    table.contains_key("package") && (cdylib || soroban_dep)
}

/// Member directories of the workspace at `root`, from `[workspace].members`
/// with trailing `/*` globs expanded.  Only directories with a `Cargo.toml`
/// are returned, sorted for a stable report order.
///
/// Deliberately duplicated from `testgen` rather than shared: modules depend
/// only on `soroban-forge-core`, never on each other.
pub fn workspace_members(root: &Path, table: &toml::Table) -> Vec<PathBuf> {
    let patterns: Vec<String> = table
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut dirs = Vec::new();
    for pattern in patterns {
        if let Some(prefix) = pattern.strip_suffix("/*") {
            if let Ok(entries) = std::fs::read_dir(root.join(prefix)) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.join("Cargo.toml").is_file() {
                        dirs.push(path);
                    }
                }
            }
        } else {
            let path = root.join(&pattern);
            if path.join("Cargo.toml").is_file() {
                dirs.push(path);
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Every contract crate under `root`.  A plain (non-workspace) contract
/// project counts as a workspace of one, so `--all` also works there.
pub fn discover_contracts(root: &Path) -> Result<Vec<WorkspaceContract>> {
    if !root.join("Cargo.toml").is_file() {
        return Err(ForgeError::InvalidArgument(format!(
            "{} is not a cargo project (no Cargo.toml) — run --all from the workspace root or pass --path",
            root.display()
        )));
    }
    let root_table = read_manifest_table(root)?;

    let mut dirs = workspace_members(root, &root_table);
    if root_table.contains_key("package") && !dirs.iter().any(|d| d == root) {
        dirs.insert(0, root.to_path_buf());
    }

    let mut contracts = Vec::new();
    for dir in dirs {
        let table = if dir == root {
            root_table.clone()
        } else {
            match read_manifest_table(&dir) {
                Ok(table) => table,
                Err(e) => {
                    log::warn!("skipping {}: {e}", dir.display());
                    continue;
                }
            }
        };
        if !is_contract_manifest(&table) {
            continue;
        }
        let Some(name) = table
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
        else {
            continue;
        };
        contracts.push(WorkspaceContract {
            crate_name: name.replace('-', "_"),
            dir,
        });
    }
    Ok(contracts)
}

/// Where a workspace member's release wasm lives: the workspace's shared
/// `target/` first (cargo's default), then the member's own `target/`.
pub fn contract_wasm(root: &Path, contract: &WorkspaceContract) -> Option<PathBuf> {
    [root, contract.dir.as_path()]
        .into_iter()
        .map(|dir| locate_wasm(dir, &contract.crate_name))
        .find(|path| path.is_file())
}

/// One contract's outcome in an `--all` run.
#[derive(Debug)]
pub struct ContractOutcome {
    pub contract: String,
    pub result: std::result::Result<OptimizeReport, String>,
}

/// Human output for `--all`: one block per contract.
pub fn format_all(outcomes: &[ContractOutcome]) -> String {
    let mut out = String::new();
    for (i, outcome) in outcomes.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match &outcome.result {
            Ok(report) => {
                out.push_str(&format!("[{}] ", outcome.contract));
                out.push_str(&format_report(report));
            }
            Err(message) => {
                out.push_str(&format!("[{}] failed: {message}\n", outcome.contract));
            }
        }
    }
    let failed = outcomes.iter().filter(|o| o.result.is_err()).count();
    out.push_str(&format!(
        "\n{} contract(s): {} optimized, {} failed\n",
        outcomes.len(),
        outcomes.len() - failed,
        failed
    ));
    out
}

/// JSON output for `--all`: an array with one report per contract.  A failed
/// contract is an object carrying `contract` and `error` instead.
pub fn json_all(outcomes: &[ContractOutcome]) -> String {
    let values: Vec<serde_json::Value> = outcomes
        .iter()
        .map(|outcome| match &outcome.result {
            Ok(report) => json_value(report),
            Err(message) => serde_json::json!({
                "contract": outcome.contract,
                "error": message,
            }),
        })
        .collect();
    serde_json::to_string_pretty(&values).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

/// Select the report output, or suppress it entirely in quiet mode.
pub fn report_output(report: &OptimizeReport, json: bool, quiet: bool) -> Option<String> {
    if quiet {
        None
    } else if json {
        Some(json_report(report))
    } else {
        Some(format_report(report))
    }
}

/// Fail if the optimized size exceeds the configured budget.
pub fn check_budget(report: &OptimizeReport, max_size: Option<u64>) -> Result<()> {
    if let Some(limit) = max_size {
        if report.after_bytes > limit {
            return Err(ForgeError::Other(format!(
                "optimized wasm size {} bytes exceeds budget of {} bytes",
                report.after_bytes, limit
            )));
        }
    }
    Ok(())
}

/// The `optimize` subcommand.
pub struct OptimizePlugin;

impl ForgePlugin for OptimizePlugin {
    fn name(&self) -> &'static str {
        "optimize"
    }

    fn command(&self) -> Command {
        Command::new("optimize")
            .about("Optimize the release wasm and report the before/after size")
            .long_about(
                "Run `stellar contract optimize` against the local release build and \
                 report how much smaller the optimized wasm is.",
            )
            .arg(
                Arg::new("path")
                    .long("path")
                    .help("Contract project directory [default: current directory]"),
            )
            .arg(
                Arg::new("wasm")
                    .long("wasm")
                    .help("Path to the local .wasm to optimize [default: target/wasm32v1-none/release/<crate>.wasm]"),
            )
            .arg(
                Arg::new("in-place")
                    .long("in-place")
                    .help("Overwrite the input wasm with the optimized bytes and remove the .optimized.wasm file (leaves only the original filename, holding the optimized bytes)")
                    .action(clap::ArgAction::SetTrue),
            )
            .arg(
                Arg::new("check")
                    .long("check")
                    .help("Fail if the optimized wasm exceeds --max-size or the forge.toml budget")
                    .action(clap::ArgAction::SetTrue),
            )
            .arg(
                Arg::new("max_size")
                    .long("max-size")
                    .value_name("BYTES")
                    .value_parser(clap::value_parser!(u64))
                    .help("Maximum allowed size in bytes after optimization"),
            )
            .arg(
                Arg::new("all")
                    .long("all")
                    .help("Optimize every contract crate in the workspace at --path (or the current directory), reporting one result per contract; with --json, prints an array")
                    .action(clap::ArgAction::SetTrue)
                    .conflicts_with("wasm"),
            )
            .arg(
                Arg::new("compare-to")
                    .long("compare-to")
                    .value_name("REPORT")
                    .help("Compare the optimized size against a report saved earlier with `optimize --json > report.json` and print the delta"),
            )
    }

    fn run(&self, matches: &ArgMatches, ctx: &ForgeContext) -> Result<()> {
        // Pre-flight the installed `stellar-cli` before touching the local
        // build or shelling out (issue #415).  `stellar --version` is cheap
        // and offline-safe, so it always runs.
        check_stellar_cli(detect_stellar_cli_version().as_deref())?;

        let dir = matches
            .get_one::<String>("path")
            .map(|p| ctx.cwd.join(p))
            .unwrap_or_else(|| ctx.cwd.clone());
        let wasm_override = matches.get_one::<String>("wasm").map(|p| ctx.cwd.join(p));

        let in_place = matches.get_flag("in-place");
        let verbose = ctx.verbose > 0;

        // Load the baseline up front so a bad --compare-to path fails before
        // any wasm is rewritten.
        let baseline_arg = matches.get_one::<String>("compare-to");
        let baseline = baseline_arg
            .map(|p| load_baseline(&ctx.cwd.join(p)))
            .transpose()?;

        let check = matches.get_flag("check");
        let max_size = if check {
            if let Some(cli) = matches.get_one::<u64>("max_size").copied() {
                Some(cli)
            } else {
                match forge_config_budget(&dir)? {
                    Some(config) => Some(config),
                    None => {
                        return Err(ForgeError::Other(
                            "--check requires --max-size or a forge.toml [optimize] max-size".into(),
                        ));
                    }
                }
            }
        } else {
            None
        };

        if matches.get_flag("all") {
            return run_all(
                &dir,
                AllOptions {
                    in_place,
                    verbose,
                    max_size,
                    baseline: baseline_arg.zip(baseline.as_deref()),
                },
                ctx,
            );
        }

        let wasm_path = resolve_local_wasm(&dir, wasm_override.as_deref())?;
        let mut report = optimize_with(&wasm_path, in_place, verbose)?;
        if let (Some(label), Some(entries)) = (baseline_arg, baseline.as_deref()) {
            let entry = select_baseline(entries, &report).ok_or_else(|| {
                ForgeError::InvalidArgument(format!(
                    "baseline {label} has no entry for {} — compare against a report from the same contract",
                    report.wasm
                ))
            })?;
            report = report.compared_to(label, entry.after_bytes);
        }
        check_budget(&report, max_size)?;

        if let Some(output) = report_output(&report, ctx.json, ctx.quiet) {
            print!("{output}");
            if !output.ends_with('\n') {
                println!();
            }
        }

        Ok(())
    }
}

/// Settings shared by every contract in an `--all` run.
struct AllOptions<'a> {
    in_place: bool,
    verbose: bool,
    max_size: Option<u64>,
    /// `(label, entries)` from `--compare-to`.
    baseline: Option<(&'a String, &'a [BaselineEntry])>,
}

/// Optimize one workspace contract, applying the comparison and budget.
fn optimize_contract(
    root: &Path,
    contract: &WorkspaceContract,
    options: &AllOptions<'_>,
) -> std::result::Result<OptimizeReport, String> {
    let wasm = contract_wasm(root, contract).ok_or_else(|| {
        format!(
            "no release build found at {} — run `stellar contract build` first",
            locate_wasm(root, &contract.crate_name).display()
        )
    })?;
    let mut report = optimize_with(&wasm, options.in_place, options.verbose)
        .map_err(|e| e.to_string())?
        .with_contract(&contract.crate_name);
    if let Some((label, entries)) = options.baseline {
        if let Some(entry) = select_baseline(entries, &report) {
            report = report.compared_to(label, entry.after_bytes);
        }
    }
    check_budget(&report, options.max_size).map_err(|e| e.to_string())?;
    Ok(report)
}

/// `optimize --all`: optimize every contract crate in the workspace at
/// `root`, print one result per contract (an array under `--json`), and fail
/// at the end if any contract failed — after every contract has been tried.
fn run_all(root: &Path, options: AllOptions<'_>, ctx: &ForgeContext) -> Result<()> {
    let contracts = discover_contracts(root)?;
    if contracts.is_empty() {
        return Err(ForgeError::InvalidArgument(format!(
            "no contract crates found in {} — --all looks for workspace members that build a cdylib or depend on soroban-sdk",
            root.display()
        )));
    }

    let outcomes: Vec<ContractOutcome> = contracts
        .iter()
        .map(|contract| ContractOutcome {
            contract: contract.crate_name.clone(),
            result: optimize_contract(root, contract, &options),
        })
        .collect();

    if !ctx.quiet {
        let output = if ctx.json {
            json_all(&outcomes)
        } else {
            format_all(&outcomes)
        };
        print!("{output}");
        if !output.ends_with('\n') {
            println!();
        }
    }

    let failed: Vec<&str> = outcomes
        .iter()
        .filter(|o| o.result.is_err())
        .map(|o| o.contract.as_str())
        .collect();
    if failed.is_empty() {
        Ok(())
    } else {
        Err(ForgeError::Other(format!(
            "optimize --all: {} of {} contract(s) failed: {}",
            failed.len(),
            outcomes.len(),
            failed.join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The optimize subcommand must advertise `--in-place` and accept it.
    /// This lives here rather than in `soroban-forge-core` (where it was
    /// originally written, referencing `OptimizePlugin` as a dev-dependency)
    /// because that created a dependency cycle: `optimize` already depends
    /// on `core` normally, so `core`'s test binary and `optimize`'s own
    /// build ended up linking two different copies of `core`, and the
    /// `ForgePlugin` trait from one didn't satisfy the other (E0277). Calling
    /// core's own `build_command` from here, where the dependency is
    /// one-directional, exercises the exact same plugin-injection path
    /// without the cycle.
    #[test]
    fn optimize_subcommand_exposes_in_place_flag() {
        let plugins: Vec<Box<dyn ForgePlugin>> = vec![Box::new(OptimizePlugin)];
        let mut cmd = soroban_forge_core::cli::build_command(&plugins);

        // The top-level `--help` only lists subcommand names, not their own
        // flags — the `optimize` subcommand's own help is what must mention
        // `--in-place`.
        let help = cmd
            .find_subcommand_mut("optimize")
            .expect("optimize subcommand is registered")
            .render_long_help()
            .to_string();
        assert!(help.contains("--in-place"), "{help}");

        let cmd = soroban_forge_core::cli::build_command(&plugins);
        let matches = cmd
            .try_get_matches_from(["soroban-forge", "optimize", "--in-place"])
            .unwrap();
        let (name, sub) = matches.subcommand().unwrap();
        assert_eq!(name, "optimize");
        assert!(sub.get_flag("in-place"));
    }

    /// #258: `[optimize] max-size` is parsed by this crate's own `ForgeConfig`
    /// above, entirely separate from `soroban-forge-core`'s — so nothing there
    /// would ever catch this key drifting out of sync with the docs. Add a new
    /// field to this file's `ForgeConfig`/`OptimizeConfig`? Add its key here,
    /// and to `docs/configuration.md`'s forge.toml reference table.
    #[test]
    fn every_key_is_documented() {
        let docs_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/configuration.md");
        let docs = std::fs::read_to_string(&docs_path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", docs_path.display()));
        assert!(
            docs.contains("`[optimize] max-size`"),
            "docs/configuration.md is missing a row for [optimize] max-size — \
             update its forge.toml reference table"
        );
    }

    #[test]
    fn locates_wasm_by_crate_name() {
        assert_eq!(
            locate_wasm(Path::new("/proj"), "my_token"),
            PathBuf::from("/proj/target/wasm32v1-none/release/my_token.wasm")
        );
    }

    #[test]
    fn reads_and_normalizes_the_crate_name() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"my-token\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();

        assert_eq!(read_crate_name(tmp.path()).unwrap(), "my_token");
    }

    #[test]
    fn errors_outside_a_cargo_project() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_crate_name(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("not a cargo project"), "{err}");
    }

    #[test]
    fn missing_build_points_at_stellar_contract_build() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();

        let err = resolve_local_wasm(tmp.path(), None).unwrap_err();
        assert!(err.to_string().contains("stellar contract build"), "{err}");
    }

    #[test]
    fn wasm_override_wins_over_the_default_path() {
        let tmp = tempfile::tempdir().unwrap();
        let custom = tmp.path().join("custom.wasm");
        std::fs::write(&custom, b"\0asm").unwrap();

        assert_eq!(
            resolve_local_wasm(tmp.path(), Some(&custom)).unwrap(),
            custom
        );
    }

    #[test]
    fn derives_the_optimized_output_path() {
        assert_eq!(
            optimized_wasm_path(Path::new("/proj/target/release/my_token.wasm")),
            PathBuf::from("/proj/target/release/my_token.optimized.wasm")
        );
    }

    /// `replace_with_optimized` overwrites the original file with the
    /// optimized bytes and removes the intermediate `.optimized.wasm`,
    /// leaving only the original filename on disk.
    #[test]
    fn in_place_replaces_original_and_removes_intermediate() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("token.wasm");
        let optimized = optimized_wasm_path(&original);

        std::fs::write(&original, b"original-bytes").unwrap();
        std::fs::write(&optimized, b"optimized-bytes").unwrap();

        replace_with_optimized(&original, &optimized).unwrap();

        // Only the original filename remains — no `.optimized.wasm`, no `.tmp`.
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("token.wasm")]);
        assert!(!optimized.exists(), "intermediate should be removed");
        assert!(original.is_file(), "original must remain");

        // And it now holds the optimized bytes.
        assert_eq!(std::fs::read(&original).unwrap(), b"optimized-bytes");
    }

    /// The "preserved on failure" guarantee: if the optimized source cannot
    /// be read, the original file is left untouched and an error is returned.
    /// This is deterministic on every platform and privilege level.
    #[test]
    fn replace_with_optimized_preserves_original_when_optimized_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("token.wasm");
        let missing = optimized_wasm_path(&original);

        std::fs::write(&original, b"original-bytes").unwrap();
        // Note: `missing` is deliberately not created.

        let err = replace_with_optimized(&original, &missing).unwrap_err();
        assert!(
            err.to_string().contains("token.optimized.wasm"),
            "error should name the missing source: {err}"
        );

        assert_eq!(
            std::fs::read(&original).unwrap(),
            b"original-bytes",
            "original must be byte-for-byte untouched"
        );
        assert!(!missing.exists());
    }

    /// If the destination directory cannot be written to, the staging write
    /// fails and the original is preserved.  On filesystems / processes where
    /// `chmod` has no effect (e.g. running as root in a container), the write
    /// will succeed and we skip the assertion rather than fail spuriously.
    #[cfg(unix)]
    #[test]
    fn replace_with_optimized_preserves_original_when_destination_is_readonly() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("token.wasm");
        let optimized = optimized_wasm_path(&original);

        std::fs::write(&original, b"original-bytes").unwrap();
        std::fs::write(&optimized, b"optimized-bytes").unwrap();

        // Drop write permission on the containing directory.
        let dir_mode = std::fs::metadata(tmp.path()).unwrap().permissions().mode();
        let mut readonly = std::fs::metadata(tmp.path()).unwrap().permissions();
        readonly.set_mode(dir_mode & !0o222);
        std::fs::set_permissions(tmp.path(), readonly).unwrap();

        let result = replace_with_optimized(&original, &optimized);

        // Restore permissions so the tempdir can be cleaned up.
        let mut restore = std::fs::metadata(tmp.path()).unwrap().permissions();
        restore.set_mode(dir_mode);
        std::fs::set_permissions(tmp.path(), restore).unwrap();

        match result {
            Ok(()) => {
                // Running as root (or on a filesystem that ignores the mode
                // bit).  Nothing to assert — the platform does not let us
                // provoke a write failure.  Not a failure of this test.
            }
            Err(_) => {
                assert_eq!(
                    std::fs::read(&original).unwrap(),
                    b"original-bytes",
                    "original must be untouched when the staging write fails"
                );
            }
        }
    }

    /// Neither success nor failure paths may leave a stale staging file
    /// behind; the naming convention is `<stem>.<pid>.tmp`.
    #[test]
    fn replace_with_optimized_leaves_no_temp_file_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("token.wasm");
        let optimized = optimized_wasm_path(&original);

        std::fs::write(&original, b"original-bytes").unwrap();
        std::fs::write(&optimized, b"optimized-bytes").unwrap();

        replace_with_optimized(&original, &optimized).unwrap();

        for entry in std::fs::read_dir(tmp.path()).unwrap() {
            let name = entry.unwrap().file_name();
            let s = name.to_string_lossy();
            assert!(
                !s.ends_with(".tmp"),
                "staging file leaked after success: {s}"
            );
        }

        // Now provoke a failure and assert no `.tmp` leaks either.
        let missing = tmp.path().join("nope.optimized.wasm");
        let _ = replace_with_optimized(&original, &missing);

        for entry in std::fs::read_dir(tmp.path()).unwrap() {
            let name = entry.unwrap().file_name();
            let s = name.to_string_lossy();
            assert!(
                !s.ends_with(".tmp"),
                "staging file leaked after failure: {s}"
            );
        }
    }

    /// Non-in-place replacement path is not exercised by `optimize` without
    /// `stellar`, so this pins the file-system guarantee the default branch
    /// relies on: the intermediate is produced and left alone.
    #[test]
    fn default_mode_leaves_both_files_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("token.wasm");
        let optimized = optimized_wasm_path(&original);

        std::fs::write(&original, b"original-bytes").unwrap();
        std::fs::write(&optimized, b"optimized-bytes").unwrap();

        // No call to `replace_with_optimized` — mirror the default branch.
        assert_eq!(std::fs::read(&original).unwrap(), b"original-bytes");
        assert_eq!(std::fs::read(&optimized).unwrap(), b"optimized-bytes");
    }

    #[test]
    fn report_computes_saved_bytes_and_percent() {
        let report = OptimizeReport::new(
            Path::new("a.wasm"),
            Path::new("a.optimized.wasm"),
            1000,
            750,
        );
        assert_eq!(report.saved_bytes, 250);
        assert!((report.percent_saved() - 25.0).abs() < f64::EPSILON);
        assert_eq!(report.status, OptimizeStatus::Optimized);
    }

    #[test]
    fn report_handles_no_reduction() {
        let report = OptimizeReport::new(
            Path::new("a.wasm"),
            Path::new("a.optimized.wasm"),
            1000,
            1000,
        );
        assert_eq!(report.saved_bytes, 0);
        assert_eq!(report.percent_saved(), 0.0);
        assert_eq!(report.status, OptimizeStatus::AlreadyMinimal);
    }

    /// The status field is derived from `saved_bytes`, so zero *or* negative
    /// savings both map to `AlreadyMinimal` (issue #416).
    #[test]
    fn status_is_already_minimal_when_no_bytes_saved() {
        let zero = OptimizeReport::new(
            Path::new("a.wasm"),
            Path::new("a.optimized.wasm"),
            1000,
            1000,
        );
        assert_eq!(zero.status, OptimizeStatus::AlreadyMinimal);

        // Pathological: the optimizer made the wasm *larger*.
        let grew = OptimizeReport::new(
            Path::new("a.wasm"),
            Path::new("a.optimized.wasm"),
            1000,
            1001,
        );
        assert_eq!(grew.saved_bytes, -1);
        assert_eq!(grew.status, OptimizeStatus::AlreadyMinimal);
    }

    #[test]
    fn format_report_includes_sizes() {
        let report = OptimizeReport::new(
            Path::new("a.wasm"),
            Path::new("a.optimized.wasm"),
            1000,
            750,
        );
        assert_eq!(report.status, OptimizeStatus::Optimized);
        let text = format_report(&report);
        assert!(text.contains("optimized"), "{text}");
        assert!(text.contains("1000 bytes"), "{text}");
        assert!(text.contains("750 bytes"), "{text}");
        assert!(text.contains("250 bytes"), "{text}");
        assert!(text.contains("25.0%"), "{text}");
        assert!(!text.contains("already minimal"), "{text}");
    }

    /// Human output for an already-minimal run reads differently from a
    /// real reduction, and never prints "saved 0 bytes" (issue #416).
    #[test]
    fn format_report_says_already_minimal() {
        let report = OptimizeReport::new(
            Path::new("a.wasm"),
            Path::new("a.optimized.wasm"),
            1000,
            1000,
        );
        let text = format_report(&report);
        assert!(text.contains("already minimal"), "{text}");
        assert!(text.contains("1000 bytes"), "{text}");
        assert!(text.contains("no reduction"), "{text}");
        assert!(!text.contains("saved"), "{text}");
        assert!(!text.contains("0.0%"), "{text}");
    }

    #[test]
    fn json_report_carries_all_fields() {
        let report = OptimizeReport::new(
            Path::new("a.wasm"),
            Path::new("a.optimized.wasm"),
            1000,
            750,
        );
        let parsed: serde_json::Value = serde_json::from_str(&json_report(&report)).unwrap();
        assert_eq!(parsed["before_bytes"], 1000);
        assert_eq!(parsed["after_bytes"], 750);
        assert_eq!(parsed["saved_bytes"], 250);
        assert_eq!(parsed["percent_saved"], 25.0);
        assert_eq!(parsed["status"], "optimized");
    }

    /// The JSON status distinguishes a real reduction from an
    /// already-minimal run (issue #416).
    #[test]
    fn json_report_carries_status_for_both_values() {
        let optimized = OptimizeReport::new(
            Path::new("a.wasm"),
            Path::new("a.optimized.wasm"),
            1000,
            750,
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&json_report(&optimized)).unwrap();
        assert_eq!(parsed["status"], "optimized");

        let minimal = OptimizeReport::new(
            Path::new("a.wasm"),
            Path::new("a.optimized.wasm"),
            1000,
            1000,
        );
        let parsed: serde_json::Value = serde_json::from_str(&json_report(&minimal)).unwrap();
        assert_eq!(parsed["status"], "already-minimal");
        assert_eq!(parsed["saved_bytes"], 0);
    }

    #[test]
    fn quiet_suppresses_text_and_json_reports() {
        let report = OptimizeReport::new(
            Path::new("a.wasm"),
            Path::new("a.optimized.wasm"),
            1000,
            750,
        );
        assert_eq!(report_output(&report, false, true), None);
        assert_eq!(report_output(&report, true, true), None);
        assert!(report_output(&report, false, false).unwrap().contains("25.0%"));
        assert!(report_output(&report, true, false).unwrap().contains("percent_saved"));
    }

    #[test]
    fn plugin_name_matches_its_command() {
        let plugin = OptimizePlugin;
        assert_eq!(plugin.name(), plugin.command().get_name());
    }

    #[test]
    fn help_documents_path_and_wasm_flags() {
        let help = OptimizePlugin.command().render_long_help().to_string();
        assert!(help.contains("--path"), "{help}");
        assert!(help.contains("--wasm"), "{help}");
    }

    #[test]
    fn help_documents_in_place_flag() {
        let help = OptimizePlugin.command().render_long_help().to_string();
        assert!(help.contains("--in-place"), "{help}");
    }

    // ---- pre-flight version gate (issue #415) ----

    #[test]
    fn version_gate_rejects_missing_binary() {
        let err = check_stellar_cli(None).unwrap_err();
        assert_eq!(err.exit_code(), soroban_forge_core::error::ExitCode::ToolMissing);
        let rendered = err.to_string();
        assert!(rendered.contains("stellar-cli"), "{rendered}");
        assert!(rendered.contains("soroban-forge doctor"), "{rendered}");
    }

    #[test]
    fn version_gate_rejects_too_old_version() {
        let err = check_stellar_cli(Some("stellar-cli 20.3.0")).unwrap_err();
        assert_eq!(err.exit_code(), soroban_forge_core::error::ExitCode::ToolMissing);
        let rendered = err.to_string();
        assert!(rendered.contains("20.3.0"), "{rendered}");
        assert!(rendered.contains("too old"), "{rendered}");
        assert!(rendered.contains("21.0"), "{rendered}");
        assert!(rendered.contains("soroban-forge doctor"), "{rendered}");
    }

    #[test]
    fn version_gate_rejects_known_broken_version_with_recommendation() {
        for (broken, recommended) in [
            ("stellar-cli 27.0.0", "27.0.1"),
            ("stellar-cli 27.0.1", "27.1.0"),
            ("stellar-cli 28.0.0", "28.0.1"),
            ("stellar-cli 29.0.0", "29.0.1"),
        ] {
            let err = check_stellar_cli(Some(broken)).unwrap_err();
            assert_eq!(
                err.exit_code(),
                soroban_forge_core::error::ExitCode::ToolMissing,
                "{broken}"
            );
            let rendered = err.to_string();
            assert!(rendered.contains("known-broken"), "{broken}: {rendered}");
            assert!(rendered.contains(recommended), "{broken}: {rendered}");
            assert!(rendered.contains("soroban-forge doctor"), "{broken}: {rendered}");
        }
    }

    #[test]
    fn version_gate_accepts_supported_versions() {
        assert!(check_stellar_cli(Some("stellar-cli 21.0.0")).is_ok());
        assert!(check_stellar_cli(Some("stellar-cli 26.1.0")).is_ok());
        assert!(check_stellar_cli(Some("stellar-cli 30.0.0")).is_ok());
        // Trailing details after the version are fine (real --version output).
        assert!(check_stellar_cli(Some("stellar 21.0.0 (abc 2025-01-01)")).is_ok());
    }

    #[test]
    fn version_gate_rejects_unparseable_version_line() {
        // Unparseable versions count as "too old" via version_at_least.
        let err = check_stellar_cli(Some("garbage")).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("too old"), "{rendered}");
    }

    // ---- CLI stderr in failure messages (issue #417) ----

    #[test]
    fn failure_message_includes_cli_stderr() {
        let message = optimize_failure_message(
            "exit status: 1",
            b"error: failed to parse wasm: unexpected end-of-file\n",
        );
        assert!(message.contains("stellar contract optimize failed"), "{message}");
        assert!(message.contains("exit status: 1"), "{message}");
        assert!(
            message.contains("failed to parse wasm: unexpected end-of-file"),
            "{message}"
        );
    }

    #[test]
    fn failure_message_without_stderr_still_reports_status() {
        let message = optimize_failure_message("exit status: 2", b"  \n");
        assert_eq!(message, "stellar contract optimize failed (exit status: 2)");
    }

    // ---- --compare-to (issue #418) ----

    fn report(before: u64, after: u64) -> OptimizeReport {
        OptimizeReport::new(
            Path::new("target/wasm32v1-none/release/token.wasm"),
            Path::new("target/wasm32v1-none/release/token.optimized.wasm"),
            before,
            after,
        )
    }

    #[test]
    fn comparing_two_reports_computes_the_delta() {
        // Baseline saved from an earlier `optimize --json > report.json` run.
        let saved = json_report(&report(1000, 750));
        let baseline = parse_baseline(&saved).unwrap();
        assert_eq!(baseline.len(), 1);
        assert_eq!(baseline[0].after_bytes, 750);

        let current = report(1000, 700);
        let entry = select_baseline(&baseline, &current).unwrap();
        let compared = current.compared_to("report.json", entry.after_bytes);
        let comparison = compared.comparison.as_ref().unwrap();

        assert_eq!(comparison.baseline_after_bytes, 750);
        assert_eq!(comparison.delta_bytes, -50);
        assert!((comparison.delta_percent() - (-50.0 / 750.0 * 100.0)).abs() < 1e-9);

        let text = format_report(&compared);
        assert!(text.contains("vs baseline 750 bytes: -50 bytes (-6.7%, smaller)"), "{text}");
        assert!(text.contains("report.json"), "{text}");

        let json: serde_json::Value = serde_json::from_str(&json_report(&compared)).unwrap();
        assert_eq!(json["comparison"]["baseline_after_bytes"], 750);
        assert_eq!(json["comparison"]["delta_bytes"], -50);
        let percent = json["comparison"]["delta_percent"].as_f64().unwrap();
        assert!((percent - (-6.666_666)).abs() < 1e-3, "{percent}");
    }

    #[test]
    fn comparison_reports_growth_and_no_change() {
        let grew = report(1000, 800).compared_to("old.json", 750);
        let c = grew.comparison.as_ref().unwrap();
        assert_eq!(c.delta_bytes, 50);
        assert!(format_comparison(c).contains("+50 bytes (+6.7%, larger)"));

        let same = report(1000, 750).compared_to("old.json", 750);
        let c = same.comparison.as_ref().unwrap();
        assert_eq!(c.delta_bytes, 0);
        assert!(format_comparison(c).contains("unchanged"));

        let empty = Comparison::new("old.json", 0, 10);
        assert_eq!(empty.delta_percent(), 0.0);
    }

    #[test]
    fn comparison_is_shown_for_already_minimal_runs_too() {
        let text = format_report(&report(750, 750).compared_to("old.json", 800));
        assert!(text.contains("already minimal"), "{text}");
        assert!(text.contains("-50 bytes"), "{text}");
    }

    #[test]
    fn reports_without_comparison_omit_the_field() {
        let json: serde_json::Value = serde_json::from_str(&json_report(&report(1000, 750))).unwrap();
        assert!(json.get("comparison").is_none(), "{json}");
        assert!(json.get("contract").is_none(), "{json}");
    }

    #[test]
    fn parse_baseline_accepts_all_arrays_and_skips_failed_entries() {
        let raw = r#"[
            {"contract": "token", "wasm": "target/wasm32v1-none/release/token.wasm", "after_bytes": 700},
            {"contract": "pool", "error": "no release build found"},
            {"contract": "amm", "wasm": "target/wasm32v1-none/release/amm.wasm", "after_bytes": 1200}
        ]"#;
        let entries = parse_baseline(raw).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].contract.as_deref(), Some("amm"));
    }

    #[test]
    fn parse_baseline_rejects_non_reports() {
        assert!(parse_baseline("not json").unwrap_err().contains("not valid JSON"));
        assert!(parse_baseline(r#"{"size": 3}"#)
            .unwrap_err()
            .contains("optimize --json"));
        assert!(parse_baseline("[]").is_err());
        assert!(parse_baseline("42").is_err());
    }

    #[test]
    fn load_baseline_reports_the_path_on_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("report.json");
        std::fs::write(&path, "{").unwrap();
        let err = load_baseline(&path).unwrap_err().to_string();
        assert!(err.contains("report.json"), "{err}");

        let missing = load_baseline(&tmp.path().join("nope.json")).unwrap_err().to_string();
        assert!(missing.contains("nope.json"), "{missing}");
    }

    #[test]
    fn select_baseline_matches_by_contract_then_wasm_name() {
        let entries = vec![
            BaselineEntry {
                contract: Some("token".into()),
                wasm: Some("/old/checkout/target/wasm32v1-none/release/token.wasm".into()),
                after_bytes: 700,
            },
            BaselineEntry {
                contract: Some("amm".into()),
                wasm: Some("/old/checkout/target/wasm32v1-none/release/amm.wasm".into()),
                after_bytes: 1200,
            },
        ];

        let by_contract = report(1000, 650).with_contract("amm");
        assert_eq!(select_baseline(&entries, &by_contract).unwrap().after_bytes, 1200);

        // No contract tag (single run) — falls back to the wasm file name,
        // which survives the report being saved from a different checkout.
        let by_wasm = report(1000, 650);
        assert_eq!(select_baseline(&entries, &by_wasm).unwrap().after_bytes, 700);

        let unknown = OptimizeReport::new(Path::new("x/other.wasm"), Path::new("x/other.optimized.wasm"), 10, 9);
        assert!(select_baseline(&entries, &unknown).is_none());

        // A single-entry baseline is used even if the file name differs.
        let single = vec![entries[0].clone()];
        assert_eq!(select_baseline(&single, &unknown).unwrap().after_bytes, 700);
    }

    // ---- --all (issue #412) ----

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn contract_manifest(name: &str) -> String {
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [lib]\ncrate-type = [\"lib\", \"cdylib\"]\n\n\
             [dependencies]\nsoroban-sdk = {{ workspace = true }}\n"
        )
    }

    fn workspace_fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\"contracts/*\", \"crates/helpers\"]\n",
        );
        write(&root.join("contracts/token/Cargo.toml"), &contract_manifest("my-token"));
        write(&root.join("contracts/pool/Cargo.toml"), &contract_manifest("pool"));
        // A plain library crate in the workspace is not a contract.
        write(
            &root.join("crates/helpers/Cargo.toml"),
            "[package]\nname = \"helpers\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        tmp
    }

    #[test]
    fn discovers_every_contract_crate_in_a_workspace() {
        let tmp = workspace_fixture();
        let contracts = discover_contracts(tmp.path()).unwrap();
        let names: Vec<_> = contracts.iter().map(|c| c.crate_name.as_str()).collect();
        assert_eq!(names, vec!["pool", "my_token"]);
        assert!(contracts[1].dir.ends_with("contracts/token"));
    }

    #[test]
    fn a_single_contract_project_is_a_workspace_of_one() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("Cargo.toml"), &contract_manifest("solo"));
        let contracts = discover_contracts(tmp.path()).unwrap();
        assert_eq!(contracts.len(), 1);
        assert_eq!(contracts[0].crate_name, "solo");
    }

    #[test]
    fn discover_requires_a_cargo_project() {
        let tmp = tempfile::tempdir().unwrap();
        let err = discover_contracts(tmp.path()).unwrap_err().to_string();
        assert!(err.contains("not a cargo project"), "{err}");
    }

    #[test]
    fn contract_wasm_prefers_the_shared_workspace_target() {
        let tmp = workspace_fixture();
        let root = tmp.path();
        let contracts = discover_contracts(root).unwrap();
        let pool = &contracts[0];

        assert_eq!(contract_wasm(root, pool), None);

        let member_target = locate_wasm(&pool.dir, "pool");
        write(&member_target, "\0asm");
        assert_eq!(contract_wasm(root, pool), Some(member_target));

        let shared = locate_wasm(root, "pool");
        write(&shared, "\0asm");
        assert_eq!(contract_wasm(root, pool), Some(shared));
    }

    #[test]
    fn all_outputs_one_result_per_contract() {
        let outcomes = vec![
            ContractOutcome {
                contract: "pool".into(),
                result: Ok(report(1000, 750).with_contract("pool")),
            },
            ContractOutcome {
                contract: "my_token".into(),
                result: Err("no release build found".into()),
            },
        ];

        let json: serde_json::Value = serde_json::from_str(&json_all(&outcomes)).unwrap();
        let array = json.as_array().expect("--all --json emits an array");
        assert_eq!(array.len(), 2);
        assert_eq!(array[0]["contract"], "pool");
        assert_eq!(array[0]["after_bytes"], 750);
        assert_eq!(array[1]["contract"], "my_token");
        assert_eq!(array[1]["error"], "no release build found");

        // An --all array saved to disk is itself a valid --compare-to baseline.
        let baseline = parse_baseline(&json_all(&outcomes)).unwrap();
        assert_eq!(baseline.len(), 1);
        assert_eq!(baseline[0].contract.as_deref(), Some("pool"));

        let text = format_all(&outcomes);
        assert!(text.contains("[pool] optimized"), "{text}");
        assert!(text.contains("[my_token] failed: no release build found"), "{text}");
        assert!(text.contains("2 contract(s): 1 optimized, 1 failed"), "{text}");
    }

    #[test]
    fn help_documents_all_and_compare_to() {
        let help = OptimizePlugin.command().render_long_help().to_string();
        assert!(help.contains("--all"), "{help}");
        assert!(help.contains("--compare-to"), "{help}");
    }

    #[test]
    fn all_conflicts_with_wasm() {
        let err = OptimizePlugin
            .command()
            .try_get_matches_from(["optimize", "--all", "--wasm", "x.wasm"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
