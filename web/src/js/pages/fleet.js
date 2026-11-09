// The fleet page, fleet.html: reads the fleet its address names, or the one this browser saved, fills
// the page from its resource and then its stream, and starts its chart and blocks. The page saves the fleet
// for "My fleet" and each device's name, which the worker page shows: the pool never sends a worker's name
// with the worker.

import * as api from "../api.js";
import { bind, state } from "../bind.js";
import { attachBlocks } from "../blocks.js";
import { attachChart } from "../chart.js";
import { handle, myFleet, reloadOnHandleChange } from "../common.js";
import * as f from "../format.js";
import * as store from "../store.js";

const page = document.querySelector("main");

// The formats the page's HTML names beyond format.js's: a pool by its name, a device's status, and how long
// an offline device stays: "the last 7 days", or "the last day".
const formats = (data) => ({
  pool: (id) => data.pools.find((p) => p.pool === id)?.name || id,
  device: (d) => f.status(d.status, d.last_seen_at),
  lastDays: (n) => (n === 1 ? "day" : `${n} days`),
});

// The fleet's places on the stats page's lists, in the page's order, leaving out each list it is not on.
const ranks = (r) => {
  const lists = ["best_share", "best_share_24h", "hash_rate"].filter((list) => r?.[list] != null);
  return lists.map((list, i) => ({ list, rank: r[list], last: i === lists.length - 1 }));
};

async function open(fleet) {
  const resource = await api.get(`/fleets/${fleet}`);
  store.set(store.FLEET, fleet);
  myFleet();

  // The resource, then each stream event: the same figures, without the fleet's handle and address. The
  // devices this browser forgot are left out of the table, and a note under it says how many. A row that
  // several connections share says so.
  const live = document.getElementById("live");
  let latest = resource;
  const draw = (data) => {
    latest = data;
    const devices = data.devices
      .filter((device) => !store.forgotten(device.handle, device))
      .map((device) => ({
        ...device,
        shared: device.connections > 1 ? `${device.connections} devices share this username` : null,
      }));
    const hidden = data.devices.length - devices.length;
    const forgotten = hidden === 0 ? null : hidden === 1 ? "1 forgotten device hidden." : `${hidden} forgotten devices hidden.`;
    bind(live, { ...data, devices, listed: data.devices, forgotten }, formats(data));
    for (const device of data.devices) if (device.name) store.set(store.NAME + device.handle, device.name);
  };
  // "Show" remembers the hidden devices again.
  document.getElementById("show-forgotten").addEventListener("click", () => {
    for (const device of latest.devices) store.remove(store.FORGOTTEN + device.handle);
    draw(latest);
  });
  // Back from a worker page can show this page from the browser's cache, without running this script again.
  window.addEventListener("pageshow", (event) => {
    if (event.persisted) draw(latest);
  });
  bind(document.getElementById("fleet-head"), resource);
  bind(document.getElementById("open-fleet"), resource);
  bind(document.getElementById("ranks"), { ranks: ranks(resource.ranks) });
  draw(resource);
  state(page, "ready");

  // "Forget" clears the saved fleet and leaves for the overview, out of the tab's history too, so Back cannot
  // save the fleet again.
  document.getElementById("forget").addEventListener("click", () => {
    store.remove(store.FLEET);
    location.replace("/");
  });

  attachChart(document.getElementById("chart"), `/fleets/${fleet}/series`);
  attachBlocks(document.getElementById("blocks"), `/fleets/${fleet}/blocks`);
  api.stream(`/fleets/${fleet}/events`, "fleet", draw);
}

reloadOnHandleChange();
// fleet.html alone opens the saved fleet, under its own address, so it can be bookmarked or shared.
const fleet = location.hash ? handle() : store.get(store.FLEET);
if (!fleet || !/^[a-z2-7]{16}$/.test(fleet)) {
  state(page, location.hash ? "not-found" : "none");
} else {
  if (!location.hash) history.replaceState(null, "", `#${fleet}`);
  open(fleet).catch((e) => state(page, e instanceof api.ApiError && e.code === "not_found" ? "not-found" : "failed"));
}
