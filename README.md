# System Alphabet (systema)

**System Alphabet** is a cross-platform, event-driven system resource manager.
It is a *non-pid1* systemd shim: it presents the well-known
`org.freedesktop.systemd1` D-Bus interface so that existing front-end tools
(`systemctl`, `busctl`, GNOME Settings, …) keep working, while the engine behind that
interface is a set of small single-purpose workers that can be swapped out.

## Architecture

```text
   systemctl / busctl / desktop shells
              │  D-Bus   org.freedesktop.systemd1
              ▼
 ┌──────────────────────────────────────────┐
 │ systema-sysw.systemd   System Wrapper    │   D-Bus surface lives HERE,
 │  Manager / Activator / per-unit objects  │   not in System A
 └───────────────────┬──────────────────────┘
                     │  /run/systema/control.socket     proto/control.proto
                     │  manager.* · admin.* · pushed unit/job events
                     │
 ┌───────────────────▼──────────────────────┐        /run/systema/notify/init.sock
 │ systema-sysi   SysAInit  (System I)      │◄────── sd_notify-style text broadcast
 │  1. spawn System A, await MANAGER_READY  │        MANAGER_READY=1, UNIT_STARTING=…
 │  2. spawn each worker, await WORKER_READY│
 │  3. control phase (daemon-reload, start) │        systema-stagingctl
 └───────────────────┬──────────────────────┘        systema-unitstatectl
                     │ spawns
 ┌───────────────────▼──────────────────────┐
 │ systema-sysa   System A  (Allocator)     │   desired state ONLY — never actual state
 │  unit loader · dependency graph ·        │
 │  scheduler · dispatch                    │
 └───────────────────┬──────────────────────┘
                     │  /run/systema/allocator.sock    proto/workload.proto
                     │  worker.register · method.call · unit.state_update · finder.*
        ┌────────────┼──────────────┬──────────────────────────┐
        ▼            ▼              ▼                          ▼
   Sys S service  Sys T target  Sys C timer  Sys K socket   Sys N path
   Sys D device   Sys E scope   Sys R slice  Sys M mount    …
        ▲
        │ finder.register_units / finder.commit_units  (staging, UID-bound)
 systema-sysf  ── spawns ──▶  systema-sysf.systemd  (parses systemd unit files)
```

Two independent planes, deliberately separated:

- **Workload plane** (`allocator.sock`) — System A and the Workers/Finder. Pure
  execution traffic; no manager or admin request ever rides this socket.
- **Control plane** (`control.socket`) — System A and the bridge, SysAInit and the
  admin CLIs. Nothing here is exposed to workers.

## Components

### Binaries (17)

| Binary | System | Role |
|--------|--------|------|
| `systema-sysi` | **SysAInit** (System I) | init: mounts API filesystems, spawns System A, then the workers serially; can run as PID 1 or as a plain process |
| `systema-sysa` | **System A** (Allocator) | global control plane: load/parse units, resolve dependencies, schedule, keep *desired* state |
| `systema-syss` | **System S** (Service) | `fork/exec` service processes, lifecycle state machine, subreaper for orphans, `Type=notify` / `Type=dbus` readiness |
| `systema-syst` | **System T** (Target) | target activation — no external processes, state tracking only |
| `systema-sysc` | **System C** (Cron) | owns every `.timer`: monotonic and calendar schedules, fires `timer.fired` |
| `systema-sysk` | **System K** (Socket) | owns every `.socket`: accept=no activation watching, accept=yes per-connection spawn |
| `systema-sysn` | **System N** (Notify) | owns every `.path`: filesystem condition watches (`PathExists=`, `PathChanged=`, …) |
| `systema-sysd` | **System D** (Device) | owns every `.device`: discovers real devices in `/dev`, injects them through the staging area |
| `systema-syse` | **System E** (External) | owns every `.scope`: transient wrappers around externally created processes (`PIDs=`) |
| `systema-sysr` | **System R** (Resource) | cgroup v2 hierarchy and CPU/memory/I-O/pid quotas for slices and services; registers `user.slice` dynamically |
| `systema-sysm.linux` | **System M** (Mount, Linux) | owns `.mount` / `.automount` on Linux (inert stub on other platforms) |
| `systema-sysm.unix` | **System M** (Mount, Unix) | same, reading the mount table via `getmntinfo(3)` / `/etc/mnttab` / `mount -p` |
| `systema-sysf` | **System F** (Finder) | generic finder runner: scans the finder paths, runs all finders concurrently, commits the staging area |
| `systema-sysf.systemd` | System F's finder | parses systemd unit files and submits them to System A |
| `systema-sysw.systemd` | **System Wrapper** | serves `org.freedesktop.systemd1` on the system bus, backed by System A over the control socket |
| `systema-stagingctl` | CLI | `list` / `commit` staging areas |
| `systema-unitstatectl` | CLI | `list` unit states as YAML |

### Libraries (6)

`sysa` (`crates/libsysa`) — shared by System A and every worker; `sysa-pager`;
`systema-sysw-common`; `systema-sysr-common`; `systema-sysr.linux`; `systema-sysm-common`.

Letters without a crate: **B** (unused), **P** — *removed*: System Init manages the
`power` unit type directly (`power` units trigger `reboot(2)` and friends in-process).

## Startup and process model

`systema-sysi` is the init-like binary:

1. Mount the API filesystems (cgroup v2, `/dev/shm`, `/dev/pts`) — a PID 1 duty.
2. Spawn System A first and wait for `MANAGER_READY` on the notify channel.
3. Spawn the workers **serially**, each having to report `WORKER_READY=<id>` before the
   next one starts. On Linux the default set is 11 processes.
4. Run the control phase over `control.socket`: `manager.hello` →
   `manager.daemon_reload` (which makes System A spawn System F) →
   `manager.register_power_units` → `manager.list_units` → `manager.start_units`.
5. Supervise until exit.

System F is deliberately **not** in the supervised set — System A spawns it on demand
through its reload task. `systema-stagingctl` and `systema-unitstatectl` are
operational CLIs, not daemons.

All binaries run single-threaded async: `#[tokio::main(flavor = "current_thread")]`.

Every daemon writes `<log-dir>/<name>.log` (default `/var/log/systema`) and mirrors to
`/dev/console`.

## Sockets

| Path | Server | Client(s) | Protocol |
|------|--------|-----------|----------|
| `/run/systema/allocator.sock` | System A | workers, Finder | length-delimited protobuf `Envelope` |
| `{runstatedir}/systema/control.socket` | System A | SysW bridge, SysAInit, both CLIs | length-delimited protobuf `Envelope` |
| `{runstatedir}/systema/notify/*.sock` | — (directory of listeners) | System A broadcasts | sd_notify-style `key=value` datagrams; SysAInit binds `init.sock` |
| `/run/systema/fdpass.sock` | System A | System S, System K | raw `SCM_RIGHTS` fd passing, no framing |
| `/run/systema/sysf.sock` | `systema-sysf daemon-reload` | (System A spawns System F directly instead) | one-byte trigger + one-byte ack |

### Framing

`tokio_util::codec::LengthDelimitedCodec` over a Unix stream, max frame 16 MiB
(4-byte big-endian length prefix). `send_envelope` / `recv_envelope` / `make_envelope`
in `libsysa::ipc` are the only place frames are built or parsed.

```protobuf
message Envelope {
    uint64 request_id = 1;   // 0 for one-way events
    string source     = 2;   // "system-a", "system-s", …
    string target     = 3;
    string method     = 4;   // "worker.register", "manager.hello", …
    bytes  payload    = 5;   // nested protobuf message
}
```

On the control socket the **first** envelope must be `manager.hello`, otherwise System
A closes the session; alternatively the first frame may be a one-shot `admin.*`
request (`admin.staging`, `admin.unitstate`), which requires the caller to be root or
the UID running System A.

### Proto sources

`proto/` holds three files, mirroring the plane boundary:

| File | Contents |
|------|----------|
| `proto/common.proto` | `Envelope` plus the shared unit model (`UnitConfig`, `ServiceConfig`, `SocketConfig`, …) |
| `proto/workload.proto` | allocator-socket traffic: `worker.*`, `method.*`, `unit.state*`, `finder.*`, `staging.query*`, `timer.fired`, … |
| `proto/control.proto` | control-socket traffic: `manager.*`, `admin.*`, pushed `unit.*` / `job.*` events |

Each plane imports only `common.proto`, so `protoc` rejects any type reference that
crosses the boundary. prost emits one Rust file per proto package, so all three land in
a single flat `sysa::proto::` module.

## Unit search paths

Unit files are read from, in order:

1. `/etc/systema/`
2. `/run/systema/`
3. `/usr/local/lib/systema/`
4. `/usr/lib/systema/`
5. `/etc/systemd/system/` *(compatibility)*
6. `/usr/lib/systemd/system/` *(compatibility)*
7. `/lib/systemd/system/` *(compatibility)*

Overridable with `SYSTEMA_UNIT_PATH` (`:`-separated).

Three further lists are defined alongside it in `crates/libsysa/build.rs`:

- **Generators** — `/run/systemd/generator`, `/run/systemd/generator.late`,
  `/etc/systemd/system-generators`, `/usr/local/lib/systemd/system-generators`,
  `/usr/lib/systemd/system-generators`, `/lib/systemd/system-generators`
- **Finders** — `/etc/systema/finder`, `/usr/etc/systema/finder`,
  `/usr/local/etc/systema/finder`, `/opt/systema/finder`
- **Binary lookup** (14 entries) — `/`, `/bin`, `/sbin`, `/lib/systema`,
  `/libexec/systema`, `/usr/bin`, `/usr/sbin`, `/usr/lib/systema`,
  `/usr/libexec/systema`, `/usr/local/bin`, `/usr/local/sbin`,
  `/usr/local/lib/systema`, `/usr/local/libexec/systema`, `/opt/systema`

Every path can be overridden at run time; the variable names are listed in
`crates/libsysa/src/paths.rs`.

## D-Bus surface

The D-Bus server is **`systema-sysw.systemd`**, not System A — System A has no `zbus`
dependency. It owns `org.freedesktop.systemd1` on the system bus and serves:

- `org.freedesktop.systemd1.Manager` — `GetUnit`, `LoadUnit`, `StartUnit`, `StopUnit`,
  `RestartUnit`, `ReloadUnit`, `TryRestartUnit`, `ReloadOrRestartUnit`,
  `EnqueueUnitJob(Many)`, `StartTransientUnit(Many)`, `SetUnitProperties`,
  `ListUnits`/`Filtered`/`ByPatterns`/`ByNames`, `ListJobs`, `ListUnitFiles`/`ByPatterns`,
  `GetUnitFileState`, `GetUnitProcesses`, `Subscribe`/`Unsubscribe`, `Reload`,
  `ResetFailed(Unit)`, `AbandonScope`, `RefUnit`/`UnrefUnit`, `GetUnitByPID`,
  `GetUnitByPIDFD`, `GetUnitByInvocationId`, plus the `JobNew`/`JobRemoved`/
  `UnitRemoved`/`Reloading` signals.
- `org.freedesktop.systemd1.Activator` — systemd-style D-Bus activation requests.
- Per-unit objects registered by kind: `Unit`, `Service`, `Socket`, `Mount`, `Slice`,
  `Scope`.

It keeps an in-memory mirror of System A's unit state, primed through
`manager.unit_snapshot` / `manager.list_snapshots`.

`systema-syss` uses D-Bus only as a *client* (waiting for a `BusName=` to become owned
for `Type=dbus`).

## Building

```bash
# System dependencies
sudo apt-get install protobuf-compiler gettext
# msgfmt is additionally needed only for the `l10n_debug` feature

cargo build --workspace
```

Notes:

- `protoc` is required — `crates/libsysa/build.rs` compiles `proto/*.proto` with
  `prost-build`.
- `gettext-rs` links the system libgettext.
- Four crates use `cargo-features = ["different-binary-name"]` to emit
  `systema-sysf.systemd`, `systema-sysm.linux`, `systema-sysm.unix` and
  `systema-sysw.systemd`; this needs a Cargo that supports that feature.
- `systema-sysm.linux` compiles to an inert stub off Linux;
  `systema-sysr.linux` degrades to a `NoopController` where cgroup v2 is unavailable.
- Workspace edition is 2021; no `rust-version` is pinned.

## Running

```bash
# Everything at once (recommended): SysAInit spawns System A and the workers
sudo mkdir -p /run/systema
sudo ./target/debug/systema-sysi

# …or start System A alone, then the workers you care about
sudo ./target/debug/systema-sysa
sudo ./target/debug/systema-syss

# Then drive it with regular systemd tooling
systemctl --system start sshd.service
systemctl --system status sshd.service

# Operational CLIs
./target/debug/systema-stagingctl list
./target/debug/systema-unitstatectl list
```

## Message localization

User-facing message strings are wrapped in `t_()` from `sysa::l10n` (gettext),
with `l10n::fmt` supplying named placeholders. Tracing logs, IDs, field names and
protocol values are deliberately *not* wrapped.

```rust
bail!(sysa::l10n::fmt(
    sysa::l10n::t_("Failed to create socket directory {path}."),
    &[("path", &parent.display().to_string())],
));
```

Regenerate the template and the Chinese catalog:

```bash
make -C po update-po    # xgettext --keyword=t_:1 --keyword=n_:1,2
```

## Design principles

1. **Control-plane / execution-plane separation.** System A holds *desired* state only;
   real runtime state lives in the workers and is pushed back as events.
2. **Two-plane IPC.** Workload traffic (`allocator.sock`) and control/admin traffic
   (`control.socket`) are separate sockets with separate message sets, sharing only the
   `Envelope` frame and the unit model.
3. **Compatibility at the edge.** The systemd D-Bus surface is served by the bridge,
   so System A stays free of `zbus` and of systemd-specific surface area.
4. **Single-threaded async.** `tokio` `current_thread` everywhere; no thread-per-service.
5. **Platform abstraction.** Worker behavior is split into `*-common` crates with
   platform crates layered on top — `systema-sysm-common` with its `.linux`/`.unix`
   flavors, `systema-sysr-common`'s `ResourceController` with both a cgroup-v2 and a
   `NoopController` implementation — so a non-Linux build keeps working with reduced
   capability instead of failing to compile.

## License

See [`LICENSE`](LICENSE).
