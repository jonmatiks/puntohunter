"""A minimal Stratum V1 client, plus a hostile variant."""

import collections
import json
import socket

from clients.miner import ShareGenerator


def stratum_prevhash(display_hash):
    """A block hash in Stratum's notify order: header byte order, each 4-byte word reversed."""
    raw = bytes.fromhex(display_hash)[::-1]
    return b"".join(raw[i : i + 4][::-1] for i in range(0, 32, 4)).hex()


class StratumError(Exception):
    def __init__(self, error):
        super().__init__(f"Stratum error {error}")
        self.code = error[0]
        self.message = error[1]


class ConnectionClosed(Exception):
    pass


class Sv1Client:
    def __init__(self, port, host="127.0.0.1", timeout=10, source=None):
        # From the IP address `source` when given: Linux routes all of 127.0.0.0/8 to the loopback, so a test's
        # connections can come from 127.0.0.2 and on.
        self.sock = socket.create_connection(
            (host, port), timeout=timeout, source_address=(source, 0) if source else None
        )
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.timeout = timeout
        self.buffer = b""
        self.notifications = collections.deque()
        self.next_id = 1
        self.extranonce1 = None
        self.extranonce2_size = None

    def close(self):
        self.sock.close()

    def send(self, message):
        self.sock.sendall((json.dumps(message) + "\n").encode())

    def read(self, timeout=None):
        """The next message from the pool."""
        self.sock.settimeout(timeout or self.timeout)
        while b"\n" not in self.buffer:
            try:
                data = self.sock.recv(65536)
            except ConnectionResetError:
                data = b""
            if not data:
                raise ConnectionClosed()
            self.buffer += data
        line, self.buffer = self.buffer.split(b"\n", 1)
        return json.loads(line)

    def request(self, method, *params):
        """Sends a request and returns its response, queueing notifications that arrive first."""
        request_id = self.next_id
        self.next_id += 1
        self.send({"id": request_id, "method": method, "params": list(params)})
        while True:
            message = self.read()
            if message.get("id") == request_id and "method" not in message:
                return message
            self.notifications.append(message)

    def call(self, method, *params):
        """Sends a request and returns its result, raising StratumError on an error."""
        response = self.request(method, *params)
        if response.get("error"):
            raise StratumError(response["error"])
        return response["result"]

    def subscribe(self, agent="puntohunter-e2e/1.0"):
        result = self.call("mining.subscribe", agent)
        self.extranonce1, self.extranonce2_size = result[1], result[2]
        return result

    def authorize(self, username, password="x"):
        return self.call("mining.authorize", username, password)

    def configure(self, mask="1fffe000", min_bit_count=2):
        return self.call(
            "mining.configure",
            ["version-rolling"],
            {"version-rolling.mask": mask, "version-rolling.min-bit-count": min_bit_count},
        )

    def next_message(self, timeout=None):
        """The next message, queued or not."""
        if self.notifications:
            return self.notifications.popleft()
        return self.read(timeout)

    def notification(self, method, timeout=None):
        """The next notification of `method`, dropping others before it."""
        while self.notifications:
            message = self.notifications.popleft()
            if message.get("method") == method:
                return message
        while True:
            message = self.read(timeout)
            if message.get("method") == method:
                return message

    def alive(self):
        """Whether the pool still answers on this connection."""
        try:
            return self.call("mining.extranonce.subscribe") is True
        except (ConnectionClosed, OSError):
            return False

    def closed_by_pool(self, timeout=None):
        """Waits for the pool to close the connection, reading past anything it sends first."""
        try:
            while True:
                self.read(timeout)
        except ConnectionClosed:
            return True
        except TimeoutError:
            return False


class Sv1Miner:
    """A V1 client that has configured version rolling, subscribed, and authorized, with a share generator
    for the jobs it receives."""

    def __init__(self, port, username, password="x", agent="puntohunter-e2e/1.0", source=None):
        self.client = Sv1Client(port, source=source)
        self.username = username
        self.client.configure()
        self.client.subscribe(agent)
        self.client.authorize(username, password)
        self.generator = ShareGenerator(self.client.extranonce1, self.client.extranonce2_size)
        self.difficulty, self.notify = None, None
        self.next_job()

    def next_job(self):
        """Reads up to the next notify, tracking the difficulty set before it."""
        while True:
            message = self.client.next_message()
            if message.get("method") == "mining.set_difficulty":
                self.difficulty = message["params"][0]
            elif message.get("method") == "mining.notify":
                self.notify = message["params"]
                return self.notify

    def clean_job(self):
        """Reads up to the next notify with clean_jobs set, as a new tip brings."""
        while not self.next_job()[8]:
            pass
        return self.notify

    def submit(self, params, username=None):
        return self.client.call("mining.submit", username or self.username, *params)

    def share(self, **kwargs):
        return self.generator.share(self.notify, self.difficulty, **kwargs)

    def rejection(self, params, username=None):
        """Submits a share that must be refused, and returns the refusal."""
        try:
            result = self.submit(params, username)
        except StratumError as e:
            return e
        raise AssertionError(f"expected a rejection, got {result!r}")


class HostileSv1Client(Sv1Client):
    """Sends what a well-behaved miner never would."""

    def send_raw(self, data):
        try:
            self.sock.sendall(data)
        except (BrokenPipeError, ConnectionResetError):
            pass  # The pool closed on us part way, which is what the caller checks.

    def send_bytewise(self, message):
        """Sends one message a byte per segment, so the pool sees it split across reads."""
        for byte in (json.dumps(message) + "\n").encode():
            self.sock.send(bytes([byte]))
