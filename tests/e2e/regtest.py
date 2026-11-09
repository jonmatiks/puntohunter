"""A Compose regtest node under its own project name and ports, so runs can sit side by side."""

import fcntl
import os
import pathlib
import socket
import subprocess
import tempfile
import tomllib
import uuid

from chain import Chain

E2E = pathlib.Path(__file__).parent
ROOT = E2E.parents[1]
COMPOSE_FILE = E2E / "docker-compose.yml"
REGTEST_CONFIG = tomllib.loads((ROOT / "config" / "regtest.toml").read_text())
WALLET = tomllib.loads((E2E / "wallet.toml").read_text())
PAYOUT_WALLET = REGTEST_CONFIG["payouts"]["wallet"]
MINERS_WALLET = "miners"
# The payout wallet's passphrase where a test encrypts it, as mainnet's is; it guards nothing.
PAYOUT_PASSPHRASE = "regtest-payouts"
FUNDER_WALLET = "funder"


_issued = set()


def free_port():
    """A port free now and never returned before in this process. The port is released at once, so the kernel
    may offer it again to the next caller: a pool taking eight in a row once got one twice, and the second
    listener could not bind."""
    while True:
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            port = s.getsockname()[1]
        if port not in _issued:
            _issued.add(port)
            return port


def build():
    """Builds the node image. Run it before starting nodes, which never build."""
    # One build at a time, so xdist workers starting together stay off each other.
    with open(pathlib.Path(tempfile.gettempdir()) / "puntohunter-e2e-build.lock", "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        subprocess.run(["docker", "compose", "-f", COMPOSE_FILE, "build", "--quiet"], check=True)


class RegtestNode:
    def __init__(self, project, rpc_port, zmq_port):
        self.project = project
        self.rpc_port = rpc_port
        self.zmq_port = zmq_port
        node = REGTEST_CONFIG["node"]
        self.chain = Chain(f"http://127.0.0.1:{rpc_port}", node["rpc_user"], node["rpc_password"])

    @classmethod
    def fresh(cls):
        """A node with a unique project name on free ports."""
        return cls(f"puntohunter-e2e-{uuid.uuid4().hex[:8]}", free_port(), free_port())

    @property
    def container(self):
        """The node's container, as Compose names it."""
        return f"{self.project}-bitcoind-1"

    def compose(self, *args):
        env = {"RPC_PORT": str(self.rpc_port), "ZMQ_PORT": str(self.zmq_port)}
        return subprocess.run(
            ["docker", "compose", "-p", self.project, "-f", COMPOSE_FILE, *args],
            env={**os.environ, **env},
            check=True,
            capture_output=True,
            text=True,
        )

    def up(self):
        """Starts the node and returns once its RPC answers."""
        self.compose("up", "--detach", "--wait", "--no-build")

    def down(self):
        self.compose("down", "--volumes", "--remove-orphans")

    def stop(self):
        """Stops the node. Its datadir is tmpfs, so up() afterwards starts a fresh chain."""
        self.compose("stop")

    def pause(self):
        """Freezes the node: RPC calls hang instead of being refused."""
        self.compose("pause")

    def unpause(self):
        self.compose("unpause")

    def logs(self):
        return self.compose("logs", "--no-color").stdout

    def address(self, role):
        """The test wallet's address for `role`, derived by the node from the descriptor."""
        return self.address_at(WALLET["index"][role])

    def miner_address(self, n):
        """The address of the `n`th simulated miner."""
        first, last = WALLET["index"]["simulated_miners"]
        assert first + n <= last, f"the test wallet has {last - first + 1} simulated miners"
        return self.address_at(first + n)

    def wallet(self, name):
        """The node's wallet `name`, at its own RPC endpoint."""
        node = REGTEST_CONFIG["node"]
        return Chain(f"{self.chain.url}/wallet/{name}", node["rpc_user"], node["rpc_password"])

    def create_wallets(self):
        """Creates the payout wallet, holding the pool wallet's key alone, and a watch-only wallet over the
        miners' addresses, the Bitaxe's, puntominer's, and the simulated miners', for the tests to read.
        Neither holds the fee addresses. Both scan the chain from its start. A third wallet, of random keys,
        funds the payout wallet as an operator would (`fund_payouts`)."""
        index = WALLET["index"]
        pool_key = WALLET["descriptor"].split("#")[0].replace("/*)", f"/{index['pool_wallet']})")
        self.chain.call("createwallet", PAYOUT_WALLET, False, True)
        self._import(PAYOUT_WALLET, [pool_key])
        first, last = index["simulated_miners"]
        miners = [index["bitaxe"], index["puntominer"], *range(first, last + 1)]
        self.chain.call("createwallet", MINERS_WALLET, True, True)
        self._import(MINERS_WALLET, [f"addr({self.address_at(n)})" for n in miners])
        self.chain.call("createwallet", FUNDER_WALLET)

    def funder_address(self):
        """An address of the funding wallet. Blocks mined to it advance the chain without paying the payout
        wallet, which then holds no coinbase but the pool's, as startup reconciliation requires."""
        if not hasattr(self, "_funder_address"):
            self._funder_address = self.wallet(FUNDER_WALLET).call("getnewaddress")
        return self._funder_address

    def fund_payouts(self, btc):
        """Sends `btc` to the payout wallet from the funding wallet, as an operator funds it, and confirms it.
        The funding wallet's coins are mined first: up to 50 BTC for each block that matures."""
        self.chain.mine(101, self.funder_address())
        funder = self.wallet(FUNDER_WALLET)
        # Regtest has no fee estimates: a rate of 2 sat/vB.
        pool_wallet = self.address("pool_wallet")
        funder.call("sendtoaddress", pool_wallet, btc, "", "", False, False, None, "unset", None, 2)
        self.chain.mine(1, self.funder_address())

    def encrypt_payouts(self):
        """Encrypts the payout wallet, which then stays locked, as on mainnet, until `unlock_payouts`."""
        self.wallet(PAYOUT_WALLET).call("encryptwallet", PAYOUT_PASSPHRASE)

    def unlock_payouts(self, seconds=60):
        """Unlocks the encrypted payout wallet for `seconds`, as the operator does for a batch."""
        self.wallet(PAYOUT_WALLET).call("walletpassphrase", PAYOUT_PASSPHRASE, seconds)

    def _import(self, wallet, descriptors):
        requests = [
            {"desc": f"{d}#{self.chain.call('getdescriptorinfo', d)['checksum']}", "timestamp": 0}
            for d in descriptors
        ]
        for result in self.wallet(wallet).call("importdescriptors", requests):
            assert result["success"], f"importdescriptors into {wallet}: {result}"

    def address_at(self, index):
        return self.chain.call("deriveaddresses", WALLET["descriptor"], [index, index])[0]

    def wallet_addresses(self):
        """Every address the test wallet gives a role or a simulated miner, derived once."""
        if not hasattr(self, "_wallet_addresses"):
            last = WALLET["index"]["simulated_miners"][1]
            self._wallet_addresses = self.chain.call("deriveaddresses", WALLET["descriptor"], [0, last])
        return self._wallet_addresses
