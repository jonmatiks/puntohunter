// The PuntoHunter API (/api/v1), called from the browser: on this origin through Caddy, which forwards each
// viewer's address to the pool, or on another over CORS. Either way the pool's per-address limits key on each
// viewer rather than on a server between them. Errors arrive as `{"error", "message"}` with a
// stable code; the message is for developers, and the pages word their own from the code.

import { API } from "/config.js";

const BASE = `${API}/api/v1`;

// An API refusal: its HTTP status, its code, and its body. A pool that cannot be reached is code "unreachable".
export class ApiError extends Error {
  constructor(status, body) {
    super((body && body.message) || `HTTP ${status}`);
    this.status = status;
    this.code = (body && body.error) || "unreachable";
    this.body = body;
  }
}

async function request(path, options) {
  let response;
  try {
    response = await fetch(BASE + path, options);
  } catch (e) {
    throw new ApiError(0, null);
  }
  const body = await response.json().catch(() => null);
  if (!response.ok) throw new ApiError(response.status, body);
  return body;
}

export const get = (path) => request(path);

// The fleet a full Stratum username opens, the private one with its fleet password. Both travel in the body,
// never in a URL.
export const lookup = (username, password) =>
  request("/lookup", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(password ? { username, password } : { username }),
  });

// The balances a token opens. The token travels in a header, never in a URL, and is never kept.
export const balance = (token) => request("/balance", { headers: { Authorization: `Bearer ${token}` } });

// The pool's health. It answers 503 with its body, not an error, while miners get no work.
export async function health() {
  const response = await fetch(`${BASE}/health`);
  return response.json();
}

// The pool resource, read once a page: its pools' names, its network, and its version.
let pool = null;
export function poolOnce() {
  pool ??= get("/pool").catch((e) => {
    pool = null;
    throw e;
  });
  return pool;
}

// What the pages call pool `id`: its name, or its id once it is no longer configured.
export async function poolNames() {
  const resource = await poolOnce().catch(() => null);
  return (id) => (resource && resource.pools.find((p) => p.id === id)?.name) || id;
}

// A live stream: calls `on` with the data of each `event`. The browser reconnects a stream that drops, but a
// stream the pool refuses, over its cap of open streams (429 or 503, Retry-After 10 s), closes for good, so it
// is opened again after that delay. The page keeps the last values meanwhile.
const STREAM_RETRY_MS = 10_000;
export function stream(path, event, on) {
  const open = () => {
    const source = new EventSource(BASE + path);
    source.addEventListener(event, (message) => on(JSON.parse(message.data)));
    source.addEventListener("error", () => {
      if (source.readyState === EventSource.CLOSED) setTimeout(open, STREAM_RETRY_MS);
    });
  };
  open();
}
