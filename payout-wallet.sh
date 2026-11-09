#!/usr/bin/env bash
# Creates the encrypted payout wallet on the pool host's node, once, from the operator's machine. It reads the
# new passphrase twice without echo and passes it over SSH on standard input, as unlock.sh does, so it never
# reaches a command line, the process list, a file, or the shell history. It prints the pool wallet's address,
# for the pool config's pool_wallet, and copies the wallet's backup, made with backupwallet, to the operator's
# machine, checked by checksum and removed from the host.
set -euo pipefail

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

# Where and how, as unlock.sh reads them.
wallet=${PUNTOHUNTER_WALLET:-payouts}
cli=${PUNTOHUNTER_BITCOIN_CLI:-bitcoin-cli}
read -ra remote <<<"${PUNTOHUNTER_REMOTE:-ssh ${PUNTOHUNTER_HOST:?set it in deploy.env or the environment (see deploy.env.example)}}"
# Where the node writes the backup, which its user can write, and where the operator's machine keeps it.
host_backup=${PUNTOHUNTER_HOST_BACKUP:?set it in deploy.env or the environment (see deploy.env.example)}
backup=${PUNTOHUNTER_WALLET_BACKUP:-$HOME/puntohunter-keys/$wallet-wallet.dat}
wallet_cli="$cli -rpcwallet=$(printf %q "$wallet")"

# Runs a shell command on the host, keeping this script's standard input from it.
on_host() {
  "${remote[@]}" "$*" </dev/null
}

if [[ -e $backup ]]; then
  echo "error: $backup exists: the wallet was made before, or move the file aside" >&2
  exit 1
fi
mkdir -p "$(dirname "$backup")"

read -rsp "passphrase for the new payout wallet $wallet: " passphrase
echo
read -rsp "the same again: " again
echo
if [[ $passphrase != "$again" ]]; then
  unset passphrase again
  echo "error: the two differ; nothing was made" >&2
  exit 1
fi
unset again
if ((${#passphrase} < 12)); then
  unset passphrase
  echo "error: use at least 12 characters; nothing was made" >&2
  exit 1
fi

# createwallet's passphrase, avoid_reuse, descriptors, and load_on_startup come on standard input, after its
# name, disable_private_keys, and blank on the command line: a descriptor wallet with keys of its own, encrypted
# from the start, and loaded whenever the node starts.
printf '%s\nfalse\ntrue\ntrue\n' "$passphrase" |
  "${remote[@]}" "$cli -stdin createwallet $(printf %q "$wallet") false false" >/dev/null
unset passphrase
echo "made the encrypted wallet $wallet"

address=$(on_host "$wallet_cli getnewaddress pool bech32")
echo "the pool wallet's address, for pool_wallet in the pool's config: $address"

on_host "$wallet_cli backupwallet $(printf %q "$host_backup")"
on_host "cat $(printf %q "$host_backup")" >"$backup"
chmod 600 "$backup"
theirs=$(on_host "sha256sum $(printf %q "$host_backup")" | cut -d' ' -f1)
ours=$(sha256sum "$backup" | cut -d' ' -f1)
if [[ $theirs != "$ours" ]]; then
  echo "error: the copy's checksum differs; the host's backup is kept at $host_backup" >&2
  exit 1
fi
on_host "rm $(printf %q "$host_backup")"
echo "backed up to $backup, sha256 $ours"
