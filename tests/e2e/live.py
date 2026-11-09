#!/usr/bin/env python3
"""The regtest chain for live runs.

`live.py up` starts the Compose node on the ports config/regtest.toml names, creates the payout wallet,
holding the pool wallet's key alone, a watch-only wallet over the miners' addresses, and a funding wallet,
and sends 10 BTC from the funding wallet to the payout wallet, so the fee reserve has spendable funds. The
payout wallet then holds no coinbase the pool did not find, as startup reconciliation requires. The chain
runs until `live.py down`. Every `up` starts a fresh chain, because the datadir is tmpfs.

`live.py fleet-up` does the same on the fleet suite's three nodes, connected over P2P, on the fixed ports
docker-compose.fleet.yml defaults to, with the wallets on node0, and writes data/regtest-fleet.toml: config/
regtest.toml with node0 as the primary and node1 and node2 as secondaries. Run the
pool with `puntohunter serve --config data/regtest-fleet.toml`, and stop the nodes with `live.py fleet-down`.
"""

import sys
from urllib.parse import urlparse

import tomli_w

import regtest
from fleet import FLEET_TIMINGS, NAMES, RegtestFleet
from regtest import REGTEST_CONFIG, ROOT, RegtestNode

PROJECT = "puntohunter-live"
FLEET_PROJECT = "puntohunter-live-fleet"
FLEET_CONFIG = ROOT / "data" / "regtest-fleet.toml"


def live_node():
    node = REGTEST_CONFIG["node"]
    return RegtestNode(
        PROJECT, urlparse(node["rpc_url"]).port, urlparse(node["zmq_hashblock"]).port
    )


def up():
    node = live_node()
    regtest.build()
    node.down()
    node.up()
    node.create_wallets()
    node.fund_payouts(10)
    height, tip = node.chain.tip()
    print(f"regtest up: RPC 127.0.0.1:{node.rpc_port}, ZMQ 127.0.0.1:{node.zmq_port}")
    print(f"tip {height} {tip}")
    for role in ("pool_wallet", "operator", "partner", "bitaxe", "puntominer"):
        print(f"{role:12} {node.address(role)}")


def down():
    live_node().down()
    print("regtest down")


def live_fleet():
    return RegtestFleet(FLEET_PROJECT, fixed_ports=True)


def fleet_up():
    fleet = live_fleet()
    regtest.build()
    fleet.down()
    fleet.up()
    primary = fleet.nodes[0]
    primary.create_wallets()
    primary.fund_payouts(10)
    fleet.synced()
    config = dict(REGTEST_CONFIG)
    config["data_dir"] = str(ROOT / "data" / "regtest-fleet")
    config["node"] = {
        **REGTEST_CONFIG["node"],
        "rpc_url": f"http://127.0.0.1:{primary.rpc_port}",
        "zmq_hashblock": f"tcp://127.0.0.1:{primary.zmq_port}",
        "stuck_secs": FLEET_TIMINGS["stuck_secs"] * 10,
        "recovery_secs": FLEET_TIMINGS["recovery_secs"] * 6,
    }
    config["secondary_nodes"] = fleet.secondaries()
    FLEET_CONFIG.parent.mkdir(exist_ok=True)
    FLEET_CONFIG.write_text(tomli_w.dumps(config))
    height, tip = primary.chain.tip()
    for name, node in zip(NAMES, fleet.nodes):
        print(f"{name:8} RPC 127.0.0.1:{node.rpc_port}, ZMQ 127.0.0.1:{node.zmq_port}")
    print(f"tip {height} {tip}")
    print(f"config {FLEET_CONFIG.relative_to(ROOT)}")


def fleet_down():
    live_fleet().down()
    print("regtest fleet down")


if __name__ == "__main__":
    commands = {"up": up, "down": down, "fleet-up": fleet_up, "fleet-down": fleet_down}
    if len(sys.argv) != 2 or sys.argv[1] not in commands:
        sys.exit("usage: live.py up|down|fleet-up|fleet-down")
    commands[sys.argv[1]]()
