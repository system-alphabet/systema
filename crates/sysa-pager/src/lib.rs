use std::env;
use std::io::{self, ErrorKind, Write};
use std::os::unix::io::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

/// Configuration for opening a pager.
#[derive(Clone, Debug)]
#[derive(Default)]
pub struct PagerConfig {
    /// When true, `open()` returns a no-op guard and no pager is started.
    pub disable: bool,
}


static STATE: Mutex<Option<PagerState>> = Mutex::new(None);

struct PagerState {
    child: Child,
    saved_stdout: libc::c_int,
    saved_stderr: libc::c_int,
}

/// Opens the pager: redirects `stdout` and `stderr` into the pager process
/// so that all normal output (`println!`, `eprintln!`, …) is automatically
/// paged.  ANSI escape sequences are preserved (``-R`` is set for *less*).
///
/// When the returned [`PagerGuard`] is dropped the pager is closed and
/// original stdout/stderr are restored.  `SIGPIPE` is ignored so that a
/// closed pager produces `BrokenPipe` I/O errors instead of killing the
/// process — callers should handle `ErrorKind::BrokenPipe` gracefully
/// (e.g. by returning early).
pub fn open(config: PagerConfig) -> io::Result<PagerGuard> {
    if config.disable {
        return Ok(PagerGuard { active: false });
    }

    if !stdout_is_tty() || terminal_is_dumb() {
        return Ok(PagerGuard { active: false });
    }

    let mut state = STATE.lock().unwrap();
    if state.is_some() {
        return Ok(PagerGuard { active: false });
    }

    ignore_sigpipe();

    let pager = find_pager();
    let pager_name = std::path::Path::new(&pager)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&pager)
        .to_string();

    let mut cmd = Command::new(&pager);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let pager_is_less = pager_name == "less" || pager_name == "pager";
    let mut less_opts = env::var("SYSTEMA_LESS")
        .or_else(|_| env::var("SYSTEMD_LESS"))
        .unwrap_or_else(|_| "FRSXMK".into());
    if !less_opts.contains('R') {
        less_opts.push('R');
    }
    if pager_is_less {
        cmd.env("LESS", &less_opts);
        cmd.env("LESSCHARSET", "utf-8");
    } else {
        cmd.env("LESS", &less_opts);
    }

    let mut child = cmd.spawn().map_err(|e| {
        io::Error::other(sysa::l10n::fmt(
            sysa::l10n::t_("failed to spawn pager '{pager}': {error}"),
            &[("pager", &pager), ("error", &e.to_string())],
        ))
    })?;

    let child_stdin = child.stdin.take().expect("stdin configured as piped");
    let child_stdin_fd = child_stdin.as_raw_fd();

    let (saved_stdout, saved_stderr) = unsafe {
        let so = libc::dup(libc::STDOUT_FILENO);
        if so < 0 {
            let e = io::Error::last_os_error();
            let _ = child.wait();
            return Err(e);
        }

        if libc::dup2(child_stdin_fd, libc::STDOUT_FILENO) < 0 {
            let e = io::Error::last_os_error();
            libc::dup2(so, libc::STDOUT_FILENO);
            libc::close(so);
            let _ = child.wait();
            return Err(e);
        }

        let se = libc::dup(libc::STDERR_FILENO);
        if se < 0 {
            let e = io::Error::last_os_error();
            libc::dup2(so, libc::STDOUT_FILENO);
            libc::close(so);
            let _ = child.wait();
            return Err(e);
        }

        if libc::dup2(child_stdin_fd, libc::STDERR_FILENO) < 0 {
            let e = io::Error::last_os_error();
            libc::dup2(se, libc::STDERR_FILENO);
            libc::close(se);
            libc::dup2(so, libc::STDOUT_FILENO);
            libc::close(so);
            let _ = child.wait();
            return Err(e);
        }

        (so, se)
    };

    drop(child_stdin);

    *state = Some(PagerState {
        child,
        saved_stdout,
        saved_stderr,
    });

    Ok(PagerGuard { active: true })
}

/// RAII guard that closes the pager on drop.
///
/// If the guard is dropped without an explicit `close()` call the pager
/// process is waited on and stdout/stderr are restored automatically.
#[must_use]
pub struct PagerGuard {
    active: bool,
}

impl Drop for PagerGuard {
    fn drop(&mut self) {
        if self.active {
            close_inner();
        }
    }
}

/// Explicitly close the pager, wait for it to finish, and restore
/// stdout/stderr.
pub fn close() {
    close_inner();
}

fn close_inner() {
    let mut guard = STATE.lock().unwrap();
    if let Some(mut state) = guard.take() {
        let _ = io::stdout().flush();
        let _ = io::stderr().flush();

        unsafe {
            libc::dup2(state.saved_stdout, libc::STDOUT_FILENO);
            libc::close(state.saved_stdout);
            libc::dup2(state.saved_stderr, libc::STDERR_FILENO);
            libc::close(state.saved_stderr);
        }

        let _ = state.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn stdout_is_tty() -> bool {
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}

fn terminal_is_dumb() -> bool {
    env::var("TERM").as_deref() == Ok("dumb")
}

fn ignore_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

fn find_pager() -> String {
    for var in &["SYSTEMA_PAGER", "SYSTEMD_PAGER", "PAGER"] {
        if let Ok(val) = env::var(var) {
            let trimmed = val.trim().to_string();
            if !trimmed.is_empty() && trimmed != "cat" {
                return trimmed;
            }
        }
    }

    for name in &["pager", "less", "more"] {
        if command_exists(name) {
            return name.to_string();
        }
    }

    "less".to_string()
}

fn command_exists(name: &str) -> bool {
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

/// `println!` variant that silently ignores `BrokenPipe` errors.
///
/// When the pager is closed (user pressed `q`), writing to stdout would
/// normally trigger a panic.  This macro catches the error and discards it
/// so the program can continue or exit gracefully.
#[macro_export]
macro_rules! pager_println {
    ($($arg:tt)*) => {{
        use ::std::io::Write;
        let _ = writeln!(::std::io::stdout().lock(), $($arg)*);
    }};
}

/// `print!` variant that silently ignores `BrokenPipe` errors.
#[macro_export]
macro_rules! pager_print {
    ($($arg:tt)*) => {{
        use ::std::io::Write;
        let _ = write!(::std::io::stdout().lock(), $($arg)*);
    }};
}

/// `eprintln!` variant that silently ignores `BrokenPipe` errors.
#[macro_export]
macro_rules! pager_eprintln {
    ($($arg:tt)*) => {{
        use ::std::io::Write;
        let _ = writeln!(::std::io::stderr().lock(), $($arg)*);
    }};
}

/// Wrapper around a [`Write`]r that silently swallows `BrokenPipe` errors.
///
/// Useful when you want to keep writing through a pager even after the user
/// has closed it (e.g. in cleanup / footer code) without causing panics or
/// spurious error returns.
pub struct BrokenPipeSilencer<W: Write>(pub W);

impl<W: Write> Write for BrokenPipeSilencer<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.0.write(buf) {
            Err(e) if e.kind() == ErrorKind::BrokenPipe => Ok(buf.len()),
            other => other,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.0.flush() {
            Err(e) if e.kind() == ErrorKind::BrokenPipe => Ok(()),
            other => other,
        }
    }
}
