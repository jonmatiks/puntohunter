#!/usr/bin/env python3
"""The acceptance reports, run by hand over the pool of a live run and by
the e2e suite over its own pools. Each takes the running pool's config, prints what it checked, and exits 1
naming each difference or leak it found.

`reports.py conservation --config <config>` reads the pool's database and its node. For each pooled block it
checks the coinbase's miner portion, and for each hybrid block its pool output, against what the books
recorded, the block's allocations, and the ledger's credits; then the allocations of the blocks on the chain
against each address's paid and unpaid balances, and each payout batch's transaction against the balances it
reserved and the fee it took from the fee reserve.

`reports.py privacy --config <config> [--token <balance token>]...` fetches every API route, stream, and series
of the running pool, and its metrics, through the harness's scanning client (clients/http.py), with the payout
addresses of the config and of every miner in the database. A token opens its balance too.

Run both from the directory the pool runs in, as its data_dir is relative to it; `--database` reads another
copy of the database instead.
"""

import argparse
import contextlib
import pathlib
import sqlite3
import sys
import tomllib

from chain import Chain, RpcError, sats
from clients.http import Http, PrivacyLeak

RANGES = ("1h", "24h", "7d")
# How long a stream may take to send its first event.
EVENT_TIMEOUT = 15


def database_path(config, database=None):
    if database:
        return pathlib.Path(database)
    return pathlib.Path(config.get("data_dir", pathlib.Path("data") / config["network"])) / "puntohunter.sqlite3"


def rows(database, sql, *params):
    """The rows a query of the database returns, as dicts. The database is opened read-only, so a running pool
    can be read."""
    with contextlib.closing(sqlite3.connect(f"file:{database}?mode=ro", uri=True)) as db:
        db.row_factory = sqlite3.Row
        return [dict(row) for row in db.execute(sql, params)]


def node(config):
    """The pool's node, with the credentials its config names."""
    settings = config["node"]
    if "cookie_file" in settings:
        user, password = pathlib.Path(settings["cookie_file"]).read_text().strip().split(":", 1)
    else:
        user, password = settings["rpc_user"], settings["rpc_password"]
    return Chain(settings["rpc_url"], user, password), (user, password)


def port(bind):
    return int(bind.rsplit(":", 1)[1])


def conservation(config, database=None):
    """Checks every satoshi of the pool wallet's coinbase outputs through allocation, credit, and payment.
    Returns the lines of the report and the differences found, each a line naming where."""
    database = database_path(config, database)
    chain, credentials = node(config)
    pool_wallet = config["addresses"]["pool_wallet"]
    lines, differences = [f"conservation over {database} and the node at {chain.url}"], []

    blocks = rows(
        database,
        "SELECT b.hash, b.height, b.pool, b.status, b.pool_sats, s.amount, "
        "(SELECT SUM(sats) FROM allocations a WHERE a.block = b.hash) AS allocated, "
        "(SELECT COALESCE(SUM(CASE WHEN kind = 'pending' THEN sats WHEN kind = 'reversal' THEN -sats END), 0) "
        " FROM ledger l WHERE l.block = b.hash) AS credited, "
        "EXISTS (SELECT 1 FROM pauses p WHERE p.block = b.hash) AS paused "
        "FROM blocks b LEFT JOIN snapshots s ON s.block = b.hash "
        "WHERE b.pool_sats > 0 ORDER BY b.height, b.hash",
    )
    # What each (address, pool) should hold, paid or not: its allocations in the blocks credited.
    owed, coinbases, allocated = {}, 0, 0
    for block in blocks:
        name = f"{block['pool']} block {block['height']} {block['hash']}"
        on_chain = block["status"] not in ("rejected", "orphaned")
        output = coinbase_output(chain, block["hash"], pool_wallet) if block["status"] != "rejected" else None
        if on_chain and output is None:
            differences.append(f"{name}: the node does not have it")
        elif output is not None and output != block["pool_sats"]:
            differences.append(f"{name}: its coinbase pays the pool wallet {output} sats, recorded {block['pool_sats']}")
        if block["amount"] is None:
            differences.append(f"{name}: not allocated")
        elif block["amount"] != block["pool_sats"]:
            differences.append(f"{name}: {block['amount']} sats allocated of {block['pool_sats']}")
        if (block["allocated"] or 0) != (block["amount"] or 0):
            differences.append(f"{name}: its allocations sum to {block['allocated']} sats, not {block['amount']}")
        credit = (block["allocated"] or 0) if on_chain or block["paused"] else 0
        if block["credited"] != credit:
            differences.append(f"{name}, {block['status']}: the ledger credits {block['credited']} sats, not {credit}")
        if on_chain:
            coinbases += output or 0
            allocated += block["allocated"] or 0
        if credit:
            for row in rows(database, "SELECT address, pool, sats FROM allocations WHERE block = ?", block["hash"]):
                owed[(row["address"], row["pool"])] = owed.get((row["address"], row["pool"]), 0) + row["sats"]
        lines.append(
            f"{name}, {block['status']}: pays the pool wallet {output if output is not None else '–'} sats, "
            f"{block['allocated'] or 0} allocated, {block['credited']} credited"
        )
    on_chain = sum(1 for block in blocks if block["status"] not in ("rejected", "orphaned"))
    lines.append(
        f"{len(blocks)} blocks with a pool output, {on_chain} on the chain: their coinbases pay the pool wallet "
        f"{coinbases} sats, and {allocated} are allocated"
    )

    totals = {"pending": 0, "available": 0, "reserved": 0, "paid": 0}
    held = {}
    for row in rows(database, "SELECT * FROM balances"):
        key = (row["address"], row["pool"])
        for kind in totals:
            totals[kind] += row[kind]
            if row[kind] < 0:
                differences.append(f"{key[0]} ({key[1]}): {kind} balance {row[kind]} sats")
        held[key] = sum(row[kind] for kind in totals)
    for key in sorted(set(owed) | set(held)):
        if owed.get(key, 0) != held.get(key, 0):
            differences.append(
                f"{key[0]} ({key[1]}): allocated {owed.get(key, 0)} sats, holds {held.get(key, 0)} paid and unpaid"
            )
    lines.append(
        "balances: " + ", ".join(f"{kind} {value}" for kind, value in totals.items())
        + f" sats, {sum(totals.values())} in all, of {sum(owed.values())} allocated in blocks credited"
    )

    wallet = config.get("payouts", {}).get("wallet")
    for batch in rows(database, "SELECT * FROM batches ORDER BY id"):
        lines.append(check_batch(database, chain, credentials, wallet, batch, differences))
    reserve = rows(
        database,
        "SELECT COALESCE(SUM(CASE kind WHEN 'credit' THEN sats END), 0) AS credited, "
        "COALESCE(SUM(CASE kind WHEN 'paid' THEN sats END), 0) AS paid FROM ledger WHERE address IS NULL",
    )[0]
    lines.append(
        f"fee reserve: {reserve['credited']} sats credited, {reserve['paid']} paid in fees, "
        f"{reserve['credited'] - reserve['paid']} held"
    )
    lines.append("balanced" if not differences else f"{len(differences)} differences:")
    lines.extend(f"- {difference}" for difference in differences)
    return lines, differences


def coinbase_output(chain, block_hash, address):
    """What block `block_hash`'s coinbase pays `address`, in sats, or None if the node does not have it."""
    try:
        coinbase = chain.call("getblock", block_hash, 2)["tx"][0]
    except RpcError:
        return None
    return sum(sats(o["value"]) for o in coinbase["vout"] if o["scriptPubKey"].get("address") == address)


def check_batch(database, chain, credentials, wallet, batch, differences):
    """Checks a payout batch's transaction pays each recipient what the batch reserved, and its paid entries,
    once confirmed, against them. Returns its line of the report."""
    name = f"batch {batch['id']}, {batch['state']}"
    entries = rows(
        database,
        "SELECT kind, address, SUM(sats) AS sats FROM ledger WHERE batch = ? GROUP BY kind, address",
        batch["id"],
    )
    reserved = {e["address"]: e["sats"] for e in entries if e["kind"] == "reserved"}
    paid = {e["address"]: e["sats"] for e in entries if e["kind"] == "paid"}
    fee = reserved.pop(None, 0)
    if fee != batch["fee"]:
        differences.append(f"{name}: reserves {fee} sats of fee from the reserve, its fee is {batch['fee']}")
    if paid and paid != {**reserved, None: fee}:
        differences.append(f"{name}: its paid entries {paid} differ from what it reserved")
    if batch["state"] == "confirmed" and not paid:
        differences.append(f"{name}: confirmed without paid entries")
    elif batch["state"] != "confirmed" and paid:
        differences.append(f"{name}: paid entries before it confirmed")
    if batch["tx"] is None:
        return f"{name}: {len(reserved)} recipients reserved {sum(reserved.values())} sats, fee {fee}"
    outputs = {}
    for output in chain.call("decoderawtransaction", batch["tx"])["vout"]:
        address = output["scriptPubKey"].get("address")
        outputs[address] = outputs.get(address, 0) + sats(output["value"])
    for address, sats_reserved in reserved.items():
        if outputs.get(address, 0) != sats_reserved:
            differences.append(f"{name}: pays {address} {outputs.get(address, 0)} sats, reserved {sats_reserved}")
    change = sum(value for address, value in outputs.items() if address not in reserved)
    line = (
        f"{name} {batch['txid']}: pays {len(reserved)} recipients {sum(reserved.values())} sats, "
        f"{change} change"
    )
    if wallet and batch["state"] in ("broadcast", "confirmed"):
        payouts = Chain(f"{chain.url}/wallet/{wallet}", *credentials)
        spent = -sats(payouts.call("gettransaction", batch["txid"])["fee"])
        if spent != batch["fee"]:
            differences.append(f"{name}: its transaction's fee is {spent} sats, the batch's {batch['fee']}")
        line += f", fee {spent} sats"
    return line


def privacy(config, database=None, tokens=()):
    """Fetches every API route, stream, and series of the running pool, and its metrics, through the scanning
    client. Returns the lines of the report, a request each, and the leaks found."""
    database = database_path(config, database)
    workers = rows(database, "SELECT fleet, handle, worker, address FROM fleets ORDER BY fleet, handle")
    addresses = (
        set(config["addresses"].values())
        | {fee["address"] for fee in config.get("fees", [])}
        | {w["address"] for w in workers if w["address"]}
        | {row["address"] for row in rows(database, "SELECT DISTINCT address FROM allocations")}
    )
    portal = Http(port(config["portal"]["bind"]), addresses)
    metrics = Http(port(config["metrics"]["bind"]), addresses)
    lines, leaks = [f"privacy scan of the portal on :{portal.port} and metrics on :{metrics.port}, for "
                    f"{len(addresses)} payout addresses"], []

    def fetch(where, request):
        try:
            response = request()
        except PrivacyLeak as leak:
            leaks.append(str(leak))
            lines.append(f"{where}: LEAK")
            return None
        lines.append(f"{where}: {response.status}")
        return response

    def stream(path):
        try:
            events = portal.events(path, EVENT_TIMEOUT)
            event = next(events)
            events.close()
            lines.append(f"GET {path}: event {event['event']}")
        except PrivacyLeak as leak:
            leaks.append(str(leak))
            lines.append(f"GET {path}: LEAK")
        except (StopIteration, TimeoutError):
            lines.append(f"GET {path}: no event")

    resources = sorted({f"/api/v1/fleets/{w['fleet']}" for w in workers})
    resources += [f"/api/v1/workers/{w['handle']}" for w in workers]
    for api in resources:
        # A fleet's or worker's own resource may carry its address, at its top level alone (clients/http.py).
        fetch(f"GET {api}", lambda: portal.get(api))
        fetch(f"GET {api}/blocks", lambda: portal.get(f"{api}/blocks"))
        for range_ in RANGES:
            fetch(f"GET {api}/series?range={range_}", lambda: portal.get(f"{api}/series?range={range_}"))
        stream(f"{api}/events")
    for path in ("/api/v1/pool", "/api/v1/blocks", "/api/v1/health", "/api/v1/stats"):
        fetch(f"GET {path}", lambda: portal.get(path))
    for fleet in sorted({w["fleet"] for w in workers if w["address"]}):
        worker = next(w for w in workers if w["fleet"] == fleet and w["address"])
        username = f"{worker['address']}.{worker['worker']}" if worker["worker"] else worker["address"]
        fetch(
            f"POST /api/v1/lookup for fleet {fleet}",
            lambda: portal.api("/api/v1/lookup", method="POST", body={"username": username}),
        )
    for token in [*tokens, "0" * 64]:
        fetch(
            "GET /api/v1/balance",
            lambda: portal.request("GET", "/api/v1/balance", headers={"Authorization": f"Bearer {token}"}),
        )
    fetch("GET /metrics", lambda: metrics.get("/metrics"))
    lines.append("no payout address leaked" if not leaks else f"{len(leaks)} leaks:")
    lines.extend(f"- {leak}" for leak in leaks)
    return lines, leaks


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("report", choices=("conservation", "privacy"))
    parser.add_argument("--config", required=True, help="the running pool's config")
    parser.add_argument("--database", help="a copy of the pool's database to read instead of its own")
    parser.add_argument("--token", action="append", default=[], help="a balance token whose balance to scan")
    args = parser.parse_args()
    config = tomllib.loads(pathlib.Path(args.config).read_text())
    if args.report == "conservation":
        lines, found = conservation(config, args.database)
    else:
        lines, found = privacy(config, args.database, args.token)
    print("\n".join(lines))
    sys.exit(1 if found else 0)


if __name__ == "__main__":
    main()
