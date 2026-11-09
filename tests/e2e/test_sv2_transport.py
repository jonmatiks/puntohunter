"""SV2 transport: the Noise NX handshake completes and what follows is not plaintext
SV2; the certificate verifies against the key the pool prints, and a client pinning another refuses it; the
key survives a restart; hostile input fails only its own connection."""

import socket
import stat
import threading

import pytest

from clients.sv2 import Sv2Client, Sv2Error
from wait import wait_until

# The initiator's first handshake message: an ElligatorSwift-encoded ephemeral key.
FIRST_MESSAGE = 64
# SetupConnection's message type, and SRI's plaintext frame header: extension type, message type, length.
SETUP_CONNECTION = 0x00
HEADER = 6
# The warning a close between the handshake and SetupConnection logs.
UNTRUSTED = "may not trust the pool's authority key"


class Relay:
    """Forwards one connection to the pool, recording the bytes each way as they cross the wire."""

    def __init__(self, port):
        self.upstream = port
        self.listener = socket.create_server(("127.0.0.1", 0))
        self.port = self.listener.getsockname()[1]
        self.sent, self.received = bytearray(), bytearray()
        threading.Thread(target=self._run, daemon=True).start()

    def _run(self):
        client, _ = self.listener.accept()
        pool = socket.create_connection(("127.0.0.1", self.upstream))
        for source, sink, record in ((client, pool, self.sent), (pool, client, self.received)):
            threading.Thread(target=self._pipe, args=(source, sink, record), daemon=True).start()

    @staticmethod
    def _pipe(source, sink, record):
        while data := source.recv(65536):
            record.extend(data)
            sink.sendall(data)
        sink.close()


@pytest.fixture
def clients():
    started = []

    def start():
        client = Sv2Client()
        started.append(client)
        return client

    yield start
    for client in started:
        client.close()


def looks_like_plaintext_setup(data):
    """Whether `data` starts with SRI's plaintext SetupConnection frame."""
    if len(data) < HEADER:
        return False
    extension, message_type = int.from_bytes(data[0:2], "little"), data[2]
    length = int.from_bytes(data[3:6], "little")
    return extension == 0 and message_type == SETUP_CONNECTION and length == len(data) - HEADER


def test_the_handshake_completes_and_the_wire_is_not_plaintext(pool, clients, miner_username):
    relay = Relay(pool.solo_v2_port)
    client = clients()
    client.connect(relay.port, pin=pool.authority_key())
    assert client.setup(vendor="plaintext-canary")["event"] == "setup_success"
    assert client.open_extended(miner_username)["event"] == "channel_opened"

    after_handshake = bytes(relay.sent[FIRST_MESSAGE:])
    assert after_handshake, "the client sent frames after the handshake"
    assert not looks_like_plaintext_setup(after_handshake)
    for canary in (b"plaintext-canary", miner_username.encode()):
        assert canary not in relay.sent
    tag = pool.config["coinbase"]["tag"].encode()
    assert tag not in relay.received, "the coinbase tag travels encrypted"


def test_the_certificate_verifies_against_the_printed_key_and_only_that_key(pool, clients):
    assert clients().connect(pool.solo_v2_port, pin=pool.authority_key())["event"] == "connected"

    # Any other valid key, here the spec's own example (SV2 spec 4.7.1).
    other = "9bXiEd8boQVhq7WddEcERUL5tyyJVFYdU8th3HfbNXK3Yw6GRXh"
    with pytest.raises(Sv2Error, match="handshake failed"):
        clients().connect(pool.solo_v2_port, pin=other)
    # The pool's side of the handshake completed, so all it sees is the miner hanging up before
    # SetupConnection, which it names as a likely key mismatch.
    wait_until(lambda: UNTRUSTED in pool.logs(), 10, "the warning")
    [warning] = [line for line in pool.logs().splitlines() if UNTRUSTED in line]
    assert "WARN" in warning


def test_a_miner_that_set_up_hangs_up_without_the_key_warning(pool, clients):
    client = clients()
    client.connect(pool.solo_v2_port, pin=pool.authority_key())
    client.setup()
    client.command("close")
    wait_until(lambda: "V2 disconnected" in pool.logs(), 10, "the disconnect")
    assert UNTRUSTED not in pool.logs()


def test_both_listeners_speak_v2(pool, clients):
    for port in (pool.solo_v2_port, pool.pooled_v2_port):
        client = clients()
        client.connect(port, pin=pool.authority_key())
        assert client.setup()["event"] == "setup_success"


def test_the_authority_key_survives_a_restart(pool):
    key = pool.authority_key()
    key_file = pool.data_dir / "sv2-authority.key"
    assert stat.S_IMODE(key_file.stat().st_mode) == 0o600
    pool.restart()
    assert pool.logs().count("authority_key=") == 2
    assert pool.authority_key() == key


def test_a_malformed_frame_fails_only_its_own_connection(pool, clients, miner_username):
    good = clients()
    good.connect(pool.solo_v2_port, pin=pool.authority_key())
    good.setup()

    # A frame that decrypts but holds no SetupConnection.
    garbled = clients()
    garbled.connect(pool.solo_v2_port, pin=pool.authority_key())
    garbled.command("raw_frame", msg_type=SETUP_CONNECTION, payload="ff" * 8)
    assert garbled.closed_by_pool()

    # Bytes that do not decrypt at all.
    noise = clients()
    noise.connect(pool.solo_v2_port, pin=pool.authority_key())
    noise.command("raw_bytes", hex="00" * 64)
    assert noise.closed_by_pool()

    assert good.alive()
    assert good.open_extended(miner_username)["event"] == "channel_opened"


def test_a_v1_miner_or_a_truncated_handshake_fails_only_its_own_connection(pool, clients):
    good = clients()
    good.connect(pool.solo_v2_port, pin=pool.authority_key())
    good.setup()

    # A V1 miner on the V2 port. Any 64 bytes decode as an ElligatorSwift key, so the pool answers the
    # handshake; what follows does not decrypt, and the pool closes.
    with socket.create_connection(("127.0.0.1", pool.solo_v2_port), timeout=10) as v1:
        v1.sendall(b'{"id":1,"method":"mining.subscribe","params":[]}\n'.ljust(128, b" "))
        while v1.recv(4096):
            pass

    truncated = clients()
    assert truncated.connect(pool.solo_v2_port, truncate_handshake=30)["event"] == "truncated"
    truncated.command("close")

    assert good.alive()


def test_a_stalled_handshake_is_closed_at_the_open_deadline(pool_factory, clients):
    pool = pool_factory(overrides={"stratum": {"authorize_timeout_secs": 1.0}})
    stalled = clients()
    stalled.connect(pool.solo_v2_port, truncate_handshake=30)
    assert stalled.closed_by_pool(timeout=10)
    assert "no handshake within" in pool.logs()
