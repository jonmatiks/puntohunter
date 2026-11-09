"""Session and function fixtures of the e2e suite."""

import pytest

import pool as pool_module
import regtest
from fleet import RegtestFleet
from regtest import RegtestNode


@pytest.fixture(scope="session")
def regtest_cluster():
    """One regtest node on this run's ports, with the payout, miners', and funding wallets, and one block
    mined to the funding wallet so it leaves IBD."""
    regtest.build()
    node = RegtestNode.fresh()
    try:
        node.up()
        node.create_wallets()
        node.chain.mine(1, node.funder_address())
        yield node
    finally:
        node.down()


@pytest.fixture
def fresh_node():
    """A regtest node of the test's own, for tests that stop, pause, or restart it, with the payout, miners',
    and funding wallets; one block is mined to the funding wallet so it leaves IBD."""
    regtest.build()
    node = RegtestNode.fresh()
    try:
        node.up()
        node.create_wallets()
        node.chain.mine(1, node.funder_address())
        yield node
    finally:
        node.down()


@pytest.fixture
def regtest_fleet():
    """Three regtest nodes of the test's own, connected over P2P, past IBD."""
    regtest.build()
    fleet = RegtestFleet()
    try:
        fleet.up()
        yield fleet
    finally:
        fleet.down()


@pytest.fixture
def fleet_pool(pool_binary, regtest_fleet, tmp_path):
    """The pool, taking work from the fleet: node0 as the primary, node1 and node2 as secondaries."""
    handle = pool_module.Pool(
        pool_binary, regtest_fleet.nodes[0], tmp_path, overrides=regtest_fleet.overrides()
    )
    try:
        handle.start()
        yield handle
    finally:
        handle.kill()


@pytest.fixture(scope="session")
def pool_binary():
    return pool_module.build()


@pytest.fixture
def pool(pool_binary, regtest_cluster, tmp_path):
    """The pool binary, started against the cluster and ready. The handle kills and restarts it."""
    handle = pool_module.Pool(pool_binary, regtest_cluster, tmp_path)
    try:
        handle.start()
        yield handle
    finally:
        handle.kill()


@pytest.fixture
def pool_factory(pool_binary, regtest_cluster, tmp_path):
    """Starts pools with a chosen base config and overrides, against the cluster or the given node; see
    pool.Pool. Each is killed afterwards."""
    pools = []

    def start(node=None, **kwargs):
        handle = pool_module.Pool(
            pool_binary, node or regtest_cluster, tmp_path / f"pool-{len(pools)}", **kwargs
        )
        pools.append(handle)
        handle.start()
        return handle

    start.logs = lambda: "\n".join(handle.logs() for handle in pools)
    try:
        yield start
    finally:
        for handle in pools:
            handle.kill()


@pytest.fixture
def miner_username(regtest_cluster):
    """A valid regtest username: the first simulated miner's address and a worker name."""
    return f"{regtest_cluster.miner_address(0)}.e2e"


@pytest.fixture
def solution_forcer():
    """Submits a share that is also a block through a clients.sv1.Sv1Miner. Regtest's block target
    is trivial, so the share takes a few hashes. Returns the share's params and the block's hash."""

    def force(miner):
        params, block_hash = miner.generator.block(miner.notify, miner.difficulty)
        assert miner.submit(params) is True
        return params, block_hash

    return force


@pytest.hookimpl(wrapper=True)
def pytest_runtest_makereport(item, call):
    """Attaches the pool's stdout and the node's Compose logs to a failed test's report."""
    report = yield
    if report.failed:
        sources = ("pool", "pool_factory", "fleet_pool", "regtest_cluster", "fresh_node", "regtest_fleet", "roles")
        for name in sources:
            source = item.funcargs.get(name)
            if source is None:
                continue
            try:
                report.sections.append((f"{name} logs", source.logs()))
            except Exception as e:
                report.sections.append((f"{name} logs", f"unavailable: {e!r}"))
    return report
