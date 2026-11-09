"""Share response: every submit is timed by the pool from its read to its
answer written, V1's response or the SV2 acknowledgement or error covering it, under its mode and transport,
and a burst over either protocol in each mode is answered within the bound the hardware tests hold the Bitaxe
to."""

import pytest

from clients.sv1 import Sv1Miner
from clients.sv2 import Sv2Miner
from response import answered, assert_within_bound, histogram

# Between the harness floor and regtest's network difficulty, without retargets.
PINNED = 3e-10
SHARES = 20
MODES = ("solo", "pooled", "hybrid")
TRANSPORTS = {"v1": "V1", "v2": "V2-native"}


@pytest.mark.parametrize("mode", MODES)
def test_a_burst_over_either_protocol_is_answered_within_the_bound(pool_factory, miner_username, mode):
    pool = pool_factory(overrides={"vardiff": {"floor": PINNED, "ceiling": PINNED}})
    v1 = Sv1Miner(getattr(pool, f"{mode}_v1_port"), miner_username)
    v2 = Sv2Miner(pool, getattr(pool, f"{mode}_v2_port"), miner_username)

    for _ in range(SHARES):
        assert v1.submit(v1.share()) is True
    v1.rejection(v1.share(meets=False))
    # One write: the shares are acknowledged in batches, each share timed to the batch that covers it.
    assert not v2.submit(SHARES)["errors"]
    assert v2.submit(kind="low_difficulty")["errors"]

    scrape = pool.metrics()
    for transport in TRANSPORTS.values():
        buckets = histogram(scrape, mode, transport)
        assert answered(buckets) == SHARES + 1, (transport, "every submit, refused or not")
        assert_within_bound(buckets)
    for other in set(MODES) - {mode}:
        assert not any(answered(histogram(scrape, other, t)) for t in TRANSPORTS.values()), other
    for miner in (v1.client, v2):
        miner.close()
