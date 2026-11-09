"""The block lists' finders (API reference section 2.8): a solo V1 block and a
pooled V2 block, each tagged by pool and transport, with a null for the output its pool does not pay, and each
on its finder's own list alone; and every list naming each finder's miner type, without a version. The
statuses, the nulls, and the fleet's list are checked in test_api_blocks.py. Every response passes the
privacy scan."""

import re

from clients.sv1 import Sv1Miner
from clients.sv2 import Sv2Miner
from wait import wait_until


def logged_handles(pool, event):
    return re.findall(r"handle=([a-z2-7]{16})", "\n".join(l for l in pool.logs().splitlines() if event in l))


def listed(pool, path):
    """The blocks a list holds: {hash: block}, newest first."""
    response = pool.portal.api(path)
    assert response.status == 200, (path, response.body)
    return {block["hash"]: block for block in response.json["blocks"]}


def test_each_block_names_its_finders_transport_and_miner_type_without_a_version(
    pool, regtest_cluster, solution_forcer
):
    address = regtest_cluster.miner_address(0)
    axe = Sv1Miner(pool.solo_v1_port, f"{address}.axe", agent="bitaxe/BM1370/v2.15.3")
    axe_handle = logged_handles(pool, "V1 authorized")[-1]
    punto = Sv2Miner(pool, pool.pooled_v2_port, f"{address}.punto", vendor="PuntoMiner/0.4.2")
    punto_handle = logged_handles(pool, "V2 channel opened")[-1]
    _, axe_block = solution_forcer(axe)
    punto.next_job()
    [punto_block] = punto.submit(kind="block")["hashes"]
    for block_hash in (axe_block, punto_block):
        wait_until(lambda: pool.block(block_hash).get("result"), 10, "the block answered")
    miner = {axe_handle: "bitaxe", punto_handle: "PuntoMiner"}
    wait_until(
        lambda: {r["handle"]: r["vendor"] for r in pool.rows("SELECT handle, vendor FROM fleets")} == miner,
        10,
        "each miner type recorded",
    )
    [fleet] = {r["fleet"] for r in pool.rows("SELECT fleet FROM fleets WHERE handle = ?", axe_handle)}

    blocks = listed(pool, "/api/v1/blocks")
    solo, pooled = blocks[axe_block], blocks[punto_block]
    assert (solo["pool"], solo["transport"], solo["finder"], solo["miner"]) == ("solo", "V1", axe_handle, "bitaxe")
    assert solo["pool_sats"] is None and solo["finder_sats"] > 0, "a solo block has no pool output"
    assert (pooled["pool"], pooled["transport"], pooled["finder"], pooled["miner"]) == (
        "pooled", "V2-native", punto_handle, "PuntoMiner",
    )
    assert pooled["finder_sats"] is None and pooled["pool_sats"] > 0, "a pooled block has no finder output"

    # Each finder's list holds its own block alone.
    assert list(listed(pool, f"/api/v1/workers/{axe_handle}/blocks")) == [axe_block]
    assert list(listed(pool, f"/api/v1/workers/{punto_handle}/blocks")) == [punto_block]
    rows = listed(pool, f"/api/v1/fleets/{fleet}/blocks")
    assert {block_hash: block["miner"] for block_hash, block in rows.items()} == {
        axe_block: "bitaxe", punto_block: "PuntoMiner",
    }

    # No list carries a version.
    for path in ("/api/v1/blocks", f"/api/v1/fleets/{fleet}/blocks", f"/api/v1/workers/{axe_handle}/blocks"):
        body = pool.portal.api(path).body
        for version in ("2.15.3", "BM1370", "0.4.2"):
            assert version not in body, (path, version)
    axe.client.close()
    punto.close()
