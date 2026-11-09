#!/usr/bin/env python3
"""Writes deploy/alerts.yaml and deploy/dashboard.json to the Grafana Cloud stack, then
reads the rules back and checks they match the file. Run it from the operator's machine after changing either
file:

    GRAFANA_URL=https://<stack>.grafana.net python3 deploy/grafana-push.py

The token is a stack service account's, with the Admin role, in GRAFANA_TOKEN_FILE (default
~/puntohunter-keys/grafana.token, mode 600); it is read from that file and never printed. The rules are written
with provenance, so the stack's UI shows them as provisioned and will not edit them: this file is where they
change. Needs PyYAML."""

import json
import os
import pathlib
import sys
import urllib.error
import urllib.request

import yaml

HERE = pathlib.Path(__file__).resolve().parent
URL = os.environ.get("GRAFANA_URL", "").rstrip("/")
TOKEN_FILE = pathlib.Path(os.environ.get("GRAFANA_TOKEN_FILE", "~/puntohunter-keys/grafana.token")).expanduser()


def call(method, path, body=None, missing_ok=False):
    request = urllib.request.Request(
        URL + path,
        data=None if body is None else json.dumps(body).encode(),
        method=method,
        headers={"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            text = response.read()
    except urllib.error.HTTPError as e:
        if missing_ok and e.code == 404:
            return None
        sys.exit(f"{method} {path}: HTTP {e.code} {e.read().decode(errors='replace')[:500]}")
    return json.loads(text) if text else None


def seconds(duration):
    """A duration such as 1m, 30s, or 2h, in seconds."""
    units = {"s": 1, "m": 60, "h": 3600}
    return int(duration[:-1]) * units[duration[-1]]


def main():
    if not URL:
        sys.exit("set GRAFANA_URL to the stack's address, such as https://<stack>.grafana.net")
    rules_file = yaml.safe_load((HERE / "alerts.yaml").read_text())
    dashboard = json.loads((HERE / "dashboard.json").read_text())

    for group in rules_file["groups"]:
        for rule in group["rules"]:
            for query in rule["data"]:
                uid = query["datasourceUid"]
                if uid != "__expr__" and call("GET", f"/api/datasources/uid/{uid}", missing_ok=True) is None:
                    sys.exit(f"rule {rule['uid']}: no data source {uid} in the stack")

    folders = {}
    for group in rules_file["groups"]:
        title = group["folder"]
        uid = title.lower()
        if uid not in folders:
            if call("GET", f"/api/folders/{uid}", missing_ok=True) is None:
                call("POST", "/api/folders", {"uid": uid, "title": title})
            folders[uid] = title
        rules = [
            {
                **{k: v for k, v in rule.items() if k != "for"},
                "for": rule.get("for", "0s"),
                "orgID": group.get("orgId", 1),
                "folderUID": uid,
                "ruleGroup": group["name"],
            }
            for rule in group["rules"]
        ]
        call(
            "PUT",
            f"/api/v1/provisioning/folder/{uid}/rule-groups/{group['name']}",
            {"title": group["name"], "folderUid": uid, "interval": seconds(group["interval"]), "rules": rules},
        )
        print(f"rule group {group['name']}: {len(rules)} rules written to folder {title}")

    call("POST", "/api/dashboards/db", {"dashboard": {**dashboard, "id": None}, "folderUid": "puntohunter",
                                        "overwrite": True, "message": "deploy/grafana-push.py"})
    print(f"dashboard {dashboard['title']} written")

    # The rules in the stack, as the file has them.
    stack = {rule["uid"]: rule for rule in call("GET", "/api/v1/provisioning/alert-rules")}
    for group in rules_file["groups"]:
        for rule in group["rules"]:
            there = stack.get(rule["uid"])
            if there is None:
                sys.exit(f"rule {rule['uid']} is missing from the stack")
            exprs = [q["model"].get("expr") for q in rule["data"]]
            if [q["model"].get("expr") for q in there["data"]] != exprs or there["title"] != rule["title"]:
                sys.exit(f"rule {rule['uid']} differs from deploy/alerts.yaml")
            if there["for"] != rule.get("for", "0s") or there["noDataState"] != rule["noDataState"]:
                sys.exit(f"rule {rule['uid']} differs from deploy/alerts.yaml in for or noDataState")
    print(f"the stack's {len(stack)} rules match deploy/alerts.yaml")


if __name__ == "__main__":
    TOKEN = TOKEN_FILE.read_text().strip()
    main()
