//! systema-sysh.console — System H, the boot-log console frontend.
//!
//! Prints one line for every unit whose start reached a terminal state,
//! success or failure:
//!
//! ```text
//! [      4821] sshd.service OK [      4903]
//! [      5102] NetworkManager.service FAIL [      6118]
//! ```
//!
//! # Why binding, not connecting
//!
//! The notify channel has no subscription model (`sysa::notify`): System A
//! sends every event as one `key=value` datagram to **every socket file**
//! found in the notify directory.  `init.sock` there is SysAInit's own
//! listener, not a server — nothing accepts connections and nothing
//! forwards another listener's datagrams.  A new consumer therefore joins
//! by binding its own path in that directory, which is what this frontend
//! does (`<notify-dir>/console.sock`).
//!
//! System A creates the notify directory during its own startup, and the
//! console may well be launched before that (from a getty, a service, or a
//! boot hook).  Binding therefore retries every [`RETRY_INTERVAL`] until it
//! works, and stays completely silent while it does: one warning per
//! attempt would flood the very console this program writes to.
//!
//! # Output contract
//!
//! * timestamps are milliseconds on the monotonic clock (time since boot,
//!   suspend excluded), right-aligned in a ten-column field;
//! * `OK` is green, `FAIL` is red, the unit name is white and the two
//!   bracketed timestamps are gray;
//! * ANSI sequences are emitted unconditionally — stdout is never probed
//!   for a tty, so redirecting to a file or a pipe keeps the escapes.
//!
//! A completion whose announcement was never seen (the console joined
//! mid-job, or the datagram was dropped) is still printed, with its
//! completion time standing in for the start, so every completed start
//! appears exactly once.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use clap::Parser;

/// Name of this frontend's listener socket inside the notify directory.
const SOCKET_NAME: &str = "console.sock";

/// How long to wait between bind attempts while the notify directory (or
/// permission to use it) is still missing.  Also used as the pause after an
/// unexpected receive error so a broken descriptor cannot spin the CPU.
const RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Width of the timestamp column, in characters.
const TIME_WIDTH: usize = 10;

const RESET: &str = "\x1b[0m";
const GRAY: &str = "\x1b[90m";
const WHITE: &str = "\x1b[97m";
const GREEN: &str = "\x1b[92m";
const RED: &str = "\x1b[91m";

#[derive(Parser)]
#[command(
    name = "systema-sysh.console",
    about = "System H — boot-log console frontend"
)]
struct Args {
    #[arg(long, short = 'v', help = "Print version information and exit")]
    version: bool,

    #[arg(
        long,
        help = "Print full version: build options and compile-time paths"
    )]
    full_version: bool,
}

/// Build the localized CLI (same pattern as the other System Alphabet
/// binaries: `mut_arg` + `t_()` for every user-visible string).
fn build_args() -> Args {
    use clap::{CommandFactory, FromArgMatches};
    let cmd = Args::command()
        .about(sysa::l10n::t_("System H — boot-log console frontend"))
        .mut_arg("version", |a| {
            a.help(sysa::l10n::t_("Print version information and exit."))
        })
        .mut_arg("full_version", |a| {
            a.help(sysa::l10n::t_(
                "Print full version: build options and compile-time paths.",
            ))
        });
    Args::from_arg_matches(&cmd.get_matches()).unwrap_or_else(|e| e.exit())
}

fn main() {
    sysa::paths::init();
    sysa::l10n::init(sysa::paths::instance().locale_dir);

    let args = build_args();
    if args.version {
        sysa::version::print_version();
        return;
    }
    if args.full_version {
        sysa::version::print_full_version();
        return;
    }

    let notify_dir = sysa::paths::instance().notify_dir.clone();
    let (listener, socket_file) = bind_listener(&notify_dir);

    let mut console = Console::new();
    let mut buf = [0u8; 4096];
    loop {
        match listener.recv(&mut buf) {
            Ok(n) => {
                let body = String::from_utf8_lossy(&buf[..n]);
                if sysa::notify::is_shutdown(&body) {
                    // System A is exiting and waits for this listener to
                    // close before it leaves.
                    break;
                }
                if let Some(line) = console.on_datagram(&body, monotonic_ms()) {
                    println!("{}", line.render());
                    // stdout is line-buffered by std, but the console is
                    // often piped into a logger that must see each line as
                    // soon as the unit finished.
                    let _ = io::stdout().flush();
                }
            }
            // A signal delivered while blocked in recv(2); just go again.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // Not expected on a datagram listener, and warnings are off by
            // contract — pause so a persistently broken descriptor does not
            // turn the loop into a busy spin.
            Err(_) => thread::sleep(RETRY_INTERVAL),
        }
    }

    // Close the listener (the FD first, then the socket file) before exiting
    // so System A observes the closed channel.
    drop(listener);
    drop(socket_file);
}

// ---------------------------------------------------------------------------
// Listener setup
// ---------------------------------------------------------------------------

/// Bind `<notify_dir>/console.sock`, retrying silently until it works.
///
/// Binding — not connecting — is how a notify consumer joins the channel;
/// see the module documentation.  The returned guard removes the socket
/// file on a clean exit so a later run does not have to reclaim it.
fn bind_listener(notify_dir: &str) -> (UnixDatagram, SocketFile) {
    let path = PathBuf::from(notify_dir).join(SOCKET_NAME);
    loop {
        if let Some(listener) = try_bind(&path) {
            let guard = SocketFile::new(path);
            return (listener, guard);
        }
        thread::sleep(RETRY_INTERVAL);
    }
}

/// One bind attempt.  Failures are deliberately not logged.
fn try_bind(path: &Path) -> Option<UnixDatagram> {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    match UnixDatagram::bind(path) {
        Ok(listener) => Some(listener),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            // The name is stale: a previous console was killed before it
            // could unlink it, and nothing but a listener ever writes here,
            // so the file cannot be serving anyone.  System A only reclaims
            // it on the next broadcast, which may be hours away.
            let _ = fs::remove_file(path);
            UnixDatagram::bind(path).ok()
        }
        Err(_) => None,
    }
}

/// The socket file this process created, unlinked on drop — but only while
/// the path still names *our* socket: another console started in the
/// meantime may have taken the name over.
struct SocketFile {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl SocketFile {
    fn new(path: PathBuf) -> Self {
        let (dev, ino) = fs::symlink_metadata(&path)
            .map(|md| (md.dev(), md.ino()))
            .unwrap_or((0, 0));
        SocketFile { path, dev, ino }
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        if let Ok(md) = fs::symlink_metadata(&self.path) {
            if md.dev() == self.dev && md.ino() == self.ino {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Event decoding
// ---------------------------------------------------------------------------

/// Milliseconds on the monotonic clock: time since boot, with suspended
/// time excluded.  Absolute rather than relative to this process, so a
/// console started mid-boot reports timestamps on the same scale as the
/// rest of the boot log.
fn monotonic_ms() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime(CLOCK_MONOTONIC, &ts) writes one timespec into
    // an aligned, fully owned local; it cannot fail for this clock id.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64) * 1000 + (ts.tv_nsec as u64) / 1_000_000
}

/// One completed start, ready to print.
struct Line {
    unit: String,
    start: u64,
    end: u64,
    ok: bool,
}

impl Line {
    /// `[<start>] <unit> <OK|FAIL> [<end>]`, with the color contract of the
    /// module documentation.  Padding is computed on the digits, so the
    /// escape sequences never disturb the ten-column alignment.
    fn render(&self) -> String {
        let status = if self.ok {
            format!("{GREEN}OK{RESET}")
        } else {
            format!("{RED}FAIL{RESET}")
        };
        format!(
            "{GRAY}[{:>WIDTH$}]{RESET} {WHITE}{}{RESET} {status} {GRAY}[{:>WIDTH$}]{RESET}",
            self.start,
            self.unit,
            self.end,
            WIDTH = TIME_WIDTH,
        )
    }
}

/// Start-time bookkeeping for units that were announced but have not
/// finished starting yet.
struct Console {
    starts: HashMap<String, u64>,
}

impl Console {
    fn new() -> Self {
        Console {
            starts: HashMap::new(),
        }
    }

    /// Apply one notify datagram.  `now` is the monotonic time of receipt,
    /// used both for a fresh announcement and for the completion itself.
    ///
    /// Returns the line to print when the datagram reports a completed
    /// start; every other event (worker readiness, stops, …) is ignored.
    fn on_datagram(&mut self, body: &str, now: u64) -> Option<Line> {
        let mut starting = None;
        let mut started = None;
        let mut stopped = None;
        let mut result = None;
        for line in body.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "UNIT_STARTING" => starting = Some(value),
                "UNIT_STARTED" => started = Some(value),
                "UNIT_STOPPED" => stopped = Some(value),
                "RESULT" => result = Some(value),
                _ => {}
            }
        }

        // A stop abandons any start still in flight for that unit; a fresh
        // announcement (a restart) replaces the recorded time.
        if let Some(unit) = stopped {
            self.starts.remove(unit);
        }
        if let Some(unit) = starting {
            self.starts.insert(unit.to_string(), now);
        }

        let unit = started?;
        let start = self.starts.remove(unit).unwrap_or(now);
        Some(Line {
            unit: unit.to_string(),
            start,
            end: now,
            ok: result == Some("success"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip SGR sequences so assertions read as the terminal shows them.
    fn plain(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn start_then_completion_reports_both_times() {
        let mut console = Console::new();
        assert!(console
            .on_datagram("UNIT_STARTING=sshd.service\n", 1_234)
            .is_none());
        let line = console
            .on_datagram("UNIT_STARTED=sshd.service\nRESULT=success\n", 1_300)
            .expect("completion prints a line");
        assert_eq!(
            plain(&line.render()),
            "[      1234] sshd.service OK [      1300]"
        );
    }

    #[test]
    fn failed_start_reports_fail() {
        let mut console = Console::new();
        console.on_datagram("UNIT_STARTING=broken.service\n", 10);
        let line = console
            .on_datagram("UNIT_STARTED=broken.service\nRESULT=failed\n", 20)
            .unwrap();
        assert!(!line.ok);
        assert_eq!(
            plain(&line.render()),
            "[        10] broken.service FAIL [        20]"
        );
    }

    #[test]
    fn completion_without_announcement_uses_its_own_time() {
        let mut console = Console::new();
        let line = console
            .on_datagram("UNIT_STARTED=late.service\nRESULT=success\n", 4_821)
            .unwrap();
        assert_eq!(line.start, 4_821);
        assert_eq!(line.end, 4_821);
    }

    #[test]
    fn stop_drops_a_pending_start() {
        let mut console = Console::new();
        console.on_datagram("UNIT_STARTING=flapping.service\n", 100);
        assert!(console
            .on_datagram("UNIT_STOPPED=flapping.service\nRESULT=success\n", 200)
            .is_none());
        let line = console
            .on_datagram("UNIT_STARTED=flapping.service\nRESULT=success\n", 900)
            .unwrap();
        assert_eq!(line.start, 900);
    }

    #[test]
    fn unrelated_events_are_ignored() {
        let mut console = Console::new();
        for body in [
            "MANAGER_READY=1\n",
            "STATUS=ipc-ready\n",
            "WORKER_READY=system-s-1\n",
            "UNIT_STOPPING=sshd.service\n",
            "nonsense\n",
            "",
        ] {
            assert!(console.on_datagram(body, 5).is_none(), "body: {body:?}");
        }
    }

    #[test]
    fn timestamps_are_right_aligned_in_ten_columns() {
        let line = Line {
            unit: "a.service".to_string(),
            start: 7,
            end: 1_234_567_890,
            ok: true,
        };
        assert_eq!(
            plain(&line.render()),
            "[         7] a.service OK [1234567890]"
        );
        // Ten-digit values are not truncated: the field simply grows.
        let wide = Line {
            unit: "b.service".to_string(),
            start: 12_345_678_901,
            end: 12_345_678_902,
            ok: true,
        };
        assert!(plain(&wide.render()).starts_with("[12345678901] b.service OK ["));
    }

    #[test]
    fn colors_are_always_emitted() {
        let line = Line {
            unit: "a.service".to_string(),
            start: 1,
            end: 2,
            ok: true,
        };
        let rendered = line.render();
        assert!(rendered.contains("\x1b[90m["));
        assert!(rendered.contains("\x1b[97ma.service\x1b[0m"));
        assert!(rendered.contains("\x1b[92mOK\x1b[0m"));

        let failed = Line {
            unit: "a.service".to_string(),
            start: 1,
            end: 2,
            ok: false,
        };
        assert!(failed.render().contains("\x1b[91mFAIL\x1b[0m"));
    }
}
