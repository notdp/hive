#!/bin/sh
set -eu

cargo_root=${CARGO_HOME:-${HOME:?HOME is not set}/.cargo}
if [ -n "${HIVE_INSTALL_DIR:-}" ]; then
    bin_dir=$HIVE_INSTALL_DIR/bin
elif [ -n "${CARGO_DIST_FORCE_INSTALL_DIR:-}" ]; then
    bin_dir=$CARGO_DIST_FORCE_INSTALL_DIR/bin
elif [ -n "${HIVE_UNMANAGED_INSTALL:-}" ]; then
    bin_dir=$HIVE_UNMANAGED_INSTALL
    if [ "$bin_dir" = "$cargo_root" ]; then
        bin_dir=$bin_dir/bin
    fi
else
    bin_dir=$cargo_root/bin
fi

installer=$(mktemp)
trap 'rm -f "$installer"' 0
trap 'exit 1' HUP INT TERM
curl -fsSL "${HIVE_INSTALLER_URL:-https://github.com/notdp/hive/releases/latest/download/hive-installer.sh}" -o "$installer"
sh "$installer"
rm -f "$installer"
trap - 0
if [ ! -x "$bin_dir/hive" ]; then
    echo "install: installer finished, but $bin_dir/hive is not executable; plugin registration was not run. Run hive plugin setup manually." >&2
    exit 1
fi
# Claude's command-source plugin calls `hive plugin sync` during setup.
bin_dir=$(CDPATH= cd -- "$bin_dir" && pwd -P)
PATH="$bin_dir:${PATH:-/usr/bin:/bin}"
export PATH
exec "$bin_dir/hive" plugin setup
