"""The V1 side of the load suite: a process of asyncio workers, each a V1 connection that sends a
valid share every `interval` and notes when each new tip's clean job reached it. Started by `start` in a
process of its own, so the workers' timing is not the test's; `V1Load.stop` ends them and returns what they
saw, in the shape sv2-testclient's `report` has."""

import asyncio
import collections
import json
import multiprocessing
import socket
import time

from clients.miner import ShareGenerator

SETUP_TIMEOUT = 60


def display_hash(prevhash):
    """A mining.notify prevhash as the block hash it names, in display order."""
    words = bytes.fromhex(prevhash)
    return b"".join(words[i : i + 4][::-1] for i in range(0, 32, 4))[::-1].hex()


class Stats:
    def __init__(self):
        self.ready = 0
        self.failed, self.closed = [], []
        self.submitted = self.accepted = 0
        self.errors = collections.Counter()
        self.tips = collections.defaultdict(list)

    def report(self):
        return {
            "ready": self.ready,
            "failed": self.failed,
            "closed": self.closed,
            "submitted": self.submitted,
            "accepted": self.accepted,
            "errors": dict(self.errors),
            "tips": dict(self.tips),
        }


class Worker:
    def __init__(self, host, port, username, stats):
        self.host, self.port, self.username, self.stats = host, port, username, stats
        self.next_id = 1
        self.notify = self.difficulty = None
        self.seen = set()

    async def start(self):
        """Connects, configures version rolling, subscribes, authorizes, and waits for the first job."""
        self.reader, self.writer = await asyncio.open_connection(self.host, self.port)
        self.writer.get_extra_info("socket").setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        await self.call("mining.configure", ["version-rolling"], {"version-rolling.mask": "1fffe000"})
        _, extranonce1, extranonce2_size = await self.call("mining.subscribe", "puntohunter-load/1.0")
        self.generator = ShareGenerator(extranonce1, extranonce2_size)
        assert await self.call("mining.authorize", self.username, "x") is True
        while self.notify is None:
            self.handle(json.loads(await self.reader.readline()))

    async def call(self, method, *params):
        request_id = self.send(method, *params)
        while True:
            message = json.loads(await self.reader.readline())
            if message.get("id") == request_id and "method" not in message:
                if message.get("error"):
                    raise RuntimeError(f"{method}: {message['error']}")
                return message["result"]
            self.handle(message)

    def send(self, method, *params):
        request_id, self.next_id = self.next_id, self.next_id + 1
        message = {"id": request_id, "method": method, "params": list(params)}
        self.writer.write((json.dumps(message) + "\n").encode())
        return request_id

    def handle(self, message, at=None):
        method = message.get("method")
        if method == "mining.set_difficulty":
            self.difficulty = message["params"][0]
        elif method == "mining.notify":
            self.notify = message["params"]
            if self.notify[8] and self.notify[1] not in self.seen:
                self.seen.add(self.notify[1])
                self.stats.tips[display_hash(self.notify[1])].append(at or time.time())
        elif "id" in message and method is None:
            if message.get("error"):
                self.stats.errors[str(message["error"][0])] += 1
            elif message.get("result") is True:
                self.stats.accepted += 1

    async def read(self):
        while line := await self.reader.readline():
            at = time.time()
            self.handle(json.loads(line), at)
        self.stats.closed.append(f"{self.username}: closed by the pool")

    async def mine(self, interval, offset):
        await asyncio.sleep(offset)
        loop = asyncio.get_running_loop()
        due = loop.time()
        while True:
            params = self.generator.share(self.notify, self.difficulty)
            self.send("mining.submit", self.username, *params)
            self.stats.submitted += 1
            due += interval
            await asyncio.sleep(max(0.0, due - loop.time()))


async def run(pipe, host, port, usernames, interval):
    stats = Stats()
    workers = [Worker(host, port, username, stats) for username in usernames]
    started = await asyncio.gather(
        *(asyncio.wait_for(worker.start(), SETUP_TIMEOUT) for worker in workers), return_exceptions=True
    )
    for worker, result in zip(workers, started):
        if isinstance(result, BaseException):
            stats.failed.append(f"{worker.username}: {result!r}")
    ready = [worker for worker, result in zip(workers, started) if not isinstance(result, BaseException)]
    stats.ready = len(ready)
    pipe.send({"ready": stats.ready, "failed": stats.failed})
    # Spread over the interval, so the shares do not arrive in step.
    tasks = [asyncio.create_task(worker.read()) for worker in ready] + [
        asyncio.create_task(worker.mine(interval, interval * i / len(ready))) for i, worker in enumerate(ready)
    ]
    while not pipe.poll():
        await asyncio.sleep(0.2)
    for task in tasks:
        task.cancel()
    # A worker that stopped of its own accord, as on a share it could not make, says why.
    for result in await asyncio.gather(*tasks, return_exceptions=True):
        if not isinstance(result, (asyncio.CancelledError, type(None))):
            stats.closed.append(repr(result))
    for worker in ready:
        worker.writer.close()
    pipe.send(stats.report())


def main(pipe, host, port, usernames, interval):
    asyncio.run(run(pipe, host, port, usernames, interval))


class V1Load:
    """Workers for `usernames` on `host`:`port`, in a process of their own."""

    def __init__(self, host, port, usernames, interval):
        context = multiprocessing.get_context("spawn")
        self.pipe, child = context.Pipe()
        self.process = context.Process(target=main, args=(child, host, port, usernames, interval), daemon=True)
        self.process.start()

    def started(self, timeout=SETUP_TIMEOUT + 30):
        """Waits for every worker to hold a job or fail, and returns {"ready": n, "failed": [...]}."""
        assert self.pipe.poll(timeout), "the V1 load process did not start"
        return self.pipe.recv()

    def stop(self):
        """Ends the workers and returns their report."""
        self.pipe.send("stop")
        assert self.pipe.poll(60), "the V1 load process did not report"
        report = self.pipe.recv()
        self.process.join(10)
        return report

    def kill(self):
        if self.process.is_alive():
            self.process.kill()
