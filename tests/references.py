#!/usr/bin/env python3
"""The references check: fails when a tracked file cites a document outside the repository, names a host of
its own, or carries an address that is not a reserved one.

The repository stands on its own (AGENTS.md): no comment, test, config, or page cites a spec, plan, proposal,
milestone, or task kept elsewhere, and tests and default configs use example.com and the documentation ranges
(RFC 5737) instead of real hosts and addresses. The rules here are generic, so the check names nothing it
guards. More patterns, one regular expression a line, can come from the REFERENCES_DENY environment variable,
which CI fills from a secret.

Run from the repository root: `python3 tests/references.py`. It prints each match and exits 1 on any.
"""

import ipaddress
import os
import re
import subprocess
import sys

SKIP = re.compile(r"^(Cargo\.lock|web/src/vendor/|tests/references\.py$)")

CITATIONS = [
    r"docs/(specs|plans|proposals|business)/",
    r"\b(engine|deployment) spec\b",
    r"\bspecs? (section )?[A-D]\.\d",
    r"\(spec\b(?!\))",
    r"\bplan,? sections?\b",
    r"\bproposal (section|decision)",
    r"\bpools design\b",
    r"\b(hybrid-mode|portal-api|engine|deployment)\.md\b",
    r"\b[Mm]ilestone \d",
    r"\bM\d{1,2}\.(\d+|[A-Z])\b",
    r"\btask M\d",
    r"\([A-D]\.\d+[),;]",
]

HOSTS = [
    # Subdomains of the project's own domain name hosts of its own; examples use example.com.
    r"\b[a-z0-9-]+\.puntohunter\.com\b",
    # A monitoring account's endpoint.
    r"\b[a-z0-9-]+\.grafana\.net\b",
]

IPV4 = re.compile(r"(?<![\w.])(\d{1,3}(?:\.\d{1,3}){3})(?![\w.]*\d)")
ALLOWED = [
    ipaddress.ip_network(n)
    for n in ("0.0.0.0/32", "127.0.0.0/8", "192.0.2.0/24", "198.51.100.0/24", "203.0.113.0/24")
]


def patterns():
    extra = [line.strip() for line in os.environ.get("REFERENCES_DENY", "").splitlines() if line.strip()]
    return [re.compile(p) for p in CITATIONS + HOSTS + extra]


# A comment or docstring line's leading marker, dropped where a citation runs on from the line before.
LEAD = re.compile(r"^\s*(//[/!]?|#|--|\*)?\s*")


def reserved(text):
    try:
        address = ipaddress.ip_address(text)
    except ValueError:
        return True  # not an address, such as a version number
    return any(address in network for network in ALLOWED)


def main():
    files = subprocess.run(["git", "ls-files", "-z"], capture_output=True, check=True).stdout.decode().split("\0")
    rules = patterns()
    found = 0
    for path in files:
        if not path or SKIP.search(path) or not os.path.isfile(path):
            continue
        try:
            lines = open(path, encoding="utf-8").read().splitlines()
        except UnicodeDecodeError:
            continue
        for number, line in enumerate(lines, 1):
            hits = [m.group(0) for rule in rules for m in rule.finditer(line)]
            hits += [m.group(1) for m in IPV4.finditer(line) if not reserved(m.group(1))]
            # A citation split across this line and the next, such as "(spec" and "C.2)".
            if number < len(lines):
                joined = line + " " + LEAD.sub("", lines[number])
                hits += [
                    m.group(0)
                    for rule in rules
                    for m in rule.finditer(joined)
                    if m.start() < len(line) < m.end()
                ]
            for hit in hits:
                print(f"{path}:{number}: {hit}")
                found += 1
    if found:
        print(f"{found} references to remove: cite nothing outside the repository, and use example.com and "
              "the documentation ranges", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
