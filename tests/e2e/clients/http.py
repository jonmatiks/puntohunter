"""The HTTP client every test fetches the pool through: the portal's files, JSON, and SSE, and /metrics. It
fails any response that contains a payout address of the run, headers included, so the privacy scan
covers every response a test touches without being asked. The one exception is the top-level
`address` key of a fleet's own JSON resource; anywhere else, on any other path, a
worker's included, in a page, or in a stream, an address still fails. A fleet secret a test adds to `secrets`
fails anywhere.

Redirects are never followed for the caller: a redirect is a response of its own, scanned like any other, and
`follow` fetches its target through the same scan."""

import http.client
import json
import re
import socket
from dataclasses import dataclass
from urllib.parse import urlencode

TIMEOUT = 10
# The JSON resources that may carry their own address, in their top-level `address` key.
ADDRESS_RESOURCE = re.compile(r"/api/v1/fleets/[a-z2-7]{16}")


def short(address):
    """`address` as a fleet page shows it: its first and last 8 characters, bc1qw508…7kv8f3t4."""
    return address if len(address) <= 17 else f"{address[:8]}…{address[-8:]}"


def listed(address):
    """`address` as the stats lists show it: its first 6 and last 4 characters, bc1qw5…f3t4."""
    return f"{address[:6]}…{address[-4:]}"


class PrivacyLeak(AssertionError):
    pass


@dataclass
class Response:
    status: int
    headers: dict
    # An image's bytes are decoded as Latin-1, one character a byte, so the scan still reads all of them.
    body: str
    # The address a fleet's own JSON resource carries in its top-level `address` key.
    address: str | None = None

    def header(self, name):
        return self.headers.get(name.lower())

    @property
    def json(self):
        """The body as JSON."""
        return json.loads(self.body)


class Http:
    def __init__(self, port, addresses, host="127.0.0.1"):
        self.host, self.port = host, port
        # Compared case-insensitively: an upper-case bech32 address is the same address.
        self.addresses = sorted({a.lower() for a in addresses})
        assert self.addresses, "nothing to scan for"
        # Fleet secrets a test set on its devices, which no response may carry.
        self.secrets = []

    def scan(self, text, where):
        lowered = text.lower()
        for address in self.addresses:
            if address in lowered:
                raise PrivacyLeak(f"payout address {address} in {where}")
        for secret in self.secrets:
            if secret in text:
                raise PrivacyLeak(f"a fleet secret in {where}")

    def request(self, method, path, body=None, headers=None):
        connection = http.client.HTTPConnection(self.host, self.port, timeout=TIMEOUT)
        try:
            connection.request(method, path, body, headers or {})
            response = connection.getresponse()
            headers = {k.lower(): v for k, v in response.getheaders()}
            raw = response.read()
            image = headers.get("content-type", "").startswith("image/")
            result = Response(response.status, headers, raw.decode("latin-1" if image else "utf-8"))
        finally:
            connection.close()
        where = f"{method} {path}"
        for name, value in result.headers.items():
            self.scan(value, f"the {name} header of {where}")
        if method == "GET" and ADDRESS_RESOURCE.fullmatch(path) and result.status == 200:
            resource = json.loads(result.body)
            result.address = resource.pop("address", None)
            # The rest of the resource, its keys included, may carry no address.
            self.scan(json.dumps(resource), where)
            self.scan(result.body.replace(json.dumps(result.address), "", 1), where)
            return result
        self.scan(result.body, where)
        return result

    def get(self, path):
        return self.request("GET", path)

    def post(self, path, form):
        """POSTs `form` form-encoded, as a browser submits a form."""
        return self.request(
            "POST",
            path,
            urlencode(form),
            {"Content-Type": "application/x-www-form-urlencoded"},
        )

    def api(self, path, method="GET", body=None, headers=None):
        """A JSON API call, `body` sent as JSON: the response, scanned as any other, with its JSON body checked
        to parse."""
        headers = dict(headers or {})
        if body is not None:
            body = body if isinstance(body, str) else json.dumps(body)
            headers.setdefault("Content-Type", "application/json")
        response = self.request(method, path, body, headers)
        assert response.header("content-type") == "application/json", (path, response.headers)
        response.json  # noqa: B018 - it parses
        return response

    def follow(self, response):
        """The page a redirect points to."""
        assert response.status in (301, 302, 303, 307, 308), response.status
        return self.get(response.header("location"))

    def events(self, path, timeout=TIMEOUT, headers=None):
        """The server-sent events of `path`, each as {"event": name, "data": text}, scanned as they arrive.
        Reading stops when the caller stops iterating, or raises TimeoutError once `timeout` seconds pass
        without an event."""
        connection = http.client.HTTPConnection(self.host, self.port, timeout=timeout)
        try:
            connection.request("GET", path, headers={"Accept": "text/event-stream", **(headers or {})})
            response = connection.getresponse()
            for name, value in response.getheaders():
                self.scan(value, f"the {name} header of GET {path}")
            assert response.status == 200, response.status
            event, data = "message", []
            while True:
                try:
                    line = response.fp.readline()
                except socket.timeout as e:
                    raise TimeoutError(f"no event on {path} within {timeout}s") from e
                if not line:
                    return
                line = line.decode().rstrip("\n")
                self.scan(line, f"an event of GET {path}")
                if line == "":
                    if data:
                        yield {"event": event, "data": "\n".join(data)}
                    event, data = "message", []
                elif line.startswith("event:"):
                    event = line[6:].strip()
                elif line.startswith("data:"):
                    data.append(line[5:].removeprefix(" "))
        finally:
            connection.close()
