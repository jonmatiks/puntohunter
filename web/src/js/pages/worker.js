// The worker page, worker.html: reads the device its address names, fills the page from its resource and then
// its stream, and starts its chart and blocks. Its title is the name this browser saved on the fleet page, if
// any: the pool never sends a worker's name with the worker.

import * as api from "../api.js";
import { bind, state } from "../bind.js";
import { attachBlocks } from "../blocks.js";
import { attachChart } from "../chart.js";
import { handle, reloadOnHandleChange } from "../common.js";
import * as f from "../format.js";
import * as store from "../store.js";

const page = document.querySelector("main");

async function open(worker) {
  const [resource, name, pool] = await Promise.all([
    api.get(`/workers/${worker}`),
    api.poolNames(),
    api.poolOnce().catch(() => null),
  ]);
  // The formats the page's HTML names beyond format.js's: a pool by its name, a device's status, and how
  // many connections it holds, named only past one.
  const formats = {
    pool: name,
    device: (l) => f.status(l.status, l.last_seen_at),
    connections: (n) => (n > 1 ? `, ${n} connections` : ""),
  };
  const setup = pool && pool.pools.find((p) => p.id === resource.pool);
  const title = store.get(store.NAME + worker) || "Worker";
  bind(document.getElementById("worker-head"), { ...resource, setup, title }, formats);

  // "Forget", shown while the device is offline: hides it from its fleet page in this browser until it
  // connects again, and follows the header's "My fleet" where this browser saved one. Pressed once more, it
  // shows the device there again.
  const forget = document.getElementById("forget-worker");
  let latest = resource.live;
  const showForget = () => {
    const hidden = store.forgotten(worker, latest);
    forget.hidden = latest.status !== "offline";
    forget.textContent = hidden ? "Show on the fleet page" : "Forget";
    forget.title = hidden ? "" : "Hide it from its fleet page in this browser until it connects again";
  };
  forget.addEventListener("click", () => {
    if (store.get(store.FORGOTTEN + worker)) {
      store.remove(store.FORGOTTEN + worker);
      showForget();
      return;
    }
    store.set(store.FORGOTTEN + worker, String(latest.last_seen_at));
    showForget();
    const myFleet = document.getElementById("my-fleet");
    if (!myFleet.hidden) location.assign(myFleet.href);
  });

  // The resource's live values, then each stream event's.
  const live = document.getElementById("live");
  const draw = (values) => {
    latest = values;
    bind(live, values, formats);
    showForget();
  };
  draw(resource.live);
  state(page, "ready");

  attachChart(document.getElementById("chart"), `/workers/${worker}/series`);
  attachBlocks(document.getElementById("blocks"), `/workers/${worker}/blocks`);
  api.stream(`/workers/${worker}/events`, "live", draw);
}

reloadOnHandleChange();
const worker = handle();
if (!worker) state(page, "not-found");
else open(worker).catch((e) => state(page, e instanceof api.ApiError && e.code === "not_found" ? "not-found" : "failed"));
