"use strict";

const POLL_MS = 2000;
const POLL_MAX_BACKOFF_MS = 30000;
const POLL_HIDDEN_MS = 0; // 0 = stop polling while the app is in the background
const INF_ETA = 8640000;   // qBittorrent's "infinite" ETA sentinel

const $ = (id) => document.getElementById(id);

const el = {
    conn: $("conn"), down: $("g-down"), up: $("g-up"), free: $("g-free"),
    alt: $("alt-btn"), searchBtn: $("search-btn"), search: $("search"),
    chips: $("chips"), list: $("list"), empty: $("empty"), fab: $("fab"),
    rowTpl: $("row-tpl"), toast: $("toast"),
    addSheet: $("add-sheet"), addForm: $("add-form"), addUrls: $("add-urls"),
    addFiles: $("add-files"), fileLabel: $("file-label"), addCat: $("add-cat"),
    addPaused: $("add-paused"), addSubmit: $("add-submit"), paste: $("paste-btn"),
    handlerBtn: $("handler-btn"),
    detail: $("detail-sheet"), dName: $("d-name"), dBar: $("d-bar"), dGrid: $("d-grid"),
    dActions: $("d-actions"), dConfirm: $("d-confirm"), dToggle: $("d-toggle"),
};

const ui = {
    filter: "all",
    query: "",
    torrents: new Map(),  // hash -> torrent from the last poll
    rows: new Map(),      // hash -> row element
    order: "",            // joined hashes of the rendered order
    detailHash: null,
    categories: [],
};

// ------------------------------------------------------------- formatting

function fmtBytes(n) {
    if (!(n >= 0)) return "–";
    const units = ["B", "KB", "MB", "GB", "TB", "PB"];
    let i = 0;
    while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
    if (i === 0) return `${n} B`;
    return `${n >= 100 ? n.toFixed(0) : n >= 10 ? n.toFixed(1) : n.toFixed(2)} ${units[i]}`;
}

const fmtSpeed = (n) => `${fmtBytes(n)}/s`;

function fmtEta(s) {
    if (!(s >= 0) || s >= INF_ETA) return "∞";
    const d = Math.floor(s / 86400), h = Math.floor(s % 86400 / 3600),
          m = Math.floor(s % 3600 / 60), sec = Math.floor(s % 60);
    if (d) return `${d}d ${h}h`;
    if (h) return `${h}h ${m}m`;
    if (m) return `${m}m ${sec}s`;
    return `${sec}s`;
}

function fmtPct(p) {
    // never show 100% for a torrent that still has bytes left
    const v = Math.floor(p * 1000) / 10;
    return `${v.toFixed(v >= 100 || v === 0 ? 0 : 1)}%`;
}

function fmtDate(ts) {
    if (!(ts > 0)) return "–";
    return new Date(ts * 1000).toLocaleString(undefined, {
        year: "numeric", month: "short", day: "numeric", hour: "2-digit", minute: "2-digit",
    });
}

// ------------------------------------------------------------ state model

// kind drives colour, sorting and filters; label is what the user reads
const STATES = {
    downloading:        ["dl", "Downloading"],
    forcedDL:           ["dl", "Downloading [F]"],
    metaDL:             ["dl", "Fetching metadata"],
    forcedMetaDL:       ["dl", "Fetching metadata [F]"],
    stalledDL:          ["wait", "Stalled"],
    queuedDL:           ["wait", "Queued"],
    checkingDL:         ["wait", "Checking"],
    allocating:         ["wait", "Allocating"],
    checkingResumeData: ["wait", "Checking resume data"],
    moving:             ["wait", "Moving"],
    uploading:          ["seed", "Seeding"],
    forcedUP:           ["seed", "Seeding [F]"],
    stalledUP:          ["seed", "Seeding"],
    queuedUP:           ["seed", "Queued"],
    checkingUP:         ["wait", "Checking"],
    pausedDL:           ["paused", "Paused"],
    stoppedDL:          ["paused", "Stopped"],
    pausedUP:           ["paused", "Completed"],
    stoppedUP:          ["paused", "Completed"],
    error:              ["error", "Error"],
    missingFiles:       ["error", "Missing files"],
};

const kindOf = (t) => (STATES[t.state] || ["wait", t.state || "Unknown"])[0];
const labelOf = (t) => (STATES[t.state] || ["wait", t.state || "Unknown"])[1];

const FILTERS = {
    all:    () => true,
    active: (t) => t.dlspeed > 0 || t.upspeed > 0,
    dl:     (t) => t.progress < 1 && kindOf(t) !== "paused" && kindOf(t) !== "error",
    seed:   (t) => t.progress >= 1 && kindOf(t) !== "paused" && kindOf(t) !== "error",
    done:   (t) => t.progress >= 1,
    paused: (t) => kindOf(t) === "paused",
    error:  (t) => kindOf(t) === "error",
};

function sortRank(t) {
    switch (kindOf(t)) {
        case "error": return 0;
        case "dl": return 1;
        case "wait": return t.progress < 1 ? 2 : 4;
        case "seed": return t.upspeed > 0 ? 3 : 5;
        default: return 6;
    }
}

function compare(a, b) {
    return sortRank(a) - sortRank(b)
        || (b.added_on || 0) - (a.added_on || 0)
        || a.name.localeCompare(b.name);
}

// -------------------------------------------------------------- rendering

function setText(node, text) {
    if (node.textContent !== text) node.textContent = text;
}

// one speed per row keeps the meta line on a 360px screen; the sheet shows both
function rateText(t) {
    if (t.progress < 1) return t.dlspeed > 0 ? `↓ ${fmtSpeed(t.dlspeed)}` : "";
    return t.upspeed > 0 ? `↑ ${fmtSpeed(t.upspeed)}` : "";
}

function makeRow(hash) {
    const row = el.rowTpl.content.firstElementChild.cloneNode(true);
    row.dataset.hash = hash;
    row.parts = {
        name: row.querySelector(".row-name"),
        bar: row.querySelector(".bar i"),
        state: row.querySelector(".state"),
        pct: row.querySelector(".pct"),
        rate: row.querySelector(".rate"),
        eta: row.querySelector(".eta"),
    };
    return row;
}

function updateRow(row, t) {
    const p = row.parts;
    const kind = kindOf(t);
    if (row.dataset.kind !== kind) row.dataset.kind = kind;

    setText(p.name, t.name);
    const width = `${(t.progress * 100).toFixed(1)}%`;
    if (p.bar.style.width !== width) p.bar.style.width = width;

    // seed counts explain a stall; on an active download the speed says enough
    const seeds = kind === "wait" ? ` · ${t.num_seeds}/${t.num_complete} seeds` : "";
    setText(p.state, labelOf(t) + (t.progress < 1 ? seeds : ""));
    setText(p.pct, t.progress < 1 ? fmtPct(t.progress) : fmtBytes(t.size));
    setText(p.rate, rateText(t));
    setText(p.eta, kind === "dl" && t.eta < INF_ETA ? fmtEta(t.eta) : "");
}

function render() {
    const all = [...ui.torrents.values()];
    const q = ui.query.toLowerCase();
    const searched = q ? all.filter((t) => t.name.toLowerCase().includes(q)) : all;

    for (const chip of el.chips.children) {
        const n = searched.filter(FILTERS[chip.dataset.f]).length;
        setText(chip.querySelector("b"), String(n));
    }

    const shown = searched.filter(FILTERS[ui.filter]).sort(compare);

    // drop rows for torrents that disappeared or are filtered out
    const keep = new Set(shown.map((t) => t.hash));
    for (const [hash, row] of ui.rows) {
        if (!keep.has(hash)) { row.remove(); ui.rows.delete(hash); }
    }

    for (const t of shown) {
        let row = ui.rows.get(t.hash);
        if (!row) { row = makeRow(t.hash); ui.rows.set(t.hash, row); }
        updateRow(row, t);
    }

    // only touch DOM order when it changed, so scrolling and taps aren't disturbed
    const order = shown.map((t) => t.hash).join();
    if (order !== ui.order) {
        ui.order = order;
        const frag = document.createDocumentFragment();
        for (const t of shown) frag.append(ui.rows.get(t.hash));
        el.list.replaceChildren(frag);
    }

    el.empty.hidden = shown.length > 0;
    if (!shown.length) {
        setText(el.empty, all.length === 0 ? "No torrents yet. Tap + to add one."
            : q ? `Nothing matches “${ui.query}”.` : "Nothing in this view.");
    }

    if (ui.detailHash) renderDetail();
}

function renderServer(s) {
    setText(el.down, `↓ ${fmtSpeed(s.dl_info_speed || 0)}`);
    setText(el.up, `↑ ${fmtSpeed(s.up_info_speed || 0)}`);
    setText(el.free, s.free_space_on_disk >= 0 ? `${fmtBytes(s.free_space_on_disk)} free` : "");
    el.alt.setAttribute("aria-pressed", String(!!s.use_alt_speed_limits));

    const status = s.connection_status;
    el.conn.className = "dot " + (status === "connected" ? "ok" : status === "firewalled" ? "firewalled" : "error");
    el.conn.title = status ? `qBittorrent: ${status}` : "qBittorrent";
}

function renderCategories(list) {
    if (list.join("\n") === ui.categories.join("\n")) return;
    ui.categories = list;
    const current = el.addCat.value || storage.get("category") || "";
    el.addCat.replaceChildren(new Option("No category", ""), ...list.map((c) => new Option(c, c)));
    el.addCat.value = list.includes(current) ? current : "";
}

// ---------------------------------------------------------------- polling

let pollTimer = null;
let polling = false;
let pollQueued = false;
let reloadedForLogin = false;
let failures = 0;

async function api(path, opts = {}) {
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), opts.timeout || 12000);
    try {
        const res = await fetch(path, { ...opts, signal: ctrl.signal, credentials: "same-origin" });
        if (res.status === 401) {
            // session gone: reloading lets the server redirect to the login form.
            // Once per page load, so a cookie the browser refuses can't loop.
            if (!reloadedForLogin) {
                reloadedForLogin = true;
                location.reload();
            }
            throw new Error("Login required");
        }
        const body = await res.json().catch(() => ({}));
        if (!res.ok) throw new Error(body.error || `HTTP ${res.status}`);
        return body;
    } catch (e) {
        if (e.name === "AbortError") throw new Error("Server timed out");
        if (e instanceof TypeError) throw new Error("Server unreachable");
        throw e;
    } finally {
        clearTimeout(timer);
    }
}

async function poll() {
    clearTimeout(pollTimer);
    if (polling) {
        // the in-flight reply may predate an action the caller just made
        pollQueued = true;
        return;
    }
    polling = true;
    pollQueued = false;
    try {
        const data = await api("api/state");
        ui.torrents = new Map(data.torrents.map((t) => [t.hash, t]));
        renderServer(data.server || {});
        renderCategories(data.categories || []);
        render();
        failures = 0;
    } catch (e) {
        failures++;
        el.conn.className = "dot error";
        el.conn.title = e.message;
        setText(el.down, "↓ –");
        setText(el.up, "↑ –");
        if (!ui.torrents.size) {
            el.empty.hidden = false;
            setText(el.empty, e.message);
        }
    } finally {
        polling = false;
        if (pollQueued && !document.hidden) poll();
        else schedule();
    }
}

function schedule() {
    clearTimeout(pollTimer);
    // back off while the server is down instead of retrying every 2 s forever
    const delay = document.hidden ? POLL_HIDDEN_MS
        : Math.min(POLL_MS * 2 ** Math.min(failures, 4), POLL_MAX_BACKOFF_MS);
    if (delay > 0) pollTimer = setTimeout(poll, delay);
}

document.addEventListener("visibilitychange", () => {
    if (document.hidden) clearTimeout(pollTimer);
    else poll();
});

// ------------------------------------------------------------------ toast

let toastTimer = null;
function toast(msg, bad = false) {
    el.toast.textContent = msg;
    el.toast.className = "show" + (bad ? " bad" : "");
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { el.toast.className = bad ? "bad" : ""; }, bad ? 4000 : 2200);
}

// localStorage may throw (private mode, blocked storage) — it only holds conveniences
const storage = {
    get(k) { try { return localStorage.getItem("qbm." + k); } catch { return null; } },
    set(k, v) { try { localStorage.setItem("qbm." + k, v); } catch { /* ignore */ } },
};

// ----------------------------------------------------------------- sheets

function openSheet(d) {
    if (!d.open) d.showModal();
}

for (const d of [el.addSheet, el.detail]) {
    // a tap on the dimmed backdrop lands on the <dialog> itself, outside its box
    d.addEventListener("click", (e) => {
        if (e.target !== d) return;
        const r = d.getBoundingClientRect();
        const inside = e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom;
        if (!inside) d.close();
    });
    d.querySelectorAll("[data-close]").forEach((b) => b.addEventListener("click", () => d.close()));
}

// ---------------------------------------------------------------- details

function renderDetail() {
    const t = ui.torrents.get(ui.detailHash);
    if (!t) {
        // removed elsewhere (or just deleted from here)
        if (el.detail.open) el.detail.close();
        return;
    }
    setText(el.dName, t.name);
    el.dBar.style.width = `${(t.progress * 100).toFixed(1)}%`;

    const rows = [
        ["Status", labelOf(t)],
        ["Progress", `${fmtPct(t.progress)} of ${fmtBytes(t.size)}`],
        ["Down / Up", `${fmtSpeed(t.dlspeed)} / ${fmtSpeed(t.upspeed)}`],
        ["ETA", t.progress < 1 ? fmtEta(t.eta) : "–"],
        ["Seeds", `${t.num_seeds} (${t.num_complete})`],
        ["Peers", `${t.num_leechs} (${t.num_incomplete})`],
        ["Ratio", (t.ratio ?? 0).toFixed(2)],
        ["Uploaded", fmtBytes(t.uploaded)],
        ["Category", t.category || "–"],
        ["Added", fmtDate(t.added_on)],
        ["Completed", fmtDate(t.completion_on)],
        ["Save path", t.save_path || "–"],
    ];

    // rebuilt in place each poll; cheap at this size and keeps values live
    if (el.dGrid.children.length !== rows.length * 2) {
        el.dGrid.replaceChildren(...rows.flatMap(() => [document.createElement("dt"), document.createElement("dd")]));
    }
    rows.forEach(([k, v], i) => {
        setText(el.dGrid.children[i * 2], k);
        setText(el.dGrid.children[i * 2 + 1], v);
    });

    setText(el.dToggle, kindOf(t) === "paused" ? "Resume" : "Pause");
}

function openDetail(hash) {
    ui.detailHash = hash;
    el.dActions.hidden = false;
    el.dConfirm.hidden = true;
    renderDetail();
    openSheet(el.detail);
}

el.detail.addEventListener("close", () => { ui.detailHash = null; });

el.list.addEventListener("click", (e) => {
    const row = e.target.closest(".row");
    if (row) openDetail(row.dataset.hash);
});
el.list.addEventListener("keydown", (e) => {
    const row = e.target.closest(".row");
    if (row && (e.key === "Enter" || e.key === " ")) { e.preventDefault(); openDetail(row.dataset.hash); }
});

async function copyText(text) {
    try {
        await navigator.clipboard.writeText(text);
        return true;
    } catch {
        // clipboard API needs a secure context; fall back to a selection copy
        const ta = document.createElement("textarea");
        ta.value = text;
        ta.setAttribute("readonly", "");
        el.detail.append(ta);
        ta.select();
        const ok = document.execCommand("copy");
        ta.remove();
        return ok;
    }
}

el.detail.addEventListener("click", async (e) => {
    const btn = e.target.closest("button[data-act]");
    if (!btn || !ui.detailHash) return;
    const hash = ui.detailHash;
    const t = ui.torrents.get(hash);
    let act = btn.dataset.act;

    if (act === "ask-delete") { el.dActions.hidden = true; el.dConfirm.hidden = false; return; }
    if (act === "cancel-delete") { el.dActions.hidden = false; el.dConfirm.hidden = true; return; }

    if (act === "copy") {
        try {
            const { magnet } = await api(`api/magnet/${hash}`);
            toast(await copyText(magnet) ? "Magnet link copied" : "Copy failed", false);
        } catch (err) {
            toast(err.message, true);
        }
        return;
    }

    if (act === "toggle") act = t && kindOf(t) === "paused" ? "resume" : "pause";

    btn.disabled = true;
    try {
        await api("api/action", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ action: act, hashes: [hash] }),
        });
        if (act === "delete" || act === "delete_files") {
            el.detail.close();
            toast(act === "delete_files" ? "Deleted with files" : "Deleted");
        } else {
            toast({ pause: "Paused", resume: "Resumed", recheck: "Rechecking", reannounce: "Reannounced", force_start: "Force started" }[act]);
        }
        poll();
    } catch (err) {
        toast(err.message, true);
    } finally {
        btn.disabled = false;
    }
});

// -------------------------------------------------------------------- add

const MAGNET_RE = /magnet:\?[^\s"'<>]+/gi;
const TORRENT_URL_RE = /https?:\/\/[^\s"'<>]+/gi;
// "see magnet:?xt=...)." — sentence punctuation is never the end of a link
const trimTail = (s) => s.replace(/[).,;:!?\]}]+$/, "");
const HASH_RE = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/i;

/** Pull every usable link out of arbitrary shared text. */
function extractLinks(...texts) {
    const found = [];
    for (const raw of texts) {
        if (!raw) continue;
        const text = String(raw).trim();
        const magnets = text.match(MAGNET_RE);
        if (magnets) { found.push(...magnets.map(trimTail)); continue; }
        if (HASH_RE.test(text)) { found.push(`magnet:?xt=urn:btih:${text}`); continue; }
        const urls = text.match(TORRENT_URL_RE);
        if (urls) found.push(...urls.map(trimTail));
    }
    return [...new Set(found)];
}

function updateFileLabel() {
    const n = el.addFiles.files.length;
    el.fileLabel.textContent = n === 0 ? ".torrent file…" : n === 1 ? el.addFiles.files[0].name : `${n} files`;
}

function resetAdd() {
    el.addUrls.value = "";
    el.addFiles.value = "";
    el.addPaused.checked = false;
    updateFileLabel();
}

function openAdd(links = []) {
    if (links.length) {
        const existing = el.addUrls.value.trim();
        el.addUrls.value = [...new Set([...(existing ? existing.split("\n") : []), ...links])].join("\n");
    }
    openSheet(el.addSheet);
    // with a prefilled link the next tap should be "Add", not the keyboard
    (links.length ? el.addSubmit : el.addUrls).focus();
}

el.fab.addEventListener("click", () => openAdd());
el.addFiles.addEventListener("change", updateFileLabel);

el.paste.addEventListener("click", async () => {
    try {
        const text = await navigator.clipboard.readText();
        const links = extractLinks(text);
        if (!links.length) { toast("Clipboard has no magnet link", true); return; }
        el.addUrls.value = links.join("\n");
    } catch {
        toast("Clipboard blocked — long-press the box and paste", true);
        el.addUrls.focus();
    }
});

el.addForm.addEventListener("submit", async (e) => {
    e.preventDefault();

    const lines = el.addUrls.value.split("\n").map((l) => l.trim()).filter(Boolean);
    const files = [...el.addFiles.files];
    if (!lines.length && !files.length) { toast("Paste a magnet link or pick a .torrent", true); return; }

    const fd = new FormData();
    if (lines.length) fd.append("urls", lines.join("\n"));
    for (const f of files) fd.append("torrents", f, f.name);
    fd.append("category", el.addCat.value);
    fd.append("paused", String(el.addPaused.checked));

    el.addSubmit.disabled = true;
    try {
        const r = await api("api/add", { method: "POST", body: fd, timeout: 60000 });
        storage.set("category", el.addCat.value);
        toast(r.count > 1 ? `Added ${r.count} torrents` : "Torrent added");
        resetAdd();
        el.addSheet.close();
        poll();
    } catch (err) {
        toast(err.message, true);
    } finally {
        el.addSubmit.disabled = false;
    }
});

// ------------------------------------------- incoming links (OS integration)

let lastIncoming = "";

/** Handles ?add= (magnet protocol handler) and ?text=/url=/title= (share target). */
function takeIncoming(url) {
    const p = url.searchParams;
    if (![...p.keys()].some((k) => ["add", "text", "url", "title"].includes(k))) return;

    // the launch that opened the page is delivered both in location and via launchQueue
    if (url.search === lastIncoming) return;
    lastIncoming = url.search;

    const links = extractLinks(p.get("add"), p.get("url"), p.get("text"), p.get("title"));

    // drop the params so a reload or relaunch doesn't offer the same link again
    history.replaceState(null, "", url.pathname);

    if (links.length) openAdd(links);
    else toast("That share didn't contain a magnet link", true);
}

takeIncoming(new URL(location.href));

// launch_handler focus-existing: later links arrive here instead of a new window
if ("launchQueue" in window) {
    window.launchQueue.setConsumer((params) => {
        if (params.targetURL) takeIncoming(new URL(params.targetURL));
    });
}

// Desktop browsers can register the handler from a normal tab too.
if (typeof navigator.registerProtocolHandler === "function") {
    el.handlerBtn.hidden = false;
    el.handlerBtn.addEventListener("click", () => {
        try {
            const base = location.origin + location.pathname.replace(/[^/]*$/, "");
            navigator.registerProtocolHandler("magnet", `${base}?add=%s`);
        } catch (e) {
            toast(e.message, true);
        }
    });
}

// ------------------------------------------------------------- top bar UI

el.chips.addEventListener("click", (e) => {
    const chip = e.target.closest(".chip");
    if (!chip) return;
    ui.filter = chip.dataset.f;
    storage.set("filter", ui.filter);
    for (const c of el.chips.children) c.classList.toggle("on", c === chip);
    render();
});

el.searchBtn.addEventListener("click", () => {
    el.search.hidden = !el.search.hidden;
    if (el.search.hidden) {
        el.search.value = "";
        ui.query = "";
        render();
    } else {
        el.search.focus();
    }
});

el.search.addEventListener("input", () => {
    ui.query = el.search.value.trim();
    render();
});

el.alt.addEventListener("click", async () => {
    el.alt.disabled = true;
    try {
        await api("api/altspeed", { method: "POST" });
        const on = el.alt.getAttribute("aria-pressed") !== "true";
        el.alt.setAttribute("aria-pressed", String(on));
        toast(on ? "Alternative speed limits on" : "Normal speed limits");
        poll();
    } catch (err) {
        toast(err.message, true);
    } finally {
        el.alt.disabled = false;
    }
});

// --------------------------------------------------------------------- boot

{
    const saved = storage.get("filter");
    if (saved && Object.hasOwn(FILTERS, saved)) {
        ui.filter = saved;
        for (const c of el.chips.children) c.classList.toggle("on", c.dataset.f === saved);
    }
}

if ("serviceWorker" in navigator) {
    navigator.serviceWorker.register("sw.js").catch(() => { /* app works without it */ });
}

poll();
