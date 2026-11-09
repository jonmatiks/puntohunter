"""Three regtest nodes connected over P2P under one Compose project, for the fleet suite,
and the reads of the pool's view of them."""

import os
import re
import subprocess
import uuid

from chain import RpcError
from regtest import E2E, RegtestNode, free_port
from wait import wait_until

COMPOSE_FILE = E2E / "docker-compose.fleet.yml"
SERVICES = ("node0", "node1", "node2")
# What the pool calls each node: node0 is the primary.
NAMES = ("primary", "node1", "node2")
# Short timeouts and windows, so a fault and a recovery each show within seconds.
FLEET_TIMINGS = {
    "rpc_timeout_secs": 1.0,
    "longpoll_timeout_secs": 2.0,
    "stuck_secs": 3.0,
    "recovery_secs": 5.0,
}
# docker-compose.fleet.yml's default RPC and ZMQ ports, node by node, then the signet node's.
FIXED_PORTS = (18450, 28450, 18451, 28451, 18452, 28452, 18459, 28459)
SAMPLE = re.compile(r'^(\w+)(?:\{(.*)\})? (\S+)$')
LABEL = re.compile(r'(\w+)="([^"]*)"')


class FleetNode(RegtestNode):
    """One node of the fleet: a RegtestNode whose Compose commands act on its service alone."""

    def __init__(self, fleet, service, rpc_port, zmq_port):
        super().__init__(fleet.project, rpc_port, zmq_port)
        self.fleet = fleet
        self.service = service

    def compose(self, *args):
        return self.fleet.compose(*args, self.service)

    def up(self):
        self.compose("up", "--detach", "--wait", "--no-build")

    def stop(self):
        """Stops the node. Its datadir is tmpfs, so start() begins a fresh chain, synced from the others."""
        self.compose("stop")

    def kill(self):
        """Kills the node at once, as a crash or a lost host would. A graceful stop
        blocks until bitcoind has shut down, which would hide when its RPC stopped answering."""
        self.compose("kill")

    def start(self):
        self.compose("start")
        self.fleet.forget_addresses()
        wait_until(self._answers, 30, f"{self.service} answering")

    def isolate(self):
        """Cuts the node off from its peers, while its RPC answers on."""
        self.chain.call("setnetworkactive", False)

    def reconnect(self):
        self.chain.call("setnetworkactive", True)
        for peer in SERVICES:
            if peer != self.service:
                self.chain.call("addnode", f"{peer}:18444", "onetry")

    def peers(self):
        """How many other nodes it is connected to. A pair may hold two connections, one each way, and a peer
        shows as its service name outbound and as its address inbound, so each is counted once by address."""
        hosts = {peer["addr"].rsplit(":", 1)[0] for peer in self.chain.call("getpeerinfo")}
        return len({self.fleet.addresses().get(host, host) for host in hosts})

    def _answers(self):
        try:
            return self.chain.call("getblockchaininfo") is not None
        except (OSError, RpcError):
            # Refused, or still warming up (RPC_IN_WARMUP).
            return False


class RegtestFleet:
    """Three nodes, node0 the primary, each with its own RPC and ZMQ ports, and a signet node started on
    demand for the wrong-chain case."""

    def __init__(self, project=None, fixed_ports=False):
        """A fleet under `project`, by default a fresh name, on free ports, or with `fixed_ports` on the ports
        docker-compose.fleet.yml defaults to, as live.py runs it."""
        self.project = project or f"puntohunter-fleet-{uuid.uuid4().hex[:8]}"
        ports = iter(FIXED_PORTS) if fixed_ports else iter(lambda: free_port(), None)
        self.env = {}
        self.nodes = []
        for i, service in enumerate(SERVICES):
            rpc, zmq = next(ports), next(ports)
            self.env[f"RPC_{i}"], self.env[f"ZMQ_{i}"] = str(rpc), str(zmq)
            self.nodes.append(FleetNode(self, service, rpc, zmq))
        self.wrong = FleetNode(self, "wrong", next(ports), next(ports))
        self.env["RPC_W"], self.env["ZMQ_W"] = str(self.wrong.rpc_port), str(self.wrong.zmq_port)

    def addresses(self):
        """Each node's address on the Compose network, by service name, looked up once until a node starts."""
        if not getattr(self, "_addresses", None):
            containers = [f"{self.project}-{service}-1" for service in SERVICES]
            address = "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}"
            listed = subprocess.run(
                ["docker", "inspect", "-f", address, *containers],
                check=True,
                capture_output=True,
                text=True,
            ).stdout.split()
            self._addresses = dict(zip(SERVICES, listed))
        return self._addresses

    def forget_addresses(self):
        self._addresses = None

    def compose(self, *args):
        return subprocess.run(
            ["docker", "compose", "-p", self.project, "-f", COMPOSE_FILE, "--profile", "wrong", *args],
            env={**os.environ, **self.env},
            check=True,
            capture_output=True,
            text=True,
        )

    def up(self):
        """Starts the three nodes, waits until each has both peers, and mines a block so they leave IBD."""
        self.compose("up", "--detach", "--wait", "--no-build", *SERVICES)
        self.connected()
        self.mine(0, 1)

    def connected(self):
        """Waits until every node is connected to both others."""
        wait_until(lambda: all(node.peers() == 2 for node in self.nodes), 30, "every node with both peers")

    def down(self):
        self.compose("down", "--volumes", "--remove-orphans")

    def logs(self):
        return self.compose("logs", "--no-color").stdout

    def mine(self, i, blocks=1, among=None):
        """Mines `blocks` blocks on node `i`, waits until the nodes `among`, by default every node, share its
        tip, and returns their hashes. A node cut off from its peers waits for none. Each node mines to an
        address of its own: two nodes mining on one parent in one second to one address make one block."""
        node = self.nodes[i]
        hashes = node.chain.mine(blocks, node.miner_address(i))
        if node.peers():
            self.synced(among)
        return hashes

    def synced(self, nodes=None):
        """Waits until `nodes`, by default every node, share a tip, and returns it."""
        chosen = [self.nodes[i] for i in (nodes or range(len(self.nodes)))]
        return wait_until(
            lambda: len({node.chain.tip() for node in chosen}) == 1 and chosen[0].chain.tip(),
            30,
            "the nodes on one tip",
        )

    def secondaries(self, *, wrong=False):
        """The pool's [[secondary_nodes]] for node1 and node2, and the signet node as `wrong` if asked."""
        nodes = list(zip(NAMES[1:], self.nodes[1:]))
        if wrong:
            nodes.append(("wrong", self.wrong))
        return [
            {
                "name": name,
                "rpc_url": f"http://127.0.0.1:{node.rpc_port}",
                "rpc_user": "puntohunter",
                "rpc_password": "puntohunter",
                "zmq_hashblock": f"tcp://127.0.0.1:{node.zmq_port}",
            }
            for name, node in nodes
        ]

    def overrides(self, *, wrong=False):
        """The pool overrides that make it take work from the fleet."""
        return {"node": FLEET_TIMINGS, "secondary_nodes": self.secondaries(wrong=wrong)}


def samples(pool):
    """The pool's metrics, as {(name, frozenset of label pairs): value}."""
    found = {}
    for line in pool.metrics().splitlines():
        if line and not line.startswith("#"):
            name, labels, value = SAMPLE.match(line).groups()
            found[(name, frozenset(LABEL.findall(labels or "")))] = float(value)
    return found


def selected(pool):
    """The name of the node the pool takes work from, or None."""
    found = samples(pool)
    chosen = [name for name in NAMES + ("wrong",) if found.get(selection(name)) == 1]
    return chosen[0] if chosen else None


def selection(name):
    return ("puntohunter_node_selected", frozenset({("node", name)}))


def standing(pool, name):
    """Node `name`'s standing in the pool's fleet."""
    found = samples(pool)
    [current] = [
        dict(labels)["standing"]
        for (metric, labels), value in found.items()
        if metric == "puntohunter_node_standing" and ("node", name) in labels and value == 1
    ]
    return current


def health(pool):
    """The health check's status code and body."""
    response = pool.portal.api("/api/v1/health")
    return response.status, response.json
