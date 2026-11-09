"""unlock.sh: the operator unlocks the encrypted payout wallet for a waiting batch, and
the script locks it again once the batch is signed. Run against the Compose node: its remote command is a
`docker exec` into the node's container, and the passphrase comes on standard input, as it would from a
terminal."""

import os
import subprocess
import threading

from payouts import broadcast, earn, health, mature, outputs, start
from regtest import PAYOUT_PASSPHRASE, PAYOUT_WALLET, ROOT
from wait import wait_until

UNLOCK = ROOT / "unlock.sh"


def environment(node, pool):
    return {
        **os.environ,
        "PUNTOHUNTER_REMOTE": f"docker exec -i {node.container} sh -c",
        "PUNTOHUNTER_WALLET": PAYOUT_WALLET,
        "PUNTOHUNTER_BITCOIN_CLI": "bitcoin-cli -regtest -rpcuser=puntohunter -rpcpassword=puntohunter",
        "PUNTOHUNTER_HEALTH_URL": f"http://127.0.0.1:{pool.portal_port}/api/v1/health",
    }


def command_lines():
    for pid in os.listdir("/proc"):
        if pid.isdigit():
            try:
                with open(f"/proc/{pid}/cmdline", "rb") as f:
                    yield f.read().replace(b"\0", b" ")
            except OSError:
                pass


def unlock(env, answers, *args):
    """Runs unlock.sh with `answers` on standard input, watching every process's command line for the
    passphrase while it runs. Returns the process, its output, and the command lines that carried it."""
    process = subprocess.Popen(
        [UNLOCK, *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env
    )
    secret, leaks = PAYOUT_PASSPHRASE.encode(), []

    def watch():
        while process.poll() is None:
            leaks.extend(line for line in command_lines() if secret in line)

    watcher = threading.Thread(target=watch)
    watcher.start()
    out, err = process.communicate(answers, timeout=90)
    watcher.join()
    return process.returncode, out, err, leaks


def unlocked_until(node):
    return node.wallet(PAYOUT_WALLET).call("getwalletinfo")["unlocked_until"]


def test_a_waiting_batch_is_signed_and_the_wallet_locked_again_and_a_wrong_passphrase_unlocks_nothing(
    pool_factory, fresh_node, solution_forcer
):
    pool = start(pool_factory, fresh_node)
    fresh_node.encrypt_payouts()
    owed = earn(pool, fresh_node, solution_forcer, "pooled", finder=0)
    mature(pool, fresh_node)
    wait_until(lambda: health(pool) == ("ok", "locked"), 30, "the batch waiting for the unlock")
    env = environment(fresh_node, pool)

    code, out, err, leaks = unlock(env, "not the passphrase\n", "60")
    assert code != 0, out
    assert "a payout batch is waiting to be signed" in out
    assert "the wallet was not unlocked, and stays locked" in err
    assert unlocked_until(fresh_node) == 0
    assert health(pool) == ("ok", "locked") and pool.rows("SELECT state FROM batches") == [{"state": "reserved"}]

    code, out, err, leaks = unlock(env, PAYOUT_PASSPHRASE + "\n", "60")
    assert code == 0, err
    [batch] = broadcast(pool)
    assert "the batch is signed, and the wallet locked again" in out
    assert f"txid {batch['txid']}" in out.splitlines()
    assert unlocked_until(fresh_node) == 0, "locked before its 60 s ran out"
    assert health(pool) == ("ok", "running")
    assert not leaks, "no command line carried the passphrase"
    paid = outputs(fresh_node, batch["txid"])
    assert {address: paid[address] for address in owed} == owed


def test_with_no_batch_waiting_it_asks_before_unlocking(pool_factory, fresh_node):
    pool = start(pool_factory, fresh_node)
    fresh_node.encrypt_payouts()
    wait_until(lambda: health(pool) == ("ok", "running"), 30, "payouts running, nothing due")
    env = environment(fresh_node, pool)

    code, out, err, leaks = unlock(env, "n\n")
    assert code == 0, err
    assert "no payout batch is waiting (payouts: running)" in out
    assert "left locked" in out
    assert unlocked_until(fresh_node) == 0

    code, out, err, leaks = unlock(env, f"y\n{PAYOUT_PASSPHRASE}\n", "30")
    assert code == 0, err
    assert "no batch to follow: the wallet locks itself after 30 s" in out
    assert unlocked_until(fresh_node) > 0
    assert not leaks
    fresh_node.wallet(PAYOUT_WALLET).call("walletlock")
