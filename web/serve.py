#!/usr/bin/env python3
"""Serves src/ on http://localhost:8000 for development, as build.py would build it: each page with its
partials and site.json's values, assembled afresh on every request, so an edit shows on reload.

--api points the site at a pool, such as one on the LAN, without editing site.json:

    python3 serve.py --api http://192.0.2.10:8080

That pool must list http://localhost:8000 in its [api] allowed_origins. --site and --overlay serve a site built
on this one, as build.py's do. Standard library only.
"""

import argparse
import http.server
import urllib.parse
from pathlib import Path

import build


class Handler(http.server.SimpleHTTPRequestHandler):
    site = None
    overlay = None

    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(build.SRC), **kwargs)

    # Nothing is cached, so a reload always shows the files as they are now.
    def end_headers(self):
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def send(self, status, content_type, body):
        if isinstance(body, str):
            body = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = urllib.parse.unquote(urllib.parse.urlsplit(self.path).path)
        if path == "/config.js":
            return self.send(200, "text/javascript; charset=utf-8", build.config(self.site))
        rel = Path(self.translate_path(path)).relative_to(build.SRC)
        if rel.parts[:1] == (build.PARTIALS.name,):
            rel = Path("missing")
        if (build.SRC / rel).is_dir() or (self.overlay is not None and (self.overlay / rel).is_dir()):
            if not path.endswith("/"):
                self.send_response(301)
                self.send_header("Location", path + "/")
                self.end_headers()
                return
            rel = rel / "index.html"
        file = build.find(rel, self.overlay)
        if not file.is_file():
            missing = build.page(build.find(Path("404.html"), self.overlay), self.site, self.overlay)
            return self.send(404, "text/html; charset=utf-8", missing)
        if file.suffix == ".html":
            return self.send(200, "text/html; charset=utf-8", build.page(file, self.site, self.overlay))
        self.send(200, self.guess_type(str(file)), file.read_bytes())


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--port", type=int, default=8000)
    parser.add_argument("--api", help="the pool's origin, overriding site.json, e.g. http://localhost:8080")
    parser.add_argument("--site", type=Path, help="the site.json to read (default web/site.json)")
    parser.add_argument("--overlay", type=Path, help="a directory laid over src/, its files replacing src/'s")
    args = parser.parse_args()
    Handler.site = build.load_site(args.api, args.site)
    Handler.overlay = args.overlay.resolve() if args.overlay else None
    server = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    api = Handler.site["api"]
    print(f"http://localhost:{args.port}" + (f", API {api}" if api else ""))
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
