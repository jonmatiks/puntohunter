#!/usr/bin/env bash
# Runs the end-to-end suite, which CI leaves out: it builds the release binaries and the Docker images first,
# so no test's setup builds anything within its time limit, sets up .venv when it is missing, and runs pytest
# with the binary it built. Needs Docker with Compose, Python 3, and a Rust toolchain.
#
#   tests/e2e/run.sh                      the default run, with 2 workers
#   tests/e2e/run.sh -n 4                 more workers; any pytest option goes on the end
#   tests/e2e/run.sh tests/e2e/test_pplns.py::test_name
#   tests/e2e/run.sh -m load              a marked group in place of the default run
#
# E2E_JOBS sets cargo's build jobs, for a host that runs hot; cargo's default otherwise.
set -euo pipefail

cd "$(dirname "$0")/../.."

jobs=()
if [[ -n ${E2E_JOBS:-} ]]; then
  jobs=(-j "$E2E_JOBS")
fi
cargo build --release --locked --workspace "${jobs[@]}"
docker compose -f tests/e2e/docker-compose.yml build --quiet
docker compose -f tests/e2e/docker-compose.sri.yml build --quiet

if [[ ! -x .venv/bin/pytest ]]; then
  python3 -m venv .venv
fi
.venv/bin/pip install --quiet -r tests/e2e/requirements.txt

# The whole suite unless the arguments name tests of their own.
targets=(tests/e2e)
for arg in "$@"; do
  if [[ $arg == tests/* ]]; then
    targets=()
  fi
done
PUNTOHUNTER_BIN=$PWD/target/release/puntohunter exec .venv/bin/pytest "${targets[@]}" -n 2 "$@"
