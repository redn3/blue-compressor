// Blue Compressor UI. The queue lives here; the Rust side scans, probes, previews and runs jobs.
// In the web UI the server also keeps the submitted queue, so a reload can restore it.
"use strict";

const T = window.__TAURI__;
const WEB = !T && location.protocol.startsWith("http") && location.hash !== "#demo";
const $ = (id) => document.getElementById(id);

// Outside Tauri: talk to the `bluecompress serve` HTTP/SSE backend when served over HTTP,
// or fall back to the offline stand-in when opened from a file:// URL or with #demo.
const api = T ? {
  invoke: (cmd, args) => T.core.invoke(cmd, args),
  listen: (ev, fn) => T.event.listen(ev, (e) => fn(e.payload)),
  onDragDrop: (fn) => T.webview.getCurrentWebview().onDragDropEvent((e) => fn(e.payload)),
} : WEB ? webApi() : browserStandIn();

const QUALITY = {
  1: { title: "Lossless", saving: "4 to 10% smaller", keep: 0.93,
       text: "The camera's coded data is repacked without its padding. Every pixel is identical to the original." },
  2: { title: "Archival", saving: "18 to 27% smaller", keep: 0.78,
       text: "Requantized just past the camera's own setting. The error stays below sensor noise." },
  3: { title: "High", saving: "39 to 43% smaller", keep: 0.59,
       text: "One step further. Shadow noise is smoothed slightly; detail and highlights are untouched." },
  4: { title: "Compact", saving: "58 to 62% smaller", keep: 0.4,
       text: "For offline and reference copies. Visible smoothing in deep shadows." },
};
const RESOLUTION_NOTES = {
  original: "Keeps the recorded raw resolution. Only the coded data changes.",
  "6k": "6048 × 3402 active. Clips already 6K or smaller keep their size.",
  "4k": "3840 × 2160 active (UHD). Raw is area-scaled per CFA colour, then re-encoded.",
};
const RES_LABEL = { original: "Original", "6k": "6K", "4k": "4K UHD" };
const BETA_ACK = "beta-ack-1";

// Short name of a setting, as the output folders are named: "Q2", "Q3 4K".
const variantName = (q, r) => `Q${q}${r === "6k" ? " 6K" : r === "4k" ? " 4K" : ""}`;
// Whether a job's resolution setting actually changes its size.
const resizes = (j, r) => r !== "original" && !(r === "6k" && j.width <= 6060) && !(r === "4k" && j.width <= 3852);
const settingLabel = (q, r) => `Q${q} ${QUALITY[q].title}${r !== "original" ? " · " + RES_LABEL[r] : ""}`;

const settings = Object.assign(
  { resolution: "original", quality: 2, backend: "gpu", outputFolder: null, dest: "next", sameForAll: true, squeeze: 1 },
  readSettings(),
);
let jobs = [];
let nextId = 1;
let running = false;
let paused = false;
// Totals of the current batch for the "saved so far" counter. Clips skipped at run time
// because their output already existed count as done, not towards the bytes.
const emptyBatch = () => ({ total: 0, done: 0, skipped: 0, failed: 0, inputBytes: 0, outputBytes: 0 });
let batch = emptyBatch();
let info = { gpu: "", cpu: "", platform: "" };
const els = new Map(); // job id -> card element

function readSettings() {
  try { return JSON.parse(localStorage.getItem("settings") || "{}"); } catch { return {}; }
}
function saveSettings() {
  try { localStorage.setItem("settings", JSON.stringify(settings)); } catch { /* private mode */ }
}

// Anamorphic de-squeeze: a horizontal stretch of what is shown (viewer and thumbnails).
// Display only: the raw data and the compressed output are never touched.
const SQUEEZE_PRESETS = [1, 1.25, 1.33, 1.5, 1.55, 1.6, 1.8, 2];
const SQUEEZE_MIN = 1, SQUEEZE_MAX = 3;
function clampSqueeze(v) {
  const n = Number(v);
  if (!Number.isFinite(n)) return 1;
  return Math.round(Math.min(SQUEEZE_MAX, Math.max(SQUEEZE_MIN, n)) * 100) / 100;
}
settings.squeeze = clampSqueeze(settings.squeeze);
const squeeze = () => settings.squeeze;
const squeezeLabel = (v) => `${Number(v.toFixed(2))}x`;

// Effective settings of a job: its own when settings are per clip, else the global ones.
const qualityOf = (j) => (settings.sameForAll ? settings.quality : j.quality);
const resolutionOf = (j) => (settings.sameForAll ? settings.resolution : j.resolution);

// Clips whose compressed copy for their current setting already exists are "already done":
// found when they are added and whenever a setting or the output folder changes, and left
// out of the run. "Compress again" (force) makes a new version anyway.
const outFolder = () => (settings.dest === "custom" ? settings.outputFolder : null);
const existKey = (j) => `${qualityOf(j)}|${resolutionOf(j)}|${outFolder() || ""}`;
const isAlready = (j) => j.ok && j.state === "waiting" && !j.force && !!j.existing;
// Already done in any form: found up front, or skipped by a run (kept as done).
const alreadyDone = (j) => isAlready(j) || (j.state === "done" && !!j.result && !!j.result.skipped);
const alreadyIn = (j) => j.bytes;
const alreadyOut = (j) => (j.existing && j.state === "waiting" ? j.existing.bytes : j.result.outputBytes);
const toCompress = (j) => j.ok && j.state === "waiting" && !isAlready(j);
const needsCheck = (j) => j.ok && j.state === "waiting" && !j.force && j.existKey !== existKey(j);

let existTimer = 0;
let existRun = null;
function scheduleExistingCheck() {
  if (!jobs.some(needsCheck)) return;
  clearTimeout(existTimer);
  existTimer = setTimeout(checkExisting, 120);
}
async function checkExisting() {
  while (existRun) await existRun;
  const stale = jobs.filter(needsCheck);
  if (!stale.length) return;
  existRun = (async () => {
    const folder = outFolder();
    const keys = new Map(stale.map((j) => [j.id, existKey(j)]));
    let found = [];
    try {
      for (let i = 0; i < stale.length; i += 500) {
        const items = stale.slice(i, i + 500).map((j) => ({
          id: j.id, path: j.path, quality: qualityOf(j), resolution: resolutionOf(j), width: j.width, height: j.height,
        }));
        const r = await api.invoke("check_existing", { outputFolder: folder, items });
        if (!Array.isArray(r)) throw new Error("no check_existing");
        found.push(...r);
      }
    } catch { found = null; }
    const byId = new Map((found || []).map((f) => [f.id, f]));
    for (const j of stale) {
      if (existKey(j) !== keys.get(j.id)) continue; // changed meanwhile: checked again next time
      j.existKey = keys.get(j.id);
      j.existing = byId.get(j.id) || null;
    }
    existRun = null;
    renderAll();
  })();
  await existRun;
}

// Formatting -----------------------------------------------------------------

function gb(bytes) {
  if (bytes >= 1e12) return (bytes / 1e12).toFixed(2) + " TB";
  if (bytes >= 1e9) return (bytes / 1e9).toFixed(2) + " GB";
  if (bytes >= 1e6) return (bytes / 1e6).toFixed(bytes >= 1e8 ? 0 : 1) + " MB";
  return (bytes / 1e3).toFixed(0) + " KB";
}
function clock(s) {
  if (!isFinite(s) || s < 0) return "";
  s = Math.round(s);
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
  return h ? `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}` : `${m}:${String(sec).padStart(2, "0")}`;
}
function timecode(frame, fps) {
  const f = Math.max(1, Math.round(fps || 25));
  const total = Math.floor(frame / f);
  const pad = (n) => String(n).padStart(2, "0");
  return `${pad(Math.floor(total / 3600))}:${pad(Math.floor(total / 60) % 60)}:${pad(total % 60)}:${pad(frame % f)}`;
}
const duration = (s) => (s < 60 ? `${s.toFixed(1)} s` : clock(s));
const fpsText = (f) => (f >= 100 ? f.toFixed(0) : f.toFixed(1));
const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

// Settings panel ---------------------------------------------------------------

function bindSeg(id, key, parse = (v) => v) {
  $(id).addEventListener("click", async (e) => {
    const b = e.target.closest("button");
    if (!b || running) return;
    settings[key] = parse(b.dataset.value);
    saveSettings();
    if (key === "resolution") await planTargets();
    renderAll();
  });
}

function setSeg(id, value, disabled = false) {
  for (const b of $(id).querySelectorAll("button")) {
    const on = String(value) === b.dataset.value;
    b.classList.toggle("active", on);
    b.setAttribute("aria-checked", on);
    if (disabled !== null) b.disabled = disabled;
  }
}

function renderSettings() {
  setSeg("seg-resolution", settings.resolution, running);
  setSeg("seg-quality", settings.quality, running);
  setSeg("seg-backend", settings.backend, running);
  document.body.classList.toggle("per-clip", !settings.sameForAll);
  $("same-all").checked = settings.sameForAll;
  $("same-all").disabled = running;
  $("same-sub").textContent = settings.sameForAll
    ? "Applies to every clip in the queue"
    : "Each clip keeps its own settings";
  $("label-resolution").textContent = settings.sameForAll ? "Resolution" : "Default resolution";
  $("label-quality").textContent = settings.sameForAll ? "Quality" : "Default quality";
  $("resolution-note").textContent = RESOLUTION_NOTES[settings.resolution];
  const q = QUALITY[settings.quality];
  $("quality-title").textContent = `Q${settings.quality} ${q.title}`;
  $("quality-saving").textContent = q.saving;
  $("quality-text").textContent = q.text;
  $("quality-meter").style.width = `${q.keep * 100}%`;
  $("quality-meter-label").textContent = `about ${Math.round(q.keep * 100)}% of the original`;
  $("backend-note").textContent = backendNote();
  $("seg-backend").classList.toggle("inactive", !anyResize());
  const custom = settings.dest === "custom" && settings.outputFolder;
  setSeg("seg-dest", custom ? "custom" : "next", running);
  $("dest-custom").textContent = custom ? "Folder" : "Choose folder";
  const hint = $("dest-hint");
  if (custom) {
    hint.textContent = "";
    hint.title = "File names are kept. A second version of a clip goes into a sub-folder named after its setting.";
    const path = document.createElement("span");
    path.className = "dest-path";
    path.textContent = settings.outputFolder;
    path.title = settings.outputFolder;
    const change = document.createElement("a");
    change.textContent = "Change";
    change.className = "link";
    change.addEventListener("click", chooseOutput);
    hint.append(path, change);
  } else {
    hint.textContent = settings.sameForAll
      ? `In a “Compressed ${variantName(settings.quality, settings.resolution)}” folder beside each clip.`
      : "In a “Compressed Q…” folder beside each clip, named after its setting.";
  }
  const chip = $("engine-chip");
  chip.classList.toggle("cpu", settings.backend === "cpu");
  $("engine-text").textContent = settings.backend === "cpu" || !info.gpuAvailable ? (info.cpu || "CPU") : (info.gpu || "GPU");
}

// Quality-only jobs are entropy coding, which is bit-serial and runs on the CPU whatever is
// chosen; only resizing has GPU work. Say which applies to the current queue.
function anyResize() {
  const pending = jobs.filter(toCompress);
  return pending.length ? pending.some((j) => resizes(j, resolutionOf(j))) : settings.resolution !== "original";
}
function backendNote() {
  const gpuName = (info.gpu || "").replace(/^[^:]*:\s*/, "");
  if (!anyResize()) {
    return "Not used at the original resolution. Changing only the quality re-does the bit-level entropy coding, " +
      "which runs on the CPU cores; there is no GPU work in it.";
  }
  if (settings.backend === "cpu") return "6K and 4K resizing runs on the CPU cores, about two to three times slower.";
  if (!info.gpuAvailable) return "No supported GPU found, so 6K and 4K resizing runs on the CPU cores.";
  return `6K and 4K resizing runs on the ${gpuName || "GPU"}; the entropy coding around it stays on the CPU. ` +
    "Matches the CPU result to within float rounding.";
}

// Queue ------------------------------------------------------------------------

// In the web UI clips can be added while the queue runs; they are handed to the server
// at once and compressed after the current ones.
const canAdd = () => !running || WEB;

async function addPaths(paths) {
  if (!paths || !paths.length || !canAdd()) return;
  const clips = await api.invoke("scan", { paths });
  const known = new Set(jobs.filter((j) => j.state === "waiting" || j.state === "running").map((j) => j.path));
  const fresh = [];
  for (const c of clips) {
    if (known.has(c.path)) continue;
    known.add(c.path);
    const j = {
      ...c, id: nextId++, state: c.ok ? "waiting" : "failed", fraction: 0, rate: 0, target: null,
      quality: settings.quality, resolution: settings.resolution,
    };
    jobs.push(j);
    fresh.push(j);
  }
  if (fresh.length) {
    await planTargets();
    await checkExisting();
    if (running && WEB) await enqueue(fresh.filter(toCompress), fresh.filter(isAlready));
    renderAll();
    loadThumbs();
  }
}

// Adds a fresh copy of a finished clip to the queue, right after it, to compress it again
// with other settings. Failed and cancelled clips are simply reset.
async function again(id) {
  if (running) return;
  const j = jobs.find((x) => x.id === id);
  if (!j || !j.ok) return;
  if (isAlready(j)) {
    j.force = true;
    renderAll();
    return;
  }
  if (j.state === "failed" || j.state === "cancelled") {
    Object.assign(j, { state: "waiting", error: null, fraction: 0, rate: 0, existKey: null });
    renderAll();
    return;
  }
  const copy = {
    ...j, id: nextId++, state: "waiting", fraction: 0, rate: 0, result: null, output: null, ran: null, inRun: false,
    onServer: false, force: true,
    quality: j.ran ? j.ran.quality : j.quality, resolution: j.ran ? j.ran.resolution : j.resolution,
  };
  copy.target = await targetFor(copy, resolutionOf(copy));
  jobs.splice(jobs.indexOf(j) + 1, 0, copy);
  renderAll();
  const el = jobCard(copy);
  el.scrollIntoView({ block: "nearest", behavior: "smooth" });
  el.classList.add("flash");
  setTimeout(() => el.classList.remove("flash"), 1200);
}

// Clip thumbnails: one decode per clip, one at a time, in the background.
const thumbs = new Map(); // path -> ImageBitmap | "pending" | "failed"
let thumbsBusy = false;
async function loadThumbs() {
  if (thumbsBusy) return;
  const j = jobs.find((x) => x.ok && !thumbs.has(x.path));
  if (!j) return;
  thumbsBusy = true;
  thumbs.set(j.path, "pending");
  try {
    const buf = await api.invoke("thumbnail", { path: j.path, width: Math.round(96 * dpr()) });
    const bytes = buf instanceof ArrayBuffer ? buf : new Uint8Array(buf).buffer;
    const dv = new DataView(bytes);
    const w = dv.getUint32(0, true), h = dv.getUint32(4, true);
    thumbs.set(j.path, await createImageBitmap(new ImageData(new Uint8ClampedArray(bytes, 8, w * h * 4), w, h)));
  } catch {
    thumbs.set(j.path, "failed");
  }
  thumbsBusy = false;
  for (const x of jobs) if (x.path === j.path) drawThumb(x);
  loadThumbs();
}

function drawThumb(j) {
  const c = jobCard(j).querySelector(".job-thumb-img");
  const t = thumbs.get(j.path);
  if (!(t instanceof ImageBitmap)) { c.classList.remove("ready"); return; }
  if (c.drawn === t && c.drawnSqueeze === squeeze()) return;
  c.width = Math.round(t.width * squeeze()); c.height = t.height;
  const ctx = c.getContext("2d");
  ctx.imageSmoothingQuality = "high";
  ctx.drawImage(t, 0, 0, c.width, c.height);
  c.drawn = t;
  c.drawnSqueeze = squeeze();
  c.classList.add("ready");
}

const targetCache = new Map();
async function targetFor(j, res) {
  if (res === "original") return null;
  const key = `${j.width}x${j.height}@${res}`;
  if (!targetCache.has(key)) targetCache.set(key, await api.invoke("plan_output", { width: j.width, height: j.height, resolution: res }));
  return targetCache.get(key);
}

async function planTargets() {
  await Promise.all(jobs.filter((j) => j.ok && j.state === "waiting").map(async (j) => {
    j.target = await targetFor(j, resolutionOf(j));
  }));
}

function jobCard(j) {
  let el = els.get(j.id);
  if (el) return el;
  el = $("tpl-job").content.firstElementChild.cloneNode(true);
  el.dataset.id = j.id;
  const job = () => jobs.find((x) => x.id === j.id);
  el.querySelector(".job-remove").addEventListener("click", (e) => { e.stopPropagation(); removeJob(j.id); });
  el.querySelector(".job-reveal").addEventListener("click", (e) => {
    e.stopPropagation();
    const x = job();
    api.invoke("reveal", { path: x.state === "done" ? x.output : x.path }).catch(() => {});
  });
  el.querySelector(".job-preview").addEventListener("click", (e) => { e.stopPropagation(); openViewer(job()); });
  el.querySelector(".job-again").addEventListener("click", (e) => { e.stopPropagation(); again(j.id); });
  const qSel = el.querySelector(".job-q"), rSel = el.querySelector(".job-r");
  for (const sel of [qSel, rSel]) sel.addEventListener("click", (e) => e.stopPropagation());
  qSel.addEventListener("change", () => { const x = job(); x.quality = Number(qSel.value); updateCard(x); renderFooter(); });
  rSel.addEventListener("change", async () => {
    const x = job(); x.resolution = rSel.value; x.target = await targetFor(x, x.resolution); updateCard(x); renderFooter();
  });
  el.addEventListener("click", () => { const x = job(); if (x.ok) openViewer(x); });
  els.set(j.id, el);
  return el;
}

function updateCard(j) {
  const el = jobCard(j);
  const isPaused = paused && j.state === "running";
  const already = isAlready(j);
  el.className = `job ${j.state}${already ? " already" : ""}${isPaused ? " paused" : ""}${viewer.job && viewer.job.id === j.id ? " selected" : ""}`;
  const thumb = el.querySelector(".job-thumb");
  thumb.className = `job-thumb${j.codec === "R3D NE" ? " r3d" : ""}${j.ok ? "" : " bad"}`;
  el.querySelector(".job-codec").textContent = j.ok ? j.codec : "?";
  drawThumb(j);
  el.querySelector(".job-name").textContent = j.name;
  el.title = j.ok ? `${j.path}\nClick to preview the quality` : j.path;

  const pill = el.querySelector(".job-pill");
  const pills = { waiting: "Waiting", running: "Compressing", done: "Done", failed: "Failed", cancelled: "Cancelled" };
  const skipped = j.state === "done" && j.result && j.result.skipped;
  pill.textContent = j.state === "failed" && !j.ok ? "Unreadable" : isPaused ? "Paused" : skipped || already ? "Already done" : pills[j.state];
  pill.className = `job-pill ${isPaused ? "paused" : already ? "done" : j.state}`;

  const meta = el.querySelector(".job-meta");
  if (!j.ok || (j.state === "failed" && j.error)) {
    meta.className = "job-meta error";
    meta.textContent = j.error || "Could not read this clip";
  } else {
    meta.className = "job-meta";
    const sep = '<span class="sep">·</span>';
    let html = `${j.width} × ${j.height}${sep}${j.frames} frames${sep}${j.fps.toFixed(j.fps % 1 ? 3 : 0)} fps${sep}${gb(j.bytes)}`;
    const t = j.state === "done" ? j.resultTarget : j.target;
    if (t && (t[0] !== j.width || t[1] !== j.height)) html += `${sep}<span class="target">→ ${t[0]} × ${t[1]}</span>`;
    else if (t && j.state === "waiting") html += `${sep}stays at this size`;
    meta.innerHTML = html;
  }

  const after = el.querySelector(".job-after"), againBtn = el.querySelector(".job-again");
  const finished = j.ok && (j.state === "done" || j.state === "failed" || j.state === "cancelled");
  after.hidden = !finished && !already;
  if (already) {
    const folder = j.existing.output.split(/[\\/]/).slice(-2, -1)[0] || "";
    el.querySelector(".job-after-info").innerHTML = `${esc(settingLabel(qualityOf(j), resolutionOf(j)))}<span class="sep">·</span>` +
      `<span class="job-out" title="${esc(j.existing.output)}">${esc(folder)}/</span>`;
    againBtn.textContent = "Compress again";
    againBtn.title = "A compressed copy for this setting already exists, so this clip will be skipped. Compress it anyway to make a new version.";
    againBtn.disabled = running;
  } else if (finished) {
    const info = el.querySelector(".job-after-info");
    if (j.state === "done" && j.ran) {
      const folder = j.output ? j.output.split(/[\\/]/).slice(-2, -1)[0] : "";
      info.innerHTML = `${esc(settingLabel(j.ran.quality, j.ran.resolution))}<span class="sep">·</span>` +
        `<span class="job-out" title="${esc(j.output || "")}">${esc(folder)}/</span>`;
    } else {
      info.textContent = j.ran ? settingLabel(j.ran.quality, j.ran.resolution) : "";
    }
    againBtn.textContent = j.state === "done" ? "Compress again" : "Try again";
    againBtn.title = j.state === "done"
      ? "Add this clip to the queue again, to make another version with different settings"
      : "Put this clip back in the queue";
    againBtn.disabled = running;
    if (skipped) againBtn.title = "A compressed copy was already there, so this clip was skipped. Add it again to make a new version.";
  }

  const qSel = el.querySelector(".job-q"), rSel = el.querySelector(".job-r");
  qSel.value = String(j.quality);
  rSel.value = j.resolution;
  rSel.options[1].disabled = j.width <= 6060;
  rSel.options[2].disabled = j.width <= 3852;
  qSel.disabled = rSel.disabled = running || j.state !== "waiting" || !j.ok;

  const stat = el.querySelector(".job-stat"), sub = el.querySelector(".job-stat-sub");
  const fill = el.querySelector(".job-progress-fill");
  if (j.state === "running") {
    stat.textContent = `${Math.floor(j.fraction * 100)}%`;
    const left = j.rate > 0 ? (1 - j.fraction) * j.frames / j.rate : NaN;
    sub.textContent = isPaused ? "Paused" : j.rate > 0 ? `${fpsText(j.rate)} fps · ${clock(left)} left` : "Starting";
    fill.style.width = `${j.fraction * 100}%`;
  } else if (already) {
    stat.textContent = gb(j.existing.bytes);
    sub.textContent = "Already compressed";
    sub.title = `A compressed copy already exists at ${j.existing.output}`;
    fill.style.width = "100%";
  } else if (skipped) {
    const r = j.result;
    stat.textContent = gb(r.outputBytes);
    sub.textContent = "Already compressed, skipped";
    sub.title = `A compressed copy already exists at ${r.output}`;
    fill.style.width = "100%";
  } else if (j.state === "done") {
    const r = j.result;
    const saved = 1 - r.outputBytes / Math.max(1, r.inputBytes);
    stat.innerHTML = `${gb(r.outputBytes)} <span class="good">−${(saved * 100).toFixed(1)}%</span>`;
    const onGpu = r.backend !== "CPU";
    const resized = j.ran && resizes(j, j.ran.resolution);
    const how = resized
      ? `resized on ${onGpu ? r.backend.replace(/\s*\(.*\)$/, "") + " GPU" : "CPU"}`
      : "requantized on CPU";
    sub.textContent = `${duration(r.seconds)} · ${fpsText(r.fps)} fps · ${how}`;
    sub.title = resized
      ? `Resized on ${r.backend}; entropy coding on the CPU`
      : "At the original resolution only the entropy coding is redone, which runs on the CPU. The GPU is used for 6K and 4K.";
    fill.style.width = "100%";
  } else if (j.state === "waiting" && j.ok) {
    const est = estimateBytes(j);
    sub.title = "";
    stat.textContent = `≈ ${gb(est.bytes)}`;
    sub.textContent = est.measured ? "measured in preview" : "estimated";
    fill.style.width = "0";
  } else {
    stat.textContent = "";
    sub.title = "";
    sub.textContent = j.state === "cancelled" ? "Partial output removed" : "";
    fill.style.width = "0";
  }
  el.querySelector(".job-reveal").hidden = !(j.state === "done" || j.ok);
  el.querySelector(".job-reveal").title = j.state === "done" ? `${revealLabel()} (compressed copy)` : revealLabel();
  el.querySelector(".job-preview").hidden = !j.ok;
  el.querySelector(".job-remove").hidden = running && !(WEB && j.state !== "running");
}

function revealLabel() {
  return info.platform === "macos" ? "Show in Finder" : info.platform === "windows" ? "Show in Explorer" : "Show in folder";
}

function areaRatio(j, t) {
  return t ? (t[0] * t[1]) / (j.width * j.height) : 1;
}

// Size estimate for a job's current settings: the ratio measured in a preview when there is
// one, otherwise the calibrated typical ratio for the quality.
function estimateBytes(j) {
  const q = qualityOf(j), r = resolutionOf(j);
  const m = measured.get(`${j.path}|${q}|${r}`);
  if (m != null) return { bytes: j.bytes * m, measured: true };
  return { bytes: j.bytes * QUALITY[q].keep * areaRatio(j, j.target), measured: false };
}

function dropJobs(gone) {
  if (!gone.length) return;
  const ids = new Set(gone.map((j) => j.id));
  if (viewer.job && ids.has(viewer.job.id)) closeViewer();
  for (const id of ids) {
    const el = els.get(id);
    if (el) el.remove();
    els.delete(id);
  }
  jobs = jobs.filter((j) => !ids.has(j.id));
}

function removeJob(id) {
  const j = jobs.find((x) => x.id === id);
  if (!j || j.state === "running" || (running && !WEB)) return;
  if (running && j.inRun && j.state === "waiting") batch.total = Math.max(0, batch.total - 1);
  dropJobs([j]);
  if (WEB && j.onServer) api.invoke("remove_jobs", { ids: [id] }).catch(() => {});
  renderAll();
}

const isFinished = (j) => j.state !== "waiting" && j.state !== "running";

// Clear List: with nothing running, empties the whole queue. While compressing, the clip
// being compressed and the ones still waiting for it stay; everything finished goes.
function clearList() {
  if (running) {
    dropJobs(jobs.filter(isFinished));
    if (WEB) api.invoke("remove_jobs", {}).catch(() => {});
  } else {
    dropJobs(jobs.slice());
    batch = emptyBatch();
    if (WEB) api.invoke("remove_jobs", { all: true }).catch(() => {});
  }
  renderAll();
}

function renderAll() {
  const list = $("list");
  $("empty").hidden = jobs.length > 0;
  list.hidden = jobs.length === 0;
  // Keep the cards in queue order (a copy made by "Compress again" sits under its original).
  let prev = null;
  for (const j of jobs) {
    const el = jobCard(j);
    const want = prev ? prev.nextSibling : list.firstChild;
    if (el !== want) list.insertBefore(el, want);
    prev = el;
    updateCard(j);
  }
  renderFooter();
  renderSettings();
}

// Expected output/input size ratio of a clip being compressed, before any clip of the
// batch has finished: measured in a preview if there is one, else the typical ratio.
function liveRatio(j) {
  const q = j.ran ? j.ran.quality : qualityOf(j), r = j.ran ? j.ran.resolution : resolutionOf(j);
  const m = measured.get(`${j.path}|${q}|${r}`);
  return m != null ? m : QUALITY[q].keep * areaRatio(j, j.target);
}

function renderFooter() {
  scheduleExistingCheck();
  const ok = jobs.filter((j) => j.ok);
  const waiting = ok.filter(toCompress);
  const already = ok.filter(isAlready);
  const anyDone = ok.filter(alreadyDone);
  const total = ok.reduce((s, j) => s + j.bytes, 0);
  const aIn = anyDone.reduce((s, j) => s + alreadyIn(j), 0), aOut = anyDone.reduce((s, j) => s + alreadyOut(j), 0);
  let meta = "";
  if (jobs.length) {
    meta = `${jobs.length} clip${jobs.length === 1 ? "" : "s"}`;
    if (anyDone.length) {
      const left = ok.filter((j) => toCompress(j) || j.state === "running").reduce((s, j) => s + j.bytes, 0);
      meta += ` · ${anyDone.length} already done` + (aIn > aOut ? ` (saved ${gb(aIn - aOut)})` : "") +
        (left ? ` · ${gb(left)} to compress` : "");
    } else {
      meta += ` · ${gb(total)}`;
    }
  }
  $("queue-meta").textContent = meta;
  $("queue-meta").title = anyDone.length
    ? "Clips whose compressed copy for their setting already exists are skipped and not counted in the saved-so-far total of a run."
    : "";
  const clear = $("btn-clear");
  clear.disabled = running ? !jobs.some(isFinished) : jobs.length === 0;
  clear.title = running
    ? "Removes finished clips. The clip being compressed and the waiting ones stay until the queue is done or stopped."
    : "Removes every clip from the list. Compressed files are kept.";
  $("btn-add").disabled = $("btn-add-folder").disabled = !canAdd();
  const pause = $("btn-pause");
  pause.hidden = !running;
  $("pause-label").textContent = paused ? "Resume" : "Pause";
  $("pause-icon").innerHTML = paused
    ? '<path d="M4.5 3.2v9.6L12.5 8 4.5 3.2Z" class="fill"/>'
    : '<rect x="4" y="3.5" width="2.6" height="9" rx="0.8" class="fill"/><rect x="9.4" y="3.5" width="2.6" height="9" rx="0.8" class="fill"/>';
  pause.title = paused ? "Continue compressing" : "Pause the queue. The current clip pauses where it is and continues on Resume.";

  const go = $("btn-go");
  go.classList.toggle("stop", running);
  $("go-icon").innerHTML = running
    ? '<rect x="4" y="4" width="8" height="8" rx="1.5" class="fill"/>'
    : '<path d="M4.5 3.2v9.6L12.5 8 4.5 3.2Z" class="fill"/>';
  const allDone = !waiting.length && already.length > 0;
  $("go-label").textContent = running ? "Stop"
    : waiting.length ? `Compress ${waiting.length} clip${waiting.length === 1 ? "" : "s"}`
    : allDone ? "All clips already done" : "Compress";
  go.disabled = !running && waiting.length === 0;

  const main = $("bar-main"), sub = $("bar-sub"), fill = $("bar-fill");
  const pctOf = (saved, base) => (base > 0 ? (saved / base) * 100 : 0);
  if (running) {
    const active = ok.filter((j) => j.inRun);
    const frames = active.reduce((s, j) => s + j.frames, 0);
    const doneFrames = active.reduce((s, j) => s + (j.state === "done" ? j.frames : j.state === "running" ? j.fraction * j.frames : 0), 0);
    const cur = jobs.find((j) => j.state === "running");
    const fps = cur ? cur.rate : 0;
    // Saved so far: finished clips exactly, plus the part of the current clip already
    // processed at the ratio measured so far in this batch (or the quality's typical one).
    const liveIn = cur ? cur.fraction * cur.bytes : 0;
    // Before a clip of this batch finishes, clips already compressed earlier are the best guess.
    const ratio = batch.inputBytes > 0 ? batch.outputBytes / batch.inputBytes
      : aIn > 0 ? aOut / aIn : cur ? liveRatio(cur) : 1;
    const saved = batch.inputBytes - batch.outputBytes + liveIn * (1 - ratio);
    const count = `${batch.done} of ${Math.max(batch.total, batch.done)} done`;
    const lead = paused ? "Paused · " : "";
    if (saved > 0) {
      main.innerHTML = `${lead}Saved ${gb(saved)} so far <span class="good">−${pctOf(saved, batch.inputBytes + liveIn).toFixed(1)}%</span> · ${count}`;
    } else {
      main.textContent = `${lead}${cur ? "Compressing" : "Preparing"} · ${count}`;
    }
    sub.textContent = paused
      ? (cur ? `${cur.name} · ${Math.floor(cur.fraction * 100)}% · paused` : "Resume to continue with the next clip")
      : cur ? `${cur.name}` + (fps > 0 ? ` · ${fpsText(fps)} fps · about ${clock((frames - doneFrames) / fps)} left for the queue` : "") : "";
    fill.style.width = frames ? `${(doneFrames / frames) * 100}%` : "0";
  } else if (batch.done > 0) {
    const savedIn = batch.inputBytes, savedOut = batch.outputBytes;
    const compressed = batch.done - batch.skipped;
    main.innerHTML = `Saved ${gb(Math.max(0, savedIn - savedOut))} <span class="good">−${pctOf(savedIn - savedOut, savedIn).toFixed(1)}%</span>` +
      ` · ${batch.done} of ${Math.max(batch.total, batch.done)} done`;
    sub.textContent = `${compressed} clip${compressed === 1 ? "" : "s"} compressed · ${gb(savedIn)} → ${gb(savedOut)}` +
      (batch.skipped ? ` · ${batch.skipped} skipped (already done)` : "") +
      (batch.failed ? ` · ${batch.failed} failed` : "") +
      (waiting.length ? ` · ${waiting.length} waiting` : "");
    fill.style.width = "0";
  } else if (waiting.length) {
    const est = waiting.reduce((s, j) => s + estimateBytes(j).bytes, 0);
    const tw = waiting.reduce((s, j) => s + j.bytes, 0);
    main.textContent = `${gb(tw)} → about ${gb(est)}`;
    sub.textContent = settings.sameForAll
      ? `Estimated for Q${settings.quality} ${QUALITY[settings.quality].title}` +
        (settings.resolution !== "original" ? ` at ${RES_LABEL[settings.resolution]}` : "")
      : "Estimated with each clip's own settings";
    fill.style.width = "0";
  } else if (allDone) {
    main.textContent = `All ${already.length} clip${already.length === 1 ? " is" : "s are"} already compressed`;
    sub.textContent = `${gb(aIn)} → ${gb(aOut)} · use Compress again on a clip to make a new version`;
    fill.style.width = "0";
  } else {
    main.textContent = jobs.length ? "Nothing to compress" : "No clips yet";
    sub.textContent = jobs.length ? "Add clips or clear the finished ones" : "Add clips to see the estimated savings";
    fill.style.width = "0";
  }
}

// Running ----------------------------------------------------------------------

const jobRequest = (j) => ({
  id: j.id, path: j.path, quality: j.ran.quality, resolution: j.ran.resolution, force: !!j.force,
  clip: { name: j.name, codec: j.codec, width: j.width, height: j.height, frames: j.frames, fps: j.fps, bytes: j.bytes },
  existing: j.alreadyRef || null,
});

// Clips found already done become done ("Already done") when a run starts, like the ones a
// run skips. The web server keeps them too, so a reloaded page still shows them.
function settleAlready(list) {
  for (const j of list) {
    j.ran = { quality: qualityOf(j), resolution: resolutionOf(j) };
    j.alreadyRef = { output: j.existing.output, bytes: j.existing.bytes };
    j.state = "done";
    j.output = j.existing.output;
    j.result = {
      id: j.id, skipped: true, output: j.existing.output, inputBytes: j.bytes, outputBytes: j.existing.bytes,
      frames: 0, seconds: 0, fps: 0, backend: "",
    };
    j.resultTarget = j.target;
    j.onServer = WEB;
  }
}
const runSettings = () => ({ backend: settings.backend, outputFolder: settings.dest === "custom" ? settings.outputFolder : null });

async function start() {
  await checkExisting();
  const todo = jobs.filter(toCompress);
  if (!todo.length) return;
  const settled = jobs.filter(isAlready);
  for (const j of jobs) {
    j.inRun = todo.includes(j);
    // The server drops waiting clips that are not in a new batch.
    if (j.state === "waiting" && !j.inRun) j.onServer = false;
  }
  for (const j of todo) { j.ran = { quality: qualityOf(j), resolution: resolutionOf(j) }; j.onServer = WEB; }
  running = true;
  paused = false;
  batch = emptyBatch();
  batch.total = todo.length;
  settleAlready(settled);
  // Only the web server keeps already-done clips; the desktop app gets just the work.
  const sent = WEB ? jobs.filter((j) => todo.includes(j) || settled.includes(j)) : todo;
  if (viewer.open) closeViewer();
  renderAll();
  try {
    await api.invoke("start", { jobs: sent.map(jobRequest), settings: runSettings() });
  } catch (e) {
    running = false;
    renderAll();
  }
}

// Web UI only: hands clips added during a run to the server, behind the running ones.
async function enqueue(todo, settled = []) {
  if (!todo.length && !settled.length) return;
  for (const j of todo) { j.ran = { quality: qualityOf(j), resolution: resolutionOf(j) }; j.inRun = true; j.onServer = true; }
  batch.total += todo.length;
  settleAlready(settled);
  const sent = jobs.filter((j) => todo.includes(j) || settled.includes(j));
  try {
    await api.invoke("start", { jobs: sent.map(jobRequest), settings: runSettings() });
  } catch { for (const j of todo) { j.inRun = false; j.onServer = false; } }
}

function stop() { api.invoke("cancel"); }

async function togglePause() {
  if (!running) return;
  const want = !paused;
  paused = want;
  renderAll();
  try {
    const r = await api.invoke("set_paused", { paused: want });
    if (r && typeof r.paused === "boolean") paused = r.paused;
  } catch { paused = !want; }
  renderAll();
}

// Web UI: rebuilds the list from the server's queue, so a reloaded page (or another tab)
// shows the same clips, progress and pause state. Clips only added locally are kept.
function fromServer(s) {
  const c = s.clip || {};
  return {
    path: s.path, name: c.name || s.name, ok: true, codec: c.codec || "", width: c.width || 0, height: c.height || 0,
    frames: c.frames || 0, fps: c.fps || 0, bytes: c.bytes || s.bytes, error: s.error,
    id: s.id, state: s.state, fraction: s.fraction, rate: s.fps, target: null, force: s.force,
    quality: s.quality, resolution: s.resolution, ran: { quality: s.quality, resolution: s.resolution },
    output: s.output, result: s.result, onServer: true,
  };
}

async function syncQueue(snap) {
  if (!snap || !Array.isArray(snap.jobs)) return;
  running = !!snap.running;
  paused = !!snap.paused;
  if (snap.batch) batch = { ...emptyBatch(), ...snap.batch };
  const ids = new Set(snap.jobs.map((s) => s.id));
  dropJobs(jobs.filter((j) => j.onServer && !ids.has(j.id)));
  const added = [];
  for (const s of snap.jobs) {
    let j = jobs.find((x) => x.id === s.id);
    if (!j) {
      j = fromServer(s);
      jobs.push(j);
      added.push(j);
    } else {
      Object.assign(j, {
        state: s.state, fraction: s.fraction, rate: s.fps, error: s.error, onServer: true,
        output: s.output || j.output, result: s.result || j.result,
      });
    }
    // Finished clips of a restored run still count towards "n of m" in the footer.
    const isNew = added.includes(j);
    j.inRun = running && (s.state === "waiting" || s.state === "running" || (isNew ? true : !!j.inRun));
    nextId = Math.max(nextId, s.id + 1);
  }
  for (const j of added) {
    const t = await targetFor(j, j.resolution);
    if (j.state === "done") j.resultTarget = t;
    else j.target = t;
  }
  renderAll();
  if (added.length) loadThumbs();
}

async function restoreQueue() {
  if (!WEB) return;
  try { await syncQueue(await api.invoke("get_queue")); } catch { /* older server */ }
}

function onEvents() {
  const find = (id) => jobs.find((j) => j.id === id);
  api.listen("job-start", (p) => {
    const j = find(p.id); if (!j) return;
    j.state = "running"; j.output = p.output; j.fraction = 0; j.rate = 0;
    updateCard(j); renderFooter();
    jobCard(j).scrollIntoView({ block: "nearest", behavior: "smooth" });
  });
  api.listen("job-progress", (p) => {
    const j = find(p.id); if (!j) return;
    j.fraction = p.fraction; j.rate = p.fps;
    updateCard(j); renderFooter();
  });
  api.listen("job-done", (p) => {
    batch.done++;
    if (p.skipped) batch.skipped++;
    else { batch.inputBytes += p.inputBytes; batch.outputBytes += p.outputBytes; }
    const j = find(p.id); if (!j) { renderFooter(); return; }
    j.state = "done"; j.result = p; j.output = p.output; j.resultTarget = j.target; j.existKey = null;
    updateCard(j); renderFooter();
  });
  api.listen("job-error", (p) => {
    if (!p.cancelled) batch.failed++;
    const j = find(p.id); if (!j) { renderFooter(); return; }
    j.state = p.cancelled ? "cancelled" : "failed";
    j.error = p.cancelled ? null : p.error;
    updateCard(j); renderFooter();
  });
  api.listen("queue-finished", () => {
    running = false;
    paused = false;
    // Outputs written by this run can make other waiting clips already done: check again.
    for (const j of jobs) { j.inRun = false; if (j.state === "waiting") j.existKey = null; }
    renderAll();
  });
  api.listen("queue-paused", (p) => {
    paused = !!(p && p.paused);
    renderAll();
  });
  api.listen("queue-changed", (snap) => syncQueue(snap));
  api.listen("events-reconnected", () => restoreQueue());
  api.listen("open-paths", (paths) => addPaths(paths));
  api.listen("quit-requested", () => $("quit-modal").classList.add("show"));
  api.listen("watch-status", (w) => renderWatch(w));
}

function renderWatch(w) {
  const chip = $("watch-chip"), sec = $("watch-section");
  if (!chip || !sec) return;
  if (!w || !w.enabled) {
    chip.hidden = true;
    sec.hidden = true;
    return;
  }
  chip.hidden = false;
  sec.hidden = false;
  const dot = $("watch-dot"), pill = $("watch-pill"), meter = $("watch-meter"), fill = $("watch-meter-fill");
  const q = QUALITY[w.quality] ? w.quality : 3;
  const res = w.resolution || "original";
  $("watch-preset").textContent = settingLabel(q, res);
  if ($("watch-quality")) setSeg("watch-quality", q, null);
  if ($("watch-resolution")) setSeg("watch-resolution", res, null);
  if ($("watch-clear")) {
    $("watch-clear").checked = !!w.clearAfterProcess;
    $("watch-clear-sub").textContent = w.clearAfterProcess
      ? "Deletes source clip after compression succeeds"
      : "Keeps source clips in watch input folder";
  }
  $("watch-dirs").textContent = `${w.inputDir} → ${w.outputDir}`;
  $("watch-dirs").title = `Input: ${w.inputDir}
Output: ${w.outputDir}`;
  dot.className = `chip-dot ${w.state || "idle"}`;
  if (w.state === "running" && w.currentClip) {
    const pct = Math.floor((w.fraction || 0) * 100);
    $("watch-chip-text").textContent = `Watch: ${w.currentClip} (${pct}%)`;
    pill.textContent = `${pct}%`;
    pill.className = "job-pill running";
    $("watch-stats").textContent = `${w.currentClip}${w.fps > 0 ? " · " + fpsText(w.fps) + " fps" : ""}`;
    meter.hidden = false;
    fill.style.width = `${(w.fraction || 0) * 100}%`;
  } else if (w.state === "stabilizing") {
    $("watch-chip-text").textContent = "Watch: waiting for copy";
    pill.textContent = "Copying";
    pill.className = "job-pill waiting";
    $("watch-stats").textContent = "Waiting for incoming file copy to finish";
    meter.hidden = true;
  } else {
    $("watch-chip-text").textContent = w.doneCount ? `Watch: ${w.doneCount} done` : `Watch: ${w.inputDir}`;
    pill.textContent = "Watching";
    pill.className = "job-pill done";
    if (w.lastError) {
      $("watch-stats").textContent = `Last error: ${w.lastError}`;
    } else if (w.doneCount > 0) {
      $("watch-stats").textContent = `${w.doneCount} done · ${gb(w.savedInputBytes || 0)} → ${gb(w.savedOutputBytes || 0)}`;
    } else {
      $("watch-stats").textContent = `Polling every ${w.pollSeconds || 5}s for .NEV and .R3D clips`;
    }
    meter.hidden = true;
  }
}

// Quality previewer ------------------------------------------------------------
//
// Coordinates: the frame is a grid of QW x QH CFA quads (half the sensor size), and `scale`
// is device pixels per quad, so 100% shows one quad per screen pixel. (cx, cy) is the frame
// point at the centre of the stage, in fractions of the frame. With an anamorphic
// de-squeeze the frame is shown dispW() = QW * squeeze wide: renders are still requested in
// quads and only stretched sideways when drawn.
//
// Two layers are drawn, each as before everywhere and after right of the split line:
// a fit-size "base" image of the whole frame, and when zoomed in a "region" image rendered
// by the core for exactly the visible part at screen resolution. While a new region is on
// its way the previous one stays in place over a soft upscale of the base, so panning and
// zooming never show a blank stage. The core keeps the last decoded frame, which makes
// region renders for the same frame and settings cheap; a new frame or setting decodes.

const viewer = {
  open: false, job: null, frame: 0, quality: 2, resolution: "original",
  split: 0.5, scale: 1, fit: true, cx: 0.5, cy: 0.5,
  base: null, region: null,
};
const baseCache = new Map();    // path|frame|q|r -> layer
const regionCache = new Map();  // path|frame|q|r|crop|width -> layer
const measured = new Map();     // path|q|r -> output/source frame ratio
const BASE_LIMIT = 10, REGION_LIMIT = 6;
const sched = { busy: false, slowTimer: 0 };

const settingsKey = (j, f, q, r) => `${j.path}|${f}|${q}|${r}`;
const currentKey = () => settingsKey(viewer.job, viewer.frame, viewer.quality, viewer.resolution);
const quadW = () => Math.max(1, Math.floor(viewer.job.width / 2));
const quadH = () => Math.max(1, Math.floor(viewer.job.height / 2));
const dispW = () => quadW() * squeeze();
const dpr = () => window.devicePixelRatio || 1;

function lruSet(map, key, value, limit) {
  map.delete(key);
  map.set(key, value);
  while (map.size > limit) {
    const k = map.keys().next().value;
    const old = map.get(k);
    map.delete(k);
    if (old !== viewer.base && old !== viewer.region) { old.before.close?.(); old.after.close?.(); }
  }
}
function lruGet(map, key) {
  const v = map.get(key);
  if (v) { map.delete(key); map.set(key, v); }
  return v;
}

function openViewer(j) {
  viewer.open = true;
  viewer.job = j;
  viewer.frame = Math.floor((j.frames - 1) / 2);
  viewer.quality = qualityOf(j);
  viewer.resolution = resolutionOf(j);
  viewer.split = 0.5;
  viewer.fit = true;
  viewer.cx = viewer.cy = 0.5;
  viewer.base = viewer.region = null;
  const range = $("v-frame");
  range.max = String(Math.max(0, j.frames - 1));
  range.value = String(viewer.frame);
  $("v-name").textContent = j.name;
  $("v-error").classList.remove("show");
  const hint = $("v-hint");
  hint.classList.remove("hide");
  clearTimeout(hint.timer);
  hint.timer = setTimeout(() => hint.classList.add("hide"), 4500);
  $("viewer").classList.add("show");
  for (const x of jobs) updateCard(x);
  sizeCanvas();
  viewer.scale = fitScale();
  syncLayers();
  renderViewerControls();
  draw();
  pump();
}

function closeViewer() {
  viewer.open = false;
  viewer.job = null;
  setLoading(false);
  $("viewer").classList.remove("show");
  for (const x of jobs) updateCard(x);
  renderFooter();
}

function renderViewerControls() {
  const j = viewer.job;
  if (!j) return;
  setSeg("v-quality", viewer.quality, null);
  setSeg("v-res", viewer.resolution, null);
  for (const b of $("v-res").querySelectorAll("button")) {
    b.disabled = (b.dataset.value === "6k" && j.width <= 6060) || (b.dataset.value === "4k" && j.width <= 3852);
    b.title = b.disabled ? "This clip is already at or below this size" : "";
  }
  renderZoomControls();
  renderSqueezeControls();
  renderFrameLabel();
  const shown = viewer.region || viewer.base;
  const q = shown ? shown.quality : viewer.quality, r = shown ? shown.resolution : viewer.resolution;
  $("v-tag-after").innerHTML = `<b>Q${q}</b> ${esc(QUALITY[q].title)}${r !== "original" ? " · " + esc(RES_LABEL[r]) : ""}`;
  const same = qualityOf(j) === viewer.quality && resolutionOf(j) === viewer.resolution;
  const apply = $("v-apply");
  apply.textContent = settings.sameForAll ? "Use for all clips" : "Apply to this clip";
  apply.disabled = same || running || j.state !== "waiting";
  apply.title = same ? "These are the settings it will be compressed with" : "";
  renderEstimate();
}

function renderFrameLabel(frame = viewer.frame) {
  const j = viewer.job;
  $("v-frame").style.setProperty("--p", `${(frame / Math.max(1, j.frames - 1)) * 100}%`);
  $("v-frame-label").textContent = `Frame ${frame + 1} of ${j.frames}`;
  $("v-tc").textContent = timecode(frame, j.fps);
}

function renderSqueezeControls() {
  const sel = $("v-squeeze"), custom = $("v-squeeze-custom");
  const preset = SQUEEZE_PRESETS.includes(squeeze());
  if (!custom.matches(":focus")) custom.value = String(squeeze());
  sel.value = preset && !sel.customOpen ? String(squeeze()) : "custom";
  custom.hidden = sel.value !== "custom";
  $("v-squeeze-group").classList.toggle("on", squeeze() !== 1);
  const j = viewer.job;
  $("v-meta").textContent = `${j.codec} · ${j.width} × ${j.height} · ${j.frames} frames · ${j.fps.toFixed(j.fps % 1 ? 3 : 0)} fps · ${gb(j.bytes)}`
    + (squeeze() !== 1 ? ` · de-squeezed ${squeezeLabel(squeeze())}, ${((j.width * squeeze()) / j.height).toFixed(2)}:1 on screen` : "");
}

// A new de-squeeze changes the frame's shape on screen: refit (or keep the zoom and centre
// when zoomed in), redraw, and stretch the queue thumbnails too.
function setSqueeze(v) {
  const next = clampSqueeze(v);
  if (next === settings.squeeze) { renderViewerControls(); return; }
  settings.squeeze = next;
  saveSettings();
  if (viewer.open) {
    if (viewer.fit) viewer.scale = fitScale();
    else viewer.scale = Math.max(viewer.scale, fitScale());
    clampView();
    $("v-mini-canvas").drawn = null;
    syncLayers();
    renderViewerControls();
    draw();
    pump();
  }
  for (const x of jobs) if (els.has(x.id)) drawThumb(x);
}

function renderZoomControls() {
  const pct = viewer.scale * 100;
  setSeg("v-zoom", viewer.fit ? "fit" : Math.abs(pct - 100) < 0.5 ? "1" : Math.abs(pct - 200) < 0.5 ? "2" : "", null);
  $("stage").classList.toggle("zoomed", !viewer.fit);
  $("v-zoom-badge").textContent = `${pct >= 99.5 ? Math.round(pct) : pct.toFixed(0)}%`;
}

function renderEstimate() {
  const j = viewer.job;
  const layer = baseCache.get(currentKey()) || (viewer.region && viewer.region.key === currentKey() ? viewer.region : null);
  const ratio = layer ? layer.ratio : measured.get(`${j.path}|${viewer.quality}|${viewer.resolution}`);
  if (ratio == null) {
    $("v-est-main").textContent = "Estimating…";
    $("v-est-sub").textContent = `Q${viewer.quality} ${QUALITY[viewer.quality].title}`;
    return;
  }
  $("v-est-main").innerHTML = `≈ ${gb(j.bytes * ratio)} <span class="good">−${((1 - ratio) * 100).toFixed(1)}%</span>`;
  $("v-est-sub").textContent = layer
    ? `This frame ${gb(layer.src)} → ${gb(layer.out)}, extrapolated to the clip`
    : "From another frame, extrapolated to the clip";
}

// Pick the layers to show for the current frame and settings from the caches. Layers of an
// earlier choice stay up until the new ones arrive, so the stage never goes blank.
function syncLayers() {
  const key = currentKey();
  const base = lruGet(baseCache, key);
  if (base) viewer.base = base;
  const r = lruGet(regionCache, regionKey());
  if (r) viewer.region = r;
  else if (viewer.region && viewer.region.key !== key && (base || viewer.fit)) viewer.region = null;
  if (viewer.fit && viewer.base && viewer.base.key === key) viewer.region = null;
}

// Geometry ---------------------------------------------------------------------

function sizeCanvas() {
  const c = $("v-canvas"), st = $("stage");
  c.width = Math.max(1, Math.round(st.clientWidth * dpr()));
  c.height = Math.max(1, Math.round(st.clientHeight * dpr()));
}

function fitScale() {
  const c = $("v-canvas");
  return Math.min(c.width / dispW(), c.height / quadH());
}

function clampView() {
  const c = $("v-canvas");
  const fw = dispW() * viewer.scale, fh = quadH() * viewer.scale;
  const hx = c.width / 2 / fw, hy = c.height / 2 / fh;
  viewer.cx = hx >= 0.5 ? 0.5 : Math.min(1 - hx, Math.max(hx, viewer.cx));
  viewer.cy = hy >= 0.5 ? 0.5 : Math.min(1 - hy, Math.max(hy, viewer.cy));
}

// The frame's rectangle on the canvas in device pixels.
function frameRect() {
  const c = $("v-canvas");
  const w = dispW() * viewer.scale, h = quadH() * viewer.scale;
  return { x: c.width / 2 - viewer.cx * w, y: c.height / 2 - viewer.cy * h, w, h };
}

// The visible part of the frame, in fractions, snapped so tiny pans reuse a render.
function visibleCrop() {
  const c = $("v-canvas"), f = frameRect();
  const snap = (v) => Math.round(v * 4096) / 4096;
  const x0 = snap(Math.max(0, -f.x / f.w)), y0 = snap(Math.max(0, -f.y / f.h));
  const x1 = snap(Math.min(1, (c.width - f.x) / f.w)), y1 = snap(Math.min(1, (c.height - f.y) / f.h));
  return [x0, y0, Math.max(1 / 4096, x1 - x0), Math.max(1 / 4096, y1 - y0)];
}

function regionKey() {
  if (!viewer.job || viewer.fit) return "";
  const crop = visibleCrop();
  const width = Math.round(crop[2] * quadW() * viewer.scale);
  return `${currentKey()}|${crop.join(",")}|${width}`;
}

function zoomTo(scale, anchor) {
  const c = $("v-canvas");
  const fs = fitScale();
  scale = Math.min(8, Math.max(fs, scale));
  const ax = anchor ? anchor.x * dpr() : c.width / 2, ay = anchor ? anchor.y * dpr() : c.height / 2;
  const f = frameRect();
  const u = (ax - f.x) / f.w, v = (ay - f.y) / f.h;
  viewer.scale = scale;
  viewer.fit = scale <= fs * 1.001;
  if (viewer.fit) {
    viewer.scale = fs;
    viewer.cx = viewer.cy = 0.5;
  } else {
    viewer.cx = (c.width / 2 - (ax - u * dispW() * scale)) / (dispW() * scale);
    viewer.cy = (c.height / 2 - (ay - v * quadH() * scale)) / (quadH() * scale);
    clampView();
  }
  $("v-hint").classList.add("hide");
  syncLayers();
  renderZoomControls();
  draw();
  pump();
}

// Drawing ----------------------------------------------------------------------

let drawQueued = false;
function draw() {
  if (drawQueued) return;
  drawQueued = true;
  requestAnimationFrame(() => {
    drawQueued = false;
    if (!viewer.open) return;
    const c = $("v-canvas");
    const ctx = c.getContext("2d");
    ctx.clearRect(0, 0, c.width, c.height);
    const splitX = Math.round(viewer.split * c.width);
    $("v-split").style.left = `${viewer.split * 100}%`;
    const f = frameRect();
    const layers = [];
    if (viewer.base) layers.push({ l: viewer.base, x: f.x, y: f.y, w: f.w, h: f.h });
    if (viewer.region && !viewer.fit) {
      const [x, y, w, h] = viewer.region.crop;
      layers.push({ l: viewer.region, x: f.x + x * f.w, y: f.y + y * f.h, w: w * f.w, h: h * f.h });
    }
    for (const { l, x, y, w, h } of layers) {
      // Magnified renders show quads as crisp blocks; the base is only a soft stand-in. A
      // de-squeeze stretch is smoothed until quads are 2 px wide, so odd ratios like 1.33x
      // do not show uneven columns.
      ctx.imageSmoothingEnabled = l === viewer.base || h < l.h * 0.999 || (h <= l.h * 1.001 && viewer.scale * squeeze() < 2);
      ctx.imageSmoothingQuality = "high";
      ctx.save();
      ctx.beginPath();
      ctx.rect(0, 0, splitX, c.height);
      ctx.clip();
      ctx.drawImage(l.before, x, y, w, h);
      ctx.restore();
      ctx.save();
      ctx.beginPath();
      ctx.rect(splitX, 0, c.width - splitX, c.height);
      ctx.clip();
      ctx.drawImage(l.after, x, y, w, h);
      ctx.restore();
    }
    drawMinimap();
  });
}

function drawMinimap() {
  const mini = $("v-mini-canvas");
  if (viewer.fit || !viewer.base) return;
  const w = 168 * dpr(), h = Math.round(w * quadH() / dispW());
  if (mini.width !== w || mini.height !== h) { mini.width = w; mini.height = h; mini.drawn = null; }
  const ctx = mini.getContext("2d");
  if (mini.drawn !== viewer.base) {
    ctx.imageSmoothingQuality = "high";
    ctx.drawImage(viewer.base.after, 0, 0, w, h);
    mini.drawn = viewer.base;
  }
  const [x, y, cw, ch] = visibleCrop();
  const view = $("v-mini-view").style;
  view.left = `${x * 100}%`; view.top = `${y * 100}%`;
  view.width = `${cw * 100}%`; view.height = `${ch * 100}%`;
}

// Requests ---------------------------------------------------------------------
//
// One request at a time. Each time the engine is free, the next one is chosen from the
// current state, so whatever the user did last wins and stale work is never queued.

function nextRequest() {
  const j = viewer.job;
  if (!viewer.open || !j) return null;
  const key = currentKey();
  const common = { job: j, frame: viewer.frame, quality: viewer.quality, resolution: viewer.resolution, key };
  const c = $("v-canvas");
  const baseReq = baseCache.has(key) ? null : { ...common, kind: "base", crop: [0, 0, 1, 1], width: Math.round(Math.min(quadW(), quadW() * fitScale(), c.width / squeeze())) };
  let regionReq = null;
  if (!viewer.fit) {
    const rk = regionKey();
    if (!regionCache.has(rk)) {
      const crop = visibleCrop();
      regionReq = { ...common, kind: "region", crop, width: Math.min(8192, Math.round(crop[2] * quadW() * viewer.scale)), rkey: rk };
    }
  }
  // Zoomed in, the region is what the user is looking at; the base follows from the same decode.
  return viewer.fit ? baseReq : regionReq || baseReq;
}

async function pump() {
  if (sched.busy) return;
  const req = nextRequest();
  if (!req) { setLoading(false); return; }
  sched.busy = true;
  // Only renders that need a decode take long enough to deserve the spinner.
  clearTimeout(sched.slowTimer);
  sched.slowTimer = setTimeout(() => setLoading(true, loadingText(req)), 160);
  try {
    const buf = await api.invoke("preview", {
      path: req.job.path, frame: req.frame, quality: req.quality, resolution: req.resolution,
      backend: settings.backend, width: Math.max(16, req.width), crop: req.crop,
    });
    const layer = await toLayer(req, buf);
    if (req.kind === "base") lruSet(baseCache, req.key, layer, BASE_LIMIT);
    else lruSet(regionCache, req.rkey, layer, REGION_LIMIT);
    measured.set(`${req.job.path}|${req.quality}|${req.resolution}`, layer.ratio);
    if (viewer.open && viewer.job === req.job) {
      if (req.kind === "region" && req.key === currentKey()) viewer.region = layer;
      if (req.kind === "base" && (req.key === currentKey() || !viewer.base)) viewer.base = layer;
      syncLayers();
      $("v-error").classList.remove("show");
      renderViewerControls();
      draw();
    }
    const j = jobs.find((x) => x.path === req.job.path);
    if (j) updateCard(j);
    renderFooter();
  } catch (e) {
    if (viewer.open && req.key === currentKey()) showError(String(e));
    sched.busy = false;
    clearTimeout(sched.slowTimer);
    setLoading(false);
    return;
  }
  sched.busy = false;
  clearTimeout(sched.slowTimer);
  setLoading(false);
  pump();
}

function loadingText(req) {
  const shown = viewer.region || viewer.base;
  if (!shown || shown.frame !== req.frame) return `Decoding frame ${req.frame + 1}`;
  if (shown.quality !== req.quality || shown.resolution !== req.resolution) return `Compressing at Q${req.quality}`;
  return "Rendering";
}

async function toLayer(req, buf) {
  const bytes = buf instanceof ArrayBuffer ? buf : new Uint8Array(buf).buffer;
  const dv = new DataView(bytes);
  const w = dv.getUint32(0, true), h = dv.getUint32(4, true);
  const src = Number(dv.getBigUint64(16, true)), out = Number(dv.getBigUint64(24, true));
  const n = w * h * 4;
  const [before, after] = await Promise.all([
    createImageBitmap(new ImageData(new Uint8ClampedArray(bytes, 32, n), w, h)),
    createImageBitmap(new ImageData(new Uint8ClampedArray(bytes, 32 + n, n), w, h)),
  ]);
  return {
    key: req.key, frame: req.frame, quality: req.quality, resolution: req.resolution, crop: req.crop,
    before, after, w, h, src, out, ratio: src > 0 ? out / src : 1,
  };
}

function setLoading(on, text) {
  const el = $("v-loading");
  el.classList.toggle("show", on);
  el.classList.toggle("first", on && !viewer.base && !viewer.region);
  if (text) $("v-loading-text").textContent = text;
}

function showError(msg) {
  setLoading(false);
  const el = $("v-error");
  el.textContent = `Could not render a preview: ${msg}`;
  el.classList.add("show");
}

// Input ------------------------------------------------------------------------

function bindStage() {
  const st = $("stage");
  let drag = null;
  const local = (e) => { const r = st.getBoundingClientRect(); return { x: e.clientX - r.left, y: e.clientY - r.top }; };
  const nearSplit = (p) => Math.abs(p.x - viewer.split * st.clientWidth) < 14;
  st.addEventListener("pointermove", (e) => {
    if (e.target.closest(".v-mini")) return;
    const p = local(e);
    if (!drag) { st.classList.toggle("near-split", nearSplit(p)); return; }
    const dx = p.x - drag.last.x, dy = p.y - drag.last.y;
    drag.last = p;
    if (Math.abs(p.x - drag.start.x) + Math.abs(p.y - drag.start.y) > 3) drag.moved = true;
    if (drag.mode === "split") {
      viewer.split = Math.min(1, Math.max(0, p.x / st.clientWidth));
      draw();
    } else if (!viewer.fit) {
      viewer.cx -= (dx * dpr()) / (dispW() * viewer.scale);
      viewer.cy -= (dy * dpr()) / (quadH() * viewer.scale);
      clampView();
      draw();
      pump();
    }
  });
  st.addEventListener("pointerdown", (e) => {
    if (e.button !== 0 || e.target.closest(".v-mini")) return;
    const p = local(e);
    drag = { start: p, last: p, mode: nearSplit(p) ? "split" : "pan", moved: false };
    st.setPointerCapture(e.pointerId);
    st.classList.toggle("dragging-split", drag.mode === "split");
    st.classList.toggle("panning", drag.mode === "pan" && !viewer.fit);
    $("v-hint").classList.add("hide");
  });
  const end = (e) => {
    if (!drag) return;
    const p = local(e);
    if (!drag.moved && drag.mode === "pan" && (viewer.base || viewer.region)) zoomTo(viewer.fit ? 1 : 0, p);
    drag = null;
    st.classList.remove("dragging-split", "panning");
    syncLayers();
    draw();
  };
  st.addEventListener("pointerup", end);
  st.addEventListener("pointercancel", end);
  st.addEventListener("wheel", (e) => {
    e.preventDefault();
    if (!viewer.base && !viewer.region) return;
    // Pinch on a trackpad arrives as ctrl+wheel. A mouse wheel moves in coarse vertical
    // notches and zooms too; trackpad scrolling pans.
    const notch = e.deltaMode !== 0 || (e.deltaX === 0 && Math.abs(e.deltaY) >= 40 && Number.isInteger(e.deltaY));
    if (e.ctrlKey || e.metaKey) {
      zoomTo(viewer.scale * Math.exp(-e.deltaY * 0.012), local(e));
    } else if (notch) {
      zoomTo(viewer.scale * (e.deltaY > 0 ? 1 / 1.25 : 1.25), local(e));
    } else if (!viewer.fit) {
      viewer.cx += (e.deltaX * dpr()) / (dispW() * viewer.scale);
      viewer.cy += (e.deltaY * dpr()) / (quadH() * viewer.scale);
      clampView();
      syncLayers();
      draw();
      pump();
    }
  }, { passive: false });

  // Minimap: drag the view rectangle (or click anywhere) to move around the frame.
  const mini = $("v-mini");
  let miniDrag = false;
  const moveTo = (e) => {
    const r = mini.getBoundingClientRect();
    viewer.cx = Math.min(1, Math.max(0, (e.clientX - r.left) / r.width));
    viewer.cy = Math.min(1, Math.max(0, (e.clientY - r.top) / r.height));
    clampView();
    syncLayers();
    draw();
    pump();
  };
  mini.addEventListener("pointerdown", (e) => { e.stopPropagation(); miniDrag = true; mini.setPointerCapture(e.pointerId); moveTo(e); });
  mini.addEventListener("pointermove", (e) => { if (miniDrag) moveTo(e); });
  mini.addEventListener("pointerup", () => { miniDrag = false; });

  new ResizeObserver(() => {
    if (!viewer.open) return;
    const wasFit = viewer.fit;
    sizeCanvas();
    if (wasFit) viewer.scale = fitScale();
    else viewer.scale = Math.max(viewer.scale, fitScale());
    clampView();
    syncLayers();
    draw();
    pump();
  }).observe(st);
}

function bindViewer() {
  $("v-close").addEventListener("click", closeViewer);
  const choose = (q, r) => {
    viewer.quality = q;
    viewer.resolution = r;
    $("v-hint").classList.add("hide");
    syncLayers();
    renderViewerControls();
    draw();
    pump();
  };
  $("v-quality").addEventListener("click", (e) => {
    const b = e.target.closest("button"); if (b) choose(Number(b.dataset.value), viewer.resolution);
  });
  $("v-res").addEventListener("click", (e) => {
    const b = e.target.closest("button"); if (b && !b.disabled) choose(viewer.quality, b.dataset.value);
  });
  $("v-zoom").addEventListener("click", (e) => {
    const b = e.target.closest("button"); if (!b) return;
    zoomTo(b.dataset.value === "fit" ? 0 : Number(b.dataset.value), null);
  });
  const sqSel = $("v-squeeze"), sqCustom = $("v-squeeze-custom");
  sqSel.addEventListener("change", () => {
    sqSel.customOpen = sqSel.value === "custom";
    if (sqSel.customOpen) {
      renderSqueezeControls();
      sqCustom.focus();
      sqCustom.select();
    } else {
      setSqueeze(Number(sqSel.value));
    }
  });
  const applyCustom = () => { if (sqCustom.value !== "") setSqueeze(sqCustom.value); };
  sqCustom.addEventListener("input", () => {
    const n = Number(sqCustom.value);
    if (Number.isFinite(n) && n >= SQUEEZE_MIN && n <= SQUEEZE_MAX) setSqueeze(n);
  });
  sqCustom.addEventListener("change", applyCustom);
  sqCustom.addEventListener("blur", () => { applyCustom(); renderSqueezeControls(); });
  sqCustom.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (e.key === "Enter" || e.key === "Escape") sqCustom.blur();
  });

  // The scrubber updates its label at once and asks for the frame when it settles.
  const range = $("v-frame");
  let debounce = 0;
  const goFrame = (f, delay) => {
    const j = viewer.job; if (!j) return;
    f = Math.min(j.frames - 1, Math.max(0, f));
    range.value = String(f);
    renderFrameLabel(f);
    clearTimeout(debounce);
    debounce = setTimeout(() => {
      viewer.frame = f;
      syncLayers();
      renderViewerControls();
      draw();
      pump();
    }, delay);
  };
  range.addEventListener("input", () => goFrame(Number(range.value), 150));
  $("v-prev").addEventListener("click", () => goFrame(Number(range.value) - 1, 60));
  $("v-next").addEventListener("click", () => goFrame(Number(range.value) + 1, 60));

  $("v-apply").addEventListener("click", async () => {
    const j = viewer.job;
    if (settings.sameForAll) {
      settings.quality = viewer.quality;
      settings.resolution = viewer.resolution;
      saveSettings();
      await planTargets();
    } else {
      j.quality = viewer.quality;
      j.resolution = viewer.resolution;
      j.target = await targetFor(j, j.resolution);
    }
    renderAll();
    renderViewerControls();
  });

  document.addEventListener("keydown", (e) => {
    if (!viewer.open || e.target.tagName === "SELECT" || e.target.tagName === "INPUT" || e.metaKey || e.ctrlKey) return;
    const k = e.key.toLowerCase();
    if (e.key === "Escape") closeViewer();
    else if (e.key === "ArrowLeft") { e.preventDefault(); goFrame(Number(range.value) - (e.shiftKey ? 10 : 1), 90); }
    else if (e.key === "ArrowRight") { e.preventDefault(); goFrame(Number(range.value) + (e.shiftKey ? 10 : 1), 90); }
    else if (["1", "2", "3", "4"].includes(e.key)) choose(Number(e.key), viewer.resolution);
    else if (k === "f") zoomTo(0, null);
    else if (k === "z") zoomTo(viewer.fit ? 1 : 0, null);
    else if (e.key === "+" || e.key === "=") zoomTo(viewer.scale * 1.5, null);
    else if (e.key === "-") zoomTo(viewer.scale / 1.5, null);
  });
  bindStage();
}

// Drag and drop ----------------------------------------------------------------

function onDragDrop(p) {
  const over = (p.type === "enter" || p.type === "over") && !viewer.open;
  const empty = jobs.length === 0;
  $("drop-zone").classList.toggle("over", over && empty && canAdd());
  $("drop-overlay").classList.toggle("show", over && !empty && canAdd());
  if (p.type === "drop" && !viewer.open) addPaths(p.paths);
}

// Wiring -----------------------------------------------------------------------

async function chooseOutput() {
  if (running) return;
  const f = await api.invoke("pick_folder", { title: "Save Compressed Clips To" });
  if (f) { settings.outputFolder = f; settings.dest = "custom"; }
  saveSettings(); renderSettings();
}
async function pickClips() { if (canAdd()) addPaths(await api.invoke("pick_clips")); }
async function pickFolder() {
  if (!canAdd()) return;
  const f = await api.invoke("pick_folder", { title: "Add Folder" });
  if (f) addPaths([f]);
}

async function init() {
  info = await api.invoke("app_info");
  info.gpuAvailable = !!info.gpu && !/^(CPU|not available)/i.test(info.gpu);
  document.body.classList.add(info.platform);
  if (info.web) {
    document.body.classList.add("web");
    if ($("drop-title")) $("drop-title").textContent = "Choose clips or folders on this server";
    let hasSaved = false;
    try { hasSaved = !!localStorage.getItem("settings"); } catch { /* ignore */ }
    if (!hasSaved) {
      if (QUALITY[info.defaultQuality]) settings.quality = info.defaultQuality;
      if (RES_LABEL[info.defaultResolution]) settings.resolution = info.defaultResolution;
    }
    if ($("watch-clear")) {
      $("watch-clear").addEventListener("change", (e) => {
        api.invoke("set_watch_settings", { clearAfterProcess: e.target.checked }).then(renderWatch).catch(() => {});
      });
    }
    if ($("watch-quality")) {
      $("watch-quality").addEventListener("click", (e) => {
        const b = e.target.closest("button");
        if (!b) return;
        api.invoke("set_watch_settings", { quality: Number(b.dataset.value) }).then(renderWatch).catch(() => {});
      });
    }
    if ($("watch-resolution")) {
      $("watch-resolution").addEventListener("click", (e) => {
        const b = e.target.closest("button");
        if (!b) return;
        api.invoke("set_watch_settings", { resolution: b.dataset.value }).then(renderWatch).catch(() => {});
      });
    }
    if (info.watch) renderWatch(info.watch);
  }
  bindSeg("seg-resolution", "resolution");
  bindSeg("seg-quality", "quality", Number);
  bindSeg("seg-backend", "backend");
  $("same-all").addEventListener("change", async (e) => {
    settings.sameForAll = e.target.checked;
    // Each clip starts from the settings it was getting until now.
    if (!settings.sameForAll) for (const j of jobs) { j.quality = settings.quality; j.resolution = settings.resolution; }
    saveSettings();
    await planTargets();
    renderAll();
    if (viewer.open) renderViewerControls();
  });
  $("seg-dest").addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (!b || running) return;
    if (b.dataset.value === "next") { settings.dest = "next"; saveSettings(); renderSettings(); }
    else if (settings.dest === "custom" && settings.outputFolder) return;
    else if (settings.outputFolder) { settings.dest = "custom"; saveSettings(); renderSettings(); }
    else chooseOutput();
  });
  $("btn-add").addEventListener("click", pickClips);
  $("btn-add-folder").addEventListener("click", pickFolder);
  $("empty-add").addEventListener("click", pickClips);
  $("empty-add-folder").addEventListener("click", pickFolder);
  $("btn-clear").addEventListener("click", clearList);
  $("btn-pause").addEventListener("click", togglePause);
  $("btn-go").addEventListener("click", () => (running ? stop() : start()));
  $("quit-cancel").addEventListener("click", () => $("quit-modal").classList.remove("show"));
  $("beta-ok").addEventListener("click", () => {
    $("beta-modal").classList.remove("show");
    try { localStorage.setItem(BETA_ACK, "1"); } catch { /* private mode */ }
  });
  let acked = false;
  try { acked = localStorage.getItem(BETA_ACK) === "1"; } catch { /* private mode */ }
  if (!acked) $("beta-modal").classList.add("show");
  $("quit-confirm").addEventListener("click", () => api.invoke("confirm_quit"));
  document.addEventListener("keydown", (e) => {
    const mod = e.metaKey || e.ctrlKey;
    if (mod && e.key.toLowerCase() === "o") { e.preventDefault(); e.shiftKey ? pickFolder() : pickClips(); }
    if (e.key === "Escape") $("quit-modal").classList.remove("show");
  });
  document.addEventListener("contextmenu", (e) => e.preventDefault());
  bindViewer();
  onEvents();
  api.onDragDrop(onDragDrop);
  running = await api.invoke("is_running");
  await restoreQueue();
  renderAll();
  addPaths(await api.invoke("initial_paths"));
}

init();

// Browser stand-in (no Tauri): fake clips, progress and previews, for layout work only.
function browserStandIn() {
  const handlers = {};
  const emit = (ev, p) => (handlers[ev] || []).forEach((f) => f(p));
  let cancel = false;
  let paused = false;
  let last = "";
  const sample = [
    ["/Volumes/CARD/DCIM/CLIP_0001.NEV", "N-RAW", 8268, 4652, 360, 2.60e9],
    ["/Volumes/CARD/DCIM/CLIP_0002.NEV", "N-RAW", 8268, 4652, 1210, 8.74e9],
    ["/Volumes/CARD/A001/A001_C001_0101AB.R3D", "R3D NE", 6060, 3410, 276, 2.14e9],
  ];
  const hash = (x, y, f) => {
    let h = (x * 374761393 + y * 668265263 + f * 2147483647) | 0;
    h = Math.imul(h ^ (h >>> 13), 1274126177);
    return ((h ^ (h >>> 16)) >>> 0) / 4294967296 - 0.5;
  };
  const fakePreview = (a) => {
    const clip = sample.find((s) => s[0] === a.path) || sample[0];
    const qw = clip[2] / 2, qh = clip[3] / 2;
    const [cx, cy, cw, ch] = a.crop;
    const w = a.width, h = Math.max(1, Math.round(w * (ch * qh) / (cw * qw))), n = w * h * 4;
    const buf = new ArrayBuffer(32 + 2 * n), dv = new DataView(buf);
    dv.setUint32(0, w, true); dv.setUint32(4, h, true); dv.setUint32(8, clip[4], true);
    const keep = { 1: 0.91, 2: 0.82, 3: 0.63, 4: 0.43 }[a.quality] * (a.resolution === "4k" ? 0.22 : a.resolution === "6k" ? 0.54 : 1);
    dv.setBigUint64(16, 7200000n, true); dv.setBigUint64(24, BigInt(Math.round(7200000 * keep)), true);
    const px = new Uint8Array(buf, 32);
    const s = { 1: 0, 2: 0.3, 3: 0.55, 4: 0.85 }[a.quality];
    for (let y = 0; y < h; y++) {
      const qy = Math.floor((cy + ch * (y + 0.5) / h) * qh), fy = qy / qh;
      for (let x = 0; x < w; x++) {
        const qx = Math.floor((cx + cw * (x + 0.5) / w) * qw), fx = qx / qw;
        const i = (y * w + x) * 4;
        const hill = 0.55 + 0.08 * Math.sin(fx * 7 + a.frame / 60);
        const leaf = 0.5 + 0.5 * Math.sin(fx * 180) * Math.cos(fy * 140);
        const base = fy < hill ? [90 + 60 * (1 - fy), 130 + 50 * (1 - fy), 200 + 40 * (1 - fy)] : [30 * leaf + 10, 90 * leaf + 20, 30 * leaf + 10];
        const noise = hash(qx, qy, a.frame) * 30;
        const smooth = (hash(qx + 1, qy, a.frame) + hash(qx, qy + 1, a.frame) + hash(qx - 1, qy, a.frame)) * 10;
        for (let c = 0; c < 3; c++) {
          px[i + c] = base[c] + noise;
          px[n + i + c] = base[c] + noise * (1 - s) + smooth * s;
        }
        px[i + 3] = px[n + i + 3] = 255;
      }
    }
    return buf;
  };
  return {
    listen: (ev, fn) => { (handlers[ev] ||= []).push(fn); },
    onDragDrop: () => {},
    invoke: async (cmd, a) => {
      switch (cmd) {
        case "app_info": return { version: "0.1.6", gpu: "Metal: Apple GPU", cpu: "CPU (12 threads)", platform: "macos" };
        case "is_running": return false;
        case "initial_paths": return location.hash === "#demo" ? ["demo"] : [];
        case "scan": return sample.map(([path, codec, width, height, frames, bytes]) => ({
          path, name: path.split("/").pop(), ok: true, codec, width, height, frames, fps: 25, bytes, error: null,
        }));
        case "plan_output": return a.resolution === "6k" && a.width > 6060 ? [6060, 3410] : a.resolution === "4k" ? [3852, 2168] : [a.width, a.height];
        case "preview": {
          const key = `${a.path}|${a.frame}|${a.quality}|${a.resolution}`;
          await new Promise((r) => setTimeout(r, key === last ? 20 : 450));
          last = key;
          return fakePreview(a);
        }
        case "thumbnail": {
          const p = new Uint8Array(fakePreview({ ...a, frame: 90, quality: 1, resolution: "original", crop: [0, 0, 1, 1] }));
          const dv = new DataView(p.buffer);
          const w = dv.getUint32(0, true), h = dv.getUint32(4, true);
          const out = new Uint8Array(8 + w * h * 4);
          out.set(p.subarray(0, 8)); out.set(p.subarray(32, 32 + w * h * 4), 8);
          return out.buffer;
        }
        case "pick_clips": return ["demo"];
        case "pick_folder": return "/Volumes/Archive/Compressed";
        case "cancel": cancel = true; paused = false; return;
        case "set_paused": paused = !!a.paused; emit("queue-paused", { paused }); return { paused };
        case "start": {
          cancel = false;
          paused = false;
          (async () => {
            for (const j of a.jobs) {
              if (cancel) break;
              emit("job-start", { id: j.id, output: j.path });
              const total = 60;
              let i = 0;
              for (; i <= total && !cancel; i++) {
                await new Promise((r) => setTimeout(r, 40));
                while (paused && !cancel) await new Promise((r) => setTimeout(r, 100));
                emit("job-progress", { id: j.id, fraction: i / total, fps: 38 + Math.random() * 4 });
              }
              if (cancel) { emit("job-error", { id: j.id, error: "cancelled", cancelled: true }); break; }
              emit("job-done", { id: j.id, skipped: false, output: j.path, inputBytes: 2.6e9, outputBytes: 2.12e9, frames: 360, seconds: 9.1, fps: 39.6, backend: "CPU" });
            }
            emit("queue-finished", null);
          })();
          return;
        }
        default: return null;
      }
    },
  };
}

// Web UI backend (`bluecompress serve`): HTTP command calls, SSE events, and NAS file browser.
function webApi() {
  const handlers = {};
  const emit = (ev, p) => (handlers[ev] || []).forEach((f) => f(p));

  const connectEvents = () => {
    const es = new EventSource("/api/events");
    let opened = false;
    // After a dropped connection (server restart, network), events were missed: resync.
    es.onopen = () => {
      if (opened) emit("events-reconnected", null);
      opened = true;
    };
    es.onmessage = (e) => {
      try {
        const msg = JSON.parse(e.data);
        if (msg && msg.event) emit(msg.event, msg.payload);
      } catch { /* ignore */ }
    };
  };
  connectEvents();

  let lastDir = "";
  try { lastDir = localStorage.getItem("webLastDir") || ""; } catch { /* ignore */ }
  const saveLastDir = (d) => {
    lastDir = d;
    try { localStorage.setItem("webLastDir", d); } catch { /* ignore */ }
  };

  let fsResolve = null;
  let fsState = { mode: "clips", path: "", parent: null, entries: [], selected: new Set() };
  let fsBound = false;

  async function fetchDir(path) {
    const q = path ? `?path=${encodeURIComponent(path)}` : "";
    const r = await fetch(`/api/fs/list${q}`);
    if (!r.ok) throw new Error(await r.text());
    return r.json();
  }

  function closeFs(val) {
    $("fs-modal").classList.remove("show");
    $("fs-mkdir-row").hidden = true;
    $("fs-error").hidden = true;
    const fn = fsResolve;
    fsResolve = null;
    if (fn) fn(val);
  }

  function showFsError(msg) {
    const el = $("fs-error");
    if (!msg) { el.hidden = true; el.textContent = ""; return; }
    el.textContent = msg;
    el.hidden = false;
  }

  function renderFs() {
    $("fs-path-input").value = fsState.path;
    $("fs-up").disabled = !fsState.parent;

    const rootsEl = $("fs-roots");
    rootsEl.innerHTML = "";
    for (const r of fsState.roots || []) {
      const b = document.createElement("button");
      b.className = `fs-root-btn${fsState.path === r ? " active" : ""}`;
      b.textContent = r;
      b.addEventListener("click", () => loadDir(r));
      rootsEl.append(b);
    }

    const clipsInDir = fsState.entries.filter((e) => e.kind === "clip");
    const selAllBtn = $("fs-select-all");
    selAllBtn.hidden = !(fsState.mode === "clips" && clipsInDir.length > 0);
    if (!selAllBtn.hidden) {
      const allSelected = clipsInDir.every((c) => fsState.selected.has(c.path));
      selAllBtn.textContent = allSelected ? "Clear Selection" : `Select All Clips (${clipsInDir.length})`;
    }

    $("fs-mkdir-toggle").hidden = fsState.mode !== "output";
    $("fs-add-current-dir").hidden = fsState.mode !== "clips";

    const listEl = $("fs-list");
    listEl.innerHTML = "";
    if (!fsState.entries.length) {
      const empty = document.createElement("div");
      empty.className = "fs-empty";
      empty.textContent = "No subfolders or .NEV / .R3D clips in this folder";
      listEl.append(empty);
    } else {
      for (const item of fsState.entries) {
        const row = document.createElement("div");
        const isSel = fsState.selected.has(item.path);
        row.className = `fs-row ${item.kind}${isSel ? " selected" : ""}`;
        if (item.kind === "dir") {
          row.innerHTML = `<svg viewBox="0 0 16 16" class="fs-row-ico"><path d="M1.8 4.2c0-.7.5-1.2 1.2-1.2h3l1.4 1.6h5.6c.7 0 1.2.5 1.2 1.2v6.2c0 .7-.5 1.2-1.2 1.2H3c-.7 0-1.2-.5-1.2-1.2V4.2Z"/></svg>` +
            `<span class="fs-row-name">${esc(item.name)}</span>`;
          if (fsState.mode === "clips" || fsState.mode === "input-folder") {
            const addBtn = document.createElement("button");
            addBtn.className = "link-btn";
            addBtn.textContent = "Add Folder";
            addBtn.addEventListener("click", (e) => {
              e.stopPropagation();
              closeFs(fsState.mode === "clips" ? [item.path] : item.path);
            });
            row.append(addBtn);
          }
          row.addEventListener("click", () => loadDir(item.path));
        } else {
          row.innerHTML = `<svg viewBox="0 0 16 16" class="fs-row-ico"><path d="M3 2.5h7l3 3v8H3z"/></svg>` +
            `<span class="fs-row-name">${esc(item.name)}</span>` +
            (item.codec ? `<span class="fs-row-badge">${esc(item.codec)}</span>` : "") +
            `<span class="fs-row-meta">${gb(item.size)}</span>`;
          if (fsState.mode === "clips") {
            row.addEventListener("click", () => {
              if (fsState.selected.has(item.path)) fsState.selected.delete(item.path);
              else fsState.selected.add(item.path);
              renderFs();
            });
            row.addEventListener("dblclick", () => closeFs([item.path]));
          } else {
            row.style.opacity = "0.65";
          }
        }
        listEl.append(row);
      }
    }

    const n = fsState.selected.size;
    if (fsState.mode === "clips") {
      $("fs-summary").textContent = n ? `${n} clip${n === 1 ? "" : "s"} selected` : `Folder: ${fsState.path}`;
      $("fs-confirm").textContent = n ? `Add ${n} Clip${n === 1 ? "" : "s"}` : "Add Folder";
      $("fs-confirm").disabled = !fsState.path;
    } else if (fsState.mode === "input-folder") {
      $("fs-summary").textContent = fsState.path;
      $("fs-confirm").textContent = "Add This Folder";
      $("fs-confirm").disabled = !fsState.path;
    } else if (fsState.mode === "output") {
      $("fs-summary").textContent = fsState.path;
      $("fs-confirm").textContent = "Select Output Folder";
      $("fs-confirm").disabled = !fsState.path;
    } else {
      $("fs-summary").textContent = fsState.path;
      $("fs-confirm").textContent = "Done";
      $("fs-confirm").disabled = false;
    }
  }

  async function loadDir(targetPath, highlightFile) {
    showFsError("");
    try {
      const data = await fetchDir(targetPath);
      fsState.path = data.path;
      fsState.parent = data.parent;
      fsState.roots = data.roots || [];
      fsState.entries = data.entries || [];
      fsState.selected.clear();
      if (highlightFile) fsState.selected.add(highlightFile);
      saveLastDir(data.path);
      renderFs();
    } catch (e) {
      showFsError(e.message || String(e));
    }
  }

  function bindFsOnce() {
    if (fsBound) return;
    fsBound = true;
    $("fs-close").addEventListener("click", () => closeFs(fsState.mode === "clips" ? [] : null));
    $("fs-cancel").addEventListener("click", () => closeFs(fsState.mode === "clips" ? [] : null));
    $("fs-up").addEventListener("click", () => { if (fsState.parent) loadDir(fsState.parent); });
    $("fs-go").addEventListener("click", () => loadDir($("fs-path-input").value.trim()));
    $("fs-path-input").addEventListener("keydown", (e) => {
      if (e.key === "Enter") { e.preventDefault(); loadDir($("fs-path-input").value.trim()); }
    });
    $("fs-select-all").addEventListener("click", () => {
      const clips = fsState.entries.filter((e) => e.kind === "clip");
      const allSelected = clips.every((c) => fsState.selected.has(c.path));
      if (allSelected) fsState.selected.clear();
      else for (const c of clips) fsState.selected.add(c.path);
      renderFs();
    });
    $("fs-mkdir-toggle").addEventListener("click", () => {
      const row = $("fs-mkdir-row");
      row.hidden = !row.hidden;
      if (!row.hidden) { $("fs-mkdir-input").value = ""; $("fs-mkdir-input").focus(); }
    });
    $("fs-mkdir-cancel").addEventListener("click", () => { $("fs-mkdir-row").hidden = true; });
    const doMkdir = async () => {
      const name = $("fs-mkdir-input").value.trim();
      if (!name) return;
      try {
        const r = await fetch("/api/fs/mkdir", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ parent: fsState.path, name }),
        });
        if (!r.ok) throw new Error(await r.text());
        const res = await r.json();
        $("fs-mkdir-row").hidden = true;
        await loadDir(res.path);
      } catch (e) {
        showFsError(e.message || String(e));
      }
    };
    $("fs-mkdir-create").addEventListener("click", doMkdir);
    $("fs-mkdir-input").addEventListener("keydown", (e) => {
      if (e.key === "Enter") { e.preventDefault(); doMkdir(); }
    });
    $("fs-add-current-dir").addEventListener("click", () => {
      if (fsState.path) closeFs([fsState.path]);
    });
    $("fs-confirm").addEventListener("click", () => {
      if (fsState.mode === "clips") {
        closeFs(fsState.selected.size ? Array.from(fsState.selected) : fsState.path ? [fsState.path] : []);
      } else if (fsState.mode === "reveal") {
        closeFs(null);
      } else {
        closeFs(fsState.path || null);
      }
    });
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && $("fs-modal").classList.contains("show")) {
        closeFs(fsState.mode === "clips" ? [] : null);
      }
    });
  }

  function openFsModal(mode, title, sub, initialPath, highlightFile) {
    bindFsOnce();
    fsState.mode = mode;
    fsState.selected.clear();
    $("fs-title").textContent = title;
    $("fs-sub").textContent = sub;
    $("fs-mkdir-row").hidden = true;
    $("fs-error").hidden = true;
    $("fs-modal").classList.add("show");
    loadDir(initialPath || lastDir || "", highlightFile);
    return new Promise((resolve) => { fsResolve = resolve; });
  }

  return {
    listen: (ev, fn) => { (handlers[ev] ||= []).push(fn); },
    onDragDrop: () => {},
    invoke: async (cmd, a = {}) => {
      if (cmd === "pick_clips") {
        return openFsModal(
          "clips",
          "Add Clips",
          "Select one or more .NEV / .R3D clips, or add a whole folder",
          lastDir
        );
      }
      if (cmd === "pick_folder") {
        const isOutput = a && a.title && /save|output/i.test(a.title);
        return openFsModal(
          isOutput ? "output" : "input-folder",
          (a && a.title) || "Choose Folder",
          isOutput ? "Choose where compressed clips should be saved" : "Subfolders are scanned recursively for .NEV and .R3D clips",
          isOutput && settings.outputFolder ? settings.outputFolder : lastDir
        );
      }
      if (cmd === "reveal") {
        const p = (a && a.path) || "";
        const slash = Math.max(p.lastIndexOf("/"), p.lastIndexOf("\\"));
        const dir = slash > 0 ? p.slice(0, slash) : "/";
        return openFsModal("reveal", "Clip Location", p, dir, p);
      }
      const r = await fetch(`/api/invoke/${cmd}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(a || {}),
      });
      if (!r.ok) throw new Error(await r.text());
      if (cmd === "preview" || cmd === "thumbnail") return r.arrayBuffer();
      return r.json();
    },
  };
}
