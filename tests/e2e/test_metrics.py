"""Operator metrics: shares and refusals of each class are counted under
their pool and transport, the node's state, tip, and template age are reported, connected workers are counted
by pool and transport, a series for each configured pool, and no series carries a per-worker label nor a
`mode`."""

import re

import pytest

from clients.sv1 import Sv1Miner
from clients.sv2 import Sv2Miner
from wait import wait_until

# Between the harness floor and regtest's network difficulty, so shares need not be blocks.
PINNED = 3e-10
SAMPLE = re.compile(r'^(\w+)(?:\{(.*)\})? (\S+)$')
LABEL = re.compile(r'(\w+)="([^"]*)"')
# The only labels a series may carry: none names a worker. A node's name is the operator's.
LABELS = {"pool", "transport", "class", "state", "hash", "le", "node", "standing", "answer", "from", "to"}


def samples(text):
    """The scrape's samples, as {(name, frozenset of label pairs): value}."""
    found = {}
    for line in text.splitlines():
        if not line or line.startswith("#"):
            continue
        name, labels, value = SAMPLE.match(line).groups()
        found[(name, frozenset(LABEL.findall(labels or "")))] = float(value)
    return found


def value(scrape, name, **labels):
    return samples(scrape).get((name, frozenset(labels.items())), 0.0)


@pytest.fixture
def pinned(pool_factory):
    return pool_factory(overrides={"vardiff": {"floor": PINNED, "ceiling": PINNED}})


def test_each_class_is_counted_under_its_pool_and_transport(pinned, regtest_cluster, miner_username):
    v1 = Sv1Miner(pinned.solo_v1_port, miner_username)
    v2 = Sv2Miner(pinned, pinned.solo_v2_port, miner_username)
    pooled = Sv1Miner(pinned.pooled_v1_port, miner_username)

    share = v1.share()
    assert v1.submit(share) is True
    assert v1.submit(v1.share()) is True
    v1.rejection(share)
    v1.rejection(v1.share(meets=False))
    v1.rejection(v1.share(version_bits=0x0000_0001))
    v1.rejection(v1.share(), username=f"{miner_username}x")
    v1.rejection([v1.notify[0], "00", v1.notify[7], "00000000"])
    assert not v2.submit(3)["errors"]
    for kind in ("duplicate", "low_difficulty", "bad_version", "bad_extranonce"):
        assert v2.submit(kind=kind)["errors"]
    assert pooled.submit(pooled.share()) is True

    v1_stale, v2_stale = v1.share(), v2.job["job_id"]
    regtest_cluster.chain.mine(1, regtest_cluster.address("pool_wallet"))
    v1.clean_job()
    v2.client.wait("SetNewPrevHash", channel_id=v2.channel_id)
    v2.next_job()
    v1.rejection(v1_stale)
    assert v2.submit(kind="low_difficulty", job_id=v2_stale)["errors"]

    scrape = pinned.metrics()
    solo_v1, solo_v2 = {"pool": "solo", "transport": "V1"}, {"pool": "solo", "transport": "V2-native"}
    accepted, rejected = "puntohunter_shares_accepted_total", "puntohunter_shares_rejected_total"
    assert value(scrape, accepted, **solo_v1) == 2
    assert value(scrape, accepted, **solo_v2) == 3
    assert value(scrape, accepted, pool="pooled", transport="V1") == 1
    for labels, counts in (
        (solo_v1, {"duplicate": 1, "low_difficulty": 1, "bad_version": 1, "unauthorized": 1, "invalid": 1, "stale": 1}),
        (solo_v2, {"duplicate": 1, "low_difficulty": 1, "bad_version": 1, "invalid": 1, "stale": 1}),
    ):
        for class_, count in counts.items():
            assert value(scrape, rejected, **labels, **{"class": class_}) == count, (labels, class_)
    for miner in (v1.client, pooled.client, v2):
        miner.close()


def test_the_node_tip_and_template_are_reported(pool, regtest_cluster):
    height, tip = regtest_cluster.chain.tip()
    scrape = wait_until(
        lambda: (s := pool.metrics()) and value(s, "puntohunter_tip", hash=tip) == 1 and s, 10, "the tip"
    )
    assert value(scrape, "puntohunter_node_state", node="primary", state="healthy") == 1
    assert value(scrape, "puntohunter_node_state", node="primary", state="hung") == 0
    assert value(scrape, "puntohunter_node_selected", node="primary") == 1
    assert value(scrape, "puntohunter_node_tip_height", node="primary") == height
    assert value(scrape, "puntohunter_tip_height") == height
    assert 0 <= value(scrape, "puntohunter_template_age_seconds") < 120
    assert [k for k in samples(scrape) if k[0] == "puntohunter_tip"] == [("puntohunter_tip", frozenset({("hash", tip)}))]


def test_connected_workers_are_counted_by_pool_and_transport(pool, miner_username):
    def connected(id, transport):
        return value(pool.metrics(), "puntohunter_workers_connected", pool=id, transport=transport)

    # A series for each configured pool and transport, none of them a mode.
    series = [dict(labels) for name, labels in samples(pool.metrics()) if name == "puntohunter_workers_connected"]
    assert sorted((s["pool"], s["transport"]) for s in series) == sorted(
        (id, transport) for id in ("solo", "pooled", "hybrid") for transport in ("V1", "V2-native")
    )

    solo = Sv1Miner(pool.solo_v1_port, miner_username)
    pooled = Sv2Miner(pool, pool.pooled_v2_port, miner_username)
    # A second connection of the same worker is the same worker.
    again = Sv1Miner(pool.solo_v1_port, miner_username)
    assert connected("solo", "V1") == 1
    assert connected("pooled", "V2-native") == 1
    assert connected("solo", "V2-native") == connected("pooled", "V1") == 0
    for miner in (solo.client, again.client, pooled):
        miner.close()
    wait_until(lambda: connected("solo", "V1") == connected("pooled", "V2-native") == 0, 10, "disconnects")


def test_block_submission_latency_and_no_worker_labels(pool, miner_username, solution_forcer):
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    _, block_hash = solution_forcer(miner)
    wait_until(lambda: pool.block(block_hash).get("result"), 10, "the block answered")
    scrape = pool.metrics()
    assert value(scrape, "puntohunter_block_submission_seconds_count") == 1
    assert value(scrape, "puntohunter_block_submission_seconds_bucket", le="+Inf") == 1
    for _, labels in samples(scrape):
        assert {name for name, _ in labels} <= LABELS
    # Nothing of the worker's: its name, or its handle.
    [handle] = re.findall(r"handle=(\w+)", "\n".join(l for l in pool.logs().splitlines() if "V1 authorized" in l))
    assert handle not in scrape and ".e2e" not in scrape
    miner.client.close()
