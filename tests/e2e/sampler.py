"""Samples a pool under load once a second for test_load.py, beside the pool: run locally, or on the pool's
host over SSH as `python3 - --config <path>` with this file on standard input. It reads the pool's config,
prints one JSON line naming the pool's process and SV2 authority key, then one JSON line per sample, until it
is killed. It imports nothing outside the standard library, so it runs on the host as it is."""

import argparse
import json
import os
import sqlite3
import time
import tomllib
import urllib.request

ROLLUP_LOOKBACK = 600


def pid_of(config):
    """The process serving `config`."""
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/cmdline", "rb") as f:
                args = f.read().split(b"\0")
        except OSError:
            continue
        if b"serve" in args and config.encode() in args:
            return int(entry)
    raise SystemExit(f"no process serves {config}")


def get(url):
    with urllib.request.urlopen(url, timeout=5) as response:
        return response.read().decode()


def rss(pid):
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
    return 0


def cpu_seconds(pid):
    """The process's user and system time."""
    with open(f"/proc/{pid}/stat") as f:
        fields = f.read().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def host_ticks():
    """The host's CPU ticks in all and those stolen by the hypervisor, since boot."""
    with open("/proc/stat") as f:
        fields = [int(n) for n in f.readline().split()[1:]]
    return sum(fields[:8]), fields[7]


def figures(scrape):
    """What the load suite reads from a metrics scrape: shares by pool and by refusal class, the share response
    histogram summed over its labels, the journal's oldest unsynced commit, the connected workers, and the tip
the work builds on."""
    out = {"accepted": {}, "rejected": {}, "response": {}, "unsynced": 0.0, "workers": 0, "tip": None}
    for line in scrape.splitlines():
        if line.startswith("#") or not line:
            continue
        series, value = line.rsplit(" ", 1)
        name, _, labels = series.partition("{")
        labels = dict(
            part.split("=", 1) for part in labels.rstrip("}").replace('"', "").split(",") if "=" in part
        )
        value = float(value)
        if name == "puntohunter_shares_accepted_total":
            out["accepted"][labels["pool"]] = out["accepted"].get(labels["pool"], 0) + value
        elif name == "puntohunter_shares_rejected_total":
            out["rejected"][labels["class"]] = out["rejected"].get(labels["class"], 0) + value
        elif name == "puntohunter_share_response_seconds_bucket":
            out["response"][labels["le"]] = out["response"].get(labels["le"], 0) + value
        elif name == "puntohunter_journal_unsynced_seconds":
            out["unsynced"] = value
        elif name == "puntohunter_workers_connected":
            out["workers"] += value
        elif name == "puntohunter_tip":
            out["tip"] = labels["hash"]
    return out


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", required=True)
    config_path = parser.parse_args().config
    with open(config_path, "rb") as f:
        config = tomllib.load(f)
    pid = pid_of(config_path)
    metrics = f"http://{config['metrics']['bind']}/metrics"
    pool = json.loads(get(f"http://{config['portal']['bind']}/api/v1/pool"))
    print(json.dumps({"pid": pid, "authority_key": pool["authority_key"]}), flush=True)
    db = sqlite3.connect(f"file:{config['data_dir']}/puntohunter.sqlite3?mode=ro", uri=True)
    while True:
        at = time.time()
        total, steal = host_ticks()
        sample = {
            "at": at,
            "rss": rss(pid),
            "cpu": cpu_seconds(pid),
            "ticks": total,
            "steal": steal,
            "journal": db.execute("SELECT coalesce(max(sequence), 0) FROM journal").fetchone()[0],
            "rollups": dict(
                db.execute(
                    "SELECT start, count(*) FROM rollups WHERE start >= ? GROUP BY start",
                    (int(at) - ROLLUP_LOOKBACK,),
                ).fetchall()
            ),
            **figures(get(metrics)),
        }
        print(json.dumps(sample), flush=True)
        time.sleep(max(0.0, at + 1 - time.time()))


if __name__ == "__main__":
    main()
