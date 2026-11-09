// What every page shares, run as each page loads it: the header's theme button and "My fleet" link, the
// outage banner, and the footer. Links are plain links, so each page is a fresh load and a page's streams close
// as it is left.

import * as api from "./api.js";
import { bind } from "./bind.js";
import * as store from "./store.js";

// A worker or fleet handle, 16 lower-case base32 characters, as the fragment of a page's address carries it:
// /fleet.html#<handle>. A fragment never leaves the browser, so the host serving the page never learns it.
export function handle() {
  const match = /^#([a-z2-7]{16})$/.exec(location.hash);
  return match ? match[1] : null;
}

// A link to another fleet or worker changes only the fragment, which loads no page, so the page loads again
// for it: its figures and streams are for one handle.
export function reloadOnHandleChange() {
  window.addEventListener("hashchange", () => location.reload());
}

// The header's theme button: dark, the default, then light, then auto, which follows the system. theme.js
// applies the saved choice on load; a change redraws whatever reads the colours, such as the chart.
const THEMES = { dark: "Dark", light: "Light", auto: "Auto" };
function theme() {
  const button = document.getElementById("theme");
  const root = document.documentElement;
  const current = () => root.dataset.theme || "dark";
  const show = () => {
    for (const icon of button.querySelectorAll("[data-theme-icon]")) {
      icon.toggleAttribute("hidden", icon.dataset.themeIcon !== current());
    }
    button.setAttribute("aria-label", `Theme: ${THEMES[current()]}`);
    button.title = `Theme: ${THEMES[current()]}. Switch between dark, light, and auto`;
  };
  button.addEventListener("click", () => {
    const names = Object.keys(THEMES);
    const next = names[(names.indexOf(current()) + 1) % names.length];
    if (next === "dark") {
      delete root.dataset.theme;
      store.remove(store.THEME);
    } else {
      root.dataset.theme = next;
      store.set(store.THEME, next);
    }
    show();
    document.dispatchEvent(new Event("puntohunter:theme"));
  });
  window.matchMedia("(prefers-color-scheme: light)").addEventListener("change", () => {
    if (current() === "auto") document.dispatchEvent(new Event("puntohunter:theme"));
  });
  button.hidden = false;
  show();
}

// The header's "My fleet" link, shown once this browser saved a fleet. The fleet page opens the saved one.
export function myFleet() {
  document.getElementById("my-fleet").hidden = !/^[a-z2-7]{16}$/.test(store.get(store.FLEET) || "");
}

// The outage banner and the block tables follow the health check. Every 30 s
// it says whether miners get work, which shows or hides the banner, and the tip that work builds on, whose
// change refreshes the block tables. A block the node refuses leaves the tip where it was, so the tables also
// refresh every 10 minutes.
const HEALTH_EVERY_MS = 30_000;
const BLOCKS_FALLBACK_MS = 10 * 60_000;
function health() {
  const banner = document.getElementById("outage");
  let tip = null;
  let refreshed = Date.now();
  const refreshBlocks = () => {
    refreshed = Date.now();
    document.dispatchEvent(new Event("puntohunter:tip"));
  };
  const poll = () =>
    api
      .health()
      .then((health) => {
        banner.hidden = health.checks.work;
        if (health.tip_height !== null) {
          if (tip !== null && health.tip_height !== tip) refreshBlocks();
          tip = health.tip_height;
        }
      })
      // Unreachable: the banner and the tables keep what they show.
      .catch(() => {})
      .finally(() => {
        if (Date.now() - refreshed >= BLOCKS_FALLBACK_MS) refreshBlocks();
      });
  poll();
  setInterval(poll, HEALTH_EVERY_MS);
}

// The footer names the pool's network and version.
function footer() {
  api
    .poolOnce()
    .then((pool) => bind(document.getElementById("footer"), pool))
    .catch(() => {});
}

theme();
myFleet();
health();
footer();
