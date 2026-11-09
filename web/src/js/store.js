// What this browser keeps (API reference section 3): the fleet handle for "My fleet", device names by handle,
// the devices forgotten, the chart range, and the theme. The pool never names "your" fleet. Storage that
// fails or is empty only loses the convenience; every page still works.

export const FLEET = "puntohunter.fleet";
export const NAME = "puntohunter.name.";
// By handle: each forgotten device's last-seen time, in Unix seconds, when it was forgotten.
export const FORGOTTEN = "puntohunter.forgotten.";
export const RANGE = "puntohunter.range";
export const THEME = "puntohunter.theme";

// Whether `device`, a fleet or worker resource's live values, stays off its fleet page: it was forgotten here
// and is still offline. Seen connected, or last seen more than 10 minutes after it was forgotten, it is
// remembered again. The slack is for its last-seen time, which moves to the end of its last rollup period
// when the pool's telemetry lets it go.
const FORGET_SLACK = 600;
export function forgotten(handle, device) {
  const at = Number(get(FORGOTTEN + handle));
  if (!at) return false;
  if (device.status === "offline" && device.last_seen_at <= at + FORGET_SLACK) return true;
  remove(FORGOTTEN + handle);
  return false;
}

export function get(key) {
  try {
    return window.localStorage.getItem(key);
  } catch (e) {
    return null;
  }
}

export function set(key, value) {
  try {
    window.localStorage.setItem(key, value);
  } catch (e) {
    // Not saved: the page still works.
  }
}

export function remove(key) {
  try {
    window.localStorage.removeItem(key);
  } catch (e) {
    // Nothing to forget.
  }
}
