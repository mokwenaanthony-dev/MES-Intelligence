//! Logging in the project's format, without a logging crate.
//!
//! Every line has the time, level, thread, module, source file and **line number**, then the
//! message, in the same order as the Python MES:
//!
//! ```text
//! 2026-11-12T15:30:00.123+00:00 | INFO    | main | mes_edge::agent | src/agent.rs:142 | Started job mo00000001-0010
//! ```
//!
//! The macros ([`crate::info!`] and friends) capture `module_path!()`, `file!()` and `line!()` at
//! the place they are written, so a log line points at the code that wrote it. Lines go to standard
//! error, where Docker collects them. Like the ERP, nothing secret is logged: no passwords, and MQTT
//! URLs only without their credentials.

use std::sync::atomic::{AtomicU8, Ordering};

/// Level numbers: a message is written when its level is at least the configured one.
pub const DEBUG: u8 = 10;
/// Normal events: connected, job started, job complete.
pub const INFO: u8 = 20;
/// Something went wrong but the agent carries on (a lost connection, a bad message).
pub const WARNING: u8 = 30;
/// Something the agent cannot work around by itself.
pub const ERROR: u8 = 40;

static LEVEL: AtomicU8 = AtomicU8::new(INFO);

/// Set the level from a name such as `DEBUG` or `info` (unknown names keep the current level).
pub fn set_level(name: &str) {
    let level = match name.to_ascii_uppercase().as_str() {
        "DEBUG" => DEBUG,
        "INFO" => INFO,
        "WARNING" | "WARN" => WARNING,
        "ERROR" => ERROR,
        _ => return,
    };
    LEVEL.store(level, Ordering::Relaxed);
}

/// Whether a message at `level` would be written.
pub fn enabled(level: u8) -> bool {
    level >= LEVEL.load(Ordering::Relaxed)
}

/// Write one log line. Called by the macros; use those instead.
pub fn write(level: u8, module: &str, file: &str, line: u32, message: std::fmt::Arguments) {
    if !enabled(level) {
        return;
    }
    let name = match level {
        DEBUG => "DEBUG",
        INFO => "INFO",
        WARNING => "WARNING",
        _ => "ERROR",
    };
    let thread = std::thread::current();
    eprintln!(
        "{} | {:<7} | {} | {} | {}:{} | {}",
        crate::time::now_iso(),
        name,
        thread.name().unwrap_or("-"),
        module,
        file,
        line,
        message
    );
}

/// Log at DEBUG, with the caller's module, file and line.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::log::write($crate::log::DEBUG, module_path!(), file!(), line!(), format_args!($($arg)*)) };
}

/// Log at INFO, with the caller's module, file and line.
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log::write($crate::log::INFO, module_path!(), file!(), line!(), format_args!($($arg)*)) };
}

/// Log at WARNING, with the caller's module, file and line.
#[macro_export]
macro_rules! warning {
    ($($arg:tt)*) => { $crate::log::write($crate::log::WARNING, module_path!(), file!(), line!(), format_args!($($arg)*)) };
}

/// Log at ERROR, with the caller's module, file and line.
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log::write($crate::log::ERROR, module_path!(), file!(), line!(), format_args!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_filter_messages() {
        set_level("warning");
        assert!(!enabled(INFO) && enabled(WARNING) && enabled(ERROR));
        set_level("nonsense"); // ignored
        assert!(!enabled(INFO));
        set_level("INFO");
        assert!(enabled(INFO) && !enabled(DEBUG));
    }
}
