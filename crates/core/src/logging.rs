//! Logging setup for terminal output and optional JSON-lines log files.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Log, Metadata, Record};

use crate::{ForgeError, Result};

fn redact_log_message(message: &str) -> String {
    message
        .replace("SECRET", "[redacted]")
        .replace("secret", "[redacted]")
        .replace("seed", "[redacted]")
}

struct ForgeLogger {
    console: env_logger::Logger,
    file: Option<Mutex<File>>,
}

impl Log for ForgeLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.console.enabled(metadata)
            || (self.file.is_some() && metadata.level() <= log::Level::Debug)
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        if self.console.enabled(record.metadata()) {
            self.console.log(record);
        }

        if let Some(file) = &self.file {
            let timestamp_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let entry = serde_json::json!({
                "timestamp_ms": timestamp_ms,
                "level": record.level().as_str(),
                "target": record.target(),
                "message": redact_log_message(&record.args().to_string()),
            });
            if let Ok(mut file) = file.lock() {
                let _ = serde_json::to_writer(&mut *file, &entry);
                let _ = file.write_all(b"\n");
                let _ = file.flush();
            }
        }
    }

    fn flush(&self) {
        self.console.flush();
        if let Some(file) = &self.file {
            if let Ok(mut file) = file.lock() {
                let _ = file.flush();
            }
        }
    }
}

/// Initialize normal stderr logging and, when requested, a JSON-lines log.
///
/// `verbose` is the number of times `-v`/`--verbose` was passed: `0` keeps
/// the default `info` level, `1` (`-v`) raises it to `debug`, and `2+`
/// (`-vv`) raises it to `trace`.
pub fn init(verbose: u8, log_file: Option<&Path>) -> Result<()> {
    init_with_color(verbose, log_file, true)
}

/// Like [`init`], but with explicit color control.
///
/// When `use_color` is `false` the console logger writes plain text with no
/// ANSI escape sequences, honoring `NO_COLOR` / `--color never`.
pub fn init_with_color(verbose: u8, log_file: Option<&Path>, use_color: bool) -> Result<()> {
    let level = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level));
    builder.format_timestamp(None);
    if !use_color {
        builder.write_style(env_logger::WriteStyle::Never);
    }
    let console = builder.build();
    let max_level = console.filter();

    let file = log_file
        .map(|path| {
            File::create(path)
                .map(Mutex::new)
                .map_err(ForgeError::io(format!(
                    "creating log file {}",
                    path.display()
                )))
        })
        .transpose()?;

    let file_enabled = file.is_some();
    let logger = ForgeLogger { console, file };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        let max_level = if file_enabled {
            std::cmp::max(max_level, log::LevelFilter::Debug)
        } else {
            max_level
        };
        log::set_max_level(max_level);
    }
    Ok(())
}
