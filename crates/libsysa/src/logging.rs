//! Shared logging bootstrap for System Alphabet components.
//!
//! Every component writes the *same* record to two sinks:
//!
//! * **standard error** — always for daemons (whatever the parent attached
//!   stderr to), and for interactive tools only with `--debug`;
//! * **a log file** — resolved as `--log-file` > `SYSTEMA_LOG_FILE` >
//!   `/var/log/systema/<name>.log` for daemons; an interactive tool writes
//!   a file only when `--log-file` is given explicitly.
//!
//! The path value `-` selects stderr only (no file sink).  A log file that
//! cannot be opened — early boot before `/var` is writable, an
//! unprivileged tool, a full disk — degrades to stderr-only with a single
//! warning: a broken log location never prevents a process from starting.
//!
//! Nothing else is special-cased: there is no console mirror and no
//! device-specific stream handling.  The formatter is coloured only when
//! stderr is a terminal; the file sink always receives plain text (see
//! [`StripAnsiWriter`]).
//!
//! Spawning parents keep one log directory by passing each child
//! `--log-file <dir>/<binary>.log`, derived from this process's own target
//! ([`LogTarget::child_file`] / [`child_log_file`]) — argv beats any
//! inherited `SYSTEMA_LOG_FILE`.

use std::fs::File;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing_subscriber::EnvFilter;

/// Where this process's file sink went; set once by [`init`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogTarget {
    /// Append to this file.
    File(PathBuf),
    /// No file sink: everything that is enabled goes to stderr.
    Stderr,
}

impl LogTarget {
    /// The `--log-file` value for a child component `binary` spawned by
    /// this process: the same directory as this process's file, the
    /// child's own basename (`-/…` propagates the stderr-only choice).
    pub fn child_file(&self, binary: &str) -> PathBuf {
        match self {
            LogTarget::File(path) => {
                let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
                dir.unwrap_or_else(|| Path::new("."))
                    .join(format!("{binary}.log"))
            }
            LogTarget::Stderr => PathBuf::from("-"),
        }
    }
}

/// The target [`init`] resolved for this process, once it has run.
static RESOLVED: OnceLock<LogTarget> = OnceLock::new();

/// [`LogTarget::child_file`] of this process's own target: the `--log-file`
/// value to hand a child component `binary`.  Before [`init`] — or when
/// this process has no file sink — the child is told `-` (stderr only).
pub fn child_log_file(binary: &str) -> PathBuf {
    match RESOLVED.get() {
        Some(target) => target.child_file(binary),
        None => PathBuf::from("-"),
    }
}

/// A writer that fans each write out to two destinations.
///
/// Every record goes to the stderr sink **and** the log file.  A failure
/// on one destination never blocks or drops the other.
struct TeeWriter {
    primary: Box<dyn Write + Send>,
    mirror: Box<dyn Write + Send>,
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.primary.write(buf);
        let _ = self.mirror.write(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = self.primary.flush();
        let _ = self.mirror.flush();
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
/// The formatter may colourise stderr (when it is a terminal); the log
/// file must stay plain, so it is wrapped in this writer which removes
/// every escape sequence (CSI / OSC / two-byte) and keeps only the text.
/// Sequences may be split across `write` calls — the parser state is
/// retained between calls so no escape is ever half-emitted into the log.
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

/// Resolve the log file for a component named `name`: the explicit CLI
/// value wins, then `SYSTEMA_LOG_FILE`, then — when `default_file` applies
/// (daemons) — `<builtin log dir>/<name>.log`.  The env fallback likewise
/// belongs to daemons only: a tool that was given no `--log-file` (or `-`)
/// gets no file sink at all.
fn resolve_log_file(
    name: &str,
    cli: Option<PathBuf>,
    env: Option<std::ffi::OsString>,
    default_file: bool,
) -> Option<PathBuf> {
    // Gate the env arm on `default_file` so a tool never inherits a file
    // sink from the environment, even when called directly (the caller
    // already pre-filters with `Options::env_file`; this keeps the contract
    // total in the function itself).
    let env = default_file.then(|| env.map(PathBuf::from)).flatten();
    let value = cli.or(env).or_else(|| {
        default_file.then(|| Path::new(crate::paths::builtin_log_dir()).join(format!("{name}.log")))
    })?;
    (value != Path::new("-")).then_some(value)
}

/// [`resolve_log_file`] followed by opening the file (its directory is
/// created on demand).  An unwritable target prints one warning and
/// yields `None` so the caller degrades to stderr alone.
fn open_resolved(
    name: &str,
    cli: Option<PathBuf>,
    env: Option<std::ffi::OsString>,
    default_file: bool,
) -> Option<(PathBuf, File)> {
    let path = resolve_log_file(name, cli, env, default_file)?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(file) => Some((path, file)),
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
            None
        }
    }
}

/// What [`init`] turns on — the single policy shared by every component.
pub struct Options<'a> {
    /// Component name — the default log file's basename (e.g. `"systema-syss"`).
    pub name: &'a str,
    /// Filter expression (`info`, `debug`, `RUST_LOG` syntax).
    pub level: &'a str,
    /// The `--log-file` value, when the command line carried one.
    pub log_file: Option<PathBuf>,
    /// Consult `SYSTEMA_LOG_FILE` (daemons: yes; interactive tools: no —
    /// they log to a file only when the command line asks for one).
    pub env_file: bool,
    /// Fall back to `<builtin log dir>/<name>.log` when nothing was given
    /// (daemons: yes; interactive tools: no).
    pub default_file: bool,
    /// Enable the stderr sink (daemons: always; tools: `--debug` only).
    pub stderr: bool,
}

/// Daemon policy: stderr always; the file sink resolved as
/// `--log-file` > `SYSTEMA_LOG_FILE` > the built-in default.
/// Returns where this process's logs actually go.
pub fn init_daemon(name: &str, log_file: Option<PathBuf>, level: &str) -> LogTarget {
    init(Options {
        name,
        level,
        log_file,
        env_file: true,
        default_file: true,
        stderr: true,
    })
}

/// Interactive-tool policy: a file sink only with an explicit
/// `--log-file`, stderr only with `--debug` (`stderr`).
/// Returns where this process's logs actually go.
pub fn init_tool(name: &str, log_file: Option<PathBuf>, level: &str, stderr: bool) -> LogTarget {
    init(Options {
        name,
        level,
        log_file,
        env_file: false,
        default_file: false,
        stderr,
    })
}

/// Install the global tracing subscriber for one component — the single
/// initialization every component uses (see the module docs for the sink
/// rules).  Returns where this process's logs actually go.
///
/// The effective path values are recorded at debug level so every run's
/// log documents what it resolved (see [`crate::paths::debug_dump`]).
pub fn init(opts: Options<'_>) -> LogTarget {
    let env = opts
        .env_file
        .then(|| std::env::var_os("SYSTEMA_LOG_FILE"))
        .flatten()
        .filter(|v| !v.is_empty());
    let opened = open_resolved(opts.name, opts.log_file, env, opts.default_file);
    let target = match &opened {
        Some((path, _)) => LogTarget::File(path.clone()),
        None => LogTarget::Stderr,
    };
    let _ = RESOLVED.set(target.clone());

    let filter = match opts.level.parse::<EnvFilter>() {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "{}",
                crate::l10n::fmt(
                    crate::l10n::t_("Invalid log level '{level}' ({e}); using warn"),
                    &[("level", &opts.level.to_string()), ("e", &e.to_string())]
                )
            );
            EnvFilter::new("warn")
        }
    };

    let writer: Option<Box<dyn Write + Send>> = match (opts.stderr, opened) {
        (true, Some((_, file))) => Some(Box::new(TeeWriter {
            primary: Box::new(std::io::stderr()),
            // Colours may reach the terminal stderr; the file stays plain.
            mirror: Box::new(StripAnsiWriter::new(Box::new(file))),
        })),
        (true, None) => Some(Box::new(std::io::stderr())),
        (false, Some((_, file))) => Some(Box::new(file)),
        (false, None) => None,
    };
    if let Some(writer) = writer {
        let ansi = opts.stderr && std::io::stderr().is_terminal();
        tracing_subscriber::fmt()
            .with_ansi(ansi)
            .with_env_filter(filter)
            .with_writer(std::sync::Mutex::new(writer))
            .init();
        // Record the paths this run actually resolved: environments are
        // volatile, env vars and CLI flags can change them between runs.
        let record = match &target {
            LogTarget::File(path) => path.display().to_string(),
            LogTarget::Stderr => "-".to_string(),
        };
        crate::paths::debug_dump(&record);
    }
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

    fn env(s: &str) -> Option<std::ffi::OsString> {
        Some(std::ffi::OsString::from(s))
    }

    #[test]
    fn dash_yields_no_file() {
        assert_eq!(
            resolve_log_file("whatever", Some(PathBuf::from("-")), None, true),
            None
        );
        assert_eq!(resolve_log_file("x", None, env("-"), true), None);
    }

    #[test]
    fn cli_beats_env_beats_default() {
        assert_eq!(
            resolve_log_file("name", Some("/cli/x.log".into()), env("/env/x.log"), true),
            Some(PathBuf::from("/cli/x.log"))
        );
        assert_eq!(
            resolve_log_file("name", None, env("/env/x.log"), true),
            Some(PathBuf::from("/env/x.log"))
        );
        let default = resolve_log_file("name", None, None, true).unwrap();
        assert!(default.ends_with("name.log"));
        assert!(default.starts_with(crate::paths::builtin_log_dir()));
        // Tools: no default and no env — nothing explicit, no file sink.
        assert_eq!(resolve_log_file("name", None, None, false), None);
        assert_eq!(
            resolve_log_file("name", None, env("/env/x.log"), false),
            None
        );
    }

    #[test]
    fn creates_directory_and_file() {
        let dir = std::env::temp_dir().join(format!("sysa-logtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sub = dir.join("nested");
        let opened = open_resolved("unit-test", Some(sub.join("unit-test.log")), None, false);
        let (path, _file) = opened.expect("expected a file target");
        assert!(path.starts_with(&sub) && path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unwritable_degrades_to_no_file() {
        // A path *under a file* can never be created nor opened.
        let blocker = std::env::temp_dir().join(format!("sysa-logblock-{}", std::process::id()));
        std::fs::write(&blocker, b"x").unwrap();
        let target = open_resolved(
            "unit-test",
            Some(blocker.join("sub").join("unit-test.log")),
            None,
            false,
        );
        assert!(target.is_none());
        let _ = std::fs::remove_file(&blocker);
    }

    #[test]
    fn child_file_uses_own_directory() {
        let target = LogTarget::File(PathBuf::from("/var/log/systema/systema-sysi.log"));
        assert_eq!(
            target.child_file("systema-sysa"),
            PathBuf::from("/var/log/systema/systema-sysa.log")
        );
        // A stderr-only parent tells its children to stay on stderr too.
        assert_eq!(
            LogTarget::Stderr.child_file("systema-sysa"),
            PathBuf::from("-")
        );
        // Before init nothing is resolved: the child gets `-`.
        assert_eq!(child_log_file("systema-sysa"), PathBuf::from("-"));
    }

    #[test]
    fn tee_writer_fans_out_to_both_destinations() {
        let primary_buf = Arc::new(Mutex::new(Vec::new()));
        let mirror_buf = Arc::new(Mutex::new(Vec::new()));
        let mut tee = TeeWriter {
            primary: Box::new(CaptureWriter(primary_buf.clone())),
            mirror: Box::new(CaptureWriter(mirror_buf.clone())),
        };

        tee.write_all(b"hello both\n").unwrap();
        tee.flush().unwrap();

        assert_eq!(*primary_buf.lock().unwrap(), b"hello both\n");
        assert_eq!(*mirror_buf.lock().unwrap(), b"hello both\n");
    }

    #[test]
    fn write_to_failed_sink_still_reaches_the_other() {
        struct FailWriter;
        impl Write for FailWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("sink gone"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("sink gone"))
            }
        }

        let primary_buf = Arc::new(Mutex::new(Vec::new()));
        let mut tee = TeeWriter {
            primary: Box::new(CaptureWriter(primary_buf.clone())),
            mirror: Box::new(FailWriter),
        };

        // The primary write must still succeed even when the mirror errors.
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
}
