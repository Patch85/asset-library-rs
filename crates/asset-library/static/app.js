"use strict";

const $ = (s, el = document) => el.querySelector(s);
const PAGE = 200;

const state = {
  q: "", sort: "name",
  items: [], total: 0, loading: false, done: false, gen: 0,
  selected: new Set(), anchor: null,
  tags: [], sources: [], meta: { asset_types: [], sorts: [] },
  fields: [], facets: [],
  counts: null, countsGen: 0, find: {}, expanded: new Set(),
};

// ------------------------------------------------------------------ utils

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

async function api(path, opts = {}) {
  const init = { ...opts };
  if (opts.body !== undefined && typeof opts.body !== "string") {
    init.body = JSON.stringify(opts.body);
    init.headers = { "Content-Type": "application/json" };
  }
  const res = await fetch(path, init);
  if (!res.ok) {
    let msg = res.statusText;
    try { msg = (await res.json()).detail || msg; } catch {}
    throw new Error(msg);
  }
  return opts.raw ? res : res.json();
}

/** In-page prompt/confirm (webviews don't all show the native ones). With `input`, resolves
 *  to the text or null; otherwise to true/false. */
function ask({ title, message = "", input = false, value = "", ok = "OK", danger = false }) {
  const dlg = $("#ask-dialog");
  const f = $("form", dlg);
  $(".title", dlg).textContent = title;
  $(".message", dlg).textContent = message;
  f.answer.classList.toggle("hidden", !input);
  f.answer.value = value;
  const okBtn = $('button[value="ok"]', dlg);
  okBtn.textContent = ok;
  okBtn.classList.toggle("danger", danger);
  dlg.showModal();
  if (input) f.answer.select();
  return new Promise((resolve) => dlg.addEventListener("close", () => {
    const yes = dlg.returnValue === "ok";
    resolve(input ? (yes ? f.answer.value : null) : yes);
  }, { once: true }));
}

let toastTimer;
function toast(msg, error = false, action = null) {
  const t = $("#toast");
  t.textContent = msg;
  if (action) {
    const b = Object.assign(document.createElement("button"), { textContent: action.label });
    b.addEventListener("click", () => { t.classList.add("hidden"); action.run(); });
    t.append(" ", b);
  }
  t.classList.toggle("error", error);
  t.classList.remove("hidden");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.add("hidden"), error || action ? 6000 : 2500);
}

const store = {
  get(k, d) { try { return JSON.parse(localStorage.getItem(k)) ?? d; } catch { return d; } },
  set(k, v) { try { localStorage.setItem(k, JSON.stringify(v)); } catch {} },
};

function debounce(fn, ms) {
  let t;
  return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); };
}

function humanSize(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1 << 20) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1 << 20)).toFixed(1)} MB`;
}

// ------------------------------------------------------------------ query tokens

const TOKEN_RE = /-?\w+:"[^"]*"|-?"[^"]*"|\S+/g;
const tokens = (q) => q.match(TOKEN_RE) || [];
const quoteVal = (v) => (/\s/.test(v) ? `"${v}"` : v);

function setQuery(q) {
  state.q = q.trim().replace(/\s+/g, " ");
  $("#q").value = state.q;
  store.set("q", state.q);
  reload();
}

/** Toggle `key:value` in the query. With tristate, cycles include -> exclude -> off. */
function toggleFilter(key, value, tristate = false) {
  const pos = `${key}:${quoteVal(value)}`;
  const neg = `-${pos}`;
  const toks = tokens(state.q);
  const has = toks.includes(pos), hasNeg = toks.includes(neg);
  let rest = toks.filter((t) => t !== pos && t !== neg);
  if (!has && !hasNeg) rest.push(pos);
  else if (has && tristate) rest.push(neg);
  setQuery(rest.join(" "));
}

/** Add or remove one exact query token. */
function toggleToken(tok) {
  const toks = tokens(state.q);
  setQuery((toks.includes(tok) ? toks.filter((t) => t !== tok) : [...toks, tok]).join(" "));
}

function filterState(key, value) {
  const pos = `${key}:${quoteVal(value)}`;
  const toks = tokens(state.q);
  return toks.includes(pos) ? "on" : toks.includes(`-${pos}`) ? "off" : "";
}

// ------------------------------------------------------------------ sidebar

const CAP = 8; // values shown per list until "Show all"
const FILTER_AT = 12; // lists longer than this get a filter box

/** Count for a sidebar value: over the current results once /api/counts answers, else `fallback`. */
function countOf(group, key, fallback) {
  if (!state.counts) return fallback;
  const g = group.startsWith("fields:") ? state.counts.fields[group.slice(7)] : state.counts[group];
  return g?.[key] ?? 0;
}

/** A capped, filterable list. Entries: {attrs, label, n, st ("on"|"off"|""), title}. Active
 *  entries are pinned first and always shown; the rest hide at zero results. */
function listHtml(key, entries, noun) {
  const f = (state.find[key] || "").trim().toLowerCase();
  const avail = entries.filter((e) => !e.st && e.n !== 0);
  const pinned = entries.filter((e) => e.st);
  const rest = avail.filter((e) => !f || e.label.toLowerCase().includes(f));
  const cap = Math.max(CAP - pinned.length, 3);
  const all = f || state.expanded.has(key) || rest.length <= cap + 2; // "Show all" hides at least 3
  const shown = all ? rest : rest.slice(0, cap);
  const li = (e) => `<li ${e.attrs} class="${e.st === "on" ? "active" : e.st === "off" ? "excluded" : ""}"
      title="${esc(e.title || "")}"><span class="label">${esc(e.label)}</span>${e.n == null ? "" : `<span class="n">${e.n.toLocaleString()}</span>`}</li>`;
  let more = "";
  if (!f && rest.length > shown.length) more = `<li class="more" data-more="${esc(key)}">Show all ${rest.length.toLocaleString()}</li>`;
  else if (!f && all && rest.length > CAP) more = `<li class="more" data-more="${esc(key)}">Show fewer</li>`;
  else if (f && !rest.length) more = `<li class="muted">No ${noun} match</li>`;
  const box = avail.length > FILTER_AT || f
    ? `<input type="search" class="list-filter" data-find="${esc(key)}" value="${esc(state.find[key] || "")}"
         placeholder="Filter ${avail.length.toLocaleString()} ${noun}…" autocomplete="off">` : "";
  return `${box}<ul class="nav">${pinned.map(li).join("")}${shown.map(li).join("")}${more}</ul>`;
}

/** Sidebar sections that hold a filter on `key:` (for the badge on a collapsed section). */
function activeIn(prefixes) {
  return tokens(state.q).filter((t) => prefixes.some((p) => t.replace(/^-/, "").startsWith(`${p}:`))).length;
}

function renderSections() {
  const collapsed = new Set(store.get("collapsed", []));
  const prefixes = {
    library: ["is"], types: ["type"], colors: ["color"], tags: ["tag"], sources: ["source"],
    props: state.facets.map((fc) => fc.key.toLowerCase()).concat("has"),
  };
  document.querySelectorAll(".sidebar [data-sec]").forEach((sec) => {
    const key = sec.dataset.sec;
    const closed = collapsed.has(key);
    sec.classList.toggle("collapsed", closed);
    const h = sec.querySelector(":scope > h3, :scope > h4");
    const n = closed ? activeIn(prefixes[key] || [key.replace(/^prop:/, "")]) : 0;
    let badge = h.querySelector(".badge");
    if (!n) badge?.remove();
    else {
      badge ??= h.querySelector(".sec-title").insertAdjacentElement("afterend", Object.assign(document.createElement("span"), { className: "badge" }));
      badge.textContent = n;
      badge.title = `${n} active filter${n === 1 ? "" : "s"}`;
    }
  });
}

function renderSidebar() {
  // Re-rendering replaces the list filter boxes; keep focus and caret in the one being typed in.
  const focused = document.activeElement?.dataset?.find;
  const caret = focused != null ? document.activeElement.selectionStart : 0;

  for (const li of $("#quick").children) {
    const q = li.dataset.q;
    li.classList.toggle("active", q ? tokens(state.q).includes(q) : state.q === "");
  }

  $("#types").innerHTML = state.meta.asset_types.map((t) => ({ t, n: countOf("types", t, null), st: filterState("type", t) }))
    .filter((x) => x.st || x.n !== 0)
    .map(({ t, n, st }) => `<li data-type="${esc(t)}" class="${st ? "active" : ""}">
       <span class="label">${esc(t)}</span>${n == null ? "" : `<span class="n">${n.toLocaleString()}</span>`}</li>`).join("");

  $("#colors").innerHTML = Object.entries(state.meta.colors || {}).map(([c, hex]) => {
    const st = filterState("color", c);
    return `<button class="swatch ${st}" data-color="${c}" title="${c} (click again to exclude)"
              style="--c:${hex}"></button>`;
  }).join("");

  // Sources stay listed at zero (they're also where folders are managed).
  $("#sources").innerHTML = state.sources.length
    ? state.sources.map((s) => {
      const n = countOf("sources", String(s.id), s.count);
      return `<li data-source="${s.id}" title="${esc(s.path)}" class="${filterState("source", String(s.id)) ? "active" : ""} ${n ? "" : "dim"}">
         <span class="label">${esc(s.name)}</span><span class="n">${n.toLocaleString()}</span></li>`;
    }).join("")
    : `<li class="muted" id="no-sources">Add a folder to begin</li>`;

  const toks = tokens(state.q);
  const props = state.facets.map((fc) => {
    const k = fc.key.toLowerCase();
    const entries = fc.values.map((v) => ({
      attrs: `data-pkey="${esc(fc.key)}" data-pval="${esc(v.value)}"`,
      label: String(v.value),
      n: countOf(`fields:${k}`, String(v.value), v.count),
      st: filterState(fc.key, String(v.value)),
      title: "Click: include, again: exclude, again: clear",
    }));
    // Choices keep their option order; other values go most-used first.
    if (fc.kind === "text") entries.sort((a, b) => b.n - a.n || a.label.localeCompare(b.label));
    const notSet = toks.includes(`-has:${fc.key}`);
    if (!notSet && !entries.some((e) => e.st || e.n)) return "";
    return `<div class="sub-sec" data-sec="prop:${esc(k)}"><h4><span class="sec-title">${esc(fc.label)}</span></h4>
      ${listHtml(`prop:${k}`, entries, "values")}
      <ul class="nav"><li class="${notSet ? "active" : ""}" data-phas="${esc(fc.key)}"
          title="Assets of this kind without a ${esc(fc.label)}"><span class="label muted">not set</span></li></ul></div>`;
  }).join("");
  $("#props").innerHTML = props || (state.facets.some((fc) => fc.values.length)
    ? '<p class="muted small">None in these results.</p>'
    : '<p class="muted small">None set yet. Select assets to set them, or ✎ to edit.</p>');

  document.querySelectorAll("datalist.fv").forEach((d) => d.remove());
  for (const fc of state.facets.filter((x) => x.kind === "text")) {
    const d = Object.assign(document.createElement("datalist"), { id: `fv-${fc.key}`, className: "fv" });
    d.innerHTML = fc.values.map((v) => `<option value="${esc(v.value)}">`).join("");
    document.body.append(d);
  }

  const tagEntries = state.tags.map((t) => ({
    attrs: `data-tag="${esc(t.name)}"`, label: t.name, n: countOf("tags", t.name, t.count),
    st: filterState("tag", t.name), title: "Click: include, again: exclude, again: clear. Right-click: rename/delete",
  })).sort((a, b) => b.n - a.n || a.label.localeCompare(b.label));
  $("#tags").innerHTML = state.tags.length ? listHtml("tags", tagEntries, "tags") : '<p class="muted small">No tags yet.</p>';

  $("#tag-datalist")?.remove();
  const dl = document.createElement("datalist");
  dl.id = "tag-datalist";
  dl.innerHTML = state.tags.map((t) => `<option value="${esc(t.name)}">`).join("");
  document.body.append(dl);

  renderSections();
  renderChips();
  if (focused != null) {
    const box = document.querySelector(`.list-filter[data-find="${CSS.escape(focused)}"]`);
    box?.focus();
    box?.setSelectionRange(caret, caret);
  }
}

const FLAG_LABELS = { fav: "Favorites", untagged: "Untagged", missing: "Missing files", transparent: "Transparent" };

/** What a query token means, for its chip. */
function chipLabel(tok) {
  const m = tok.match(/^(\w+):"?([^"]*)"?$/);
  if (tok.startsWith('~"')) return ["Described as", tok.slice(2, -1)];
  if (!m) return ["", tok];
  const [, key, val] = m;
  if (key === "is") return ["", FLAG_LABELS[val] || val];
  if (key === "similar") return ["Looks like", `#${val}`];
  if (key === "source") return ["Source", state.sources.find((s) => String(s.id) === val)?.name || val];
  if (key === "has") return ["Has", state.facets.find((f) => f.key === val)?.label || val];
  const fc = state.facets.find((f) => f.key.toLowerCase() === key.toLowerCase());
  return [fc ? fc.label : key[0].toUpperCase() + key.slice(1), val];
}

function renderChips() {
  const toks = tokens(state.q);
  const box = $("#chips");
  box.classList.toggle("hidden", !toks.length);
  box.innerHTML = toks.map((tok, i) => {
    const neg = tok.startsWith("-") && tok.length > 1;
    const [key, val] = chipLabel(neg ? tok.slice(1) : tok);
    return `<span class="chip ${neg ? "excluded" : ""}" title="${esc(tok)}">
      ${neg ? '<span class="muted">not</span> ' : ""}${key ? `<span class="muted">${esc(key)}</span> ` : ""}${esc(val)}
      <button class="x" data-drop="${i}" title="Remove">×</button></span>`;
  }).join("") + (toks.length > 1 ? '<button class="ghost small" data-drop="all">Clear all</button>' : "");
}

async function loadCounts() {
  const gen = ++state.countsGen;
  try {
    const c = await api(`/api/counts?q=${encodeURIComponent(state.q)}`);
    if (gen === state.countsGen) { state.counts = c; renderSidebar(); }
  } catch {
    if (gen === state.countsGen) { state.counts = null; renderSidebar(); } // e.g. AI off: global counts
  }
}

async function refreshSidebar() {
  [state.tags, state.sources, state.facets] =
    await Promise.all([api("/api/tags"), api("/api/sources"), api("/api/facets")]);
  renderSidebar();
  loadCounts();
}

// ------------------------------------------------------------------ grid

function reload() {
  state.gen++;
  state.loading = false; // any in-flight load belongs to the old generation and is discarded
  state.items = [];
  state.done = false;
  state.selected.clear();
  state.anchor = null;
  $("#grid").innerHTML = "";
  renderSidebar();
  loadCounts();
  renderDetail();
  loadMore();
}

async function loadMore() {
  if (state.loading || state.done) return;
  state.loading = true;
  const gen = state.gen;
  try {
    const p = new URLSearchParams({ q: state.q, sort: state.sort, limit: PAGE, offset: state.items.length });
    const data = await api(`/api/assets?${p}`);
    if (gen !== state.gen) return;
    const start = state.items.length;
    state.items.push(...data.items);
    state.total = data.total;
    state.done = state.items.length >= data.total || data.items.length === 0;
    $("#grid").insertAdjacentHTML("beforeend", data.items.map((it, i) => cardHtml(it, start + i)).join(""));
    renderCounts();
  } catch (e) {
    toast(e.message, true);
  } finally {
    if (gen === state.gen) state.loading = false;
  }
  // keep filling if the sentinel is still on screen
  if (!state.done && gen === state.gen && isVisible($("#sentinel"))) loadMore();
}

function isVisible(el) {
  const r = el.getBoundingClientRect();
  return r.top < window.innerHeight + 400;
}

function cardHtml(it, idx) {
  const cls = ["card", state.selected.has(it.id) && "selected", it.missing && "missing"].filter(Boolean).join(" ");
  const title = `${it.name}${it.width ? ` — ${it.width}×${it.height}` : ""}${it.tags.length ? `\n${it.tags.join(", ")}` : ""}`;
  return `<div class="${cls}" data-idx="${idx}" title="${esc(title)}">
    <div class="img"><img src="/api/assets/${it.id}/thumb" loading="lazy" draggable="true" alt="" onerror="this.replaceWith(Object.assign(document.createElement('span'),{className:'broken',textContent:'unreadable'}))"></div>
    <div class="name">${esc(it.name)}</div>
    ${it.favorite ? '<span class="fav">★</span>' : ""}
  </div>`;
}

function rerenderCard(idx) {
  const el = $(`.card[data-idx="${idx}"]`);
  if (el) el.outerHTML = cardHtml(state.items[idx], idx);
}

function renderCounts() {
  const ranked = /(^|\s)~|similar:\d/.test(state.q);
  $("#count").textContent = ranked
    ? `Top ${state.total} matches, best first`
    : `${state.total.toLocaleString()} item${state.total === 1 ? "" : "s"}`;
  const n = state.selected.size;
  $("#sel-count").textContent = n ? `${n} selected` : "";
  $("#export-results").classList.toggle("hidden", ranked || state.total === 0);
  const empty = $("#empty");
  empty.classList.toggle("hidden", state.total > 0 || state.loading);
  empty.innerHTML = state.sources.length
    ? "Nothing matches this search."
    : 'No folders yet. <button id="empty-add" class="primary">Add your token folder</button>';
}

function syncSelectionClasses() {
  for (const el of $("#grid").children) {
    const it = state.items[+el.dataset.idx];
    el.classList.toggle("selected", state.selected.has(it.id));
  }
  renderCounts();
  renderDetail();
}

function onCardClick(e) {
  const card = e.target.closest(".card");
  if (!card) return;
  const idx = +card.dataset.idx;
  const id = state.items[idx].id;
  if (e.shiftKey && state.anchor !== null) {
    const [a, b] = [Math.min(state.anchor, idx), Math.max(state.anchor, idx)];
    if (!e.ctrlKey && !e.metaKey) state.selected.clear();
    for (let i = a; i <= b; i++) state.selected.add(state.items[i].id);
  } else if (e.ctrlKey || e.metaKey) {
    state.selected.has(id) ? state.selected.delete(id) : state.selected.add(id);
    state.anchor = idx;
  } else {
    state.selected.clear();
    state.selected.add(id);
    state.anchor = idx;
  }
  syncSelectionClasses();
}

function onDragStart(e) {
  const card = e.target.closest(".card");
  if (!card) return;
  const it = state.items[+card.dataset.idx];
  if (state.meta.desktop) {
    // A real file drag from the app, so drops work everywhere (and several files at once).
    e.preventDefault();
    const ids = state.selected.has(it.id) ? [...state.selected] : [it.id];
    api("/api/drag", { method: "POST", body: { ids } }).catch((err) => toast(err.message, true));
    return;
  }
  const url = new URL(`/api/assets/${it.id}/file`, location.href).href;
  // Chrome/Edge: dropping onto Explorer or the desktop saves the original file.
  const filename = it.rel_path.split("/").pop();
  e.dataTransfer.setData("DownloadURL", `application/octet-stream:${filename}:${url}?download=1`);
  e.dataTransfer.setData("text/uri-list", url);
  e.dataTransfer.setData("text/plain", url);
}

// ------------------------------------------------------------------ detail panel

const selectedItems = () => state.items.filter((it) => state.selected.has(it.id));
let detailGen = 0;

async function renderDetail() {
  const panel = $("#detail");
  const sel = selectedItems();
  const gen = ++detailGen;
  if (sel.length === 0) { panel.classList.add("hidden"); return; }
  panel.classList.remove("hidden");
  if (sel.length === 1) {
    try {
      const a = await api(`/api/assets/${sel[0].id}`);
      if (gen === detailGen) renderSingle(a);
    } catch (e) { toast(e.message, true); }
  } else {
    renderMulti(sel);
  }
}

// ------------------------------------------------------------------ properties

const fieldsFor = (type) => state.fields.filter((f) => f.asset_type === type);

/** One definition per key across `types`, in field order. */
function fieldsForTypes(types) {
  const out = [];
  for (const f of state.fields.filter((x) => types.includes(x.asset_type)))
    if (!out.some((o) => o.key === f.key)) out.push(f);
  return out.sort((a, b) => a.position - b.position);
}

/** Inputs for property values. With `multi`, empty means "leave as is" and keys in `mixed`
 *  show "(mixed)"; a choice can be cleared with its "clear" option. */
function fieldControls(fields, values, { multi = false, mixed = new Set() } = {}) {
  if (!fields.length) return "";
  return `<div class="props-grid">${fields.map((f) => {
    const v = values[f.key];
    const ph = mixed.has(f.key) ? "(mixed)" : "—";
    const input = f.kind === "choice"
      ? `<select data-field="${esc(f.key)}"><option value="">${ph}</option>${f.options.map((o) =>
          `<option ${v !== undefined && String(v).toLowerCase() === o.toLowerCase() ? "selected" : ""}>${esc(o)}</option>`).join("")}
         ${multi ? '<option value="__clear">clear</option>' : ""}</select>`
      : `<input data-field="${esc(f.key)}" ${f.kind === "number" ? 'type="number" step="any"' : `list="fv-${esc(f.key)}"`}
           value="${v === undefined ? "" : esc(v)}" placeholder="${ph}" autocomplete="off">`;
    return `<span>${esc(f.label)}</span>${input}`;
  }).join("")}</div>`;
}

function bindFieldControls(root, multi, onChange) {
  root.querySelectorAll("[data-field]").forEach((el) => el.addEventListener("change", () => {
    const raw = el.value.trim();
    if (raw === "__clear") return onChange(el.dataset.field, null);
    if (raw === "" && multi) return;
    onChange(el.dataset.field, raw === "" ? null : el.type === "number" ? Number(raw) : raw);
  }));
}

function typeOptions(current, placeholder) {
  return (placeholder ? `<option value="">${placeholder}</option>` : "") +
    state.meta.asset_types.map((t) => `<option ${t === current ? "selected" : ""}>${esc(t)}</option>`).join("");
}

function renderSingle(a) {
  const panel = $("#detail");
  panel.innerHTML = `
    <div class="preview"><img src="/api/assets/${a.id}/file" alt=""></div>
    <label>Name <input id="d-name" value="${esc(a.name)}"></label>
    <label>Tags
      <div class="chips">${a.tags.map((t) => `<span class="chip">${esc(t)}<button data-rm="${esc(t)}" title="Remove">×</button></span>`).join("")}</div>
      <input id="tag-input" list="tag-datalist" placeholder="Add tags (Enter or comma)…">
    </label>
    <div id="suggestions"></div>
    <label>Type <select id="d-type">${typeOptions(a.asset_type)}</select></label>
    ${fieldControls(fieldsFor(a.asset_type), a.fields)}
    ${Object.keys(a.colors).length ? `<div class="colors-row">${Object.entries(a.colors).slice(0, 5).map(([c, f]) =>
      `<button class="swatch" data-color="${c}" style="--c:${state.meta.colors[c]}"
         title="${c} ${Math.round(f * 100)}% · click to find more ${c}"></button>`).join("")}</div>` : ""}
    <label>Notes <textarea id="d-notes" placeholder="Stat block, source, usage ideas…">${esc(a.notes)}</textarea></label>
    <div class="actions">
      <button id="d-fav">${a.favorite ? "★ Favorited" : "☆ Favorite"}</button>
      ${a.has_embedding ? '<button id="d-similar">Find similar</button>' : ""}
      <button id="d-reveal">Show in folder</button>
      <button id="d-open">Open</button>
      <button id="d-export">Export…</button>
    </div>
    <div class="meta">
      ${a.width ? `${a.width}×${a.height} px · ` : ""}${humanSize(a.size)}${a.has_alpha ? " · transparent" : ""}<br>
      ${a.missing ? '<b style="color:var(--danger)">File missing on disk</b><br>' : ""}
      ${esc(a.full_path)}
    </div>`;

  const save = async (patch) => {
    try {
      const updated = await api(`/api/assets/${a.id}`, { method: "PATCH", body: patch });
      const idx = state.items.findIndex((it) => it.id === a.id);
      if (idx >= 0) {
        Object.assign(state.items[idx], {
          name: updated.name, tags: updated.tags, favorite: updated.favorite, asset_type: updated.asset_type,
          fields: updated.fields,
        });
        rerenderCard(idx);
      }
      if (patch.tags || patch.fields || patch.asset_type) refreshSidebar();
      return updated;
    } catch (e) { toast(e.message, true); }
  };

  $("#d-name").addEventListener("change", (e) => save({ name: e.target.value }));
  $("#d-notes").addEventListener("change", (e) => save({ notes: e.target.value }));
  $("#d-type").addEventListener("change", async (e) => renderSingle(await save({ asset_type: e.target.value })));
  bindFieldControls(panel, false, async (key, value) => {
    const updated = await save({ fields: { [key]: value } });
    if (updated) renderSingle(updated);
  });
  $("#d-fav").addEventListener("click", async () => renderSingle(await save({ favorite: !a.favorite })));
  $("#d-reveal").addEventListener("click", () => api(`/api/assets/${a.id}/reveal`, { method: "POST" }));
  $("#d-open").addEventListener("click", () => openOriginal(a.id));
  $("#d-similar")?.addEventListener("click", () => setQuery(`similar:${a.id}`));
  panel.querySelectorAll(".colors-row [data-color]").forEach((b) =>
    b.addEventListener("click", () => toggleFilter("color", b.dataset.color)));
  renderSuggestions([a.id]);
  $("#d-export").addEventListener("click", openExport);
  panel.querySelectorAll("[data-rm]").forEach((b) => b.addEventListener("click", async () => {
    renderSingle(await save({ tags: a.tags.filter((t) => t !== b.dataset.rm) }));
  }));
  bindTagInput(async (names) => {
    renderSingle(await save({ tags: [...a.tags, ...names] }));
    $("#tag-input")?.focus();
  });
}

function renderMulti(sel) {
  const counts = new Map();
  for (const it of sel) for (const t of it.tags) counts.set(t, (counts.get(t) || 0) + 1);
  const tags = [...counts].sort((x, y) => y[1] - x[1] || x[0].localeCompare(y[0]));
  const allFav = sel.every((it) => it.favorite);
  const fields = fieldsForTypes([...new Set(sel.map((it) => it.asset_type))]);
  const common = {}, mixed = new Set();
  for (const f of fields) {
    const vals = new Set(sel.map((it) => JSON.stringify(it.fields?.[f.key] ?? null)));
    if (vals.size === 1) { const v = JSON.parse([...vals][0]); if (v !== null) common[f.key] = v; }
    else mixed.add(f.key);
  }

  $("#detail").innerHTML = `
    <h2>${sel.length} selected</h2>
    <label>Tags on selection <span class="muted">(dashed = only some)</span>
      <div class="chips">${tags.map(([t, n]) =>
        `<span class="chip ${n < sel.length ? "partial" : ""}">${esc(t)}<span class="n">${n}</span>
           <button data-rm="${esc(t)}" title="Remove from all selected">×</button></span>`).join("") || '<span class="muted">none</span>'}</div>
      <input id="tag-input" list="tag-datalist" placeholder="Add tags to all (Enter or comma)…">
    </label>
    <div id="suggestions"></div>
    <label>Type <select id="m-type">${typeOptions(null, "Set type…")}</select></label>
    ${fieldControls(fields, common, { multi: true, mixed })}
    <div class="actions">
      <button id="m-fav">${allFav ? "Unfavorite all" : "★ Favorite all"}</button>
      <button id="m-export" class="primary">Export ${sel.length}…</button>
      <button id="m-clear">Clear selection</button>
    </div>`;

  $("#m-type").addEventListener("change", (e) => e.target.value && bulk({ asset_type: e.target.value }));
  bindFieldControls($("#detail"), true, (key, value) => bulk({ set_fields: { [key]: value } }));
  $("#m-fav").addEventListener("click", () => bulk({ favorite: !allFav }));
  $("#m-export").addEventListener("click", openExport);
  $("#m-clear").addEventListener("click", () => { state.selected.clear(); syncSelectionClasses(); });
  $("#detail").querySelectorAll("[data-rm]").forEach((b) =>
    b.addEventListener("click", () => bulk({ remove_tags: [b.dataset.rm] })));
  bindTagInput(async (names) => { await bulk({ add_tags: names }); $("#tag-input")?.focus(); });
  renderSuggestions(sel.map((it) => it.id));
}

// ------------------------------------------------------------------ suggestions

let suggestGen = 0;
async function renderSuggestions(ids) {
  const gen = ++suggestGen;
  let list;
  try { list = await api("/api/suggestions", { method: "POST", body: { ids } }); }
  catch (e) { return; }
  const box = $("#suggestions");
  if (gen !== suggestGen || !box) return;
  if (!list.length) { box.innerHTML = ""; return; }
  const multi = ids.length > 1;
  box.innerHTML = `
    <label>Suggestions <span class="muted">click to add · × to dismiss</span></label>
    <div class="chips">${list.map((s, i) => `
      <span class="chip suggest" title="${esc(s.source)} · ${Math.round(s.score * 100)}% confident${
        multi ? ` · applies to ${s.count} of ${ids.length}` : ""}" style="--conf:${0.45 + s.score * 0.55}">
        <button data-accept="${i}">+ ${s.field ? `${esc(s.label)}: ${esc(s.value)}` : esc(s.tag)}</button>${
          multi ? `<span class="n">${s.count}</span>` : ""}
        <button data-dismiss="${i}" title="Dismiss">×</button></span>`).join("")}</div>
    <div class="actions small-actions">
      <button id="s-accept-all" class="ghost small">Add all ${list.length}</button>
      <button id="s-ignored" class="ghost small">Never-suggest list…</button>
    </div>`;

  const accept = async (items) => {
    for (const s of items) {
      await bulk(s.field ? { set_fields: { [s.field]: s.value } } : { add_tags: [s.tag] }, s.ids, false);
    }
    refreshSidebar();
    renderDetail();
  };
  box.querySelectorAll("[data-accept]").forEach((b) =>
    b.addEventListener("click", () => accept([list[+b.dataset.accept]])));
  box.querySelectorAll("[data-dismiss]").forEach((b) => b.addEventListener("click", async () => {
    const s = list[+b.dataset.dismiss];
    await api("/api/suggestions/reject", { method: "POST", body: { ids: s.ids, tag: s.tag } });
    renderSuggestions(ids);
    const what = s.field ? `${s.label}: ${s.value}` : s.tag;
    toast(`Dismissed "${what}"`, false, {
      label: "Never suggest it", run: async () => {
        await api("/api/suggestions/reject", { method: "POST", body: { tag: s.tag, everywhere: true } });
        toast(`"${what}" won't be suggested again`);
      },
    });
  }));
  $("#s-accept-all").addEventListener("click", () => accept(list));
  $("#s-ignored").addEventListener("click", openIgnored);
}

async function openIgnored() {
  const dlg = $("#ignored-dialog");
  const render = async () => {
    const tags = await api("/api/suggestions/ignored");
    $(".chips", dlg).innerHTML = tags.map((t) =>
      `<span class="chip">${esc(t)}<button data-unignore="${esc(t)}" title="Allow again">×</button></span>`
    ).join("") || '<span class="muted">Nothing yet.</span>';
    dlg.querySelectorAll("[data-unignore]").forEach((b) => b.addEventListener("click", async () => {
      await api(`/api/suggestions/ignored/${encodeURIComponent(b.dataset.unignore)}`, { method: "DELETE" });
      render();
    }));
  };
  await render();
  dlg.showModal();
}

async function bulk(patch, ids = [...state.selected], refresh = true) {
  const target = new Set(ids);
  try {
    await api("/api/assets/bulk", { method: "POST", body: { ids, ...patch } });
  } catch (e) { return toast(e.message, true); }
  state.items.forEach((it, idx) => {
    if (!target.has(it.id)) return;
    if (patch.add_tags) it.tags = [...new Set([...it.tags, ...patch.add_tags.map(normTag)])].sort();
    if (patch.remove_tags) it.tags = it.tags.filter((t) => !patch.remove_tags.includes(t));
    if (patch.asset_type) it.asset_type = patch.asset_type;
    if (patch.set_fields || patch.asset_type) {
      const keys = new Set(fieldsFor(it.asset_type).map((f) => f.key));
      const next = Object.fromEntries(Object.entries(it.fields || {}).filter(([k]) => keys.has(k)));
      for (const [k, v] of Object.entries(patch.set_fields || {}))
        if (keys.has(k)) v === null ? delete next[k] : next[k] = v;
      it.fields = next;
    }
    if (patch.favorite !== undefined) it.favorite = patch.favorite;
    rerenderCard(idx);
  });
  if (!refresh) return;
  refreshSidebar();
  renderDetail();
}

const normTag = (s) => s.trim().toLowerCase().split(/\s+/).join("-");

function bindTagInput(onAdd) {
  const input = $("#tag-input");
  const commit = () => {
    const names = input.value.split(",").map(normTag).filter(Boolean);
    input.value = "";
    if (names.length) onAdd(names);
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); commit(); }
    if (e.key === "Escape") input.blur();
  });
  input.addEventListener("input", () => { if (input.value.includes(",")) commit(); });
}

/** The desktop app opens files in their default app; a browser opens a new tab. */
function openOriginal(id) {
  if (state.meta.desktop) api(`/api/assets/${id}/open`, { method: "POST" }).catch((e) => toast(e.message, true));
  else window.open(`/api/assets/${id}/file`, "_blank");
}

// ------------------------------------------------------------------ properties editor

async function openPropsEditor() {
  const dlg = $("#props-dialog");
  const f = $("form", dlg);
  f.ptype.innerHTML = typeOptions(f.ptype.value || "token");
  await renderPropsList();
  dlg.showModal();
}

async function renderPropsList() {
  state.fields = await api("/api/fields");
  const type = $("#props-dialog form").ptype.value;
  const list = fieldsFor(type);
  const box = $("#props-list");
  box.innerHTML = list.map((fd, i) => `
    <div class="prop-row" data-id="${fd.id}">
      <input data-label value="${esc(fd.label)}" title="Name">
      <input data-key value="${esc(fd.key)}" title="Search with ${esc(fd.key)}:value">
      <div class="btns"><span class="kind">${fd.kind}</span>
        <button type="button" data-move="-1" ${i === 0 ? "disabled" : ""} title="Move up">↑</button>
        <button type="button" data-move="1" ${i === list.length - 1 ? "disabled" : ""} title="Move down">↓</button>
        <button type="button" data-del class="danger" title="Delete property">×</button></div>
      ${fd.kind === "choice" ? `<div class="opts">${fd.options.map((o) =>
        `<span class="chip"><button type="button" data-ren="${esc(o)}" title="Rename (merges into an existing choice)">${esc(o)}</button>` +
        `<button type="button" data-rmopt="${esc(o)}" title="Remove">×</button></span>`).join("")}
        <input data-addopt placeholder="Add choice…"></div>` : ""}
    </div>`).join("") || '<p class="muted">No properties for this type yet.</p>';

  const patch = async (id, body) => {
    try { await api(`/api/fields/${id}`, { method: "PATCH", body }); }
    catch (e) { toast(e.message, true); }
    renderPropsList();
  };
  box.querySelectorAll(".prop-row").forEach((row, i) => {
    const id = +row.dataset.id, fd = list[i];
    // Enter would submit the dialog's form; commit the field instead.
    row.querySelectorAll("input").forEach((inp) => inp.addEventListener("keydown", (e) => {
      if (e.key === "Enter") { e.preventDefault(); inp.blur(); }
    }));
    $("[data-label]", row).addEventListener("change", (e) => patch(id, { label: e.target.value }));
    $("[data-key]", row).addEventListener("change", (e) => patch(id, { key: e.target.value }));
    row.querySelectorAll("[data-move]").forEach((b) =>
      b.addEventListener("click", () => patch(id, { position: i + +b.dataset.move })));
    $("[data-del]", row).addEventListener("click", async () => {
      if (!await ask({ title: `Delete "${fd.label}"?`, ok: "Delete", danger: true,
        message: `Its value is removed from every ${type}.` })) return;
      await api(`/api/fields/${id}`, { method: "DELETE" });
      renderPropsList();
    });
    row.querySelectorAll("[data-ren]").forEach((b) => b.addEventListener("click", async () => {
      const to = await ask({ title: `Rename "${b.dataset.ren}"`, input: true, value: b.dataset.ren, ok: "Rename",
        message: "Assets with it are updated. An existing choice's name merges them." });
      if (to && to !== b.dataset.ren) patch(id, { rename_value: { from: b.dataset.ren, to } });
    }));
    row.querySelectorAll("[data-rmopt]").forEach((b) => b.addEventListener("click", async () => {
      if (await ask({ title: `Remove "${b.dataset.rmopt}"?`, ok: "Remove", danger: true,
        message: "It's cleared from every asset that has it." })) patch(id, { remove_option: b.dataset.rmopt });
    }));
    $("[data-addopt]", row)?.addEventListener("change", (e) => e.target.value.trim() && patch(id, { add_option: e.target.value }));
  });
}

async function addProperty() {
  const f = $("#props-dialog form");
  const label = f.new_label.value.trim();
  if (!label) return toast("Give the property a name", true);
  try {
    await api("/api/fields", { method: "POST", body: {
      asset_type: f.ptype.value, key: label, label, kind: f.new_kind.value,
      options: f.new_options.value.split(",").map((o) => o.trim()).filter(Boolean),
    } });
    f.new_label.value = f.new_options.value = "";
    renderPropsList();
  } catch (e) { toast(e.message, true); }
}

// ------------------------------------------------------------------ dialogs

async function pickFolderInto(input) {
  try {
    const { path } = await api("/api/pick-folder", { method: "POST" });
    if (path) input.value = path;
  } catch (e) { toast(e.message, true); }
}

async function openSettings() {
  const dlg = $("#settings-dialog");
  const f = $("form", dlg);
  try {
    const s = await api("/api/settings");
    f.ai.checked = s.ai;
    f.ai.disabled = s.ai_locked;
    $("#ai-note").textContent = s.ai_locked
      ? "Set by ASSET_LIBRARY_AI for this run."
      : s.model_cached ? "Runs a local CLIP model (already downloaded). Nothing about your images leaves this computer."
      : "Runs a local CLIP model. The first time, it downloads about 600 MB. Nothing about your images leaves this computer.";
    f.threads.value = s.threads;
    f.threads.max = s.max_threads;
    f.batch.value = s.batch;
    f.low_priority.checked = s.low_priority;
    f.pause_on_battery.checked = s.pause_on_battery;
    dlg.showModal();
  } catch (e) { toast(e.message, true); }
}

async function onSettingsClose() {
  const dlg = $("#settings-dialog");
  if (dlg.returnValue !== "ok") return;
  const f = $("form", dlg);
  const body = { threads: +f.threads.value, batch: +f.batch.value, low_priority: f.low_priority.checked,
                 pause_on_battery: f.pause_on_battery.checked };
  if (!f.ai.disabled) body.ai = f.ai.checked;
  try {
    const before = state.meta.ai;
    await api("/api/settings", { method: "PATCH", body });
    state.meta = await api("/api/meta");
    toast("Settings saved");
    if (state.meta.ai && !before) pollScan();
  } catch (e) { toast(e.message, true); }
}

async function importLibrary() {
  const f = $("#settings-dialog form");
  const path = f.import_path.value.trim();
  if (!path) return toast("Choose the Python app's folder first", true);
  toast("Importing…");
  try {
    const r = await api("/api/import", { method: "POST", body: { path } });
    $("#settings-dialog").close("cancel");
    toast(`Imported ${r.assets.toLocaleString()} assets and ${r.thumbnails.toLocaleString()} thumbnails` +
          (r.copied_models ? ", plus the AI model" : ""));
    await refreshSidebar();
    reload();
    pollScan();
  } catch (e) { toast(e.message, true); }
}

function openSourceDialog() {
  const dlg = $("#source-dialog");
  const form = $("form", dlg);
  form.reset();
  form.asset_type.innerHTML = typeOptions("token");
  dlg.showModal();
}

async function onSourceDialogClose() {
  const dlg = $("#source-dialog");
  if (dlg.returnValue !== "ok") return;
  const f = $("form", dlg);
  try {
    await api("/api/sources", {
      method: "POST",
      body: { path: f.path.value.trim(), name: f.name.value.trim() || null,
              asset_type: f.asset_type.value, folder_tags: f.folder_tags.checked },
    });
    toast("Folder added — scanning…");
    await refreshSidebar();
    pollScan(true);
  } catch (e) { toast(e.message, true); }
}

/** With `all`, start with "every result of the search" ticked (nothing needs selecting). */
function openExport(all = false) {
  const dlg = $("#export-dialog");
  const f = $("form", dlg);
  const ranked = /(^|\s)~|similar:\d/.test(state.q);
  const offerAll = !ranked && state.total > state.selected.size;
  $("#export-all-row").classList.toggle("hidden", !offerAll);
  $(".total", dlg).textContent = state.total.toLocaleString();
  f.all.checked = offerAll && (all === true || state.selected.size === 0);
  const count = () => { $(".n", dlg).textContent = (f.all.checked ? state.total : state.selected.size).toLocaleString(); };
  f.all.onchange = count;
  count();
  f.dest.value = store.get("exportDest", "");
  dlg.showModal();
}

async function onExportClose() {
  const dlg = $("#export-dialog");
  const f = $("form", dlg);
  const body = f.all.checked
    ? { q: state.q, flatten: f.flatten.checked }
    : { ids: [...state.selected], flatten: f.flatten.checked };
  if (["zip", "folder"].includes(dlg.returnValue) && !f.all.checked && !body.ids.length)
    return toast("Select something to export, or tick “All results”", true);
  try {
    if (dlg.returnValue === "zip" && state.meta.desktop) {
      const { path } = await api("/api/export/zip-save", { method: "POST", body });
      if (path) toast(`Saved ${path}`);
    } else if (dlg.returnValue === "zip") {
      const res = await api("/api/export/zip", { method: "POST", body, raw: true });
      const a = Object.assign(document.createElement("a"), {
        href: URL.createObjectURL(await res.blob()), download: "assets.zip",
      });
      a.click();
      setTimeout(() => URL.revokeObjectURL(a.href), 10000);
    } else if (dlg.returnValue === "folder") {
      const dest = f.dest.value.trim();
      if (!dest) return toast("Choose a destination folder", true);
      store.set("exportDest", dest);
      const r = await api("/api/export/folder", { method: "POST", body: { ...body, dest } });
      toast(`Copied ${r.count} file${r.count === 1 ? "" : "s"} to ${r.dest}`);
    }
  } catch (e) { toast(e.message, true); }
}

async function onTagContext(e) {
  const li = e.target.closest("[data-tag]");
  if (!li) return;
  e.preventDefault();
  openTagDialog(li.dataset.tag);
}

/** Rename/merge, move to a property, or delete a tag. */
function openTagDialog(name) {
  const dlg = $("#tag-dialog");
  const f = $("form", dlg);
  $(".name", dlg).textContent = name;
  f.new_name.value = name;
  const keys = fieldsForTypes(state.meta.asset_types);
  f.field.innerHTML = keys.map((k) => `<option value="${esc(k.key)}">${esc(k.label)}</option>`).join("");
  const slot = $(".value-slot", dlg);
  const renderValue = () => {
    const k = keys.find((x) => x.key === f.field.value);
    if (!k) { slot.innerHTML = ""; return; }
    // Merge choices across types that share the key.
    const opts = [...new Set(state.fields.filter((x) => x.key === k.key).flatMap((x) => x.options))];
    slot.innerHTML = k.kind === "choice"
      ? `<select name="pvalue">${opts.map((o) =>
          `<option ${o.toLowerCase() === name.toLowerCase() ? "selected" : ""}>${esc(o)}</option>`).join("")}</select>`
      : `<input name="pvalue" ${k.kind === "number" ? 'type="number" step="any"' : `list="fv-${esc(k.key)}"`}
           value="${k.kind === "number" ? "" : esc(name)}" autocomplete="off">`;
  };
  f.field.onchange = renderValue;
  renderValue();
  dlg.addEventListener("close", async () => {
    const enc = encodeURIComponent(name);
    try {
      if (dlg.returnValue === "rename") {
        const next = f.new_name.value.trim();
        if (!next || next === name) return;
        await api(`/api/tags/${enc}`, { method: "PATCH", body: { new_name: next } });
      } else if (dlg.returnValue === "move") {
        const value = f.pvalue?.value.trim();
        if (!value) return toast("Choose a value", true);
        const r = await api(`/api/tags/${enc}/promote`, { method: "POST", body: { field: f.field.value, value } });
        toast(`Moved ${r.moved} asset${r.moved === 1 ? "" : "s"}` +
              (r.skipped ? ` (${r.skipped} kept the tag: their type has no such property)` : ""));
      } else if (dlg.returnValue === "delete") {
        if (!await ask({ title: `Delete tag "${name}" from every asset?`, ok: "Delete", danger: true })) return;
        await api(`/api/tags/${enc}`, { method: "DELETE" });
      } else return;
      await refreshSidebar();
      reload();
    } catch (err) { toast(err.message, true); }
  }, { once: true });
  dlg.showModal();
}

async function onSourceContext(e) {
  const li = e.target.closest("[data-source]");
  if (!li) return;
  e.preventDefault();
  const s = state.sources.find((x) => x.id === +li.dataset.source);
  if (!await ask({ title: `Remove "${s.name}" from the library?`, ok: "Remove", danger: true,
    message: "Tags and notes for its files are dropped. Files on disk are NOT touched." })) return;
  await api(`/api/sources/${s.id}`, { method: "DELETE" });
  await refreshSidebar();
  reload();
}

// ------------------------------------------------------------------ scanning

let polling = false;
/** Poll scan + thumbnail progress. Pass startedScan=true right after starting a scan. */
async function pollScan(startedScan = false) {
  if (polling) return;
  polling = true;
  const el = $("#scan-status");
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  let scanned = startedScan, summary = "", analyzing = false;
  try {
    for (;;) {
      const s = await api("/api/scan/status");
      if (s.running) {
        scanned = true;
        el.textContent = `Scanning ${s.source}… ${s.processed}`;
        await sleep(800);
        continue;
      }
      if (scanned) {
        // Scan finished: show results now; thumbnails keep filling in as they're made.
        scanned = false;
        const parts = [s.added && `${s.added} new`, s.moved && `${s.moved} moved`,
                       s.updated && `${s.updated} changed`, s.missing && `${s.missing} missing`,
                       s.skipped && `${s.skipped} not images`, s.removed && `${s.removed} removed`].filter(Boolean);
        summary = `Scanned ${s.processed}${parts.length ? ` (${parts.join(", ")})` : ""}`;
        if (s.errors.length) toast(`Scan: ${s.errors.slice(0, 3).join("; ")}`, true);
        await refreshSidebar();
        reload();
      }
      const paused = s.paused ? " (paused on battery)" : "";
      if (s.thumbs.running) {
        el.textContent = `Generating thumbnails… ${s.thumbs.done.toLocaleString()} / ${s.thumbs.total.toLocaleString()}${paused}`;
        await sleep(1500);
        continue;
      }
      if (s.analysis.running) {
        const note = s.analysis.message ? ` — ${s.analysis.message}` : "";
        el.textContent = `Analyzing images… ${s.analysis.done.toLocaleString()} / ${s.analysis.total.toLocaleString()}${note}${paused}`;
        analyzing = true;
        await sleep(1500);
        continue;
      }
      if (analyzing && s.analysis.error) toast(`Image analysis: ${s.analysis.error}`, true);
      el.textContent = summary;
      break;
    }
  } finally { polling = false; }
}

// ------------------------------------------------------------------ page zoom

/** Ctrl + wheel and Ctrl +/-/0 zoom the page. Browsers do this natively; the desktop app's
 *  webview doesn't, so it's done here with CSS zoom (and remembered). */
function bindPageZoom() {
  const steps = [0.5, 0.67, 0.75, 0.8, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2, 2.5, 3];
  let level = store.get("pageZoom", 1);
  const apply = () => { document.documentElement.style.zoom = level; store.set("pageZoom", level); };
  const step = (dir) => {
    const i = steps.findIndex((z) => z >= level - 1e-6);
    level = dir === 0 ? 1 : steps[Math.max(0, Math.min(steps.length - 1, (i < 0 ? steps.length - 1 : i) + dir))];
    apply();
  };
  apply();
  document.addEventListener("keydown", (e) => {
    if (!(e.ctrlKey || e.metaKey) || e.altKey) return;
    const dir = { "=": 1, "+": 1, "-": -1, "_": -1, "0": 0 }[e.key];
    if (dir === undefined) return;
    e.preventDefault();
    step(dir);
  });
  document.addEventListener("wheel", (e) => {
    if (!e.ctrlKey) return;
    e.preventDefault();
    step(e.deltaY < 0 ? 1 : -1);
  }, { passive: false });
}

// ------------------------------------------------------------------ init

function bindEvents() {
  const onSearch = debounce(() => setQuery($("#q").value), 250);
  $("#q").addEventListener("input", onSearch);
  $("#sort").addEventListener("change", (e) => { state.sort = e.target.value; store.set("sort", state.sort); reload(); });
  $("#zoom").addEventListener("input", (e) => {
    document.documentElement.style.setProperty("--thumb", `${e.target.value}px`);
    store.set("zoom", e.target.value);
  });
  $("#scan-btn").addEventListener("click", async () => {
    try { await api("/api/scan", { method: "POST", body: {} }); pollScan(true); }
    catch (e) { toast(e.message, true); }
  });
  $("#help-btn").addEventListener("click", () => $("#help-dialog").showModal());
  $("#settings-btn").addEventListener("click", openSettings);
  $("#settings-dialog").addEventListener("close", onSettingsClose);
  $("#import-btn").addEventListener("click", importLibrary);

  $("#quick").addEventListener("click", (e) => {
    const li = e.target.closest("li");
    if (!li) return;
    li.dataset.q ? toggleFilter(...li.dataset.q.split(":")) : setQuery("");
  });
  $("#types").addEventListener("click", (e) => {
    const li = e.target.closest("[data-type]");
    if (li) toggleFilter("type", li.dataset.type);
  });
  $("#sources").addEventListener("click", (e) => {
    const li = e.target.closest("[data-source]");
    if (li) toggleFilter("source", li.dataset.source);
    else if (e.target.closest("#no-sources")) openSourceDialog();
  });
  $("#sources").addEventListener("contextmenu", onSourceContext);
  $("#tags").addEventListener("click", (e) => {
    const li = e.target.closest("[data-tag]");
    if (li) toggleFilter("tag", li.dataset.tag, true);
  });
  $("#tags").addEventListener("contextmenu", onTagContext);
  $("#colors").addEventListener("click", (e) => {
    const b = e.target.closest("[data-color]");
    if (b) toggleFilter("color", b.dataset.color, true);
  });
  const sidebar = $(".sidebar");
  sidebar.addEventListener("input", (e) => {
    const key = e.target.dataset.find;
    if (key != null) { state.find[key] = e.target.value; renderSidebar(); }
  });
  // Picking a value from a filtered list clears that filter (the pick stays pinned). Capture
  // phase: the list's own click handler re-renders it.
  sidebar.addEventListener("click", (e) => {
    const picked = e.target.closest("li[data-tag], li[data-pkey]")?.parentElement.previousElementSibling?.dataset?.find;
    if (picked != null) delete state.find[picked];
  }, true);
  sidebar.addEventListener("click", (e) => {
    const more = e.target.closest("[data-more]");
    if (more) {
      const k = more.dataset.more;
      state.expanded.has(k) ? state.expanded.delete(k) : state.expanded.add(k);
      return renderSidebar();
    }
    const h = e.target.closest("h3, h4");
    if (h && !e.target.closest("button")) {
      const key = h.parentElement.dataset.sec;
      const c = new Set(store.get("collapsed", []));
      c.has(key) ? c.delete(key) : c.add(key);
      store.set("collapsed", [...c]);
      renderSections();
    }
  });
  $("#chips").addEventListener("click", (e) => {
    const b = e.target.closest("[data-drop]");
    if (!b) return;
    const toks = tokens(state.q);
    setQuery(b.dataset.drop === "all" ? "" : toks.filter((_, i) => i !== +b.dataset.drop).join(" "));
  });
  $("#props").addEventListener("click", (e) => {
    const v = e.target.closest("[data-pkey]");
    if (v) return toggleFilter(v.dataset.pkey, v.dataset.pval, true);
    const h = e.target.closest("[data-phas]");
    if (h) toggleToken(`-has:${h.dataset.phas}`);
  });
  $("#edit-props").addEventListener("click", openPropsEditor);
  $("#add-prop").addEventListener("click", addProperty);
  $("#props-dialog form").ptype.addEventListener("change", renderPropsList);
  ["new_label", "new_options"].forEach((n) => $("#props-dialog form")[n].addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); addProperty(); }
  }));
  $("#props-dialog").addEventListener("close", async () => { await refreshSidebar(); renderDetail(); });

  $("#add-source").addEventListener("click", openSourceDialog);
  $("#main").addEventListener("click", (e) => { if (e.target.id === "empty-add") openSourceDialog(); });
  $("#source-dialog").addEventListener("close", onSourceDialogClose);
  $("#export-dialog").addEventListener("close", onExportClose);
  $("#export-results").addEventListener("click", () => openExport(true));
  document.querySelectorAll("dialog .browse").forEach((b) =>
    b.addEventListener("click", () => pickFolderInto(b.previousElementSibling)));

  $("#grid").addEventListener("click", onCardClick);
  $("#grid").addEventListener("dblclick", (e) => {
    const card = e.target.closest(".card");
    if (card) openOriginal(state.items[+card.dataset.idx].id);
  });
  $("#grid").addEventListener("dragstart", onDragStart);

  new IntersectionObserver((entries) => {
    if (entries.some((en) => en.isIntersecting)) loadMore();
  }, { root: $("#main"), rootMargin: "400px" }).observe($("#sentinel"));

  document.addEventListener("keydown", (e) => {
    const typing = e.target.matches("input, textarea, select") || document.querySelector("dialog[open]");
    if (typing) return;
    if (e.key === "/") { e.preventDefault(); $("#q").focus(); }
    else if (e.key === "Escape") { state.selected.clear(); syncSelectionClasses(); }
    else if (e.key === "a" && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      state.items.forEach((it) => state.selected.add(it.id));
      syncSelectionClasses();
    } else if (e.key === "f" && state.selected.size) {
      bulk({ favorite: !selectedItems().every((it) => it.favorite) });
    } else if (e.key === "t" && state.selected.size) {
      e.preventDefault();
      $("#tag-input")?.focus();
    }
  });
}

async function init() {
  [state.meta, state.fields] = await Promise.all([api("/api/meta"), api("/api/fields")]);
  // ?q= in the URL (e.g. a bookmarked search) wins over the remembered search.
  state.q = new URLSearchParams(location.search).get("q") ?? store.get("q", "");
  state.sort = store.get("sort", "name");
  $("#q").value = state.q;
  $("#sort").innerHTML = state.meta.sorts.map((s) =>
    `<option value="${s}" ${s === state.sort ? "selected" : ""}>Sort: ${s}</option>`).join("");
  const zoom = store.get("zoom", 140);
  $("#zoom").value = zoom;
  document.documentElement.style.setProperty("--thumb", `${zoom}px`);
  bindEvents();
  if (state.meta.desktop) bindPageZoom();
  await refreshSidebar();
  reload();
  const s = await api("/api/scan/status");
  if (s.running || s.thumbs.running || s.analysis.running) pollScan(s.running);
}

init().catch((e) => toast(e.message, true));
