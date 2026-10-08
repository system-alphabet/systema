#!/bin/sh
# ---------------------------------------------------------------------------
# termux-env-local.sh — Runtime path configuration for Termux (Android),
# for a *local* install tree rooted at $LOCAL_PREFIX.
#
# Usage:
#   source termux-env-local.sh    # before starting any systema binary
#   source termux-env-local.sh && cargo build
#                                 # ALSO bakes these paths into the binaries
#                                 # as compile-time defaults (libsysa
#                                 # build.rs reruns on env change — no
#                                 # `cargo clean` needed)
#
# Same variable set as `termux-env.sh` (see that file and
# crates/libsysa/src/paths.rs), but the static/configuration part of the FHS
# tree is rooted at $LOCAL_PREFIX instead of $PREFIX:
#
#   /              -> $LOCAL_PREFIX
#   /usr           -> $LOCAL_PREFIX
#   /usr/local     -> $LOCAL_PREFIX/local
#   /etc           -> $LOCAL_PREFIX/etc
#   /run           -> $PREFIX/var/run      (runtime state stays in $PREFIX)
#   /var/log       -> $PREFIX/var/log
#   /opt           -> $LOCAL_PREFIX/opt
#   machine-id     -> $PREFIX/etc/machine-id
#   /bin/sh        -> $PREFIX/bin/sh       (Termux's own shell)
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
LOCAL_PREFIX="${PREFIX}/local"
export LOCAL_PREFIX

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
# Static data (/usr -> $LOCAL_PREFIX)
# ---------------------------------------------------------------------------
export SYSTEMD_LIB_UNIT_DIR="${LOCAL_PREFIX}/lib/systemd/system"
export SYSTEMA_LOCALE_DIR="${LOCAL_PREFIX}/share/locale"
export SYSTEMA_SHELL_PATH="${PREFIX}/bin/sh"
export SYSTEMA_SOCKET_HANDLER_PATH="${LOCAL_PREFIX}/lib/systema/socket-handler"

# ---------------------------------------------------------------------------
# Search paths (colon-separated; mirror the defaults in
# crates/libsysa/build.rs with the static root replaced by $LOCAL_PREFIX,
# runtime dirs by $PREFIX/var/run, duplicates collapsed)
# ---------------------------------------------------------------------------

# Unit files: /etc/systema, /run/systema, /usr/local/lib/systema,
# /usr/lib/systema, /etc/systemd/system, /usr/lib/systemd/system,
# /lib/systemd/system  (the last two both land in
# $LOCAL_PREFIX/lib/systemd/system)
export SYSTEMA_UNIT_PATH="${LOCAL_PREFIX}/etc/systema:\
${PREFIX}/var/run/systema:${LOCAL_PREFIX}/local/lib/systema:\
${LOCAL_PREFIX}/lib/systema:${LOCAL_PREFIX}/etc/systemd/system:\
${LOCAL_PREFIX}/lib/systemd/system"

# systemd generators: /run/systemd/generator, /run/systemd/generator.late,
# /etc/systemd/system-generators, /usr/local/lib/systemd/system-generators,
# /usr/lib/systemd/system-generators, /lib/systemd/system-generators
export SYSTEMA_GENERATOR_PATH="${PREFIX}/var/run/systemd/generator:\
${PREFIX}/var/run/systemd/generator.late:\
${LOCAL_PREFIX}/etc/systemd/system-generators:\
${LOCAL_PREFIX}/local/lib/systemd/system-generators:\
${LOCAL_PREFIX}/lib/systemd/system-generators"

# System F finder executables: /etc/systema/finder, /usr/etc/systema/finder,
# /usr/local/etc/systema/finder, /opt/systema/finder  (the first two both
# land in $LOCAL_PREFIX/etc/systema/finder)
export SYSTEMA_FINDER_PATH="${LOCAL_PREFIX}/etc/systema/finder:\
${LOCAL_PREFIX}/local/etc/systema/finder:\
${LOCAL_PREFIX}/opt/systema/finder"

# systema binaries: /, /bin, /sbin, /lib/systema, /libexec/systema,
# /usr/bin, /usr/sbin, /usr/lib/systema, /usr/libexec/systema,
# /usr/local/bin, /usr/local/sbin, /usr/local/lib/systema,
# /usr/local/libexec/systema, /opt/systema
export SYSTEMA_BIN_PATH="${LOCAL_PREFIX}:${LOCAL_PREFIX}/bin:\
${LOCAL_PREFIX}/sbin:${LOCAL_PREFIX}/lib/systema:\
${LOCAL_PREFIX}/libexec/systema:\
${LOCAL_PREFIX}/local/bin:${LOCAL_PREFIX}/local/sbin:\
${LOCAL_PREFIX}/local/lib/systema:${LOCAL_PREFIX}/local/libexec/systema:\
${LOCAL_PREFIX}/opt/systema"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo "termux-env-local: PREFIX = ${PREFIX}"
echo "termux-env-local: LOCAL_PREFIX = ${LOCAL_PREFIX}"
echo "termux-env-local: SYSTEMA_RUNSTATEDIR = ${SYSTEMA_RUNSTATEDIR}"
echo "termux-env-local: SYSTEMA_UNIT_PATH = ${SYSTEMA_UNIT_PATH}"
echo "termux-env-local: All 17 SYSTEMA_* / SYSTEMD_* variables from paths.rs exported."
echo "termux-env-local: Ready. Run the systema binaries."

# Restore the caller's nounset state (see the `set -u` note at the top).
[ "$_systema_env_had_nounset" -eq 1 ] || set +u
unset _systema_env_had_nounset
