"""Condition waits: the suite never sleeps to synchronize."""

import time


class WaitTimeout(AssertionError):
    pass


def wait_until(predicate, timeout, message="condition"):
    """Polls `predicate` until it returns a truthy value, which is returned, or `timeout` seconds pass."""
    deadline = time.monotonic() + timeout
    while True:
        result = predicate()
        if result:
            return result
        if time.monotonic() >= deadline:
            raise WaitTimeout(f"{message} not met within {timeout}s")
        time.sleep(0.02)
