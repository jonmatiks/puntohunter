"""Cross-origin access to the API: a browser page on a listed origin gets the
CORS headers, for a preflight and for a request, with no credentials; an unlisted origin and an empty list get
none, and none of the portal's files ever sends them. Every response passes the privacy scan."""

LISTED = "http://dashboard.example:8000"
PREFLIGHT = {"Access-Control-Request-Method": "GET", "Access-Control-Request-Headers": "authorization"}


def cors_headers(response):
    return {name: value for name, value in response.headers.items() if name.startswith("access-control-")}


def test_a_listed_origin_gets_the_headers_and_others_none(pool_factory):
    pool = pool_factory(overrides={"api": {"allowed_origins": [LISTED, "https://other.example"]}})
    preflight = pool.portal.request("OPTIONS", "/api/v1/pool", headers={"Origin": LISTED, **PREFLIGHT})
    assert preflight.status == 200
    headers = cors_headers(preflight)
    assert headers["access-control-allow-origin"] == LISTED
    assert {m.strip() for m in headers["access-control-allow-methods"].split(",")} == {"GET", "POST", "OPTIONS"}
    assert {h.strip() for h in headers["access-control-allow-headers"].split(",")} == {"authorization", "content-type"}
    assert headers["access-control-max-age"] == "600"
    assert "access-control-allow-credentials" not in headers, "the API uses no cookies"

    for path in ("/api/v1/pool", "/api/v1/health", "/api/v1/nothing"):
        response = pool.portal.request("GET", path, headers={"Origin": LISTED})
        headers = cors_headers(response)
        assert headers["access-control-allow-origin"] == LISTED, path
        assert headers["access-control-expose-headers"].lower() == "retry-after", path
        assert "access-control-allow-credentials" not in headers, path

    for origin in ("http://evil.example", "http://dashboard.example", "http://dashboard.example:8000.evil"):
        assert not cors_headers(pool.portal.request("GET", "/api/v1/pool", headers={"Origin": origin})), origin
        preflight = pool.portal.request("OPTIONS", "/api/v1/pool", headers={"Origin": origin, **PREFLIGHT})
        assert "access-control-allow-origin" not in cors_headers(preflight), origin

    for path in ("/", "/fleet.html", "/js/api.js", "/nothing-here"):
        assert not cors_headers(pool.portal.request("GET", path, headers={"Origin": LISTED})), path


def test_without_origins_the_api_is_same_origin_only(pool):
    assert not cors_headers(pool.portal.request("GET", "/api/v1/pool", headers={"Origin": LISTED}))
    preflight = pool.portal.request("OPTIONS", "/api/v1/pool", headers={"Origin": LISTED, **PREFLIGHT})
    assert not cors_headers(preflight)
