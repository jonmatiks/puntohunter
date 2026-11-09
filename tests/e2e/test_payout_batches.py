"""Payout batches: a batch pays each address its whole available balance, pooled and hybrid
together, once it reaches the threshold, with the fee from the fee reserve. A fee rate above the cap or a
short reserve defers it with balances untouched, a locked payout wallet makes it wait reserved until the
operator unlocks it, and a crash, an RPC timeout, or a reorg still ends in exactly one payment."""

import json
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from chain import sats
from payouts import (
    CONFIRMATIONS,
    RESERVE,
    balances,
    batches,
    broadcast,
    confirm,
    earn,
    health,
    held_reserve,
    mature,
    metric,
    outputs,
    payments,
    start,
)
from regtest import PAYOUT_WALLET
from wait import wait_until


def test_one_batch_pays_two_addresses_their_full_balances_and_the_reserve_pays_the_fee(
    pool_factory, fresh_node, solution_forcer
):
    pool = start(pool_factory, fresh_node)
    owed = earn(pool, fresh_node, solution_forcer, "pooled", finder=0, others=[1])
    assert len(owed) == 2
    mature(pool, fresh_node)

    [batch] = broadcast(pool)
    [batch] = confirm(pool, fresh_node)
    paid = outputs(fresh_node, batch["txid"])
    change = fresh_node.address("pool_wallet")
    assert set(paid) == {*owed, change}, "each payee, and the change to the pool wallet"
    assert {address: paid[address] for address in owed} == owed
    fee = -sats(fresh_node.wallet(PAYOUT_WALLET).call("gettransaction", batch["txid"])["fee"])
    assert fee == batch["fee"] > 0
    assert held_reserve(pool) == RESERVE - fee
    for (address, mode), row in balances(pool).items():
        assert (mode, row["available"], row["reserved"], row["paid"]) == ("pooled", 0, 0, owed[address])
    for address in owed:
        assert payments(fresh_node, address) == {batch["txid"]}


def test_balances_below_the_threshold_carry_forward_and_pooled_and_hybrid_reach_it_together(
    pool_factory, fresh_node, solution_forcer
):
    # No batch until the balances are known and the threshold set from them.
    pool = start(pool_factory, fresh_node, payouts={"interval_secs": 3600.0})
    finder, other = (fresh_node.miner_address(n) for n in range(2))
    pooled = earn(
        pool, fresh_node, solution_forcer, "pooled", finder=0, others=[1], shares=1, finder_shares=3
    )
    hybrid = earn(pool, fresh_node, solution_forcer, "hybrid", finder=0)
    assert set(hybrid) == {finder}
    mature(pool, fresh_node)
    # The finder's pooled and hybrid balances each fall short, and reach it together; the other's falls short.
    threshold = max(pooled[finder], hybrid[finder]) + 1
    assert pooled[other] < threshold <= pooled[finder] + hybrid[finder]
    pool.config["payouts"].update(interval_secs=0.5, threshold_sats=threshold)
    pool.restart()

    broadcast(pool)
    [batch] = confirm(pool, fresh_node)
    paid = outputs(fresh_node, batch["txid"])
    assert paid[finder] == pooled[finder] + hybrid[finder], "one output pays both modes"
    assert other not in paid
    reserved = pool.rows("SELECT pool, sats FROM ledger WHERE kind = 'reserved' AND address = ?", finder)
    assert {row["pool"]: row["sats"] for row in reserved} == {
        "pooled": pooled[finder],
        "hybrid": hybrid[finder],
    }
    assert balances(pool)[(other, "pooled")]["available"] == pooled[other], "carried forward"
    time.sleep(1)
    assert len(batches(pool)) == 1, "nothing more is due"


def test_more_addresses_than_outputs_per_batch_take_several_batches_with_no_dust(
    pool_factory, fresh_node, solution_forcer
):
    pool = start(pool_factory, fresh_node, payouts={"outputs_per_batch": 1})
    owed = earn(pool, fresh_node, solution_forcer, "pooled", finder=0, others=[1])
    mature(pool, fresh_node)

    # Batches spend confirmed outputs alone, and the wallet holds too little else: the second waits for the
    # first's change to confirm.
    broadcast(pool)
    fresh_node.chain.mine(1, fresh_node.funder_address())
    broadcast(pool, 2)
    found = confirm(pool, fresh_node, 2)
    paid = [outputs(fresh_node, batch["txid"]) for batch in found]
    change = fresh_node.address("pool_wallet")
    # One payee each, in address order.
    assert [set(output) - {change} for output in paid] == [{address} for address in sorted(owed)]
    for output in paid:
        # P2TR's and P2WSH's dust limit, the highest of any segwit output.
        assert all(value >= 330 for value in output.values()), output
    assert {address: output[address] for output in paid for address in owed if address in output} == owed
    inputs = pool.rows("SELECT txid, vout, batch FROM batch_inputs")
    assert {row["batch"] for row in inputs} == {1, 2}


def test_a_fee_rate_above_the_cap_and_a_short_reserve_each_defer_the_batch_with_balances_untouched(
    pool_factory, fresh_node, solution_forcer
):
    # The node takes nothing under 0.1 sat/vB, above this cap.
    pool = start(pool_factory, fresh_node, payouts={"fee_rate": 0.05, "fee_rate_cap": 0.05}, reserve=0)
    owed = earn(pool, fresh_node, solution_forcer, "pooled", finder=0)
    mature(pool, fresh_node)
    before = balances(pool)

    deferral = "payout batch deferred: the fee rate of 0.1 sat/vB is above the cap of 0.05 sat/vB"
    wait_until(lambda: deferral in pool.logs(), 10, "the fee rate deferral")
    assert metric(pool, "puntohunter_payouts_deferred") == 1
    assert batches(pool) == [] and balances(pool) == before

    pool.config["payouts"].update(fee_rate=2.0, fee_rate_cap=10.0)
    pool.restart()
    wait_until(lambda: "but the fee reserve has 0 sats free" in pool.logs(), 10, "the reserve deferral")
    assert batches(pool) == [] and balances(pool) == before

    credited = pool.credit_reserve(RESERVE)
    assert credited.returncode == 0, credited.stderr
    [batch] = broadcast(pool)
    assert outputs(fresh_node, batch["txid"])[next(iter(owed))] == sum(owed.values())
    assert metric(pool, "puntohunter_payouts_deferred") == 0


LOCKED_WAIT = "payout batch reserved: it is signed once the payout wallet is unlocked"


def test_a_batch_meeting_a_locked_wallet_waits_reserved_through_a_restart_until_the_wallet_is_unlocked(
    pool_factory, fresh_node, solution_forcer
):
    pool = start(pool_factory, fresh_node)
    fresh_node.encrypt_payouts()
    owed = earn(pool, fresh_node, solution_forcer, "pooled", finder=0, others=[1])
    mature(pool, fresh_node)

    # Funded and reserved while the wallet is locked; only the signature waits, shown and not a failure.
    wait_until(lambda: health(pool) == ("ok", "locked"), 30, "the batch waiting for the unlock")
    [batch] = batches(pool)
    assert batch["state"] == "reserved" and not batch["tx"]
    assert metric(pool, "puntohunter_payouts_locked") == 1
    assert pool.portal.api("/api/v1/pool").json["payouts"]["standing"] == {"state": "locked"}
    payout_wallet = fresh_node.wallet(PAYOUT_WALLET)
    inputs = {(row["txid"], row["vout"]) for row in pool.rows("SELECT * FROM batch_inputs")}
    assert inputs <= {(lock["txid"], lock["vout"]) for lock in payout_wallet.call("listlockunspent")}
    assert all(row["reserved"] == owed[address] for (address, _), row in balances(pool).items())
    assert "payouts wait for the next interval" not in pool.logs(), "the wait is no failure"

    # A restart reconciles the waiting batch and keeps it.
    pool.restart()
    wait_until(lambda: health(pool) == ("ok", "locked"), 30, "the batch waiting again after the restart")
    assert "payouts reconciled with the wallet and the chain" in pool.logs()
    assert [row["id"] for row in batches(pool)] == [batch["id"]]

    fresh_node.unlock_payouts()
    [batch] = broadcast(pool)
    wait_until(lambda: health(pool) == ("ok", "running"), 10, "payouts running again")
    assert metric(pool, "puntohunter_payouts_locked") == 0
    assert pool.logs().count(LOCKED_WAIT) == 2, "logged once in each run, not at each interval"
    [batch] = confirm(pool, fresh_node)
    for address in owed:
        assert payments(fresh_node, address) == {batch["txid"]}


class SlowSend:
    """An RPC proxy to the node that passes the first sendrawtransaction on at once but answers it only after
    `delay` seconds: the node has the transaction, and the pool, past its RPC timeout, has heard nothing."""

    def __init__(self, node, delay):
        target = f"http://127.0.0.1:{node.rpc_port}"
        self.held = threading.Event()
        held = self.held

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                headers = {"Authorization": self.headers["Authorization"], "Content-Type": "application/json"}
                request = urllib.request.Request(target + self.path, body, headers)
                try:
                    with urllib.request.urlopen(request, timeout=300) as response:
                        status, reply = response.status, response.read()
                except urllib.error.HTTPError as e:
                    status, reply = e.code, e.read()
                if json.loads(body)["method"] == "sendrawtransaction" and not held.is_set():
                    held.set()
                    time.sleep(delay)
                try:
                    self.send_response(status)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(reply)))
                    self.end_headers()
                    self.wfile.write(reply)
                except (BrokenPipeError, ConnectionResetError):
                    pass

            def log_message(self, *args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


@pytest.mark.parametrize("fault", ["after-reservation", "after-signing", "after-broadcast", "timeout"])
def test_a_crash_before_or_after_signing_or_broadcast_and_an_rpc_timeout_each_end_in_one_payment(
    pool_factory, fresh_node, solution_forcer, fault
):
    proxy = SlowSend(fresh_node, delay=3) if fault == "timeout" else None
    try:
        if proxy:
            overrides = {"node": {"rpc_url": proxy.url, "rpc_timeout_secs": 1.0}}
            pool = start(pool_factory, fresh_node, overrides=overrides)
        else:
            pool = start(pool_factory, fresh_node, env={"PUNTOHUNTER_FAILPOINT": fault})
        owed = earn(pool, fresh_node, solution_forcer, "pooled", finder=0, others=[1])
        mature(pool, fresh_node)

        if proxy:
            wait_until(proxy.held.is_set, 30, "the send held")
            timeout = "payouts wait for the next interval: node did not answer"
            wait_until(lambda: timeout in pool.logs(), 10, "the timeout logged")
        else:
            wait_until(pool.exited, 30, f"the pool stopped {fault}")
            [batch] = batches(pool)
            if fault == "after-reservation":
                assert batch["state"] == "reserved" and not batch["tx"], "reserved, not yet signed"
            else:
                assert batch["state"] == "signed" and batch["tx"], "the signed transaction persisted first"
                sent = batch["txid"] in fresh_node.chain.call("getrawmempool")
                assert sent == (fault == "after-broadcast")
            pool.env = {}
            pool.start()

        [batch] = broadcast(pool)
        [batch] = confirm(pool, fresh_node)
        for address in owed:
            assert payments(fresh_node, address) == {batch["txid"]}, "exactly one payment"
        assert {address: outputs(fresh_node, batch["txid"])[address] for address in owed} == owed
    finally:
        if proxy:
            proxy.close()


def test_a_payout_reorged_out_stays_reserved_until_it_confirms_again_its_inputs_never_respent(
    pool_factory, fresh_node, solution_forcer
):
    chain = fresh_node.chain
    payouts = fresh_node.wallet(PAYOUT_WALLET)
    pool = start(pool_factory, fresh_node)
    owed = earn(pool, fresh_node, solution_forcer, "pooled", finder=0)
    [(address, amount)] = owed.items()
    mature(pool, fresh_node)
    [batch] = broadcast(pool)
    txid = batch["txid"]
    inputs = [(row["txid"], row["vout"]) for row in pool.rows("SELECT txid, vout FROM batch_inputs")]

    [block] = chain.mine(1, fresh_node.funder_address())
    assert payouts.call("gettransaction", txid)["confirmations"] == 1
    # Out of the chain, and kept out of the next blocks.
    chain.call("invalidateblock", block)
    chain.call("prioritisetransaction", txid, 0, -100_000_000)
    chain.mine(CONFIRMATIONS + 1, fresh_node.funder_address())
    assert payouts.call("gettransaction", txid)["confirmations"] == 0
    time.sleep(2)
    assert [row["state"] for row in batches(pool)] == ["broadcast"]
    row = balances(pool)[(address, "pooled")]
    assert (row["available"], row["reserved"], row["paid"]) == (0, amount, 0)
    unspent = {(utxo["txid"], utxo["vout"]) for utxo in payouts.call("listunspent", 0)}
    assert not unspent & set(inputs), "no input of the batch can be spent again"
    for input_txid, vout in inputs:
        assert chain.call("gettxout", input_txid, vout, False) is not None, "unspent on the chain"

    chain.call("prioritisetransaction", txid, 0, 100_000_000)
    [batch] = confirm(pool, fresh_node)
    assert batch["txid"] == txid
    row = balances(pool)[(address, "pooled")]
    assert (row["available"], row["reserved"], row["paid"]) == (0, 0, amount)
    assert payments(fresh_node, address) == {txid}


def test_a_paid_block_leaving_the_chain_pauses_payouts_and_reverses_nothing(
    pool_factory, fresh_node, solution_forcer
):
    chain = fresh_node.chain
    pool = start(pool_factory, fresh_node)
    owed = earn(pool, fresh_node, solution_forcer, "pooled", finder=0)
    [(address, amount)] = owed.items()
    [block_hash] = [row["block"] for row in pool.rows("SELECT DISTINCT block FROM allocations")]
    mature(pool, fresh_node)
    broadcast(pool)
    confirm(pool, fresh_node)
    assert metric(pool, "puntohunter_payouts_paused") == 0
    assert health(pool) == ("ok", "running")

    # Paid, then off the chain: nothing to reverse without debiting what was paid, so payouts pause.
    chain.call("invalidateblock", block_hash)
    try:
        wait_until(lambda: metric(pool, "puntohunter_payouts_paused") == 1, 30, "payouts paused")
        assert pool.block(block_hash)["status"] == "orphaned"
        assert pool.rows("SELECT id FROM ledger WHERE block = ? AND kind = 'reversal'", block_hash) == []
        row = balances(pool)[(address, "pooled")]
        assert (row["available"], row["reserved"], row["paid"]) == (0, 0, amount)
        assert "CRITICAL: a block whose allocations were reserved or paid left the chain" in pool.logs()
        # Miners still get work, but the payouts wait for the operator.
        checked = wait_until(
            lambda: (h := pool.portal.api("/api/v1/health")).json["checks"]["payouts"] == "paused" and h,
            10,
            "paused on the health check",
        )
        assert (checked.status, checked.json["status"]) == (200, "degraded")
    finally:
        chain.call("reconsiderblock", block_hash)

    # The block back, and the pool restarted, payouts stay paused for the operator: a new balance goes unpaid.
    pool.restart()
    assert metric(pool, "puntohunter_payouts_paused") == 1
    wait_until(lambda: pool.portal.api("/api/v1/health").json["status"] == "degraded", 10, "still degraded")
    earn(pool, fresh_node, solution_forcer, "pooled", finder=1)
    mature(pool, fresh_node)
    time.sleep(2)
    assert len(batches(pool)) == 1
