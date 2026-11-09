// How the app writes the API's raw values, in the PuntoHunter portal's words (its src/portal/figures.rs):
// quantities in SI steps, times ago, spans, waits, and the odds of a block, worded as odds and never as a
// forecast (API reference section 3).

const DAY = 86_400;
const YEAR = 365 * DAY;
const PREFIXES = ["", "k", "M", "G", "T", "P", "E", "Z"];

export const now = () => Math.floor(Date.now() / 1000);

// `value` divided down by thousands, and the SI prefix for the steps taken.
function step(value) {
  let prefix = 0;
  while (value >= 1000 && prefix < PREFIXES.length - 1) {
    value /= 1000;
    prefix += 1;
  }
  return [value, PREFIXES[prefix]];
}

// Scientific notation as the portal writes it: 3.00e-10, 1.23e15.
const scientific = (value) => value.toExponential(2).replace("e+", "e");

// `value` to `digits` significant figures, with no trailing zeros after the point.
function figures(value, digits) {
  if (value === 0) return "0";
  const decimals = Math.min(100, Math.max(0, digits - 1 - Math.floor(Math.log10(Math.abs(value)))));
  const text = value.toFixed(decimals);
  return text.includes(".") ? text.replace(/\.?0+$/, "") : text;
}

// A count rounded to three significant figures, or to a whole number below 1,000: 52,345 to 52,300.
function threeFigures(value) {
  if (value < 1000) return Math.round(value);
  const scale = 10 ** (Math.floor(Math.log10(value)) - 2);
  return Math.round(value / scale) * scale;
}

// A count to three significant figures with thousands separated, 52,000; past any wait a reader can picture,
// in scientific notation.
function whole(value) {
  if (value >= 1e15) return scientific(value);
  return Math.max(1, threeFigures(value))
    .toFixed(0)
    .replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

// The number and SI prefix of `value`, to three figures; below 0.01, as regtest's difficulties are, in
// scientific notation.
function scaled(value) {
  if (value === 0) return ["0", ""];
  if (value < 0.01) return [scientific(value), ""];
  const [number, prefix] = step(value);
  const digits = number >= 100 ? 0 : number >= 10 ? 1 : 2;
  return [number.toFixed(digits), prefix];
}

// A quantity in SI steps: 512, 1.23 k.
export function si(value) {
  const [number, prefix] = scaled(value);
  return prefix ? `${number} ${prefix}` : number;
}

// A hashrate in hashes a second: 1.06 TH/s.
export function hashRate(value) {
  const [number, prefix] = scaled(value);
  return `${number} ${prefix}H/s`;
}

// A windowed hashrate, "–" while its window is still filling: an average over part of it reads low, and
// would mislead.
export const rate = (r) => (r.estimating ? "–" : hashRate(r.hash_rate));

// A difficulty in SI steps, always with two digits after the point: 512.00, 12.90 k, 1.23 T.
export function difficulty(value) {
  if (value === 0) return "0.00";
  if (value < 0.01) return scientific(value);
  const [number, prefix] = step(value);
  return prefix ? `${number.toFixed(2)} ${prefix}` : number.toFixed(2);
}

// Satoshis in BTC, without trailing zeros: 1.049895, 0.001.
export function btc(sats) {
  const coins = Math.floor(sats / 1e8);
  const rest = String(sats % 1e8).padStart(8, "0").replace(/0+$/, "");
  return rest ? `${coins}.${rest}` : String(coins);
}

// A fee or reward portion: "0.9%", or "0.01 BTC".
export const portion = (p) => ("ppm" in p ? `${p.ppm / 10_000}%` : `${btc(p.sats)} BTC`);

// A chance as "1 in N" below 1%, and as a percentage above; N in SI steps from 100,000, 1 in 6.2 M.
export function chance(p) {
  if (p >= 0.01) return `${figures(p * 100, 3)}%`;
  const n = threeFigures(1 / p);
  if (n < 100_000) return `1 in ${whole(n)}`;
  const [number, prefix] = step(n);
  return `1 in ${figures(number, 3)} ${prefix}`;
}

// A wait in the largest unit that keeps it above 2: 90 min, 3 days, 13,600 years.
export function wait(seconds) {
  if (seconds < 2 * 3600) return `${whole(seconds / 60)} min`;
  if (seconds < 2 * DAY) return `${whole(seconds / 3600)} h`;
  if (seconds < 2 * YEAR) return `${whole(seconds / DAY)} days`;
  return `${whole(seconds / YEAR)} years`;
}

// A fraction as a percentage to three significant figures, however tiny: 0.0000932%.
export const percent = (fraction) => `${figures(fraction * 100, 3)}%`;

// A fraction as a percentage to one significant figure below 1%, where only its scale matters, and to three
// from 1%: 0.00009%, 12.3%.
export function roughPercent(fraction) {
  const value = fraction * 100;
  return `${figures(value, value < 1 ? 1 : 3)}%`;
}

// How long ago Unix time `at` was, in the largest unit that keeps it above 2: 3 h ago.
export function ago(at) {
  const s = Math.max(0, now() - at);
  if (s < 120) return `${s} s ago`;
  if (s < 2 * 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 2 * DAY) return `${Math.floor(s / 3600)} h ago`;
  return `${Math.floor(s / DAY)} days ago`;
}

// A span of seconds in its two largest units: 45 s, 12 min 5 s, 3 h 12 min, 4 d 6 h.
export function span(seconds) {
  const [days, hours, minutes] = [Math.floor(seconds / DAY), Math.floor(seconds / 3600) % 24, Math.floor(seconds / 60) % 60];
  if (seconds < 60) return `${seconds} s`;
  if (seconds < 3600) return `${minutes} min ${seconds % 60} s`;
  if (seconds < DAY) return `${hours} h ${minutes} min`;
  return `${days} d ${hours} h`;
}

// The time since Unix time `at`: 3 h 12 min.
export const elapsed = (at) => span(Math.max(0, now() - at));

// Unix time `at` in the viewer's own time zone, and in UTC to the minute.
export const local = (at) => new Date(at * 1000).toLocaleString([], { dateStyle: "medium", timeStyle: "short" });
export const utc = (at) => new Date(at * 1000).toISOString().slice(0, 16).replace("T", " ") + " UTC";
// ...and as a <time> element's datetime: 2026-10-17T02:29:00.000Z.
export const iso = (at) => new Date(at * 1000).toISOString();

// An address shortened to its first and last 8 characters, as pools show one: bc1qw508…7kv8f3t4.
export function shortAddress(address) {
  return address.length <= 17 ? address : `${address.slice(0, 8)}…${address.slice(-8)}`;
}

// A block hash shortened to its first and last 8 characters: 00000000…3f2a9c1e.
export const shortHash = (hash) => `${hash.slice(0, 8)}…${hash.slice(-8)}`;

// A rank in words: 1st, 2nd, 11th.
export function ordinal(n) {
  const suffix = n % 100 >= 11 && n % 100 <= 13 ? "th" : { 1: "st", 2: "nd", 3: "rd" }[n % 10] || "th";
  return `${n}${suffix}`;
}

// Why a figure is missing (API reference section 1.4).
const WHY = {
  no_template: "no template from the node",
  no_hash_rate: "no hashrate yet",
  no_shares: "no shares in the window yet",
};
export const why = (reason) => WHY[reason] || reason;

// A device's status: "offline, last seen 3 h ago" from `last_seen_at`.
export const status = (status, lastSeenAt) => (status === "offline" ? `offline, last seen ${ago(lastSeenAt)}` : status);

// A block's status code as it reads.
export const blockStatus = (code) => (code === "confirmed_immature" ? "confirmed, immature" : code);

// Blocks until a block's reward matures.
export function maturity(blocks) {
  if (blocks === null) return "–";
  if (blocks === 0) return "mature";
  return blocks === 1 ? "1 block to go" : `${blocks} blocks to go`;
}
