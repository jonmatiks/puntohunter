"""Private balances: a balance token, issued by `puntohunter token issue`, opens its address's
pending, available, reserved, and paid balances in each pool it holds one in, in config order, at
`GET /api/v1/balance` under a bearer header, and matches the ledger. A missing or wrong token, one in the
URL, or a worker or fleet handle, opens nothing, and no token reaches the pool's log. Every response passes the
privacy scan: the view never names the address."""

import re
import subprocess

from clients.sv1 import Sv1Miner
from payouts import broadcast, confirm, earn, mature, start
from wait import wait_until

MODES = ("pooled", "hybrid")
FIELDS = ("pending", "available", "reserved", "paid")


def ledger(pool, address):
    """The address's balances in the ledger, by pool, in satoshis: each pool it holds one in."""
    rows = pool.rows("SELECT * FROM balances WHERE address = ?", address)
    return {row["pool"]: {field: row[field] for field in FIELDS} for row in rows}


def api(pool, token):
    """`GET /api/v1/balance` for `token`, by pool, its fields without the `_sats` suffix."""
    response = pool.portal.api("/api/v1/balance", headers={"Authorization": f"Bearer {token}"})
    assert response.status == 200, response.status
    assert response.header("cache-control") == "no-store"
    pools = response.json["pools"]
    assert all(set(entry) == {"pool", *(f"{f}_sats" for f in FIELDS)} for entry in pools), pools
    held = [entry["pool"] for entry in pools]
    assert held == [id for id in ("solo", "pooled", "hybrid") if id in held], "in config order"
    return {entry["pool"]: {field: entry[f"{field}_sats"] for field in FIELDS} for entry in pools}


def test_a_token_opens_its_addresss_balances_matching_the_ledger(pool_factory, fresh_node, solution_forcer):
    pool = start(pool_factory, fresh_node)
    addresses = [fresh_node.miner_address(n) for n in range(2)]
    tokens = [pool.issue_token(address) for address in addresses]
    assert tokens[0] != tokens[1]

    def matches():
        for address, token in zip(addresses, tokens):
            assert api(pool, token) == ledger(pool, address)

    matches()
    earn(pool, fresh_node, solution_forcer, "pooled", finder=0, others=[1])
    earn(pool, fresh_node, solution_forcer, "hybrid", finder=1, others=[0])
    assert all(ledger(pool, address)[mode]["pending"] > 0 for address in addresses for mode in MODES)
    matches()

    # Paid in full, once a batch confirms.
    mature(pool, fresh_node)
    broadcast(pool)
    confirm(pool, fresh_node)
    assert all(ledger(pool, address)[mode]["paid"] > 0 for address in addresses for mode in MODES)
    matches()

    for token in tokens:
        assert token not in pool.logs()


def test_a_missing_or_wrong_token_or_a_handle_opens_nothing(pool_factory, regtest_cluster, miner_username):
    pool = pool_factory()
    address = miner_username.split(".")[0]
    token = pool.issue_token(address)
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    [worker] = wait_until(
        lambda: re.findall(r"V1 authorized.*handle=([a-z2-7]{16})", pool.logs()), 10, "the worker's handle"
    )
    [fleet] = wait_until(
        lambda: [row["fleet"] for row in pool.rows("SELECT fleet FROM fleets WHERE handle = ?", worker)],
        10,
        "the fleet's handle",
    )
    miner.client.close()

    for headers in ({}, {"Authorization": f"Basic {token}"}, {"Authorization": "Bearer "}):
        refused = pool.portal.api("/api/v1/balance", headers=headers)
        assert (refused.status, refused.json["error"]) == (401, "unauthorized"), headers
        assert refused.header("www-authenticate") == "Bearer"
        assert refused.header("cache-control") == "no-store"
    assert pool.portal.api(f"/api/v1/balance?token={token}").status == 401, "never from the URL"

    wrong = "0" * 64
    for refused in (wrong, token[:-1], token.upper(), worker, fleet, address, miner_username):
        response = pool.portal.api("/api/v1/balance", headers={"Authorization": f"Bearer {refused}"})
        assert (response.status, response.json["error"]) == (401, "unauthorized"), refused
        assert response.header("www-authenticate") == "Bearer"
    assert api(pool, token) == ledger(pool, address)

    for secret in (token, wrong):
        assert secret not in pool.logs()


def test_a_token_is_issued_for_an_address_of_the_configured_network_alone(pool_factory, regtest_cluster):
    pool = pool_factory()
    mainnet = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq"
    issued = subprocess.run(
        [pool.binary, "token", "issue", mainnet, "--config", pool.config_path], capture_output=True, text=True
    )
    assert issued.returncode != 0
    assert f"{mainnet} is not a regtest address" in issued.stderr
    assert pool.rows("SELECT * FROM tokens") == []
