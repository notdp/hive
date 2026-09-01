#!/bin/sh
# SessionStart hook: prove a usable `hive` exists, then let it converge the
# Claude-side marketplace autoUpdate entry (`hive bootstrap`). This wrapper
# never installs anything -- a missing or too-old binary exits nonzero with
# the release-installer one-liner as the remediation.
#
# Exit-code contract with the binary: `hive bootstrap` exits 1 with its own
# remediation on a settings problem; a pre-bootstrap binary rejects the
# subcommand with clap's usage error (exit 2), which is the "too old" signal.
set -u

INSTALL_HINT="curl --proto '=https' --tlsv1.2 -LsSf https://github.com/notdp/hive/releases/latest/download/hive-installer.sh | sh"

if ! command -v hive >/dev/null 2>&1; then
    echo "bootstrap: hive is not on PATH; install with: $INSTALL_HINT" >&2
    exit 1
fi

hive bootstrap 2>&1
rc=$?
if [ "$rc" -eq 2 ]; then
    version="$(hive --version 2>/dev/null | sed 's/^hive, version //')"
    echo "bootstrap: active hive ${version:-unknown} predates \`hive bootstrap\`; upgrade with: $INSTALL_HINT" >&2
    exit 1
fi
exit "$rc"
