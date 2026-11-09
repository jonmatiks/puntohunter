"""The portal's site: the binary serves the reference web app,
web/src/, assembled at startup by web/build.py's rules, so each file it serves is the one build.py writes for the
same site and overlay, byte for byte, and `/` is the overview. Each goes with what a page's meta tags cannot
carry; a path that names no file gets the app's 404 page, and a method other than GET or HEAD is refused."""

import importlib.util
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
# What web/hosting/ adds on a static host, which the binary adds to every file.
HEADERS = {
    "content-security-policy": "frame-ancestors 'none'",
    "referrer-policy": "no-referrer",
    "x-content-type-options": "nosniff",
    "permissions-policy": "camera=(), microphone=(), geolocation=()",
}


def build_py():
    """web/build.py, loaded from its path."""
    spec = importlib.util.spec_from_file_location("web_build", ROOT / "web" / "build.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def built(out, site=None, overlay=None):
    """What build.py writes for `site` and `overlay` with the API on the site's own origin, as the binary always
    has it: {path: bytes}."""
    build = build_py()
    build.build(out, build.load_site(api="", path=site), overlay=overlay)
    return {path.relative_to(out).as_posix(): path.read_bytes() for path in out.rglob("*") if path.is_file()}


def raw(response):
    """A response's body as the bytes the binary sent: the harness decodes an image as Latin-1, anything else as
    UTF-8."""
    image = response.header("content-type").startswith("image/")
    return response.body.encode("latin-1" if image else "utf-8")


def assert_serves(pool, files):
    assert "index.html" in files and "404.html" in files and "config.js" in files
    for path, data in files.items():
        response = pool.portal.get(f"/{path}")
        assert response.status == 200, path
        assert raw(response) == data, f"/{path} differs from build.py's"
    assert raw(pool.portal.get("/")) == files["index.html"], "/ is the overview"
    assert not any(path.startswith("partials/") for path in files)
    assert pool.portal.get("/partials/header.html").status == 404


def test_each_file_is_the_one_build_py_writes(pool, tmp_path):
    files = built(tmp_path / "dist")
    assert_serves(pool, files)
    assert b"<title>PuntoHunter</title>" in files["index.html"]
    assert files["config.js"].endswith(b'export const API = "";\n')


def test_a_branded_site_is_the_one_build_py_writes_for_it(pool_factory, tmp_path):
    site = tmp_path / "site.json"
    site.write_text(
        json.dumps(
            {
                "name": 'Pool & "Co"',
                "title": "Home of a pool",
                "description": "Mining <b>here</b>",
                "origin": "https://pool.example.com/",
                # Ignored: the binary's pages call its own API.
                "api": "https://api.example.com",
            }
        )
    )
    overlay = tmp_path / "overlay"
    (overlay / "partials").mkdir(parents=True)
    (overlay / "data").mkdir()
    (overlay / "logo.png").write_bytes(b"\x89PNG\r\n\x1a\n" + bytes(range(256)))
    (overlay / "app.css").write_text(":root { --accent: #c0ffee; }\n")
    (overlay / "partials" / "intro.html").write_text('<p class="intro">Welcome to {{name}}.</p>\n')
    (overlay / "about-us.html").write_text(
        "<!doctype html>\n<html>\n<head>\n<!--#include head.html -->\n<title>About · {{name}}</title>\n"
        "</head>\n<body>\n<!--#include header.html -->\n<!--#include footer.html -->\n</body>\n</html>\n"
    )
    (overlay / "data" / "extra.json").write_text('{"extra": true}\n')
    pool = pool_factory(overrides={"portal": {"site": str(site), "overlay": str(overlay)}})
    files = built(tmp_path / "dist", site, overlay)
    assert_serves(pool, files)
    index = files["index.html"].decode()
    assert "<title>Home of a pool</title>" in index
    assert '<p class="intro">Welcome to Pool &amp; &quot;Co&quot;.</p>' in index
    assert '<link rel="canonical" href="https://pool.example.com/">' in index
    assert "api.example.com" not in index and "api.example.com" not in files["config.js"].decode()
    assert pool.portal.get("/data/extra.json").header("content-type") == "application/json"


def test_each_file_goes_with_its_headers_and_a_missing_one_with_the_404_page(pool, tmp_path):
    files = built(tmp_path / "dist")
    for path, content_type, cache in [
        ("/", "text/html; charset=utf-8", "no-cache"),
        ("/fleet.html", "text/html; charset=utf-8", "no-cache"),
        ("/js/api.js", "text/javascript; charset=utf-8", "no-cache"),
        ("/app.css", "text/css; charset=utf-8", "no-cache"),
        ("/logo.png", "image/png", "no-cache"),
        ("/vendor/uPlot-1.6.32.iife.min.js", "text/javascript; charset=utf-8", "public, max-age=31536000, immutable"),
        ("/no/such/page", "text/html; charset=utf-8", "no-cache"),
    ]:
        response = pool.portal.get(path)
        assert response.header("content-type") == content_type, path
        assert response.header("cache-control") == cache, path
        for name, value in HEADERS.items():
            assert response.header(name) == value, (path, name)
    missing = pool.portal.get("/no/such/page")
    assert missing.status == 404 and raw(missing) == files["404.html"]
    head = pool.portal.request("HEAD", "/fleet.html")
    assert head.status == 200 and head.body == "" and head.header("content-type") == "text/html; charset=utf-8"
    for path in ("/", "/index.html", "/lookup", "/balance"):
        assert pool.portal.request("POST", path, "", {"Content-Length": "0"}).status == 405, path
    # The server-rendered pages are gone, and nothing redirects their addresses.
    for path in ("/fleet/aaaaaaaaaaaaaaaa", "/worker/aaaaaaaaaaaaaaaa", "/fleet/aaaaaaaaaaaaaaaa/events", "/stats",
                 "/blocks", "/balance", "/balance.json", "/assets/portal.css"):
        missing = pool.portal.get(path)
        assert missing.status == 404 and raw(missing) == files["404.html"], path
    api = pool.portal.api("/api/v1/nothing")
    assert api.status == 404 and api.json["error"] == "not_found"
