#!/bin/sh
# SessionStart hook (codex): codex has no command-source plugins, so lockstep
# with the binary is re-established here. The codex plugin cache is keyed by
# the manifest version, which tracks the crate version — when the cache has
# no entry for the running binary's version, heal the local marketplace and
# re-add (re-adding is codex's upgrade verb; the refreshed plugin loads in
# the next session).
set -u

INSTALL_HINT="curl --proto '=https' --tlsv1.2 -LsSf https://github.com/notdp/hive/releases/latest/download/hive-installer.sh | sh"

if ! command -v hive >/dev/null 2>&1; then
    echo "bootstrap: hive is not on PATH; install with: $INSTALL_HINT" >&2
    exit 1
fi
command -v codex >/dev/null 2>&1 || exit 0

bin_version="$(hive --version 2>/dev/null | sed 's/^hive, version //')"
cache="${CODEX_HOME:-$HOME/.codex}/plugins/cache/hive/hive"
[ -n "$bin_version" ] && [ -d "$cache/$bin_version" ] && exit 0

hive plugin sync >/dev/null || exit 1
codex plugin add hive@hive >/dev/null 2>&1 || true
exit 0
