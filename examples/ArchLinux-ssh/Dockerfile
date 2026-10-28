# syntax=docker/dockerfile:1
#
# Arch Linux container bundling systema (SysAInit + splash console) and sshd.
#
# systema binaries are NOT built here: this expects a pre-built
# `./target/release` in the build context and copies the systema-* binaries
# straight into `/opt/systema`. Build first on the host:
#   cargo build --release
#   podman build --build-arg MIRROR=TUNA \
#       -t systema-arch -f examples/ArchLinux-ssh/Dockerfile .
#
# Layout:
#   /opt/systema/systema-*          all systema binaries
#   /opt/systema/finder/systema-sysf.*   systema-sysf family (e.g. systema-sysf.systemd)
#
# If ./target/release is missing or holds no executable systema binary, the
# build fails (COPY fails on a missing source; the RUN below double-checks
# that executables actually landed).
#
# Mirror semantics (--build-arg MIRROR):
#   (unset)          keep the base image's default Arch mirrors
#   TUNA             -> http://mirrors.tuna.tsinghua.edu.cn/archlinux
#   TUNAARM          -> https://mirrors.tuna.tsinghua.edu.cn/archlinuxarm/
#   contains "://"   -> used directly as a mirror URL
#   anything else    -> the build fails with an error
#
# NOTE: TUNAARM targets aarch64; running a non-native arch image requires an
#       arch/qemu binfmt setup.
#
# NOTE: RUN --mount=type=cache requires BuildKit (Docker 23+ / Podman 4+).
ARG MIRROR=

FROM archlinux/archlinux:latest
ARG MIRROR

# Configure the mirror and install sshd.
RUN --mount=type=cache,target=/var/cache/pacman/pkg,sharing=locked \
    set -eux; \
    if [ -z "$MIRROR" ]; then \
        echo "[mirror] keeping default Arch mirrors"; \
    elif [ "$MIRROR" = "TUNA" ]; then \
        echo "[mirror] TUNA -> http://mirrors.tuna.tsinghua.edu.cn/archlinux"; \
        echo "Server = http://mirrors.tuna.tsinghua.edu.cn/archlinux/\$repo/os/\$arch" > /etc/pacman.d/mirrorlist; \
    elif [ "$MIRROR" = "TUNAARM" ]; then \
        echo "[mirror] TUNAARM -> https://mirrors.tuna.tsinghua.edu.cn/archlinuxarm/"; \
        echo "Server = https://mirrors.tuna.tsinghua.edu.cn/archlinuxarm/os" > /etc/pacman.d/mirrorlist; \
    elif printf '%s' "$MIRROR" | grep -q "://"; then \
        echo "[mirror] URL -> $MIRROR"; \
        echo "Server = $MIRROR/\$repo/os/\$arch" > /etc/pacman.d/mirrorlist; \
    else \
        echo "ERROR: unknown MIRROR '$MIRROR' (use TUNA, TUNAARM, or a URL containing '://')" >&2; \
        exit 1; \
    fi; \
    pacman -Syu --noconfirm openssh

# Copy the pre-built systema binaries straight into /opt/systema. With no
# matching source (./target/release missing or empty of systema-*), BuildKit
# aborts here.
COPY ./target/release/systema-* /opt/systema/

# Verify executables landed, move the systema-sysf family into ./finder, and
# write /init.
RUN set -eux; \
    if ! find /opt/systema -maxdepth 1 -type f -executable | grep -q .; then \
        echo "ERROR: no executable systema binaries found in ./target/release (run 'cargo build --release' first)" >&2; \
        exit 1; \
    fi; \
    shopt -s nullglob; \
    sysf=( /opt/systema/systema-sysf.* ); \
    if (( ${#sysf[@]} > 0 )); then \
        mkdir -p /opt/systema/finder; \
        mv "${sysf[@]}" /opt/systema/finder/; \
    fi

# /init: run sysa-init and the splash console in parallel, then let
# /usr/bin/bash take over the foreground so the container stays alive.
RUN set -eux; \
    printf '%s\n' \
      '#!/bin/bash' \
      'echo "[init] starting systema workers..."' \
      '/opt/systema/systema-sysi &' \
      '/opt/systema/systema-sysh-console &' \
      'exec /usr/bin/bash' > /init; \
    chmod +x /init

ENTRYPOINT ["/init"]