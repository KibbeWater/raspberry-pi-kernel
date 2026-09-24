#!/bin/bash
# Runs the rustypi-core and rustypi-abi unit tests on this machine. .cargo/config.toml builds for the Pi by
# default, so the host target is passed explicitly.
#
#   ./test.sh              all tests
#   ./test.sh session      only tests whose name contains "session"
set -euo pipefail
cd "$(dirname "$0")"

HOST=$(rustc -vV | sed -n 's/^host: //p')
cargo test -p rustypi-core -p rustypi-abi --target "$HOST" "$@"
