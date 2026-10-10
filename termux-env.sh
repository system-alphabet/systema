#!/bin/sh
# ---------------------------------------------------------------------------
# termux-env.sh — Runtime path configuration for Termux (Android).
#
# Usage:
#   source termux-env.sh          # before starting any systema binary
#   source termux-env.sh && cargo build
#                                 # ALSO bakes these paths into the binaries
#                                 # as compile-time defaults (libsysa
#                                 # build.rs reruns on env change — no
#                                 # `cargo clean` needed)
#
# Termux uses a non-standard filesystem layout rooted at $PREFIX
# (typically /data/data/com.termux/files/usr).  This script exports the
# environment variables that `Paths` resolves at *runtime*, so that they
# match the Termux layout instead of the compile-time defaults baked in by
# `crates/libsysa/build.rs` (/run, /usr, /etc, ...).  When sourced before
# the build, build.rs reads the same variables and bakes *these* values in
# as the defaults instead; the runtime resolution keeps its precedence
# either way (runtime env > baked default).
#
# Mapping used by this script (FHS root -> $PREFIX):
#   /              -> $PREFIX
#   /usr           -> $PREFIX
#   /usr/local     -> $PREFIX/local
#   /etc           -> $PREFIX/etc
#   /run           -> $PREFIX/var/run
#   /var/log       -> $PREFIX/var/log
#   /opt           -> $PREFIX/opt
#   /bin/sh        -> $PREFIX/bin/sh
#
# Authority for the variable list and the colon-separated list syntax:
#   crates/libsysa/src/paths.rs   (resolve / resolve_list)
#   crates/libsysa/build.rs       (same variables read at build time)
# ---------------------------------------------------------------------------

# `set -u` guards only THIS file — since it is sourced, the option must
# NOT leak into the caller's interactive shell, where it breaks zsh
# plugins on every prompt (e.g. powerlevel10k with
# `_z:8: _Z_OWNER: parameter not set`).  Save the caller's nounset state
# and restore it again at the end of this file.
case $- in
    *u*) _systema_env_had_nounset=1 ;;
    *)   _systema_env_had_nounset=0 ;;
esac
set -u

# $PREFIX is normally set by Termux itself; fall back to the default.
: "${PREFIX:=/data/data/com.termux/files/usr}"

# ---------------------------------------------------------------------------
# Runtime directories (/run -> $PREFIX/var/run)
# ---------------------------------------------------------------------------
export SYSTEMA_RUNSTATEDIR="${PREFIX}/var/run"
export SYSTEMA_IPC_SOCKET="${PREFIX}/var/run/systema/allocator.sock"
export SYSTEMA_FDPASS_SOCK="${PREFIX}/var/run/systema/fdpass.sock"
export SYSTEMA_CONTROL_SOCKET="${PREFIX}/var/run/systema/control.socket"
export SYSTEMA_NOTIFY_DIR="${PREFIX}/var/run/systema/notify"
export SYSTEMA_RELOAD_SOCKET="${PREFIX}/var/run/systema/sysf.sock"
export SYSTEMD_FIRST_BOOT_FILE="${PREFIX}/var/run/systemd/first-boot"

# ---------------------------------------------------------------------------
# State / logs
# ---------------------------------------------------------------------------
export SYSTEMD_MACHINE_ID_FILE="${PREFIX}/etc/machine-id"
export SYSTEMA_LOG_DIR="${PREFIX}/var/log/systema"

# ---------------------------------------------------------------------------
# Static data (/usr -> $PREFIX)
# ---------------------------------------------------------------------------
export SYSTEMD_LIB_UNIT_DIR="${PREFIX}/lib/systemd/system"
export SYSTEMA_LOCALE_DIR="${PREFIX}/share/locale"
export SYSTEMA_SHELL_PATH="${PREFIX}/bin/sh"
export SYSTEMA_SOCKET_HANDLER_PATH="${PREFIX}/lib/systema/socket-handler"

# ---------------------------------------------------------------------------
# Search paths (colon-separated; mirror the defaults in
# crates/libsysa/build.rs with the root replaced by $PREFIX, duplicates
# collapsed)
# ---------------------------------------------------------------------------

# Unit files: /etc/systema, /run/systema, /usr/local/lib/systema,
# /usr/lib/systema, /etc/systemd/system, /usr/lib/systemd/system,
# /lib/systemd/system  (the last two both land in $PREFIX/lib/systemd/system)
export SYSTEMA_UNIT_PATH="${PREFIX}/etc/systema:\
${PREFIX}/var/run/systema:${PREFIX}/local/lib/systema:${PREFIX}/lib/systema:\
${PREFIX}/etc/systemd/system:${PREFIX}/lib/systemd/system"

# systemd generators: /run/systemd/generator, /run/systemd/generator.late,
# /etc/systemd/system-generators, /usr/local/lib/systemd/system-generators,
# /usr/lib/systemd/system-generators, /lib/systemd/system-generators
export SYSTEMA_GENERATOR_PATH="${PREFIX}/var/run/systemd/generator:\
${PREFIX}/var/run/systemd/generator.late:\
${PREFIX}/etc/systemd/system-generators:\
${PREFIX}/local/lib/systemd/system-generators:\
${PREFIX}/lib/systemd/system-generators"

# System F finder executables: /etc/systema/finder, /usr/etc/systema/finder,
# /usr/local/etc/systema/finder, /opt/systema/finder  (the first two both
# land in $PREFIX/etc/systema/finder)
export SYSTEMA_FINDER_PATH="${PREFIX}/etc/systema/finder:\
${PREFIX}/local/etc/systema/finder:\
${PREFIX}/opt/systema/finder"

# systema binaries: /, /bin, /sbin, /lib/systema, /libexec/systema,
# /usr/bin, /usr/sbin, /usr/lib/systema, /usr/libexec/systema,
# /usr/local/bin, /usr/local/sbin, /usr/local/lib/systema,
# /usr/local/libexec/systema, /opt/systema
export SYSTEMA_BIN_PATH="${PREFIX}:${PREFIX}/bin:${PREFIX}/sbin:\
${PREFIX}/lib/systema:${PREFIX}/libexec/systema:\
${PREFIX}/local/bin:${PREFIX}/local/sbin:\
${PREFIX}/local/lib/systema:${PREFIX}/local/libexec/systema:\
${PREFIX}/opt/systema"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo "termux-env: PREFIX = ${PREFIX}"
echo "termux-env: SYSTEMA_RUNSTATEDIR = ${SYSTEMA_RUNSTATEDIR}"
echo "termux-env: SYSTEMA_UNIT_PATH = ${SYSTEMA_UNIT_PATH}"
echo "termux-env: All 17 SYSTEMA_* / SYSTEMD_* variables from paths.rs exported."
echo "termux-env: Ready. Run the systema binaries."

# Restore the caller's nounset state (see the `set -u` note at the top).
[ "$_systema_env_had_nounset" -eq 1 ] || set +u
unset _systema_env_had_nounset
