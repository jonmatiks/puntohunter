#!/usr/bin/env bash
# Unlocks the payout wallet for a waiting payout batch, run from the operator's machine. It reads the pool's
# health check, reads the passphrase without echo, and hands it to the node's bitcoin-cli over SSH on standard
# input, so it never reaches a command line, the process list, a file, or the shell history. Once the batch is
# signed it locks the wallet again, without waiting out the time, and prints the batch's txid.
#
#   ./unlock.sh [seconds]    the longest the wallet stays unlocked; 300 by default
set -euo pipefail

seconds=${1:-300}
if [[ ! $seconds =~ ^[1-9][0-9]*$ ]]; then
  echo "usage: $0 [seconds]" >&2
  exit 2
fi

# Settings come from the environment, then from deploy.env beside this script (see deploy.env.example).
settings=$(dirname "$0")/deploy.env
if [[ -f $settings ]]; then
  given=()
  for name in ${!PUNTOHUNTER_@}; do
    given+=("$(declare -p "$name")")
  done
  source "$settings"
  for setting in "${given[@]}"; do
    eval "$setting"
  done
fi

# Where and how. PUNTOHUNTER_REMOTE runs one shell command on the host.
wallet=${PUNTOHUNTER_WALLET:-payouts}
cli=${PUNTOHUNTER_BITCOIN_CLI:-bitcoin-cli}
health=${PUNTOHUNTER_HEALTH_URL:?set it in deploy.env or the environment (see deploy.env.example)}
read -ra remote <<<"${PUNTOHUNTER_REMOTE:-ssh ${PUNTOHUNTER_HOST:?set it in deploy.env or the environment (see deploy.env.example)}}"
wallet_cli="$cli -rpcwallet=$(printf %q "$wallet")"

# Runs a wallet command on the host, keeping this script's standard input from it.
on_host() {
  "${remote[@]}" "$wallet_cli $*" </dev/null
}

# The health check's payouts state: locked while a batch waits for the unlock.
payouts() {
  curl -fsS --max-time 10 "$health" | grep -o '"payouts":[^,}]*' | cut -d: -f2 | tr -d '"'
}

# The txids of the wallet's sends, the batches it has signed.
sends() {
  on_host listtransactions "'*'" 1000 |
    awk '/"category"/ { send = /"send"/ } /"txid"/ && send { gsub(/[",]/, "", $2); print $2 }' | sort -u
}

state=$(payouts)
case $state in
  locked)
    echo "a payout batch is waiting to be signed"
    ;;
  null | "")
    echo "error: $health shows no payouts" >&2
    exit 1
    ;;
  *)
    echo "no payout batch is waiting (payouts: $state)"
    read -rp "unlock the payout wallet anyway? [y/N] " answer
    if [[ $answer != [yY]* ]]; then
      echo "left locked"
      exit 0
    fi
    ;;
esac

before=$(sends)
read -rsp "passphrase of the payout wallet $wallet: " passphrase
echo
# printf is a builtin: the passphrase goes down the pipe and onto no command line.
if ! printf '%s\n%s\n' "$passphrase" "$seconds" | "${remote[@]}" "$wallet_cli -stdin walletpassphrase"; then
  unset passphrase
  echo "error: the wallet was not unlocked, and stays locked" >&2
  exit 1
fi
unset passphrase
trap 'on_host walletlock || echo "error: cannot lock the wallet; it locks itself after $seconds s" >&2' EXIT
echo "unlocked for at most $seconds s"

if [[ $state != locked ]]; then
  trap - EXIT
  echo "no batch to follow: the wallet locks itself after $seconds s"
  exit 0
fi

deadline=$((SECONDS + seconds))
# Signed once the state leaves locked; a health check that does not answer is followed on.
while now=$(payouts) || now=unanswered; [[ $now == locked || $now == unanswered ]]; do
  if ((SECONDS >= deadline)); then
    echo "error: the batch was not signed within $seconds s; the wallet is locked again" >&2
    exit 1
  fi
  sleep 2
done
on_host walletlock
trap - EXIT
echo "the batch is signed, and the wallet locked again"
comm -13 <(echo "$before") <(sends) | sed 's/^/txid /'
