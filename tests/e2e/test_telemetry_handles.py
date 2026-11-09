"""Worker handles: a worker's handle is the same on either transport and across a restart, its
secret is a 0600 file in the data directory, and a found block records its finder's handle."""

import re
import stat

from clients.sv1 import Sv1Miner
from clients.sv2 import Sv2Miner
from wait import wait_until

HANDLE = re.compile(r"handle=([a-z2-7]{16})\b")


def handles(pool, event):
    """The handles the pool logged on each line naming `event`, in order."""
    return [HANDLE.search(line)[1] for line in pool.logs().splitlines() if event in line]


def test_a_worker_has_one_handle_on_either_transport_and_across_a_restart(pool, miner_username):
    v1 = Sv1Miner(pool.solo_v1_port, miner_username)
    v2 = Sv2Miner(pool, pool.solo_v2_port, miner_username)
    [authorized] = handles(pool, "V1 authorized")
    assert handles(pool, "V2 channel opened") == [authorized]
    v1.client.close()
    v2.close()

    # Another mode is another worker.
    pooled = Sv1Miner(pool.pooled_v1_port, miner_username)
    assert handles(pool, "V1 authorized")[1] != authorized
    pooled.client.close()

    secret = pool.data_dir / "handle.key"
    assert stat.S_IMODE(secret.stat().st_mode) == 0o600
    before = secret.read_text()
    pool.restart()
    again = Sv1Miner(pool.solo_v1_port, miner_username)
    assert handles(pool, "V1 authorized")[-1] == authorized
    assert secret.read_text() == before
    again.client.close()


def test_a_found_block_records_its_finders_handle(pool, miner_username, solution_forcer):
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    [authorized] = handles(pool, "V1 authorized")
    _, block_hash = solution_forcer(miner)
    wait_until(lambda: pool.block(block_hash).get("result"), 10, "the block recorded")
    assert pool.block(block_hash)["handle"] == authorized
    [found] = [line for line in pool.logs().splitlines() if "share is a block" in line]
    assert f"handle={authorized}" in found
    miner.client.close()
