"""Pool statistics on the API (API reference sections 2.1, 2.3, 2.5, and 2.8): each pooled and hybrid
device's share of its mode's current window, with the estimate of a block found now; the last payout and its
status on the pool resource, the reason a batch is deferred among them; each block's maturity and amount
allocated; the operator's accounting metrics; and the outage on the pool, a fleet, and a worker while the node
is unreachable. Every response passes the privacy scan."""

from chain import sats
from clients.sv1 import Sv1Miner
from payouts import broadcast, confirm, earn, held_reserve, mature, metric, start
from regtest import PAYOUT_WALLET
from wait import wait_until


def handle_of(pool, worker, mode):
    """The handle and fleet the pool recorded for the worker named `worker` in `mode`."""
    [row] = wait_until(
        lambda: pool.rows("SELECT handle, fleet FROM fleets WHERE worker = ? AND pool = ?", worker, mode),
        10,
        f"{worker}'s fleet row",
    )
    return row["handle"], row["fleet"]


def test_window_shares_sum_to_one_in_each_mode_with_an_estimate(pool_factory, regtest_cluster):
    pool = pool_factory()
    first, second = (regtest_cluster.miner_address(n) for n in range(2))
    # (mode, address, worker): shares, all at the harness's one difficulty.
    devices = {
        ("pooled", first, "a"): 3,
        ("pooled", first, "b"): 1,
        ("pooled", second, "c"): 2,
        ("hybrid", first, "d"): 1,
        ("hybrid", second, "e"): 3,
    }
    handles = {}
    for (mode, address, worker), shares in devices.items():
        miner = Sv1Miner(getattr(pool, f"{mode}_v1_port"), f"{address}.{worker}")
        for _ in range(shares):
            assert miner.submit(miner.share()) is True
        miner.client.close()
        handles[worker] = handle_of(pool, worker, mode)

    def part(worker):
        window = pool.portal.api(f"/api/v1/workers/{handles[worker][0]}").json["live"]["window"]
        return window.get("fraction", 0)

    for mode in ("pooled", "hybrid"):
        workers = [worker for (m, _, worker) in devices if m == mode]
        total = sum(shares for (m, _, _), shares in devices.items() if m == mode)
        # The window is kept a few seconds: wait for one walked after every share.
        wait_until(lambda: abs(sum(part(w) for w in workers) - 1) < 1e-9, 15, f"the {mode} window")
        for (m, _, worker), shares in devices.items():
            if m == mode:
                assert abs(part(worker) - shares / total) < 1e-9, worker

    assert pool.portal.api(f"/api/v1/workers/{handles['a'][0]}").json["live"]["window"]["estimate_sats"] > 0

    # The fleet of the first address: its pooled part is its two pooled devices', and so are its rows'.
    fleet = pool.portal.api(f"/api/v1/fleets/{handles['a'][1]}").json
    [pooled] = [section for section in fleet["pools"] if section["pool"] == "pooled"]
    assert abs(pooled["window"]["fraction"] - 4 / 6) < 1e-9
    rows = {d["handle"]: d["window"]["fraction"] for d in fleet["devices"]}
    assert abs(rows[handles["a"][0]] - 3 / 6) < 1e-9
    assert abs(rows[handles["d"][0]] - 1 / 4) < 1e-9


def test_the_pool_resource_gives_the_last_payout_and_its_status(pool_factory, fresh_node, solution_forcer):
    pool = start(pool_factory, fresh_node)
    assert pool.portal.api("/api/v1/pool").json["payouts"]["last"] is None

    owed = earn(pool, fresh_node, solution_forcer, "pooled", finder=0, others=[1])
    [block_hash] = [row["block"] for row in pool.rows("SELECT DISTINCT block FROM allocations")]

    def listed():
        [block] = [b for b in pool.portal.api("/api/v1/blocks").json["blocks"] if b["hash"] == block_hash]
        return block

    allocated = sum(owed.values())
    assert listed()["allocated_sats"] == allocated
    # Its one confirmation of the 101 maturity takes, once the template builds on it.
    wait_until(lambda: listed()["blocks_to_maturity"] == 100, 10, "100 blocks to go")
    wait_until(lambda: metric(pool, "puntohunter_liabilities_sats") == allocated, 10, "the liabilities")

    mature(pool, fresh_node)
    wait_until(lambda: listed()["blocks_to_maturity"] == 0, 10, "mature")
    broadcast(pool)
    [batch] = confirm(pool, fresh_node)
    payouts = pool.portal.api("/api/v1/pool").json["payouts"]
    last = payouts["last"]
    assert (last["batch"], last["state"], last["payees"]) == (1, "confirmed", 2)
    assert last["paid_sats"] == allocated and last["fee_sats"] == batch["fee"]
    assert isinstance(last["reserved_at"], int)
    assert payouts["standing"] == {"state": "running"}, "nothing holds the next batch"

    assert metric(pool, "puntohunter_liabilities_sats") == 0
    assert metric(pool, "puntohunter_fee_reserve_sats") == held_reserve(pool) == 1_000_000 - batch["fee"]
    assert metric(pool, "puntohunter_payout_batches_unconfirmed") == 0
    assert metric(pool, "puntohunter_accounting_backlog_blocks") == 0

    def held():
        balances = fresh_node.wallet(PAYOUT_WALLET).call("getbalances")["mine"]
        return sats(balances["trusted"]) + sats(balances["immature"])

    wait_until(lambda: metric(pool, "puntohunter_wallet_held_sats") == held(), 10, "what the wallet holds")
    assert metric(pool, "puntohunter_wallet_covers_liabilities") == 1


def test_a_deferred_batch_says_why(pool_factory, fresh_node, solution_forcer):
    pool = start(pool_factory, fresh_node, reserve=0)
    earn(pool, fresh_node, solution_forcer, "pooled", finder=0)
    mature(pool, fresh_node)
    standing = wait_until(
        lambda: (s := pool.portal.api("/api/v1/pool").json["payouts"]["standing"])["state"] == "deferred" and s,
        10,
        "the hold",
    )
    assert standing["reason"].startswith("the batch's fee is"), standing
    assert standing["reason"].endswith("but the fee reserve has 0 sats free"), standing


def test_the_pool_a_fleet_and_a_worker_say_when_the_node_is_unreachable_and_stop_once_it_is_back(
    pool_factory, fresh_node, miner_username
):
    fast = {"node": {"rpc_timeout_secs": 1.0, "longpoll_timeout_secs": 2.0}}
    pool = pool_factory(node=fresh_node, overrides=fast)
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    assert miner.submit(miner.share()) is True
    miner.client.close()
    worker, fleet = handle_of(pool, miner_username.split(".", 1)[1], "solo")
    resources = ["/api/v1/pool", f"/api/v1/workers/{worker}", f"/api/v1/fleets/{fleet}"]

    def outages():
        return {path for path in resources if pool.portal.api(path).json["outage"]}

    wait_until(lambda: outages() == set(), 10, "work from the node")
    fresh_node.stop()
    wait_until(lambda: outages() == set(resources), 30, "the outage on each resource")
    # The node returns on a fresh chain, its datadir being tmpfs.
    fresh_node.up()
    fresh_node.chain.mine(1, fresh_node.address("operator"))
    wait_until(lambda: outages() == set(), 30, "the outage gone")
