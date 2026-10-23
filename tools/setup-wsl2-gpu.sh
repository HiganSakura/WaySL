#!/bin/sh
# Configure a WSL2 guest so WaySL and its clients use the real GPU.
#
# WSL2 exposes the GPU only through /dev/dxg. /dev/dri/card0 and renderD128 are
# a vgem stub (DRIVER=faux_driver), which defeats Mesa's automatic driver
# detection: eglInitialize fails with 0x3003 and clients silently fall back to
# llvmpipe or abort with "Failed to create surface for any enabled backend".
# Selecting the D3D12 Gallium driver is what routes rendering to the GPU.
#
# This script only touches environment files. It is safe to re-run.
#
# Usage: sudo ./tools/setup-wsl2-gpu.sh [--check|--uninstall]

set -eu

PROFILE_FILE=/etc/profile.d/waysl-gpu.sh
SYSTEMD_FILE=/etc/systemd/system.conf.d/waysl-gpu.conf
ENVIRONMENT_FILE=/etc/environment

DRIVER=d3d12

usage() {
    cat <<'EOF'
Usage: setup-wsl2-gpu.sh [MODE]

  (no argument)  install the D3D12 Gallium driver selection
  --check        report the current driver selection and exit non-zero if absent
  --uninstall    remove the files this script installs

Must run as root, because every file it writes is system-wide.
EOF
}

require_root() {
    if [ "$(id -u)" -ne 0 ]; then
        echo "error: run as root (try: sudo $0 $*)" >&2
        exit 1
    fi
}

write_profile() {
    cat >"$PROFILE_FILE" <<EOF
# WSL2 reaches the GPU only through /dev/dxg; /dev/dri/renderD128 is the vgem
# stub, which defeats Mesa's DRI2 driver detection. Selecting the D3D12 Gallium
# driver routes rendering to the real GPU. Installed by WaySL,
# see tools/setup-wsl2-gpu.sh.
export GALLIUM_DRIVER=$DRIVER
EOF
    chmod 644 "$PROFILE_FILE"
}

write_systemd() {
    mkdir -p "$(dirname "$SYSTEMD_FILE")"
    cat >"$SYSTEMD_FILE" <<EOF
[Manager]
DefaultEnvironment=GALLIUM_DRIVER=$DRIVER
EOF
    chmod 644 "$SYSTEMD_FILE"
}

write_environment() {
    # /etc/environment covers non-login, non-systemd invocations such as
    # `wsl -- <command>`, which is how a renderer or a test is usually started.
    if [ -f "$ENVIRONMENT_FILE" ] && grep -q '^GALLIUM_DRIVER=' "$ENVIRONMENT_FILE"; then
        # Replace an existing assignment instead of appending a second one, so
        # repeated runs cannot leave conflicting values behind.
        sed -i "s/^GALLIUM_DRIVER=.*/GALLIUM_DRIVER=$DRIVER/" "$ENVIRONMENT_FILE"
    else
        printf 'GALLIUM_DRIVER=%s\n' "$DRIVER" >>"$ENVIRONMENT_FILE"
    fi
}

check() {
    status=0

    echo "installed files:"
    for file in "$PROFILE_FILE" "$SYSTEMD_FILE"; do
        if [ -f "$file" ]; then
            echo "  present  $file"
        else
            echo "  MISSING  $file"
            status=1
        fi
    done

    if grep -q "^GALLIUM_DRIVER=$DRIVER" "$ENVIRONMENT_FILE" 2>/dev/null; then
        echo "  present  $ENVIRONMENT_FILE"
    else
        echo "  MISSING  $ENVIRONMENT_FILE"
        status=1
    fi

    echo "current environment: GALLIUM_DRIVER=${GALLIUM_DRIVER:-<unset>}"
    if [ "${GALLIUM_DRIVER:-}" != "$DRIVER" ]; then
        echo "note: this shell did not inherit the setting; open a new login shell"
        status=1
    fi

    # The authoritative check is what Mesa actually selects.
    if command -v eglinfo >/dev/null 2>&1; then
        if LD_LIBRARY_PATH="/usr/lib/wsl/lib:${LD_LIBRARY_PATH:-}" \
            eglinfo -B 2>/dev/null | grep -q 'D3D12'; then
            echo "  ok       eglinfo reports D3D12"
        else
            echo "  FAILED   eglinfo does not report D3D12"
            status=1
        fi
    else
        echo "note: eglinfo not installed (apt install mesa-utils-extra) to verify"
    fi

    return $status
}

uninstall() {
    rm -f "$PROFILE_FILE" "$SYSTEMD_FILE"
    if [ -f "$ENVIRONMENT_FILE" ]; then
        sed -i '/^GALLIUM_DRIVER=/d' "$ENVIRONMENT_FILE"
    fi
    echo "removed the WaySL GPU driver selection"
}

mode=${1:-install}
case "$mode" in
--check)
    check
    ;;
--uninstall)
    require_root "$@"
    uninstall
    ;;
--help | -h)
    usage
    ;;
install)
    require_root
    if [ ! -e /dev/dxg ]; then
        echo "error: /dev/dxg is missing; this does not look like WSL2" >&2
        exit 1
    fi
    write_profile
    write_systemd
    write_environment
    echo "installed GALLIUM_DRIVER=$DRIVER for login shells, systemd and /etc/environment"
    echo "open a new shell, then verify with: eglinfo -B | grep D3D12"
    ;;
*)
    echo "error: unknown mode '$mode'" >&2
    usage >&2
    exit 2
    ;;
esac
