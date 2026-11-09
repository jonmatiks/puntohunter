"""Share response on hardware: the Bitaxe on one of a
running pool's listeners has its shares answered within the bound test_share_response.py holds the simulated
miners to, read from the same histogram, with AxeOS's own response times recorded beside it.

These run against the operator's pool, not one the harness starts, and leave the Bitaxe's settings alone. Point
the Bitaxe at a listener, then, from the repository root:

    PUNTOHUNTER_CONFIG=config/mainnet.toml PUNTOHUNTER_BITAXE=192.0.2.100 \\
        pytest tests/e2e/test_hardware.py -m hardware -rA

PUNTOHUNTER_CONFIG is the running pool's config, relative to the repository root, and PUNTOHUNTER_BITAXE the Bitaxe's
LAN address. The case for the listener the Bitaxe is on runs; the others skip, naming their port. Repeat on
each mode's V1 and V2 listener in turn. Other miners on that listener count in its histogram too."""

import json
import os
import pathlib
import statistics
import time
import tomllib
import urllib.request

import pytest

from clients.http import Http
from regtest import ROOT
from response import BOUND, answered, assert_within_bound, histogram, p99, since

pytestmark = pytest.mark.hardware

# Shares the Bitaxe has accepted over a case: at mainnet's 10 s share interval, about five minutes.
SHARES = 30
WAIT = 540
# How often AxeOS is read: a device on Wi-Fi, not a condition to wait on.
SAMPLE_EVERY = 1.0
TRANSPORTS = {"v1": "V1", "v2": "V2-native"}


def setting(name):
    value = os.environ.get(name)
    if not value:
        pytest.skip(f"{name} is not set")
    return value


def axeos(bitaxe):
    """AxeOS's own view: the pool it mines to, its shares, and its last share's response time."""
    with urllib.request.urlopen(f"http://{bitaxe}/api/system/info", timeout=5) as response:
        return json.load(response)


def active_port(info):
    return info["fallbackStratumPort"] if info.get("isUsingFallbackStratum") else info["stratumPort"]


@pytest.mark.timeout(WAIT + 60)
@pytest.mark.parametrize("transport", TRANSPORTS)
@pytest.mark.parametrize("mode", ["solo", "pooled", "hybrid"])
def test_the_bitaxes_shares_are_answered_within_the_bound(mode, transport, record_property):
    config = tomllib.loads((ROOT / pathlib.Path(setting("PUNTOHUNTER_CONFIG"))).read_text())
    bitaxe = setting("PUNTOHUNTER_BITAXE")
    listener = next((pool.get(transport) for pool in config["pools"] if pool["id"] == mode), None)
    if listener is None:
        pytest.skip(f"the pool has no {mode} {transport} listener")
    port = int(listener.rsplit(":", 1)[1])
    info = axeos(bitaxe)
    if active_port(info) != port:
        pytest.skip(f"the Bitaxe mines to port {active_port(info)}; point it at {port} to run this case")

    # The scan's addresses: the config's and the Bitaxe's own, none of which a scrape may carry.
    addresses = (
        set(config["addresses"].values())
        | {fee["address"] for fee in config.get("fees", [])}
        | {info["stratumUser"].split(".")[0]}
    )
    metrics = Http(int(config["metrics"]["bind"].rsplit(":", 1)[1]), addresses)
    labels = (mode, TRANSPORTS[transport])
    before = histogram(metrics.get("/metrics").body, *labels)

    # AxeOS's response time is its last share's alone, so it is read each time a share is accepted.
    first, seen, axeos_ms = info["sharesAccepted"], info["sharesAccepted"], []
    deadline = time.monotonic() + WAIT
    while seen - first < SHARES:
        assert time.monotonic() < deadline, f"{seen - first} of {SHARES} shares accepted within {WAIT}s"
        time.sleep(SAMPLE_EVERY)
        info = axeos(bitaxe)
        assert active_port(info) == port, "the Bitaxe left the listener"
        if info["sharesAccepted"] != seen:
            seen = info["sharesAccepted"]
            axeos_ms.append(info["responseTime"])

    shares = since(histogram(metrics.get("/metrics").body, *labels), before)
    record_property("pool_answered", answered(shares))
    record_property("pool_p99_ms", p99(shares) * 1000)
    record_property("axeos_response_ms", axeos_ms)
    print(
        f"{mode} {TRANSPORTS[transport]}: the pool answered {answered(shares):g} shares, 99% within "
        f"{p99(shares) * 1000:g} ms (bound {BOUND * 1000:g} ms); AxeOS read a median of "
        f"{statistics.median(axeos_ms):.1f} ms and a maximum of {max(axeos_ms):.1f} ms over {len(axeos_ms)} "
        "readings"
    )
    assert answered(shares) >= SHARES, "the pool answered fewer shares than the Bitaxe had accepted"
    assert_within_bound(shares)
