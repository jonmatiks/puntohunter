"""A thin driver over the sv2-testclient subprocess: one JSON command per line in, one reply out."""

import json
import subprocess

from regtest import ROOT

BINARY = ROOT / "target" / "release" / "sv2-testclient"
# Answers come within the client's own timeouts; this bounds a client that hung outright.
REPLY_TIMEOUT = 60


class Sv2Error(Exception):
    def __init__(self, reply):
        super().__init__(reply.get("reason", reply))
        self.reply = reply


class Sv2Client:
    def __init__(self):
        self.process = subprocess.Popen(
            [BINARY],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            bufsize=1,
        )

    def close(self):
        if self.process.poll() is None:
            self.process.stdin.close()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()

    def command(self, cmd, **args):
        """Sends a command and returns its reply, raising Sv2Error on an error reply."""
        self.send(cmd, **args)
        return self.reply()

    def send(self, cmd, **args):
        self.process.stdin.write(json.dumps({"cmd": cmd, **args}) + "\n")
        self.process.stdin.flush()

    def reply(self):
        """The reply to the command sent last, raising Sv2Error on an error reply."""
        line = self.process.stdout.readline()
        if not line:
            raise Sv2Error({"reason": f"sv2-testclient exited with {self.process.wait()}"})
        reply = json.loads(line)
        if reply["event"] == "error":
            raise Sv2Error(reply)
        return reply

    def connect(self, port, pin=None, **args):
        return self.command("connect", port=port, pin=pin, **args)

    def setup(self, **args):
        return self.command("setup", **args)

    def open_extended(self, user, **args):
        return self.command("open_extended_channel", user=user, **args)

    def open_standard(self, user, **args):
        return self.command("open_standard_channel", user=user, **args)

    def job(self, channel_id, **args):
        """The channel's active job, waiting for one; `after` asks for one newer than that job id."""
        return self.command("job", channel_id=channel_id, **args)

    def submit(self, channel_id, count=1, kind="valid", **args):
        """Grinds and sends `count` shares of `kind` in one write, and returns once each is answered."""
        return self.command("submit", channel_id=channel_id, count=count, kind=kind, **args)

    def wait(self, message, **args):
        """The next message named `message` the pool sent."""
        return self.command("wait", message=message, **args)

    def messages(self):
        return self.command("messages")["messages"]

    def closed_by_pool(self, timeout=10):
        return self.command("closed", timeout=timeout)["closed"]

    def alive(self):
        """Whether the pool still answers a connection that is set up: it must refuse a channel open for a
        username that is not an address, and keep the socket."""
        try:
            reply = self.command("open_extended_channel", user="not-an-address", timeout=5)
        except Sv2Error:
            return False
        return reply["event"] == "channel_error"


class Sv2Miner:
    """An SV2 client that has connected with the pool's key pinned, set up, and opened a channel, holding
    its first job."""

    def __init__(self, pool, port, username, standard=False, vendor=None, source=None, **open_args):
        self.client = Sv2Client()
        self.client.connect(port, pin=pool.authority_key(), **({"source": source} if source else {}))
        self.client.setup(**({"vendor": vendor} if vendor else {}))
        opener = self.client.open_standard if standard else self.client.open_extended
        self.channel = opener(username, **open_args)
        assert self.channel["event"] == "channel_opened", self.channel
        self.channel_id = self.channel["channel_id"]
        self.job = self.client.job(self.channel_id)
        # The opening job's messages are read; waits start from what comes next.
        self.client.messages()

    def submit(self, count=1, kind="valid", **args):
        return self.client.submit(self.channel_id, count, kind, **args)

    def next_job(self, **args):
        self.job = self.client.job(self.channel_id, after=self.job["job_id"], **args)
        return self.job

    def close(self):
        self.client.close()


class Sv2Load:
    """Sessions for `usernames` in one sv2-testclient process, each sending a valid share every `interval`,
    on standard channels where `standard`, else extended ones. The process starts them at once;
    `started` waits for them."""

    def __init__(self, host, port, pin, usernames, interval, standard=False):
        self.client = Sv2Client()
        self.client.send(
            "load", host=host, port=port, pin=pin, users=usernames, interval=interval, standard=standard,
            timeout=60,
        )

    def started(self):
        """{"ready": n, "failed": [...]} once every session holds a job or has failed."""
        return self.client.reply()

    def stop(self):
        """Ends the sessions and returns their report."""
        report = self.client.command("report")
        self.client.command("stop")
        self.client.close()
        return report

    def kill(self):
        if self.client.process.poll() is None:
            self.client.process.kill()
