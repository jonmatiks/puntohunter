// The overview, index.html: fills the page from the pool resource, refreshes its statistics and the group's
// totals every 30 s, and runs the username lookup.

import * as api from "../api.js";
import { bind, state } from "../bind.js";
import "../common.js";
import "../copy.js";
import * as f from "../format.js";

const REFRESH_MS = 30_000;
const page = document.querySelector("main");

// Which of a pool's payout rules index.html shows: its finder's whole reward, its miners' by work, or both,
// with a finder part each device picks, or one part for all.
function rule(p) {
  if (p.kind === "solo" || p.kind === "pooled") return p.kind;
  return p.personal ? "personal" : "fixed";
}

const view = (pool) => ({
  ...pool,
  pools: pool.pools.map((p) => ({ ...p, rule: rule(p) })),
  payout_terms: pool.pools.some((p) => p.window !== null) || Boolean(pool.payouts),
});

// The formats the page's HTML names beyond format.js's: a pool's fees as a list, "0.9% to the operator and
// 0.1% to the developer".
const formats = {
  fees: (fees) =>
    fees
      .map((fee, i) => `${i === 0 ? "" : i === fees.length - 1 ? " and " : ", "}${f.portion(fee.portion)} to the ${fee.name}`)
      .join(""),
};

// The lookup: a full username opens its address's fleet, the private one with its fleet password. Both go to the
// pool by POST, never in a URL. A refusal
// shows the answer index.html words for its code, or the one for an unreachable pool.
function lookup() {
  const form = document.getElementById("lookup-form");
  const button = form.querySelector("button");
  const answer = document.getElementById("lookup-answer");
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    button.disabled = true;
    bind(answer, {});
    try {
      const { fleet } = await api.lookup(form.elements.username.value.trim(), form.elements.password.value);
      location.assign(`/fleet.html#${fleet}`);
    } catch (e) {
      const worded = answer.querySelector(`[data-if="${CSS.escape(e.code)}"]`);
      bind(answer, { ...e.body, [worded ? e.code : "unreachable"]: true });
      button.disabled = false;
    }
  });
}

// The group's totals, which hide their card when the pool is in no group: it answers 404 not_found then. Any
// other failure keeps the last totals on screen.
function group() {
  const card = document.getElementById("group");
  const read = () =>
    api
      .get("/group")
      .then((totals) => bind(card, totals))
      .catch((e) => {
        if (e.code === "not_found") bind(card, {});
      });
  read();
  setInterval(read, REFRESH_MS);
}

async function open() {
  const pool = await api.poolOnce();
  bind(document.querySelector("[data-state=ready]"), view(pool), formats);
  state(page, "ready");
  // A link to one of the page's cards, such as /#miner-settings, found it hidden until the pool answered.
  document.getElementById(location.hash.slice(1))?.scrollIntoView();
  lookup();
  group();
  // The statistics refresh; a failed read keeps the last ones on screen.
  const statistics = document.getElementById("statistics");
  setInterval(() => {
    api
      .get("/pool")
      .then((fresh) => bind(statistics, view(fresh), formats))
      .catch(() => {});
  }, REFRESH_MS);
}

open().catch(() => state(page, "failed"));
