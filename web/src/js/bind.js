// Fills a page's HTML with the API's data. The HTML says where each value goes; this sets text and a few
// attributes, never markup, so nothing the API sends, such as a worker name its owner typed, can inject HTML.
//
//   data-bind="hash_rate.1m"   the text: the value at that path, "–" when it is null, missing, or []
//   data-format="rate"         ...written by that function of format.js, or one the page adds
//   data-empty="none yet"      ...or this text in place of "–"
//   data-if="window"           shown only when the value is present: not null, missing, false, "", or []
//   data-if="kind=pooled"      shown only when the value is that text
//   data-if="!window"          shown only when the test after the ! fails: "!kind=pooled" too
//   data-each="devices"        one copy of the element's <template> per item, each filled from that item
//   data-href="/worker.html#{handle}"  a link, with each {path} filled in and URL-encoded
//   data-link="link"           a link to the URL at that path, as it is, if it is http or https; none otherwise
//   data-title="hash"          a tooltip: the value at that path, or "started_at|utc" written by a format
//   data-datetime="started_at|iso"  a <time>'s machine-readable moment, the same way
//   data-class="status"        a class: the value at that path
//
// A path is read from the item a <template> was copied for, and from the data around it when the item has no
// such field, so a pool's row can read "chain.tip_height" from its fleet. "." is the item itself.

import * as f from "./format.js";

const NONE = "–";

// The value at `path`, read from the innermost scope that has its first name.
function read(scope, path) {
  if (path === ".") return scope.data;
  const [first, ...rest] = path.split(".");
  let at = scope;
  while (at && (at.data === null || typeof at.data !== "object" || !(first in at.data))) at = at.parent;
  if (!at) return undefined;
  return rest.reduce((value, name) => (value === null || value === undefined ? undefined : value[name]), at.data[first]);
}

const present = (value) =>
  !(value === null || value === undefined || value === false || value === "" || (Array.isArray(value) && value.length === 0));

function test(scope, condition) {
  const negated = condition.startsWith("!");
  const [path, equals] = (negated ? condition.slice(1) : condition).split("=");
  const value = read(scope, path);
  const shown = equals === undefined ? present(value) : present(value) && String(value) === equals;
  return negated ? !shown : shown;
}

// The value `value` as `name` writes it, or as itself.
function write(value, name, formats) {
  if (!name) return Array.isArray(value) ? value.join(", ") : String(value);
  if (!formats[name]) throw new Error(`no format named "${name}"`);
  return formats[name](value);
}

const missing = (value) => value === null || value === undefined || (Array.isArray(value) && value.length === 0);

function text(scope, element, formats) {
  const value = read(scope, element.dataset.bind);
  return missing(value) ? (element.dataset.empty ?? NONE) : write(value, element.dataset.format, formats);
}

// An attribute's value from "path" or "path|format"; null, to leave the attribute out, when the value is missing.
function attribute(scope, spec, formats) {
  const [path, name] = spec.split("|");
  const value = read(scope, path);
  return missing(value) ? null : write(value, name, formats);
}

function set(element, name, value) {
  if (value === null) element.removeAttribute(name);
  else element.setAttribute(name, value);
}

// Fills one element's own attributes; true when its children should be filled too.
function apply(element, scope, formats) {
  const d = element.dataset;
  if (d.if !== undefined) {
    element.hidden = !test(scope, d.if);
    if (element.hidden) return false;
  }
  if (d.bind !== undefined) element.textContent = text(scope, element, formats);
  if (d.href !== undefined) {
    element.href = d.href.replace(/\{([^}]+)\}/g, (_, path) => encodeURIComponent(read(scope, path) ?? ""));
  }
  if (d.link !== undefined) {
    const url = read(scope, d.link);
    set(element, "href", typeof url === "string" && /^https?:\/\//i.test(url) ? url : null);
  }
  if (d.title !== undefined) set(element, "title", attribute(scope, d.title, formats));
  if (d.datetime !== undefined) set(element, "datetime", attribute(scope, d.datetime, formats));
  if (d.class !== undefined) {
    // The class this element was given last time comes off first, so a device that goes offline loses "online".
    if (element.bound) element.classList.remove(element.bound);
    element.bound = read(scope, d.class) || null;
    if (element.bound) element.classList.add(element.bound);
  }
  if (d.each !== undefined) {
    const template = element.querySelector(":scope > template");
    const items = read(scope, d.each) || [];
    const copies = items.map((item) => {
      const copy = template.content.cloneNode(true);
      walk(copy, { data: item, parent: scope }, formats);
      return copy;
    });
    element.replaceChildren(template, ...copies);
    return false;
  }
  return d.bind === undefined;
}

function walk(node, scope, formats) {
  for (const element of node.children) {
    if (element.tagName === "TEMPLATE") continue;
    if (apply(element, scope, formats)) walk(element, scope, formats);
  }
}

// Fills `root` and everything in it from `data`. `formats` adds to, or replaces, the functions of format.js.
export function bind(root, data, formats = {}) {
  const all = { ...f, ...formats };
  const scope = { data, parent: null };
  if (apply(root, scope, all)) walk(root, scope, all);
}

// Shows the one element in `root` marked data-state="<name>", and hides the others: loading, ready, failed.
export function state(root, name) {
  for (const element of root.querySelectorAll("[data-state]")) element.hidden = element.dataset.state !== name;
}
