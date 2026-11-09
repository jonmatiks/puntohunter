"""Chain helpers over the regtest node's JSON-RPC, shared by the e2e suite and live.py."""

import base64
import json
import urllib.error
import urllib.request
from decimal import Decimal


def sats(btc):
    """An amount the node reports in BTC, in satoshis."""
    return int(Decimal(str(btc)) * 100_000_000)


class RpcError(Exception):
    def __init__(self, method, code, message):
        super().__init__(f"{method}: {message} (code {code})")
        self.code = code


class Chain:
    def __init__(self, url, user, password, timeout=30):
        self.url = url
        self.timeout = timeout
        token = base64.b64encode(f"{user}:{password}".encode()).decode()
        self.headers = {"Authorization": f"Basic {token}", "Content-Type": "application/json"}

    def call(self, method, *params):
        body = json.dumps({"jsonrpc": "1.0", "id": method, "method": method, "params": params})
        request = urllib.request.Request(self.url, body.encode(), self.headers)
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                reply = json.load(response)
        except urllib.error.HTTPError as e:
            # bitcoind answers an RPC error with an HTTP error status and the error in the body.
            if e.headers.get_content_type() != "application/json":
                raise
            reply = json.load(e)
        if reply.get("error"):
            raise RpcError(method, reply["error"]["code"], reply["error"]["message"])
        return reply["result"]

    def mine(self, blocks, address):
        """Mines `blocks` blocks paying `address` and returns their hashes."""
        return self.call("generatetoaddress", blocks, address)

    def tip(self):
        """Returns the tip as (height, hash)."""
        info = self.call("getblockchaininfo")
        return info["blocks"], info["bestblockhash"]
