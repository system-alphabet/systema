# AGENTS.md

Rust workspace (edition 2021): a non-pid-1 systemd shim. Architecture docs:
`README.md` (component/letter tables, socket map); design/process docs in
`doc/` are Chinese decision-record notes — background reading only. Code
changes do **not** need to be written back into these docs.

## Toolchain
- **Nightly cargo required**: four crates use `cargo-features =
  ["different-binary-name"]` (built binary filenames `systema-sysf.systemd`,
  `systema-sysm.linux`, `systema-sysm.unix`, `systema-sysw.systemd`); stable
  cargo fails on the workspace.
- Build deps: `protoc` and system gettext. Build: `cargo build --workspace`.
  The repo bar is **0 errors / 0 warnings** on every crate.
- No rustfmt.toml / clippy.toml / CI config in-repo. Gate flow from docs:
  `rustfmt --check` **only on changed files**, plus a cross-target matrix
  (x86_64-linux, FreeBSD ×2 configs, OpenBSD, Android aarch64+x86_64) all 0e/0w.
- Commit messages: Conventional Commits (`feat:`, `fix:`, `refactor:`, …).
  The subject after the colon starts with a capital letter and ends with a
  period. Language split: `doc/` and `.po` in Chinese; code comments,
  identifiers and commit messages in English.

## Workspace gotchas
- **Directory name ≠ package name**: `crates/libsysa` is package **`sysa`**,
  `crates/libsysa-ipc` is **`sysa-ipc`**. `cargo test -p libsysa` fails with
  "did not match any packages" — always target packages `sysa`/`sysa-ipc` etc.
- `libsysa` re-exports the whole `sysa-ipc` facade (`pub use sysa_ipc::{…}`)
  precisely so every consumer keeps the old `sysa::ipc`/`sysa::proto`/
  `sysa::worker_ipc` paths. Never move modules without preserving re-exports.
- All proto Rust types are generated into `OUT_DIR` at build time by
  `crates/libsysa-ipc/build.rs` (which also bakes compile-time path defaults
  and compiles `.po` → `.mo`). **Edit `proto/*.proto` only**; never commit
  generated files. `libsysa/build.rs` only emits build info.
- `libsysa-ipc/build.rs` bakes default filesystem paths from env vars at build
  time; `SYSTEMA_*` env overrides at runtime. On Termux/Android **source
  `termux-env.sh` (or `termux-env-local.sh`) before `cargo build`** so the
  baked defaults match `$PREFIX`; an env change re-runs build.rs — no
  `cargo clean` needed.

## Architecture facts agents get wrong
- **Two-plane IPC, enforced at protoc level.** Each of
  `proto/workload.proto` (`worker.*`, `method.*`, `unit.*`, `finder.*`,
  `staging.query*`) and `proto/control.proto` (`manager.*`, `admin.*`,
  pushed `unit.*`/`job.*` events) may import only `proto/common.proto`.
  Workload traffic rides `allocator.sock`, control/admin rides
  `control.socket`; nothing crosses. Put new messages in the correct plane.
- Method strings (`manager.start_units`, `worker.ack`, …) are the routing
  keys and public API — renaming one is a breaking change
  (`doc/ipc-api-stability.md`). Session order: first frame `ipc.version`
  negotiation, then `manager.hello` (an `admin.*` request may arrive as
  the first frame instead).
- System A holds **desired state only**; actual state arrives from workers
  as pushed events and is cached in `CachedUnitState`
  (`crates/backend/systema-sysa/src/state.rs`). Don't move runtime state
  into System A's own structs.
- Every binary stays **single-threaded async**:
  `#[tokio::main(flavor = "current_thread")]`. No blocking calls on the
  async runtime (sync `parking_lot` locks only for short critical
  sections).
- Platform differences **degrade, never fail to compile**: platform-only
  deps go under `[target.'cfg(any(target_os = "linux", target_os =
  "android"))'.dependencies]` (e.g. `libc` lives in the `*.linux` crates
  only), and capability must fall back to a `Noop*Controller`-style
  implementation on other targets. A bare unconditional `libc`/`nix`
  dependency breaks the FreeBSD/OpenBSD/Android matrix.
- Worker lifecycle (reconnect+backoff, graceful `worker.exit`
  handshake) lives **inside** `WorkerIpc::run()` in `sysa-ipc`, but the
  loop watches **no** process signals: a caller asks it to leave through
  `WorkerIpc::shutdown(reason)` / a `ShutdownHandle`, and the loop exits
  at a loop boundary (cleanup on the still-open connection, then the
  goodbye). Signal handling lives in `libsysa::signals`; a worker that
  wants SIGTERM/SIGINT semantics calls
  `sysa::signals::forward_shutdown(ipc.shutdown_handle())` before
  `run()` (System Init's power worker instead calls `shutdown(...)` from
  its own disconnect path). Never tear the socket down yourself — e.g.
  racing the loop with your own `shutdown_signal()` in `main` drops the
  connection before the goodbye; use `WorkerIpc::on_shutdown(cleanup)`
  for local cleanup (see its doc comment).
- Power units (reboot/halt/…) are handled in-process by `systema-sysi`;
  there is deliberately no worker for them.

## Decoupling rules
- **One function, one job**: every function does a single, well-defined
  thing — split rather than interleave concerns inside a function.
- **One component, one job (`do one thing and do it well`)**: a component's
  responsibilities must stay within its boundary. Example: System A (the
  allocator/scheduler) must never hand-parse unit/service config files —
  parsing belongs to the Finder (`systema-sysf-systemd`), which submits
  `UnitIR`/staging units; System A only consumes the parsed model.

## Conventions
- **l10n**: every user-visible string goes through `sysa::l10n::t_()`, with
  named placeholders via `l10n::fmt(t_("… {e}"), &[("e", …)])`. Deliberately
  *not* wrapped: tracing logs, IDs, field names, protocol values. After
  adding strings run `make -C po update-po` (repo root).
- Every binary shares one fixed `main` skeleton / flag set:
  `paths::init()` → `l10n::init()` → `signals::install_signal_guard()` →
  clap parsed manually with `.mut_arg(…)` localized help →
  `logging::init_daemon(name, log_file, level)`. Flags: `-D`,
  `--log-level` (default `warn`), `--log-file` (`-` = stderr only),
  `-v`/`--version`, `--full-version`.
- Daemon logging always writes **stderr + log file**; an unopenable log
  file must degrade to stderr with one warning, never abort the process.
- Permission-denied UX: the cross-domain hint (needs sudo / pass `--user`)
  is appended **server-side** (`connect_control()` and the control-port
  privilege checks); clients must not string-match messages — they are
  localized.
- The `org.freedesktop.systemd1` D-Bus surface lives only in
  `systema-sysw.systemd` (and sysw's per-flavor crates); System A and all
  workers stay zbus-free (syss may use D-Bus as a *client* only).

## Tests
- Per package: `cargo test -p sysa-ipc`; single test:
  `cargo test -p sysa-ipc <name>`. Unit tests are colocated `mod tests`
  with `#[tokio::test]`; integration tests in `crates/*/tests/`.
- Worker-IPC tests run against a fake System A at a test-only socket path
  (injection via `#[cfg(test)]` in `sysa-ipc/worker_ipc.rs`) — no root, no
  `/run` sockets, fast on Linux.
- Actually running daemons needs root for `/run/systema` sockets; the
  user-domain instance is `systema-sysu --login` with all sockets under
  `$XDG_RUNTIME_DIR/systema` (strict separation; see `doc/user-session.md`).

## Workflow rules
- Do **not** `git commit` or `git add` (stage) anything after making
  changes — only on the user's explicit request.
- When a change **fully implements / fully fixes** the feature it touches,
  provide a commit message written in **American English**
  (Conventional Commits style, per Toolchain) — don't run the commit.
- Compile tests ("does it build?"): check `findmnt -no FSTYPE -T target`.
  If `target/` is mounted on **tmpfs**, build the **current platform
  only** — no cross-compilation — unless the user explicitly asks.
- If `target/` is mounted on tmpfs, clear `target/` (e.g. `cargo clean`)
  before every build.
- After a build finishes, stop there: do **not** attempt to deploy,
  install, or run the result anywhere unless the user explicitly asks.

## Debugging
- If debugging is blocked by missing runtime information (real system, VM,
  or target-device state), **ask the user for it** instead of hunting for
  it yourself.
- **Never read, mount, or modify the test VM image files** — they are out
  of bounds.
