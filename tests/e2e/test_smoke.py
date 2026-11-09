"""The harness itself: the pool starts, reports ready, and survives a kill and restart."""


def test_pool_reports_ready(pool):
    assert "puntohunter_ready 1" in pool.metrics().splitlines()


def test_pool_restarts_after_kill(pool):
    first = pool.process.pid
    pool.restart()
    assert pool.process.pid != first
    assert "puntohunter_ready 1" in pool.metrics().splitlines()
