//! Shared logging bootstrap for System Alphabet daemons.
//!
//! Every worker writes its own log file under [`crate::paths::Paths::log_dir`]
//! (`<log-dir>/<name>.log`, created on demand); the special value `-`
//! selects stderr instead.  In addition, every daemon mirrors its logs to
//! `/dev/console` (`SYSTEMA_CONSOLE` to override) so boot progress is
//! visible on the physical console — but only when that device actually
//! exists and is openable for writing; a missing/unwritable console (the
//! norm in containers and headless environments) silently disables the
//! mirror.
//!
//! When the console is a terminal, or the kernel command line requests it
//! via `console-is-tty`, the console stream gets ANSI colours; the primary
//! log (file / stderr) always stays plain so the escape sequences are never
//! stored in the log file.  Failures always fall back to stderr so that a
//! broken log directory never prevents a process from starting.

use std::fs::File;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use tracing_subscriber::EnvFilter;

/// Where [`init`] sent the logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogTarget {
    /// Append to this file.
    File(PathBuf),
    /// Standard error.
    Stderr,
}

/// A writer that fans each write out to two destinations.
///
/// Every daemon writes its primary log (a file, or stderr) **and** mirrors
/// the same bytes to the console device so boot progress is visible on a
/// physical/remote console.  A failure on one destination (e.g. the console
/// not being writable) never blocks or drops the other.
struct TeeWriter {
    primary: Box<dyn Write + Send>,
    console: Box<dyn Write + Send>,
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.primary.write(buf);
        let _ = self.console.write(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = self.primary.flush();
        let _ = self.console.flush();
        Ok(())
    }
}

/// State of the ANSI escape sequence parser inside [`StripAnsiWriter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnsiState {
    /// Ordinary text.
    Idle,
    /// Saw the ESC leader (`0x1B`); absorbing any intermediate bytes.
    Esc,
    /// Inside a CSI sequence (`ESC [ …`) — params/intermediates until the
    /// final byte, which is discarded along with everything in between.
    CsiParam,
    /// Inside an OSC sequence (`ESC ] …`) — discarded until BEL or ST.
    Osc,
    /// OSC just saw ESC; a following `\` (ST) terminates the sequence.
    OscEsc,
}

/// A writer that strips ANSI escape sequences before forwarding to its
/// inner writer.
///
/// The formatter is colourised for the console; the primary log must stay
/// plain, so it is wrapped in this writer which removes every escape
/// sequence (CSI / OSC / two-byte) and keeps only the text.  Sequences may
/// be split across `write` calls — the parser state is retained between
/// calls so no escape is ever half-emitted into the log.
struct StripAnsiWriter {
    inner: Box<dyn Write + Send>,
    state: AnsiState,
}

impl StripAnsiWriter {
    fn new(inner: Box<dyn Write + Send>) -> Self {
        StripAnsiWriter {
            inner,
            state: AnsiState::Idle,
        }
    }
}

impl Write for StripAnsiWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut out = Vec::with_capacity(buf.len());
        for &b in buf {
            match self.state {
                AnsiState::Idle => {
                    if b == 0x1b {
                        self.state = AnsiState::Esc;
                    } else {
                        out.push(b);
                    }
                }
                AnsiState::Esc => match b {
                    0x5b => self.state = AnsiState::CsiParam, // ESC [
                    0x5d => self.state = AnsiState::Osc,      // ESC ]
                    0x20..=0x2f => { /* intermediate byte — keep absorbing */ }
                    0x40..=0x5f => self.state = AnsiState::Idle, // two-byte escape
                    _ => self.state = AnsiState::Idle,
                },
                AnsiState::CsiParam => {
                    if (0x40..=0x7e).contains(&b) {
                        self.state = AnsiState::Idle; // final byte ends the CSI
                    }
                }
                AnsiState::Osc => {
                    if b == 0x1b {
                        self.state = AnsiState::OscEsc;
                    } else if b == 0x07 {
                        self.state = AnsiState::Idle; // BEL ends OSC
                    }
                }
                AnsiState::OscEsc => {
                    self.state = if b == 0x5c {
                        AnsiState::Idle // ST ends OSC
                    } else {
                        AnsiState::Osc
                    };
                }
            }
        }
        self.inner.write_all(&out)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Resolve the effective target for `log_name` without installing a
/// subscriber: `-` yields stderr, anything else the append-mode log file
/// (with the directory auto-created).  Unwritable targets degrade to
/// stderr with a warning on the real stderr.
fn resolve_target(log_dir: &str, log_name: &str) -> LogTarget {
    if log_dir == "-" {
        return LogTarget::Stderr;
    }
    let dir = Path::new(log_dir);
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(format!("{log_name}.log"));
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(_) => LogTarget::File(path),
        Err(e) => {
            eprintln!(
                "{}",
                crate::l10n::fmt(
                    crate::l10n::t_("Cannot open log file {path} ({e}); falling back to stderr"),
                    &[
                        ("path", &(path.display()).to_string()),
                        ("e", &e.to_string())
                    ]
                )
            );
            LogTarget::Stderr
        }
    }
}

/// Whether the kernel command line requests a TTY console via the
/// `console-is-tty` parameter.  systemd honours this to decide whether it
/// may use ANSI styling on `/dev/console`.
fn kernel_wants_console_tty(cmdline: &str) -> bool {
    cmdline.split_whitespace().any(|w| w == "console-is-tty")
}

/// Whether the console device should carry ANSI colours: it is an actual
/// terminal, or the kernel command line carries `console-is-tty` (e.g. when
/// the console is a serial line that still renders escape sequences).
fn console_wants_ansi(file: &File) -> bool {
    if file.is_terminal() {
        return true;
    }
    std::fs::read_to_string("/proc/cmdline")
        .map(|c| kernel_wants_console_tty(&c))
        .unwrap_or(false)
}

/// Open the console device (`/dev/console` by default; override with the
/// `SYSTEMA_CONSOLE` environment variable).
///
/// The console is opened **only** when the device exists *and* openable for
/// writing; a missing or unwritable console (the norm in containers or
/// headless environments) silently disables the mirror — no warning is
/// emitted because this is an expected condition.
///
/// Returns the console writer and whether its stream should be colourised
/// (see [`console_wants_ansi`]).
fn open_console() -> Option<(Box<dyn Write + Send>, bool)> {
    let path = std::env::var("SYSTEMA_CONSOLE").unwrap_or_else(|_| "/dev/console".to_string());
    if !Path::new(&path).exists() {
        return None;
    }
    let file = std::fs::OpenOptions::new().write(true).open(&path).ok()?;
    let wants_ansi = console_wants_ansi(&file);
    Some((Box::new(file), wants_ansi))
}

/// Install the global tracing subscriber for a daemon named `log_name`
/// (the log file basename, e.g. `"systema-syss"`).
///
/// `level` follows the usual filter syntax (`info`, `debug`, RUST_LOG
/// expressions).  Returns where logs actually go.
///
/// The effective path values are recorded at debug level so every run's
/// log documents what it resolved (see [`crate::paths::debug_dump`]).
///
/// The primary log goes to the configured file (or stderr) and is mirrored
/// to the console device when it is openable.  When the console is a TTY
/// (or `console-is-tty` is on the kernel command line) the formatter emits
/// ANSI colours and the console receives them, while the primary stream is
/// wrapped in a [`StripAnsiWriter`] so the file stays plain.
pub fn init(log_dir: &str, log_name: &str, level: &str) -> LogTarget {
    let target = resolve_target(log_dir, log_name);
    let console = open_console();
    let console_ansi = console.as_ref().map(|(_, ansi)| *ansi).unwrap_or(false);
    let filter = match level.parse::<EnvFilter>() {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "{}",
                crate::l10n::fmt(
                    crate::l10n::t_("Invalid log level '{level}' ({e}); using info"),
                    &[("level", &level.to_string()), ("e", &e.to_string())]
                )
            );
            EnvFilter::new("info")
        }
    };
    let fmt = tracing_subscriber::fmt()
        .with_ansi(console_ansi)
        .with_env_filter(filter);

    let primary: Box<dyn Write + Send> = match &target {
        LogTarget::File(path) => {
            // Re-open independently: the probe handle above is dropped.
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                Ok(file) => Box::new(file),
                Err(e) => {
                    eprintln!(
                        "{}",
                        crate::l10n::fmt(
                            crate::l10n::t_(
                                "Cannot open log file {path} ({e}); falling back to stderr"
                            ),
                            &[
                                ("path", &(path.display()).to_string()),
                                ("e", &e.to_string())
                            ]
                        )
                    );
                    Box::new(std::io::stderr())
                }
            }
        }
        LogTarget::Stderr => Box::new(std::io::stderr()),
    };

    // Colours go to the console only; strip them from the primary stream so
    // no escape sequence is ever persisted into the log file.
    let primary = if console_ansi {
        Box::new(StripAnsiWriter::new(primary)) as Box<dyn Write + Send>
    } else {
        primary
    };
    let writer = match console {
        Some((console, _)) => Box::new(TeeWriter { primary, console }),
        None => primary,
    };
    fmt.with_writer(std::sync::Mutex::new(writer as Box<dyn Write + Send>))
        .init();
    // Record the paths this run actually resolved: environments are
    // volatile, env vars and CLI flags can change them between runs.
    crate::paths::debug_dump(log_dir);
    target
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Captures written bytes into a shared buffer.
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);
    impl Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn dash_selects_stderr() {
        assert_eq!(resolve_target("-", "whatever"), LogTarget::Stderr);
    }

    #[test]
    fn creates_directory_and_file() {
        let dir = std::env::temp_dir().join(format!("sysa-logtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sub = dir.join("nested");
        let target = resolve_target(sub.to_str().unwrap(), "unit-test");
        match &target {
            LogTarget::File(p) => assert!(p.starts_with(&sub) && p.exists()),
            LogTarget::Stderr => panic!("expected a file target"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unwritable_degrades_to_stderr() {
        // A path *under a file* can never be created nor opened.
        let blocker = std::env::temp_dir().join(format!("sysa-logblock-{}", std::process::id()));
        std::fs::write(&blocker, b"x").unwrap();
        let target = resolve_target(blocker.join("sub").to_str().unwrap(), "unit-test");
        assert_eq!(target, LogTarget::Stderr);
        let _ = std::fs::remove_file(&blocker);
    }

    #[test]
    fn tee_writer_fans_out_to_both_destinations() {
        let primary_buf = Arc::new(Mutex::new(Vec::new()));
        let console_buf = Arc::new(Mutex::new(Vec::new()));
        let mut tee = TeeWriter {
            primary: Box::new(CaptureWriter(primary_buf.clone())),
            console: Box::new(CaptureWriter(console_buf.clone())),
        };

        tee.write_all(b"hello console\n").unwrap();
        tee.flush().unwrap();

        assert_eq!(*primary_buf.lock().unwrap(), b"hello console\n");
        assert_eq!(*console_buf.lock().unwrap(), b"hello console\n");
    }

    #[test]
    fn write_to_failed_console_still_reaches_primary() {
        struct FailWriter;
        impl Write for FailWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("console gone"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("console gone"))
            }
        }

        let primary_buf = Arc::new(Mutex::new(Vec::new()));
        let mut tee = TeeWriter {
            primary: Box::new(CaptureWriter(primary_buf.clone())),
            console: Box::new(FailWriter),
        };

        // The primary write must still succeed even when the console errors.
        tee.write_all(b"log line\n").unwrap();
        tee.flush().unwrap();

        assert_eq!(*primary_buf.lock().unwrap(), b"log line\n");
    }

    #[test]
    fn strips_sgr_colour_codes() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w = StripAnsiWriter::new(Box::new(CaptureWriter(buf.clone())));

        w.write_all(b"\x1b[1mINFO\x1b[0m message\n").unwrap();
        assert_eq!(*buf.lock().unwrap(), b"INFO message\n");
    }

    #[test]
    fn strips_escapes_split_across_writes() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w = StripAnsiWriter::new(Box::new(CaptureWriter(buf.clone())));

        // The CSI sequence is chopped in the middle of a write call; the
        // parser must carry the state over so no partial escape leaks out.
        w.write_all(b"prefix \x1b[31").unwrap();
        w.write_all(b"mred\x1b[0m suffix\n").unwrap();
        assert_eq!(*buf.lock().unwrap(), b"prefix red suffix\n");
    }

    #[test]
    fn strips_osc_and_bel_terminated_sequences() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w = StripAnsiWriter::new(Box::new(CaptureWriter(buf.clone())));

        w.write_all(b"before\x1b]0;title\x07after\n").unwrap();
        w.write_all(b"x\x1b]8;;http://e\x1b\\y\n").unwrap();
        assert_eq!(*buf.lock().unwrap(), b"beforeafter\nxy\n");
    }

    #[test]
    fn strips_two_byte_escapes() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w = StripAnsiWriter::new(Box::new(CaptureWriter(buf.clone())));

        w.write_all(b"a\x1b7b\x1b(c\n").unwrap();
        assert_eq!(*buf.lock().unwrap(), b"ab\n");
    }

    #[test]
    fn passes_through_plain_text_unchanged() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w = StripAnsiWriter::new(Box::new(CaptureWriter(buf.clone())));

        w.write_all(b"no escapes here\n").unwrap();
        assert_eq!(*buf.lock().unwrap(), b"no escapes here\n");
    }

    #[test]
    fn kernel_wants_console_tty_parses_cmdline() {
        assert!(kernel_wants_console_tty(
            "BOOT_IMAGE=/vmlinuz root=/dev/sda1 console-is-tty quiet"
        ));
        assert!(!kernel_wants_console_tty(
            "BOOT_IMAGE=/vmlinuz root=/dev/sda1 console=tty0 quiet"
        ));
        assert!(!kernel_wants_console_tty(""));
        // Prefix/suffix matches must not fool the exact token comparison.
        assert!(!kernel_wants_console_tty("not-console-is-tty"));
        assert!(!kernel_wants_console_tty("console-is-tty=1"));
    }
}
