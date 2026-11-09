#!/usr/bin/env bash
# Builds and runs the pool on mainnet (the guide's installation page, The Mainnet Script). Ctrl-C stops it.
# The log goes to the terminal and to data/main/logs/, one file per run.
set -euo pipefail

cd "$(dirname "$0")"

cargo build --release

log_dir=data/main/logs
mkdir -p "$log_dir"
log="$log_dir/puntohunter-$(date -u +%Y%m%dT%H%M%SZ).log"
echo "logging to $log"

# tee -i ignores Ctrl-C, so it keeps the pool's shutdown lines.
target/release/puntohunter serve --config config/mainnet.toml 2>&1 | tee -i "$log"
