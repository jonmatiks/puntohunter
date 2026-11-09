"""SV2 connection setup: SetupConnection negotiates within min/max version and
the supported flags, and an out-of-range version is answered with SetupConnectionError on an open socket."""

import pytest

from clients.sv2 import Sv2Client

# Mining-protocol SetupConnection flags (SV2 spec 5.3.1).
REQUIRES_STANDARD_JOBS, REQUIRES_WORK_SELECTION, REQUIRES_VERSION_ROLLING = 1, 2, 4


@pytest.fixture
def client(pool):
    client = Sv2Client()
    client.connect(pool.solo_v2_port, pin=pool.authority_key())
    yield client
    client.close()


def test_negotiates_version_2_within_a_wider_range(client):
    reply = client.setup(min_version=1, max_version=3)
    assert reply == {"event": "setup_success", "used_version": 2, "flags": 0}


def test_an_out_of_range_version_is_an_error_not_a_dropped_socket(client, miner_username):
    for low, high in ((3, 4), (1, 1)):
        reply = client.setup(min_version=low, max_version=high)
        assert reply["event"] == "setup_error"
        assert reply["error_code"] == "protocol-version-mismatch"
    # The socket is still open, and the miner may try again.
    assert client.setup()["event"] == "setup_success"
    assert client.open_extended(miner_username)["event"] == "channel_opened"


def test_refuses_another_protocol(client):
    reply = client.setup(protocol=2)
    assert (reply["event"], reply["error_code"]) == ("setup_error", "unsupported-protocol")


def test_accepts_the_flags_real_miners_send_and_names_the_ones_it_refuses(client):
    reply = client.setup(flags=REQUIRES_WORK_SELECTION | REQUIRES_VERSION_ROLLING)
    assert reply["event"] == "setup_error"
    assert reply["error_code"] == "unsupported-feature-flags"
    assert reply["flags"] == REQUIRES_WORK_SELECTION
    # ESP-Miner's flags on a standard channel.
    reply = client.setup(flags=REQUIRES_STANDARD_JOBS | REQUIRES_VERSION_ROLLING)
    assert reply == {"event": "setup_success", "used_version": 2, "flags": 0}


def test_a_refusal_is_logged_with_its_reason(pool, client):
    client.setup(min_version=3, max_version=3)
    [refused] = [line for line in pool.logs().splitlines() if "V2 SetupConnection refused" in line]
    assert "protocol-version-mismatch" in refused
