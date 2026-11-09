"""The load suite: 1,000 simulated workers, V1 from the harness and SV2 from sv2-testclient, solo
and pooled together, across several rollup periods and at least 20 new tips.

Solo workers send a share every 10 s, a Bitaxe at the production vardiff aim. Pooled workers send
one every 2.4 s, so the journal takes about 208 shares a second, above the 200 it is sized for. The difficulty is pinned
at the harness floor, below regtest's network difficulty, so no share is a block and vardiff stays out of it.

Run it alone, `pytest -m load -s test_load.py`, against the Compose stack. With PUNTOHUNTER_LOAD_SSH set to a
host's SSH destination and PUNTOHUNTER_LOAD_CONFIG to the config of a regtest instance running there, it loads
that instance instead, mining through a tunnel to its node and sampling it over SSH, and records the figures
without asserting them."""

import json
import math
import os
import subprocess
import sys
import threading
import time
import tomllib
import urllib.parse

import pytest

from chain import Chain
from clients.load import V1Load
from clients.sv2 import Sv2Load
from pool import HARNESS_FLOOR
from regtest import E2E, WALLET, free_port
from wait import wait_until

pytestmark = [pytest.mark.load, pytest.mark.timeout(1200)]

# Each group's workers are split across two load processes; a V2 group's are on extended channels in one and
# standard channels in the other.
GROUPS = [("solo", "v1"), ("pooled", "v1"), ("solo", "v2"), ("pooled", "v2")]
PER_PROCESS = 125
WORKERS = len(GROUPS) * 2 * PER_PROCESS
INTERVAL = {"solo": 10.0, "pooled": 2.4}
SUSTAIN = 250
TIP_EVERY = 10.0
ROLLUP_SECS = 60

# Spec 4.3's bounds.
MEMORY_CEILING = 512 * 2**20
FAN_OUT_P99 = 0.25
MIN_TIPS = 20
JOURNAL_RATE = 200
UNSYNCED_MAX = 1.0
RESPONSE_P99 = "0.05"
ROLLUP_LAG = 5.0


class Samples:
    """The sampler's lines, read as they come: first the pool's process and authority key, then a sample a
    second."""

    def __init__(self, process):
        self.process = process
        line = process.stdout.readline()
        assert line, "the sampler exited before its first line"
        first = json.loads(line)
        self.pid, self.authority_key = first["pid"], first["authority_key"]
        self.lines = []
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.process.stdout:
            self.lines.append(json.loads(line))

    def latest(self):
        return wait_until(lambda: self.lines and self.lines[-1], 10, "a sample")

    def mark(self):
        """The index the next sample will have."""
        return len(self.lines)

    def close(self):
        self.process.kill()
        self.process.wait()


class Local:
    """The release binary against the Compose node, sampled beside it."""

    def __init__(self, pool_factory, node):
        self.pool = pool_factory(overrides={"vardiff": {"floor": HARNESS_FLOOR, "ceiling": HARNESS_FLOOR}})
        assert self.pool.config["telemetry"]["rollup_secs"] == ROLLUP_SECS
        self.host = "127.0.0.1"
        self.ports = {(id, p): self.pool.port(id, p) for id, p in GROUPS}
        self.chain = node.chain
        self.samples = Samples(
            subprocess.Popen(
                [sys.executable, E2E / "sampler.py", "--config", self.pool.config_path],
                stdout=subprocess.PIPE,
                text=True,
            )
        )

    def close(self):
        self.samples.close()


class Remote:
    """A regtest instance on another host: its node through an SSH tunnel, sampled over SSH."""

    def __init__(self, destination, config_path):
        read = subprocess.run(["ssh", destination, "cat", config_path], capture_output=True, check=True)
        config = tomllib.loads(read.stdout.decode())
        assert config["network"] == "regtest", "the load suite runs against a regtest instance only"
        self.host = destination.rsplit("@", 1)[-1]
        pools = {pool["id"]: pool for pool in config["pools"]}
        self.ports = {(id, p): int(pools[id][p].rsplit(":", 1)[1]) for id, p in GROUPS}
        node = urllib.parse.urlsplit(config["node"]["rpc_url"])
        local = free_port()
        self.tunnel = subprocess.Popen(
            ["ssh", "-N", "-o", "ExitOnForwardFailure=yes", "-L", f"{local}:{node.hostname}:{node.port}", destination]
        )
        self.chain = Chain(f"http://127.0.0.1:{local}", config["node"]["rpc_user"], config["node"]["rpc_password"])
        wait_until(self._answers, 30, "the node through the tunnel")
        with open(E2E / "sampler.py") as script:
            sampler = subprocess.Popen(
                ["ssh", destination, "python3", "-", "--config", config_path],
                stdin=script,
                stdout=subprocess.PIPE,
                text=True,
            )
        self.samples = Samples(sampler)

    def _answers(self):
        try:
            return self.chain.call("getblockcount") >= 0
        except OSError:
            return False

    def close(self):
        self.samples.close()
        self.tunnel.kill()
        self.tunnel.wait()


@pytest.fixture
def target(request):
    if destination := os.environ.get("PUNTOHUNTER_LOAD_SSH"):
        target = Remote(destination, os.environ["PUNTOHUNTER_LOAD_CONFIG"])
    else:
        target = Local(request.getfixturevalue("pool_factory"), request.getfixturevalue("regtest_cluster"))
    try:
        yield target
    finally:
        target.close()


def percentile(values, p):
    ordered = sorted(values)
    return ordered[max(0, math.ceil(p * len(ordered)) - 1)]


def test_a_thousand_workers_hold_the_spec_figures(target):
    first, last = WALLET["index"]["simulated_miners"]
    addresses = target.chain.call("deriveaddresses", WALLET["descriptor"], [first, last])
    loads = []
    try:
        for group, (pool, protocol) in enumerate(GROUPS):
            port = target.ports[(pool, protocol)]
            for part in range(2):
                users = [
                    f"{addresses[i % len(addresses)]}.load{group}{part}w{i}" for i in range(PER_PROCESS)
                ]
                if protocol == "v1":
                    loads.append(V1Load(target.host, port, users, INTERVAL[pool]))
                else:
                    pin = target.samples.authority_key
                    loads.append(Sv2Load(target.host, port, pin, users, INTERVAL[pool], standard=part == 1))
        failed = [reason for load in loads for reason in load.started()["failed"]]
        assert not failed, failed[:5]
        wait_until(lambda: target.samples.latest()["workers"] == WORKERS, 60, "every worker connected")

        # Sustained: a new tip every TIP_EVERY seconds, each timed from just before the node is asked to mine
        # it, so a fan-out is measured from no later than the node accepting the block.
        begin = target.samples.mark()
        tips = []
        end_by = time.monotonic() + SUSTAIN
        while time.monotonic() < end_by:
            due = time.monotonic() + TIP_EVERY
            asked = time.time()
            [block] = target.chain.mine(1, addresses[0])
            tips.append((block, asked, time.time()))
            time.sleep(max(0.0, due - time.monotonic()))
        end = target.samples.mark()
        reports = [load.stop() for load in loads]
        # The last period's rows, written at its end, have ROLLUP_LAG to appear.
        written_by = target.samples.lines[end - 1]["at"] + ROLLUP_LAG
        wait_until(lambda: target.samples.latest()["at"] > written_by, 15, "the rollup's last write")
    finally:
        for load in loads:
            load.kill()

    samples = target.samples.lines
    window = samples[begin:end]
    start, stop = window[0], window[-1]
    seconds = stop["at"] - start["at"]

    def delta(key):
        return {k: v - start[key].get(k, 0) for k, v in stop[key].items()}

    # The fan-out: when each worker held a job on each tip, by protocol. Each tip's first arrival is the
    # path's own delay, the node's answer and the network's, and the spread after it the pool's fan-out across
    # its workers, which on another host is the figure the network does not blur.
    held = {}
    for load, report in zip(loads, reports):
        protocol = "v1" if isinstance(load, V1Load) else "v2"
        for block, times in report["tips"].items():
            held.setdefault(block, []).extend((protocol, at) for at in times)
    fan_outs, by_protocol, firsts, spreads, mined, missed = [], {}, [], [], [], {}
    for block, asked, answered in tips:
        times = held.get(block, [])
        for protocol, at in times:
            fan_outs.append(at - asked)
            by_protocol.setdefault(protocol, []).append(at - asked)
        if times:
            first_at = min(at for _, at in times)
            firsts.append(first_at - asked)
            spreads += [at - first_at for _, at in times]
        mined.append(answered - asked)
        if len(times) < WORKERS:
            missed[block] = WORKERS - len(times)

    # The rollup: each period inside the window has a row for every worker, written soon after it ends.
    rollups = {}
    for boundary in range(math.ceil(start["at"] / ROLLUP_SECS) * ROLLUP_SECS, int(stop["at"]) + 1, ROLLUP_SECS):
        if boundary - ROLLUP_SECS < start["at"]:
            continue
        key = str(boundary - ROLLUP_SECS)
        written = [s["at"] for s in samples[begin:] if s["rollups"].get(key, 0) >= WORKERS]
        rollups[key] = round(written[0] - boundary, 1) if written else None

    response = delta("response")
    figures = {
        "workers": WORKERS,
        "seconds": round(seconds),
        "tips": len(tips),
        "memory_mib": round(max(s["rss"] for s in samples[:end]) / 2**20),
        "fan_out_p99_ms": round(percentile(fan_outs, 0.99) * 1000),
        "fan_out_max_ms": round(max(fan_outs) * 1000),
        "fan_out_p99_ms_by_protocol": {k: round(percentile(v, 0.99) * 1000) for k, v in by_protocol.items()},
        "first_arrival_median_ms": round(percentile(firsts, 0.5) * 1000),
        "spread_p99_ms": round(percentile(spreads, 0.99) * 1000),
        "mining_call_median_ms": round(percentile(mined, 0.5) * 1000),
        "missed_tips": missed,
        "accepted_per_sec": {pool: round(n / seconds, 1) for pool, n in delta("accepted").items()},
        "journal_per_sec": round((stop["journal"] - start["journal"]) / seconds, 1),
        "unsynced_max_s": max(s["unsynced"] for s in window),
        "response_within_50ms": round(response[RESPONSE_P99] / response["+Inf"], 5),
        "rejected": {k: v for k, v in delta("rejected").items() if v},
        "rollup_lag_s": rollups,
        "rollup_rows_per_sec": round(WORKERS / ROLLUP_SECS, 1),
        "pool_cores": round((stop["cpu"] - start["cpu"]) / seconds, 2),
        "steal_pct": round(100 * (stop["steal"] - start["steal"]) / (stop["ticks"] - start["ticks"]), 2),
        "closed": [reason for report in reports for reason in report["closed"]][:5],
    }
    print(f"\nload figures: {json.dumps(figures, indent=2)}")
    if isinstance(target, Remote):
        return

    assert not figures["closed"]
    assert figures["memory_mib"] * 2**20 < MEMORY_CEILING
    assert len(tips) >= MIN_TIPS
    assert not missed, "every worker holds a job on every tip"
    assert percentile(fan_outs, 0.99) < FAN_OUT_P99
    assert figures["journal_per_sec"] >= JOURNAL_RATE
    assert figures["unsynced_max_s"] < UNSYNCED_MAX
    assert figures["response_within_50ms"] >= 0.99
    assert set(figures["rejected"]) <= {"stale"}, "only shares on a replaced tip are refused"
    assert len(rollups) >= 3 and all(lag is not None and lag <= ROLLUP_LAG for lag in rollups.values())
