// The OpenADR inspector: reads this VTN's own API with credentials the user supplies.
// Everything the VTN returns is inserted as text (textContent, attributes) — never as markup.
"use strict";

const BASE = document.querySelector('meta[name="openadr-base"]').content;
const REFRESH_MS = 5000;
const HOUR = 3600e3;
const WINDOW = { before: 1 * HOUR, after: 23 * HOUR };

const state = { view: "timeline", tokens: {}, ven: null, timer: null };

// ---- small DOM helpers ------------------------------------------------------------------------

function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === "class") node.className = value;
    else if (key === "text") node.textContent = value;
    else node.setAttribute(key, value);
  }
  for (const child of children) node.append(child);
  return node;
}

function clear(node) {
  while (node.firstChild) node.firstChild.remove();
}

const $ = (id) => document.getElementById(id);

// ---- credentials -----------------------------------------------------------------------------

// `#bl=id:secret&ven=id:secret`, as `openadr dev` prints it. A fragment never reaches the server;
// it is moved to sessionStorage and dropped from the address bar at once.
function credentialsFromFragment() {
  if (!location.hash.startsWith("#")) return null;
  const params = new URLSearchParams(location.hash.slice(1));
  history.replaceState(null, "", location.pathname);
  if (!params.get("bl")) return null;
  return { bl: params.get("bl"), ven: params.get("ven") || "" };
}

function savedCredentials() {
  try {
    return JSON.parse(sessionStorage.getItem("openadr-inspector") || "null");
  } catch {
    return null;
  }
}

async function token(pair) {
  const i = pair.indexOf(":");
  if (i < 1) throw new Error("expected id:secret");
  const body = new URLSearchParams({
    grant_type: "client_credentials",
    client_id: pair.slice(0, i),
    client_secret: pair.slice(i + 1),
  });
  const response = await fetch(`${BASE}/auth/token`, { method: "POST", body });
  if (!response.ok) throw new Error(`${pair.slice(0, i)}: the token endpoint answered ${response.status}`);
  return (await response.json()).access_token;
}

async function signIn(credentials) {
  state.tokens.bl = await token(credentials.bl);
  state.tokens.ven = credentials.ven ? await token(credentials.ven) : null;
  state.ven = credentials.ven ? credentials.ven.slice(0, credentials.ven.indexOf(":")) : null;
  sessionStorage.setItem("openadr-inspector", JSON.stringify(credentials));
  $("who").textContent = state.ven ? `business logic + ${state.ven}` : "business logic";
}

// ---- API ---------------------------------------------------------------------------------------

async function api(path, role = "bl", options = {}) {
  const url = path.startsWith("/admin") || path === "/health" ? path : `${BASE}/${path}`;
  const response = await fetch(url, {
    ...options,
    headers: { Authorization: `Bearer ${state.tokens[role]}`, Accept: "application/json" },
  });
  if (response.status === 401) {
    sessionStorage.removeItem("openadr-inspector");
    throw new Error("signed out: the token was refused");
  }
  if (!response.ok) throw new Error(`${path} answered ${response.status}`);
  return response.json();
}

// ---- time --------------------------------------------------------------------------------------

function durationMs(iso) {
  if (!iso || /^P9999Y/.test(iso)) return Infinity;
  const m = /^P(?:(\d+)D)?(?:T(?:(\d+)H)?(?:(\d+)M)?(?:(\d+(?:\.\d+)?)S)?)?$/.exec(iso);
  if (!m) return 0;
  return ((+m[1] || 0) * 24 * 3600 + (+m[2] || 0) * 3600 + (+m[3] || 0) * 60 + (+m[4] || 0)) * 1000;
}

function startMs(iso, fallback) {
  if (!iso) return fallback;
  if (iso.startsWith("0001-01-01")) return fallback; // the "now" sentinel
  const t = Date.parse(iso);
  return Number.isNaN(t) ? fallback : t;
}

const clock = (t) => new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

// The intervals of an event as [start, end, payloads], each interval inheriting the event's
// period where it does not override it.
function intervals(event, now) {
  const period = event.intervalPeriod || {};
  const base = startMs(period.start, now);
  const step = durationMs(period.duration);
  return (event.intervals || []).map((interval, i) => {
    const own = interval.intervalPeriod || {};
    const start = own.start ? startMs(own.start, base) : base + (Number.isFinite(step) ? i * step : 0);
    const length = own.duration ? durationMs(own.duration) : step;
    return { start, end: start + length, payloads: interval.payloads || [] };
  });
}

const firstNumber = (payloads) => {
  for (const p of payloads) for (const v of p.values || []) if (typeof v === "number") return v;
  return null;
};

const describe = (payloads) =>
  payloads.map((p) => `${p.type} ${(p.values || []).map((v) => (typeof v === "object" ? JSON.stringify(v) : v)).join(", ")}`).join("\n");

// ---- views -------------------------------------------------------------------------------------

async function renderTimeline() {
  const [programs, events] = await Promise.all([api("programs?limit=50"), api("events?limit=50")]);
  const names = Object.fromEntries(programs.map((p) => [p.id, p.programName]));
  const now = Date.now();
  const from = now - WINDOW.before;
  const to = now + WINDOW.after;
  const x = (t) => `${(100 * (Math.min(Math.max(t, from), to) - from)) / (to - from)}%`;

  const axis = $("axis");
  clear(axis);
  for (let t = Math.ceil(from / (3 * HOUR)) * 3 * HOUR; t < to; t += 3 * HOUR) {
    const tick = el("span", { text: clock(t) });
    tick.style.left = x(t);
    axis.append(tick);
  }

  const rows = $("rows");
  clear(rows);
  const sorted = [...events].sort((a, b) => (names[a.programID] || "").localeCompare(names[b.programID] || ""));
  for (const event of sorted) {
    const spans = intervals(event, now).filter((s) => s.end > from && s.start < to);
    const values = spans.map((s) => firstNumber(s.payloads)).filter((v) => v !== null);
    const lo = Math.min(...values);
    const hi = Math.max(...values);
    const lane = el("div", { class: "lane" });
    for (const span of spans) {
      const seg = el("div", { class: "seg" });
      seg.style.left = x(span.start);
      seg.style.width = `calc(${x(span.end)} - ${x(span.start)})`;
      const v = firstNumber(span.payloads);
      seg.style.setProperty("--a", v === null || hi === lo ? "0.55" : String(0.2 + (0.75 * (v - lo)) / (hi - lo)));
      seg.dataset.tip = `${describe(span.payloads)}\n${clock(span.start)} – ${Number.isFinite(span.end) ? clock(span.end) : "∞"}`;
      lane.append(seg);
    }
    const label = el(
      "div",
      { class: "label" },
      el("b", { text: event.eventName || event.id, title: event.id }),
      el("small", { text: `${names[event.programID] || event.programID} · ${(event.intervals || []).length} intervals` }),
    );
    rows.append(el("div", { class: "row" }, label, lane));
  }
  const marker = el("div", { class: "now" });
  marker.style.left = `calc(14rem + (100% - 14rem) * ${(now - from) / (to - from)})`;
  rows.append(marker);
  $("timeline-empty").hidden = events.length > 0;
}

async function renderPrivacy() {
  const [programs, events] = await Promise.all([api("programs?limit=50"), api("events?limit=50")]);
  const names = Object.fromEntries(programs.map((p) => [p.id, p.programName]));
  const rows = $("privacy-rows");
  clear(rows);
  $("privacy-noven").hidden = Boolean(state.tokens.ven);
  $("ven-col").textContent = state.ven ? `Visible to ${state.ven}` : "VEN";

  let visible = new Set();
  if (state.tokens.ven) {
    const seen = await api("events?limit=50", "ven");
    visible = new Set(seen.map((e) => e.id));
  }
  const summary = $("privacy-summary");
  clear(summary);
  summary.append(
    el("div", { class: "tile" }, el("b", { text: String(events.length) }), el("span", { text: "events business logic sees" })),
  );
  if (state.tokens.ven) {
    summary.append(
      el("div", { class: "tile" }, el("b", { text: String(visible.size) }), el("span", { text: `events ${state.ven} sees` })),
      el("div", { class: "tile" }, el("b", { text: String(events.length - visible.size) }), el("span", { text: `hidden from ${state.ven}` })),
    );
  }
  for (const event of events) {
    const targets = el("td");
    for (const t of event.targets || []) targets.append(el("span", { class: "tag", text: t }));
    if (!(event.targets || []).length) targets.append(el("span", { class: "muted", text: "untargeted" }));
    const seen = el("td");
    if (state.tokens.ven) {
      const yes = visible.has(event.id);
      seen.append(el("span", { class: yes ? "yes" : "no", text: yes ? "visible" : "hidden" }));
    }
    rows.append(
      el("tr", {}, el("td", { text: event.eventName || event.id }), el("td", { text: names[event.programID] || event.programID }), targets, seen),
    );
  }
  if (!events.length) rows.append(el("tr", {}, el("td", { class: "empty-row", colspan: "4", text: "No events." })));
}

async function renderDeliveries() {
  const [health, dead, subscribers] = await Promise.all([api("/health"), api("/admin/outbox"), api("/admin/subscribers")]);
  const tiles = $("health");
  clear(tiles);
  const tile = (value, label) => el("div", { class: "tile" }, el("b", { text: String(value) }), el("span", { text: label }));
  const age = health.outbox.oldestPendingSeconds;
  tiles.append(
    tile(health.outbox.pending, "pending notifications"),
    tile(age === null ? "—" : `${age}s`, "oldest pending"),
    tile(health.outbox.dead, "abandoned"),
    tile(health.subscribersCutOff, "subscribers cut off"),
    tile(health.reports.count, "reports stored"),
  );
  const deadRows = $("dead-rows");
  clear(deadRows);
  for (const d of dead) {
    deadRows.append(
      el("tr", {},
        el("td", { text: new Date(d.enqueuedAt).toLocaleString() }),
        el("td", { text: `${d.objectType} ${d.objectId}` }),
        el("td", { text: d.destination }),
        el("td", { text: String(d.attempts) }),
        el("td", { text: d.lastError || "" })),
    );
  }
  if (!dead.length) deadRows.append(el("tr", {}, el("td", { class: "empty-row", colspan: "5", text: "Nothing abandoned." })));
  const subRows = $("subscriber-rows");
  clear(subRows);
  for (const s of subscribers) {
    subRows.append(
      el("tr", {},
        el("td", { text: s.subscriptionId }),
        el("td", { text: String(s.consecutiveFailures) }),
        el("td", { text: s.retryAt ? new Date(s.retryAt).toLocaleString() : "—" }),
        el("td", { text: s.lastError || "" })),
    );
  }
  if (!subscribers.length) subRows.append(el("tr", {}, el("td", { class: "empty-row", colspan: "4", text: "Every subscriber is healthy." })));
}

const VIEWS = { timeline: renderTimeline, privacy: renderPrivacy, deliveries: renderDeliveries };

async function refresh() {
  try {
    await VIEWS[state.view]();
    $("updated").textContent = `updated ${clock(Date.now())}`;
  } catch (error) {
    $("updated").textContent = error.message;
    if (/signed out/.test(error.message)) showSignIn(error.message);
  }
}

function show(view) {
  state.view = view;
  for (const tab of document.querySelectorAll(".tab")) tab.classList.toggle("active", tab.dataset.view === view);
  for (const name of Object.keys(VIEWS)) $(name).hidden = name !== view;
  refresh();
}

function showSignIn(message) {
  clearInterval(state.timer);
  for (const name of Object.keys(VIEWS)) $(name).hidden = true;
  $("signin").hidden = false;
  const error = $("signin-error");
  error.hidden = !message;
  error.textContent = message || "";
}

async function start(credentials) {
  await signIn(credentials);
  $("signin").hidden = true;
  show(state.view);
  clearInterval(state.timer);
  state.timer = setInterval(refresh, REFRESH_MS);
}

// ---- wiring ------------------------------------------------------------------------------------

for (const tab of document.querySelectorAll(".tab")) tab.addEventListener("click", () => show(tab.dataset.view));

$("signin-form").addEventListener("submit", async (submit) => {
  submit.preventDefault();
  const form = new FormData(submit.target);
  try {
    await start({ bl: form.get("bl").trim(), ven: form.get("ven").trim() });
  } catch (error) {
    showSignIn(error.message);
  }
});

$("retry").addEventListener("click", async () => {
  const button = $("retry");
  button.disabled = true;
  try {
    const result = await api("/admin/outbox/retry", "bl", { method: "POST" });
    $("retry-result").textContent = `${result.revived} notification(s) due again, ${result.subscribersRestored} subscriber(s) restored`;
    await refresh();
  } catch (error) {
    $("retry-result").textContent = error.message;
  } finally {
    button.disabled = false;
  }
});

const tip = $("tip");
document.addEventListener("mousemove", (move) => {
  const target = move.target.closest && move.target.closest("[data-tip]");
  tip.hidden = !target;
  if (target) {
    tip.textContent = target.dataset.tip;
    tip.style.left = `${move.clientX + 12}px`;
    tip.style.top = `${move.clientY + 12}px`;
  }
});

const credentials = credentialsFromFragment() || savedCredentials();
if (credentials) start(credentials).catch((error) => showSignIn(error.message));
else showSignIn();
