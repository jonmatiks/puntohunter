"""The privacy scan: the harness's HTTP client fails any response carrying a
payout address of the run, a deliberately planted one included, in a page, a redirect, a stream, or JSON, but for
the top-level address of a fleet's own JSON resource; a worker's resource may carry none. What the binary serves
is checked in test_portal_site.py."""

import http.server
import json
import threading

import pytest

from clients.http import Http, PrivacyLeak


class Planted(http.server.BaseHTTPRequestHandler):
    """Serves a page, a redirect, and an event stream, each carrying `address` somewhere."""

    address = None

    def do_GET(self):
        shown = f'<code class="hash" data-field="address">{self.address}</code>'
        resource = {"handle": "aaaaaaaaaaaaaaaa", "address": self.address, "live": {"status": "online"}}
        planted = {
            # A resource may carry its own address at its top level, and nowhere else.
            "/api/v1/workers/aaaaaaaaaaaaaaaa": resource,
            "/api/v1/fleets/aaaaaaaaaaaaaaaa": resource,
            "/api/v1/fleets/bbbbbbbbbbbbbbbb": {**resource, "devices": [{"name": self.address}]},
            "/api/v1/workers/cccccccccccccccc": {**resource, "live": {"note": self.address.upper()}},
            "/api/v1/workers/aaaaaaaaaaaaaaaa/blocks": {"blocks": [{"address": self.address}]},
            "/api/v1/pool": {"address": self.address},
        }
        if self.path in planted:
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps(planted[self.path]).encode())
            return
        if self.path in ("/worker/aaaaaaaaaaaaaaaa", "/fleet/aaaaaaaaaaaaaaaa", "/shown"):
            self.send_response(200)
            self.end_headers()
            self.wfile.write(shown.encode())
            return
        if self.path == "/worker/bbbbbbbbbbbbbbbb":
            self.send_response(200)
            self.end_headers()
            self.wfile.write(f"{shown}<p>{self.address}</p>".encode())
            return
        if self.path == "/redirect":
            self.send_response(303)
            self.send_header("Location", f"/worker/{self.address}")
            self.end_headers()
            return
        self.send_response(200)
        if self.path == "/events":
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            self.wfile.write(b"event: live\ndata: fine\n\n")
            self.wfile.write(f"event: live\ndata: {self.address.upper()}\n\n".encode())
        else:
            self.end_headers()
            self.wfile.write(f"<p>paid to {self.address}</p>".encode())

    def log_message(self, *args):
        pass


@pytest.fixture
def planted(pool):
    address = sorted(pool.payout_addresses)[0]
    handler = type("Handler", (Planted,), {"address": address})
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield Http(server.server_address[1], pool.payout_addresses)
    server.shutdown()


def test_a_planted_address_is_caught_in_a_page_a_redirect_and_an_event(planted):
    # No page carries an address, a fleet's included: the fleet page reads its own from its JSON resource.
    for path in ("/fleet/aaaaaaaaaaaaaaaa", "/worker/aaaaaaaaaaaaaaaa", "/shown", "/worker/bbbbbbbbbbbbbbbb", "/"):
        with pytest.raises(PrivacyLeak):
            planted.get(path)
    with pytest.raises(PrivacyLeak, match="location header"):
        planted.get("/redirect")
    events = planted.events("/events")
    assert next(events)["data"] == "fine"
    with pytest.raises(PrivacyLeak):
        next(events)


def test_a_planted_address_is_caught_in_json_but_for_a_resources_own(planted, pool):
    address = sorted(pool.payout_addresses)[0]
    assert planted.api("/api/v1/fleets/aaaaaaaaaaaaaaaa").address == address
    for path in (
        "/api/v1/workers/aaaaaaaaaaaaaaaa",
        "/api/v1/fleets/bbbbbbbbbbbbbbbb",
        "/api/v1/workers/cccccccccccccccc",
        "/api/v1/workers/aaaaaaaaaaaaaaaa/blocks",
        "/api/v1/pool",
    ):
        with pytest.raises(PrivacyLeak):
            planted.api(path)
