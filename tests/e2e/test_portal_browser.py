"""The portal's pages in a browser: headless Chrome, driven
through Playwright on the host's own install, opens the pages the binary serves against a regtest pool. The
lookup opens a fleet with no username, address, worker name, or password in any URL, and shows its refusals;
the fleet and worker pages fill, take live events, and draw their charts, the worker page with no address and
no name; the overview, stats, blocks, and balance pages fill. No page logs a console error, an uncaught error,
or a CSP violation, and no request fails but those a page expects."""

import re
from urllib.parse import urlparse

import pytest
from playwright.sync_api import Error, expect, sync_playwright

from clients.http import listed, short
from clients.sv1 import Sv1Miner
from clients.sv2 import Sv2Miner
from wait import wait_until

PHONE = {"width": 390, "height": 844}
# A fleet password: 8 characters or more.
PASSWORD = "garage-pass-1"
# How long a page may take to show what the pool sends, its live events included.
SHOWN_MS = 10_000
# A pool without [group] answers its group resource 404, which the overview reads to hide the group's card.
NO_GROUP = (404, "/api/v1/group")


@pytest.fixture(scope="session")
def browser():
    """One headless Chrome per test process: the host's own, so nothing downloads a browser."""
    with sync_playwright() as playwright:
        try:
            chrome = playwright.chromium.launch(channel="chrome")
        except Error as e:
            pytest.fail(f"the browser suite needs Google Chrome on the test host: {e}")
        yield chrome
        chrome.close()


class Session:
    """A fresh browser context on one pool, as a new visitor's: every request's URL, and every console error,
    uncaught error, and failed response, which `check` asserts on at the end of a test."""

    def __init__(self, browser, pool):
        self.base = f"http://127.0.0.1:{pool.portal_port}"
        self.context = browser.new_context(viewport=PHONE)
        self.urls, self.errors, self.failed = [], [], []
        self.context.on("request", lambda request: self.urls.append(request.url))
        self.context.on(
            "response",
            lambda response: response.status >= 400
            and self.failed.append((response.status, urlparse(response.url).path)),
        )

    def open(self, path):
        page = self.context.new_page()
        # A failed response is logged as "Failed to load resource"; `failed` holds it, with its path.
        page.on(
            "console",
            lambda message: message.type == "error"
            and not message.text.startswith("Failed to load resource")
            and self.errors.append(message.text),
        )
        page.on("pageerror", lambda error: self.errors.append(str(error)))
        page.goto(self.base + path)
        return page

    def check(self, *expected):
        assert not self.errors, self.errors
        assert not [failed for failed in self.failed if failed not in expected], self.failed

    def close(self):
        self.context.close()


@pytest.fixture
def session(browser):
    sessions = []

    def open_session(pool):
        sessions.append(Session(browser, pool))
        return sessions[-1]

    yield open_session
    for each in sessions:
        each.close()


def ready(page):
    expect(page.locator("[data-state=ready]").first).to_be_visible(timeout=SHOWN_MS)


def handles(pool, event):
    return re.findall(r"handle=([a-z2-7]{16})", "\n".join(l for l in pool.logs().splitlines() if event in l))


def lookup(pool, username, password=""):
    body = {"username": username, "password": password}
    found = wait_until(
        lambda: (r := pool.portal.api("/api/v1/lookup", method="POST", body=body)).status == 200 and r,
        10,
        f"a fleet for {username}",
    )
    return found.json["fleet"]


def test_the_lookup_opens_the_fleet_with_no_username_in_any_url(pool, regtest_cluster, session):
    address = regtest_cluster.miner_address(0)
    pool.portal.secrets.append(PASSWORD)
    Sv1Miner(pool.solo_v1_port, f"{address}.kitchen")
    Sv1Miner(pool.solo_v1_port, f"{address}.garage", password=PASSWORD)
    open_fleet, private_fleet = lookup(pool, f"{address}.kitchen"), lookup(pool, f"{address}.garage", PASSWORD)
    assert open_fleet != private_fleet
    browsing = session(pool)
    page = browsing.open("/")
    for username, password, fleet, device in [
        (f"{address}.kitchen", "", open_fleet, "kitchen"),
        (f"{address}.garage", PASSWORD, private_fleet, "garage"),
    ]:
        page.goto(browsing.base + "/")
        page.fill("input[name=username]", username)
        page.fill("input[name=password]", password)
        page.click("#lookup-form button")
        page.wait_for_url(f"**/fleet.html#{fleet}", timeout=SHOWN_MS)
        ready(page)
        expect(page.locator("#live")).to_contain_text(device, timeout=SHOWN_MS)
    page.goto(browsing.base + "/")
    for username, refusal in [(address, "worker_name_required"), (f"{address}.nobody", "not_found")]:
        page.fill("input[name=username]", username)
        page.fill("input[name=password]", "")
        page.click("#lookup-form button")
        expect(page.locator(f"#lookup-answer [data-if={refusal}]")).to_be_visible(timeout=SHOWN_MS)
    for url in browsing.urls:
        for secret in (address.lower(), "kitchen", "garage", "nobody", PASSWORD):
            assert secret not in url.lower(), url
    browsing.check(NO_GROUP, (400, "/api/v1/lookup"), (404, "/api/v1/lookup"))


def test_the_fleet_page_fills_takes_live_events_and_draws_its_chart(pool, regtest_cluster, session):
    address = regtest_cluster.miner_address(0)
    miner = Sv1Miner(pool.solo_v1_port, f"{address}.kitchen")
    Sv2Miner(pool, pool.solo_v2_port, f"{address}.shed")
    assert miner.submit(miner.share()) is True
    fleet = lookup(pool, f"{address}.kitchen")
    browsing = session(pool)
    page = browsing.open(f"/fleet.html#{fleet}")
    ready(page)
    expect(page.locator("#fleet-head code")).to_have_text(short(address))
    expect(page.locator("#live article h2").first).to_have_text("Solo")
    for device in ("kitchen", "shed"):
        expect(page.locator("#live tbody[data-each=devices]")).to_contain_text(device)
    accepted = page.locator("#live article [data-bind=accepted]").first
    expect(accepted).to_have_text("1", timeout=SHOWN_MS)
    assert miner.submit(miner.share()) is True
    expect(accepted).to_have_text("2", timeout=SHOWN_MS)
    expect(page.locator("#chart canvas").first).to_be_visible(timeout=SHOWN_MS)
    expect(page.locator("#my-fleet")).to_be_visible()
    browsing.check()


def test_the_worker_page_takes_a_live_event_and_shows_no_address_or_name(pool, regtest_cluster, session):
    address = regtest_cluster.miner_address(0)
    miner = Sv1Miner(pool.solo_v1_port, f"{address}.kitchen")
    assert miner.submit(miner.share()) is True
    [handle] = handles(pool, "V1 authorized")
    browsing = session(pool)
    page = browsing.open(f"/worker.html#{handle}")
    ready(page)
    live = page.locator("#live")
    expect(live).to_contain_text("online", timeout=SHOWN_MS)
    accepted = live.locator("[data-bind=accepted]").first
    expect(accepted).to_have_text("1", timeout=SHOWN_MS)
    assert miner.submit(miner.share()) is True
    expect(accepted).to_have_text("2", timeout=SHOWN_MS)
    expect(page.locator("#chart canvas").first).to_be_visible(timeout=SHOWN_MS)
    text = page.locator("body").inner_text()
    assert address.lower() not in text.lower() and short(address) not in text, "no address"
    assert "kitchen" not in text, "no name this browser did not save from the fleet page"
    browsing.check()


def test_the_overview_stats_blocks_and_balance_pages_fill(pool, regtest_cluster, session, solution_forcer):
    address = regtest_cluster.miner_address(0)
    miner = Sv1Miner(pool.solo_v1_port, f"{address}.kitchen")
    _, block_hash = solution_forcer(miner)
    token = pool.issue_token(address)
    browsing = session(pool)

    page = browsing.open("/")
    ready(page)
    expect(page.locator("#statistics")).to_contain_text("1 connected", timeout=SHOWN_MS)
    expect(page.locator("main")).to_contain_text(f"127.0.0.1:{pool.solo_v1_port}")
    expect(page.locator("#outage")).to_be_hidden()

    page = browsing.open("/stats.html")
    ready(page)
    expect(page.locator("main")).to_contain_text(listed(address), timeout=SHOWN_MS)

    page = browsing.open("/blocks.html")
    expect(page.locator("#blocks tbody tr").first).to_be_visible(timeout=SHOWN_MS)
    expect(page.locator("#blocks")).to_contain_text(f"{block_hash[:8]}…")

    page = browsing.open("/balance.html")
    page.fill("input[name=token]", token)
    page.click("#balance-form button")
    expect(page.locator("#balance-result")).to_be_visible(timeout=SHOWN_MS)
    for url in browsing.urls:
        assert token not in url, url
    browsing.check(NO_GROUP)
