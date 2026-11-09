#!/usr/bin/env python3
"""Builds the site from src/ into dist/: plain files that any static host serves as they are.

    python3 build.py
    python3 build.py --api https://pool.example.com --host cloudflare
    python3 build.py --site ../site/site.json --overlay ../site/overlay

In each page, <!--#include NAME --> takes the file src/partials/NAME, and {{key}} a value from site.json:

    name         the pool's name, in the header and the titles
    title        optional: the main page's title, in place of the name
    description  the pages' meta description
    api          the origin of the pool's API, such as https://pool.example.com, with no path and no trailing
                 slash; empty for this site's own origin, where the host passes /api/v1 through to the pool
    origin       optional: the site's own origin, such as https://pool.example.com; each page that search
                 engines may index then gets its canonical link

build.py writes the API origin into config.js and into each page's Content-Security-Policy. --host adds the
files a host reads from the site itself, such as Cloudflare's _headers. --overlay lays a directory over src/:
its file replaces src/'s of the same path, partials included, and a file src/ lacks is added. --site reads
another site.json. Standard library only.
"""

import argparse
import html
import json
import re
import shutil
from pathlib import Path

ROOT = Path(__file__).resolve().parent
SRC = ROOT / "src"
PARTIALS = SRC / "partials"
# The line break after an include goes with it when the partial is empty, so an unused hook leaves no blank line.
INCLUDE = re.compile(r"<!--#include\s+([\w.-]+)\s*-->(\n?)")
PLACEHOLDER = re.compile(r"\{\{(\w+)\}\}")
ORIGIN = re.compile(r"^https?://(\[[0-9A-Fa-f:]+\]|[A-Za-z0-9.-]+)(:\d+)?$")
# The first line of the config.js a build writes, by which a later build knows its output is safe to replace.
STAMP = "// Written by build.py from site.json; edit that, not this."
# Files a host reads from the site's own root, by host.
HOST_FILES = {"cloudflare": ["_headers"]}
# Hosts that serve a page without its .html, as /fleet for fleet.html, and redirect the .html address there.
CLEAN_URLS = {"cloudflare"}
# A page that carries this is left out of search, so it gets no canonical link.
NOINDEX = '<meta name="robots" content="noindex">'


def load_site(api=None, path=None):
    site = json.loads((path or ROOT / "site.json").read_text())
    if api is not None:
        site["api"] = api
    for key in ("api", "origin"):
        site[key] = site.get(key, "").rstrip("/")
        if site[key] and not ORIGIN.match(site[key]):
            raise SystemExit(f"error: {key} must be an origin such as https://pool.example.com, not {site[key]!r}")
    return site


def values(site):
    return {
        "name": site["name"],
        "title": site.get("title") or site["name"],
        "description": site["description"],
        "connect_src": " ".join(["'self'"] + ([site["api"]] if site["api"] else [])),
    }


def find(rel, overlay=None):
    """The file at `rel` in src/, or the overlay's file of that path, which replaces it."""
    if overlay is not None and (overlay / rel).is_file():
        return overlay / rel
    return SRC / rel


def shown(path):
    return path.relative_to(ROOT) if path.is_relative_to(ROOT) else path


def escape(text):
    return html.escape(text, quote=False).replace('"', "&quot;")


def included(partial, match):
    text = partial.read_text()
    return text + match.group(2) if text else ""


def page(path, site, overlay=None):
    """The page at `path`, with its partials included and its {{keys}} filled in."""
    text = path.read_text()
    for _ in range(5):
        text, count = INCLUDE.subn(lambda m: included(find(Path("partials", m.group(1)), overlay), m), text)
        if not count:
            break
    known = values(site)

    def fill(match):
        if match.group(1) not in known:
            raise SystemExit(f"error: {shown(path)}: nothing in site.json for {match.group(0)}")
        return escape(known[match.group(1)])

    return PLACEHOLDER.sub(fill, text)


def canonical(text, rel, site, host=None):
    """`text` with its canonical link, when site.json names the origin and search engines may index the page."""
    if not site["origin"] or rel == Path("404.html") or NOINDEX in text:
        return text
    address = "/" + rel.as_posix()
    if address.endswith("/index.html"):
        address = address.removesuffix("index.html")
    elif host in CLEAN_URLS:
        address = address.removesuffix(".html")
    link = f'<link rel="canonical" href="{escape(site["origin"] + address)}">\n'
    return text.replace("</head>", link + "</head>", 1)


def config(site):
    return (
        f"{STAMP}\n"
        "// Where the PuntoHunter pool serves its API: the origin in front of /api/v1. Empty means this site's own\n"
        "// origin, where the host passes /api/v1 through to the pool.\n"
        f"export const API = {json.dumps(site['api'])};\n"
    )


def build(out, site, host=None, overlay=None):
    if out.exists() and any(out.iterdir()):
        stamp = out / "config.js"
        if not (stamp.is_file() and stamp.read_text().startswith(STAMP)):
            raise SystemExit(f"error: {out} holds files build.py did not write; name an empty or new directory")
        shutil.rmtree(out)
    files = {source.relative_to(SRC) for source in SRC.rglob("*") if source.is_file()}
    if overlay is not None:
        files |= {source.relative_to(overlay) for source in overlay.rglob("*") if source.is_file()}
    for rel in sorted(files):
        if rel.parts[0] == PARTIALS.name:
            continue
        source = find(rel, overlay)
        target = out / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        if source.suffix == ".html":
            target.write_text(canonical(page(source, site, overlay), rel, site, host))
        else:
            shutil.copy2(source, target)
    (out / "config.js").write_text(config(site))
    for name in HOST_FILES.get(host, []):
        shutil.copy2(ROOT / "hosting" / host / name, out / name)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--out", type=Path, default=ROOT / "dist", help="where to write the site (default dist/)")
    parser.add_argument("--api", help="the pool's API origin, overriding site.json")
    parser.add_argument("--host", choices=sorted(HOST_FILES), help="add the files this host reads from the site")
    parser.add_argument("--site", type=Path, help="the site.json to read (default web/site.json)")
    parser.add_argument("--overlay", type=Path, help="a directory laid over src/, its files replacing src/'s")
    args = parser.parse_args()
    site = load_site(args.api, args.site)
    overlay = args.overlay.resolve() if args.overlay else None
    if overlay is not None and not overlay.is_dir():
        raise SystemExit(f"error: --overlay {args.overlay} is not a directory")
    build(args.out.resolve(), site, args.host, overlay)
    print(f"Built {args.out}" + (f", API {site['api']}" if site["api"] else ", API on its own origin"))


if __name__ == "__main__":
    main()
