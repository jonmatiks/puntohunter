"""`GET /api/v1/health` (API reference section 2.10): `ok` and 200 on a running pool; `unavailable`
and 503 while the node gives no work, then `ok` once it is back; `degraded` with `pooled_credit: "stopped"` once
a journal commit fails. The paused-payouts case is test_payout_batches.py's. No body carries error text: only
codes. Every response passes the privacy scan."""

from clients.sv1 import Sv1Miner
from wait import wait_until

FAST_TIMEOUTS = {"node": {"rpc_timeout_secs": 1.0, "longpoll_timeout_secs": 2.0}}
NODE_STATES = {"starting", "healthy", "ibd", "outage", "wrong_chain"}


def health(pool):
    response = pool.portal.api("/api/v1/health")
    assert response.header("cache-control") == "no-store"
    body = response.json
    # Codes only, never the underlying error text.
    assert set(body) == {"status", "version", "uptime_secs", "tip_height", "checks"}, body
    assert set(body["checks"]) == {"node", "work", "pooled_credit", "payouts"}, body
    assert body["checks"]["node"] in NODE_STATES, body
    assert body["checks"]["pooled_credit"] in ("accepting", "stopped", None), body
    assert (response.status == 503) == (body["status"] == "unavailable"), body
    return response.status, body


def test_ok_on_a_running_pool(pool, regtest_cluster):
    status, body = wait_until(lambda: (h := health(pool))[1]["status"] == "ok" and h, 10, "ok")
    assert status == 200
    assert body["checks"] == {"node": "healthy", "work": True, "pooled_credit": "accepting", "payouts": None}
    assert body["tip_height"] == regtest_cluster.chain.call("getblockcount")
    assert body["version"] == pool.portal.api("/api/v1/pool").json["version"]
    assert 0 <= body["uptime_secs"] < 600


def test_unavailable_while_the_node_is_down_and_ok_once_it_is_back(pool_factory, fresh_node):
    pool = pool_factory(node=fresh_node, overrides=FAST_TIMEOUTS)
    wait_until(lambda: health(pool)[1]["status"] == "ok", 10, "ok")
    fresh_node.stop()
    status, body = wait_until(lambda: (h := health(pool))[1]["status"] == "unavailable" and h, 30, "unavailable")
    assert status == 503
    assert body["checks"]["node"] == "outage" and body["checks"]["work"] is False
    assert body["tip_height"] is None
    # The node returns on a fresh chain, its datadir being tmpfs.
    fresh_node.up()
    fresh_node.chain.mine(1, fresh_node.address("operator"))
    status, body = wait_until(lambda: (h := health(pool))[1]["status"] == "ok" and h, 30, "ok again")
    assert status == 200 and body["checks"]["node"] == "healthy"


def test_degraded_once_pooled_credit_stops(pool_factory, regtest_cluster):
    pool = pool_factory(env={"PUNTOHUNTER_FAILPOINT": "journal-commit-fails"})
    wait_until(lambda: health(pool)[1]["status"] == "ok", 10, "ok")
    miner = Sv1Miner(pool.pooled_v1_port, f"{regtest_cluster.miner_address(0)}.failed")
    assert miner.rejection(miner.share()) is not None
    miner.client.close()
    status, body = health(pool)
    assert (status, body["status"], body["checks"]["pooled_credit"]) == (200, "degraded", "stopped")
    assert body["checks"]["work"] is True, "miners still get work"


def test_pooled_credit_is_null_with_pooled_and_hybrid_off(pool_factory):
    pool = pool_factory(overrides={"pools": {"pooled": None, "hybrid": None}})
    assert wait_until(lambda: health(pool)[1]["status"] == "ok", 10, "ok")
    assert health(pool)[1]["checks"]["pooled_credit"] is None

