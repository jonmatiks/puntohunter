"""The Compose regtest node: runs side by side, and derives the checked-in regtest addresses."""

import regtest
from regtest import REGTEST_CONFIG, RegtestNode


def test_two_nodes_run_side_by_side():
    regtest.build()
    a, b = RegtestNode.fresh(), RegtestNode.fresh()
    try:
        a.up()
        b.up()
        [block_a] = a.chain.mine(1, a.address("pool_wallet"))
        b.chain.mine(2, b.address("pool_wallet"))
        assert a.chain.tip() == (1, block_a)
        assert b.chain.tip()[0] == 2
    finally:
        a.down()
        b.down()


def test_regtest_config_pays_the_wallet_roles(regtest_cluster):
    assert REGTEST_CONFIG["addresses"]["pool_wallet"] == regtest_cluster.address("pool_wallet")
    for fee in REGTEST_CONFIG["fees"]:
        assert fee["address"] == regtest_cluster.address(fee["name"]), fee["name"]
