// The stats page, stats.html: fills the page from the stats resource and reads it again every 30 s. While the
// pool lists countries, the footer carries the credit its country database's licence asks for: DB-IP's or
// MaxMind's, and none for a database of another source.

import * as api from "../api.js";
import { bind, state } from "../bind.js";
import "../common.js";

const REFRESH_MS = 30_000;
const page = document.querySelector("main");

// Each row with its place on its list, 1 for the first.
const ranked = (rows) => rows.map((row, i) => ({ ...row, rank: i + 1 }));

const view = (stats) => ({
  ...stats,
  hash_rates: ranked(stats.hash_rates),
  best_shares: { round: ranked(stats.best_shares.round), last_24h: ranked(stats.best_shares.last_24h) },
  // A miner type's best this round is 0 before one, which reads "–".
  miners: stats.miners.map((row) => ({ ...row, best: row.best > 0 ? row.best : null })),
  // The pool sends countries only with a country database, and an empty list when no worker is connected.
  geo: "countries" in stats,
});

async function show() {
  const [stats, name] = await Promise.all([api.get("/stats"), api.poolNames()]);
  bind(document.querySelector("[data-state=ready]"), view(stats), { pool: name });
  for (const source of ["dbip", "maxmind"]) {
    document.getElementById(`geo-credit-${source}`).hidden = stats.country_source !== source;
  }
  state(page, "ready");
}

// A failed refresh keeps the last stats on screen.
show()
  .then(() => setInterval(() => show().catch(() => {}), REFRESH_MS))
  .catch(() => state(page, "failed"));
