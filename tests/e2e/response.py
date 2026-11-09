"""Share response time: the pool's own histogram of a submit read to its answer written, read and
judged the same way by the simulated suite and the hardware tests, so their results compare directly."""

import math
import re

NAME = "puntohunter_share_response_seconds"
# Spec 4.3's bound on the 99th percentile, one of the histogram's bucket edges.
BOUND = 0.05
BUCKET = re.compile(rf"^{NAME}_bucket\{{(.*)\}} (\S+)$", re.M)
LABEL = re.compile(r'(\w+)="([^"]*)"')


def histogram(scrape, pool, transport):
    """The cumulative bucket counts of `pool` and `transport`'s shares, as {upper edge: count}, the last edge
    infinite; empty before any share is answered."""
    buckets = {}
    for labels, count in BUCKET.findall(scrape):
        labels = dict(LABEL.findall(labels))
        if (labels["pool"], labels["transport"]) == (pool, transport):
            buckets[float(labels["le"])] = float(count)
    return buckets


def since(after, before):
    """The shares answered between two readings of one histogram."""
    return {edge: count - before.get(edge, 0.0) for edge, count in after.items()}


def answered(buckets):
    return buckets.get(math.inf, 0.0)


def p99(buckets):
    """The smallest bucket edge at or under which 99% of the shares were answered: the 99th percentile, to the
    bucket."""
    return min((edge for edge, count in buckets.items() if count >= 0.99 * answered(buckets)), default=math.inf)


def assert_within_bound(buckets):
    assert answered(buckets) > 0, "no share answered"
    assert p99(buckets) <= BOUND, f"99th percentile {p99(buckets) * 1000:g} ms, over {BOUND * 1000:g} ms: {buckets}"
