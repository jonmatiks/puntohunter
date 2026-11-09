// The hash-rate chart: drawn by uPlot from a series resource (API reference section 2.7) over the range its
// tabs pick, with refused shares on an axis of their own, and fetched again just after each bucket ends,
// since the series only changes then.

import * as api from "./api.js";
import * as store from "./store.js";

const HEIGHT = 200;
const PREFIXES = ["", "k", "M", "G", "T", "P", "E", "Z"];

function si(value) {
  if (value === 0) return "0 ";
  let prefix = 0;
  while (value >= 1000 && prefix < PREFIXES.length - 1) {
    value /= 1000;
    prefix += 1;
  }
  // Trailing zeros go only after a decimal point: 800 stays 800, 1.50 becomes 1.5.
  let text = value.toPrecision(3);
  if (text.includes(".")) text = text.replace(/\.?0+$/, "");
  return `${text} ${PREFIXES[prefix]}`;
}

// The lines a response holds: a worker's series, or a fleet's per pool, each named as its pool.
function lines(json) {
  if (json.start) return [["Hashrate", json]];
  const pools = json.pools.filter((series) => series.start.length > 0).map((series) => [series.name, series]);
  return pools.length > 0 ? pools : [["Hashrate", json.pools[0]]];
}

// The lines on one time axis across the whole range, with a zero for each bucket that had no row, and the
// refused shares of them all.
function points(sets) {
  const { since, until, period } = sets[0][1];
  const time = [];
  for (let t = since; t < until; t += period) time.push(t);
  const index = new Map(time.map((t, i) => [t, i]));
  const refused = time.map(() => 0);
  const rates = sets.map(([, series]) => {
    const rate = time.map(() => 0);
    series.start.forEach((t, i) => {
      rate[index.get(t)] = series.hash_rate[i];
      refused[index.get(t)] += series.rejected[i];
    });
    return rate;
  });
  return [time, ...rates, refused];
}

// The buckets merged so each point keeps a few pixels: a day at one a minute crowds 1,440 points into a band.
// A merged bucket is the shortest step below that does, aligned on the clock so a refresh does not shift it;
// its rates are the means of its buckets', which is the rate over it, its refused shares their sum.
const STEPS = [1, 2, 5, 10, 15, 30, 60, 120, 180, 360, 720, 1440].map((minutes) => minutes * 60);
const PIXELS = 3;
function merge(data, period, width) {
  const time = data[0];
  const need = (time.length * period * PIXELS) / Math.max(width, 1);
  const step = STEPS.find((s) => s >= need && s % period === 0) || Math.ceil(need / period) * period;
  if (step <= period) return data;
  const merged = data.map(() => []);
  const counts = [];
  time.forEach((t, i) => {
    const start = t - (t % step);
    if (merged[0][merged[0].length - 1] !== start) {
      merged.forEach((column, c) => column.push(c === 0 ? start : 0));
      counts.push(0);
    }
    const j = counts.length - 1;
    counts[j] += 1;
    for (let c = 1; c < data.length; c++) merged[c][j] += data[c][i];
  });
  for (let c = 1; c < data.length - 1; c++) merged[c] = merged[c].map((sum, j) => sum / counts[j]);
  // The range's ends cut their merged buckets short, and one under half full is too noisy to draw.
  const full = counts.map((count) => count * period * 2 >= step);
  return merged.map((column) => column.filter((_, j) => full[j]));
}

// Each line's trailing mean over a twelfth of the range, which shows its trend through the noise: five
// minutes over an hour, two hours over a day. The first points average what came before them.
function averages(data, since, until) {
  const time = data[0];
  const window = (until - since) / 12;
  return data.slice(1, -1).map((rate) => {
    let from = 0;
    let sum = 0;
    return rate.map((value, i) => {
      sum += value;
      while (time[from] <= time[i] - window) sum -= rate[from++];
      return sum / (i - from + 1);
    });
  });
}

// Draws the series at `path`, such as "/fleets/<handle>/series", in `card`, which holds its markup: buttons
// marked data-range="1h" and so on, aria-pressed="true" on the default, a .plot for uPlot, and a .empty note
// for a series with no history yet.
export function attachChart(card, path) {
  const plotted = card.querySelector(".plot");
  const empty = card.querySelector(".empty");
  const tabs = [...card.querySelectorAll("[data-range]")];
  const ranges = tabs.map((tab) => tab.dataset.range);
  const pressed = tabs.find((tab) => tab.getAttribute("aria-pressed") === "true");
  let range = ranges.includes(store.get(store.RANGE)) ? store.get(store.RANGE) : pressed ? pressed.dataset.range : ranges[0];
  let plot = null;
  let shape = null;
  let timer = null;
  let last = null;

  const press = () => tabs.forEach((tab) => tab.setAttribute("aria-pressed", String(tab.dataset.range === range)));
  press();
  tabs.forEach((tab) =>
    tab.addEventListener("click", () => {
      range = tab.dataset.range;
      press();
      store.set(store.RANGE, range);
      refresh();
    }),
  );

  function draw(json) {
    last = json;
    if (typeof uPlot === "undefined") return;
    const sets = lines(json);
    // The plot's width, less its two axes.
    const merged = merge(points(sets), sets[0][1].period, plotted.clientWidth - 110);
    const { since, until } = sets[0][1];
    const data = [...merged.slice(0, -1), ...averages(merged, since, until), merged[merged.length - 1]];
    empty.hidden = sets.some(([, series]) => series.start.length > 0);
    const key = sets.map(([label]) => label).join();
    if (plot && key === shape) {
      plot.setData(data);
      return;
    }
    if (plot) plot.destroy();
    shape = key;
    const style = getComputedStyle(document.documentElement);
    const colour = (name) => style.getPropertyValue(name).trim();
    // Green is kept for the averages, so no pool's line reads as one.
    const strokes = [colour("--accent"), colour("--fg")];
    // A fleet in more pools than there are colours: each further round of colours draws dashed, then dotted.
    // The averages are all green, so each pool's is told apart by these alone.
    const dashes = [undefined, [6, 4], [2, 3]];
    const line = (label, i, average) => ({
      label: average ? (sets.length > 1 ? `${label} average` : "Average") : label,
      stroke: average ? colour("--good") : strokes[i % strokes.length],
      dash: dashes[(average ? i : Math.floor(i / strokes.length)) % dashes.length],
      width: average ? 1.5 : 1,
      value: (u, v) => (v == null ? "–" : `${si(v)}H/s`),
    });
    const axis = { stroke: colour("--muted"), grid: { stroke: colour("--border") }, ticks: { stroke: colour("--border") } };
    plot = new uPlot(
      {
        width: plotted.clientWidth,
        height: HEIGHT,
        // Both axes start at zero, so a steady rate reads as steady rather than its noise filling the height.
        scales: {
          y: { range: (u, min, max) => uPlot.rangeNum(0, max > 0 ? max : 1, 0.1, true) },
          refused: { range: (u, min, max) => [0, Math.max(1, max)] },
        },
        series: [
          {},
          ...sets.map(([label], i) => line(label, i, false)),
          ...sets.map(([label], i) => line(label, i, true)),
          {
            label: "Refused",
            scale: "refused",
            stroke: colour("--warn"),
            width: 1,
            value: (u, v) => (v == null ? "–" : String(v)),
          },
        ],
        axes: [
          axis,
          { ...axis, values: (u, splits) => splits.map((v) => `${si(v)}H/s`), size: 70 },
          {
            ...axis,
            scale: "refused",
            side: 1,
            grid: { show: false },
            values: (u, splits) => splits.map((v) => (Number.isInteger(v) ? String(v) : "")),
            size: 40,
          },
        ],
      },
      data,
      plotted,
    );
  }

  function schedule(period) {
    const now = Date.now() / 1000;
    const next = (Math.floor(now / period) + 1) * period + 2;
    clearTimeout(timer);
    timer = setTimeout(refresh, (next - now) * 1000);
  }

  function refresh() {
    api
      .get(`${path}?range=${range}`)
      .then((json) => {
        draw(json);
        schedule(json.start ? json.period : json.pools[0].period);
      })
      .catch(() => schedule(60));
  }

  window.addEventListener("resize", () => {
    if (plot) plot.setSize({ width: plotted.clientWidth, height: HEIGHT });
  });
  // uPlot draws on a canvas with the colours read at build time, so a new theme builds it again.
  document.addEventListener("puntohunter:theme", () => {
    shape = null;
    if (last) draw(last);
  });
  // Drawn once the card is on the page, which gives the plot its width.
  requestAnimationFrame(refresh);
}

