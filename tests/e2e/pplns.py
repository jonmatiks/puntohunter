"""Helpers of the PPLNS tests: a block's snapshot and allocations as the pool wrote them, and the
allocation reckoned independently from the journal rows, under policy 2 each share's work weighted by its job's
pool output over its miner portion."""

from wait import wait_until


def work(target):
    """A share's work at its 32-byte big-endian target, in 2^-16 hashes: 2^256 / (floor(target / 2^16) + 1)."""
    return 2**256 // ((int.from_bytes(target, "big") >> 16) + 1)


def compact_target(bits):
    """The 32-byte big-endian target of compact `bits`."""
    exponent, mantissa = bits >> 24, bits & 0x7FFFFF
    value = mantissa << 8 * (exponent - 3) if exponent > 3 else mantissa >> 8 * (3 - exponent)
    return value.to_bytes(32, "big")


def size(multiplier, min_work, bits):
    """W in work units, as src/pool/pplns.rs computes it: k times the network's work, or W_min difficulty-1
    shares' worth if that is more, each float product rounded down."""
    network, difficulty_1 = work(compact_target(bits)), work(compact_target(0x1D00FFFF))
    return max(int(multiplier * float(network)), int(min_work * float(difficulty_1)), 1)


def snapshot(pool, block_hash):
    """The block's snapshot once it is written, and its allocations as {address: sats}."""
    rows = wait_until(
        lambda: pool.rows("SELECT * FROM snapshots WHERE block = ?", block_hash), 10, "the block's snapshot"
    )
    allocations = pool.rows("SELECT address, sats, pool FROM allocations WHERE block = ?", block_hash)
    assert {row["pool"] for row in allocations} <= {rows[0]["pool"]}
    return rows[0], {row["address"]: row["sats"] for row in allocations}


def weighted(row, version):
    """A journal row's work as the solution share's policy `version` counts it: plain under policy 1, and under
    policy 2 floor(work × pool_sats / miner_sats)."""
    plain = work(row["target"])
    return plain * row["pool_sats"] // row["miner_sats"] if version >= 2 else plain


def reckoned(pool, snap):
    """The PPLNS allocation of the snapshot's amount, reckoned here from the journal: walk back from the cutoff
    over the block's pool until W is credited, the oldest share only in part, each share's work weighted under
    the solution share's policy; floors by work, then the leftover satoshis by largest remainder, ties to the
    lower address."""
    [policy] = pool.rows(
        "SELECT p.version FROM journal j JOIN policies p ON p.id = j.policy WHERE j.sequence = ?", snap["cutoff"]
    )
    rows = pool.rows(
        "SELECT sequence, address, target, miner_sats, pool_sats FROM journal WHERE pool = ? AND sequence <= ? "
        "ORDER BY sequence DESC",
        snap["pool"],
        snap["cutoff"],
    )
    size, credits, total = int(snap["size"]), {}, 0
    for row in rows:
        credited = min(weighted(row, policy["version"]), size - total)
        credits[row["address"]] = credits.get(row["address"], 0) + credited
        total += credited
        first = row["sequence"]
        if total == size:
            break
    assert (first, str(total)) == (snap["first"], snap["total"])
    amount = snap["amount"]
    floors = {address: amount * w // total for address, w in credits.items()}
    remainders = {address: amount * w % total for address, w in credits.items()}
    leftover = amount - sum(floors.values())
    for address in sorted(credits, key=lambda a: (-remainders[a], a))[:leftover]:
        floors[address] += 1
    return {address: sats for address, sats in floors.items() if sats}, credits
