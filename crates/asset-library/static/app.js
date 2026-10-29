"use strict";

const $ = (s, el = document) => el.querySelector(s);
const PAGE = 200;

const state = {
  q: "", sort: "name",
  items: [], total: 0, loading: false, done: false, gen: 0,
  selected: new Set(), anchor: null,
  tags: [], sources: [], meta: { asset_types: [], sorts: [] },
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

function filterState(key, value) {
  const pos = `${key}:${quoteVal(value)}`;
  const toks = tokens(state.q);
  return toks.includes(pos) ? "on" : toks.includes(`-${pos}`) ? "off" : "";
}

// ------------------------------------------------------------------ sidebar

function renderSidebar() {
  for (const li of $("#quick").children) {
    const q = li.dataset.q;
    li.classList.toggle("active", q ? tokens(state.q).includes(q) : state.q === "");
  }

  $("#types").innerHTML = state.meta.asset_types.map((t) =>
    `<li data-type="${esc(t)}" class="${filterState("type", t) ? "active" : ""}">
       <span class="label">${esc(t)}</span></li>`).join("");

  $("#colors").innerHTML = Object.entries(state.meta.colors || {}).map(([c, hex]) => {
    const st = filterState("color", c);
    return `<button class="swatch ${st}" data-color="${c}" title="${c} (click again to exclude)"
              style="--c:${hex}"></button>`;
  }).join("");

  $("#sources").innerHTML = state.sources.length
    ? state.sources.map((s) =>
      `<li data-source="${s.id}" title="${esc(s.path)}" class="${filterState("source", String(s.id)) ? "active" : ""}">
         <span class="label">${esc(s.name)}</span><span class="n">${s.count}</span></li>`).join("")
    : `<li class="muted" id="no-sources">Add a folder to begin</li>`;

  const f = $("#tag-filter").value.trim().toLowerCase();
  $("#tags").innerHTML = state.tags
    .filter((t) => !f || t.name.includes(f))
    .map((t) => {
      const st = filterState("tag", t.name);
      return `<li data-tag="${esc(t.name)}" class="${st === "on" ? "active" : st === "off" ? "excluded" : ""}"
                  title="Click: include, again: exclude, again: clear. Right-click: rename/delete">
                <span class="label">${esc(t.name)}</span><span class="n">${t.count}</span></li>`;
    }).join("");

  $("#tag-datalist")?.remove();
  const dl = document.createElement("datalist");
  dl.id = "tag-datalist";
  dl.innerHTML = state.tags.map((t) => `<option value="${esc(t.name)}">`).join("");
  document.body.append(dl);
}

async function refreshSidebar() {
  [state.tags, state.sources] = await Promise.all([api("/api/tags"), api("/api/sources")]);
  renderSidebar();
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
        });
        rerenderCard(idx);
      }
      if (patch.tags) refreshSidebar();
      return updated;
    } catch (e) { toast(e.message, true); }
  };

  $("#d-name").addEventListener("change", (e) => save({ name: e.target.value }));
  $("#d-notes").addEventListener("change", (e) => save({ notes: e.target.value }));
  $("#d-type").addEventListener("change", (e) => save({ asset_type: e.target.value }));
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
    <div class="actions">
      <button id="m-fav">${allFav ? "Unfavorite all" : "★ Favorite all"}</button>
      <button id="m-export" class="primary">Export ${sel.length}…</button>
      <button id="m-clear">Clear selection</button>
    </div>`;

  $("#m-type").addEventListener("change", (e) => e.target.value && bulk({ asset_type: e.target.value }));
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
    <label>Suggested tags <span class="muted">click to add · × to dismiss</span></label>
    <div class="chips">${list.map((s, i) => `
      <span class="chip suggest" title="${esc(s.source)} · ${Math.round(s.score * 100)}% confident${
        multi ? ` · applies to ${s.count} of ${ids.length}` : ""}" style="--conf:${0.45 + s.score * 0.55}">
        <button data-accept="${i}">+ ${esc(s.tag)}</button>${multi ? `<span class="n">${s.count}</span>` : ""}
        <button data-dismiss="${i}" title="Dismiss">×</button></span>`).join("")}</div>
    <div class="actions small-actions">
      <button id="s-accept-all" class="ghost small">Add all ${list.length}</button>
      <button id="s-ignored" class="ghost small">Never-suggest list…</button>
    </div>`;

  const accept = async (items) => {
    for (const s of items) await bulk({ add_tags: [s.tag] }, s.ids, false);
    refreshSidebar();
    renderDetail();
  };
  box.querySelectorAll("[data-accept]").forEach((b) =>
    b.addEventListener("click", () => accept([list[+b.dataset.accept]])));
  box.querySelectorAll("[data-dismiss]").forEach((b) => b.addEventListener("click", async () => {
    const s = list[+b.dataset.dismiss];
    await api("/api/suggestions/reject", { method: "POST", body: { ids: s.ids, tag: s.tag } });
    renderSuggestions(ids);
    toast(`Dismissed "${s.tag}"`, false, {
      label: "Never suggest it", run: async () => {
        await api("/api/suggestions/reject", { method: "POST", body: { tag: s.tag, everywhere: true } });
        toast(`"${s.tag}" won't be suggested again`);
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

function openExport() {
  const dlg = $("#export-dialog");
  $(".n", dlg).textContent = state.selected.size;
  $("form", dlg).dest.value = store.get("exportDest", "");
  dlg.showModal();
}

async function onExportClose() {
  const dlg = $("#export-dialog");
  const f = $("form", dlg);
  const body = { ids: [...state.selected], flatten: f.flatten.checked };
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
  const name = li.dataset.tag;
  const next = await ask({ title: `Rename tag "${name}"`, input: true, value: name, ok: "Rename",
    message: "An existing name merges them. Leave empty to delete it from all assets." });
  if (next === null || next === name) return;
  try {
    if (next.trim() === "") {
      if (!await ask({ title: `Delete tag "${name}" from every asset?`, ok: "Delete", danger: true })) return;
      await api(`/api/tags/${encodeURIComponent(name)}`, { method: "DELETE" });
    } else {
      await api(`/api/tags/${encodeURIComponent(name)}`, { method: "PATCH", body: { new_name: next } });
    }
    await refreshSidebar();
    reload();
  } catch (err) { toast(err.message, true); }
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
  $("#tag-filter").addEventListener("input", renderSidebar);

  $("#add-source").addEventListener("click", openSourceDialog);
  $("#main").addEventListener("click", (e) => { if (e.target.id === "empty-add") openSourceDialog(); });
  $("#source-dialog").addEventListener("close", onSourceDialogClose);
  $("#export-dialog").addEventListener("close", onExportClose);
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
  state.meta = await api("/api/meta");
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
  await refreshSidebar();
  reload();
  const s = await api("/api/scan/status");
  if (s.running || s.thumbs.running || s.analysis.running) pollScan(s.running);
}

init().catch((e) => toast(e.message, true));
