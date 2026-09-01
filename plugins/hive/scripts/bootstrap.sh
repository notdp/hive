#!/bin/sh
# SessionStart hook (claude): presence check only. Skill content rides the
# hive binary — the marketplace's command source re-runs `hive plugin path`
# once per session, so a present binary means current skills. Nothing is
# ever installed from here.
set -u

INSTALL_HINT="curl --proto '=https' --tlsv1.2 -LsSf https://github.com/notdp/hive/releases/latest/download/hive-installer.sh | sh"

if ! command -v hive >/dev/null 2>&1; then
    echo "bootstrap: hive is not on PATH; install with: $INSTALL_HINT" >&2
    exit 1
fi
exit 0
