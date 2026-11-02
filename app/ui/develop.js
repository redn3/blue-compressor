// Develop tab: quick, basic development of the RAW clips in the queue (white balance,
// exposure, tone) to a 10-bit HEVC MOV. Settings start from what each clip recorded.
"use strict";

// Marks a string for translation where tr() runs later, at render time.
const N_ = (s) => s;

const DEV_WB = [
  ["asShot", N_("As Shot"), 0], ["daylight", N_("Daylight"), 5200], ["cloudy", N_("Cloudy"), 6000], ["shade", N_("Shade"), 8000],
  ["tungsten", N_("Tungsten"), 3000], ["fluorescent", N_("Fluorescent"), 4000], ["flash", N_("Flash"), 5400], ["custom", N_("Custom"), 0],
];
const DEV_ROLL = [["none", N_("None")], ["hard", N_("Hard")], ["medium", N_("Medium")], ["soft", N_("Soft")], ["verySoft", N_("Very Soft")]];
const DEV_TONE = [["none", N_("None")], ["low", N_("Low")], ["medium", N_("Medium")], ["high", N_("High")]];
const DEV_SIZES = [[0, ""], [3840, "4K"], [1920, "1080p"]];
const DEV_QUALITY = [["standard", N_("Standard")], ["high", N_("High")]];
// Noise reduction sliders (all formats), 0 is off.
const NR_SLIDERS = [
  ["nrLuma", N_("Luma"), 0, 100, 1, false, "", 0],
  ["nrChroma", N_("Chroma"), 0, 100, 1, false, "", 0],
];

// The ISO values the R3D SDK accepts (ImageProcessingLimits::ISOList, including the six added
// in SDK 9.0). The ISO slider and box only land on these.
const ISO_STOPS = [50, 64, 80, 100, 125, 160, 200, 250, 320, 400, 500, 640, 800, 1000, 1280, 1600, 2000, 2500, 3200, 4000, 5000, 6400, 8000, 10000, 12800, 16000, 20000, 25600];
const snapIso = (v) => ISO_STOPS.reduce((b, x) => (Math.abs(Math.log(x / v)) < Math.abs(Math.log(b / v)) ? x : b), ISO_STOPS[0]);

// Each slider: key, label, min, max, step, log scale, unit, format digits.
const R3D_SLIDERS = [
  ["iso", N_("ISO"), 50, 25600, 1, true, "", 0],
  ["exposure", N_("Exposure Adjust"), -7, 7, 0.1, false, "", 1],
  ["kelvin", N_("Color Temp"), 1700, 10000, 10, true, "K", 0],
  ["tint", N_("Tint"), -100, 100, 1, false, "", 0],
  ["contrast", N_("Contrast"), -1, 1, 0.01, false, "", 2],
];
const NRAW_SLIDERS = [
  ["kelvin", N_("Temperature"), 2000, 50000, 10, true, "K", 0],
  ["tint", N_("Tint"), -150, 150, 1, false, "", 0],
  ["exposure", N_("Exposure"), -5, 5, 0.1, false, "", 1],
  ["highlights", N_("Highlights"), -100, 100, 1, false, "", 0],
  ["shadows", N_("Shadows"), -100, 100, 1, false, "", 0],
  ["saturation", N_("Saturation"), -100, 100, 1, false, "", 0],
  ["colorBoost", N_("Color Boost"), -100, 100, 1, false, "", 0],
  ["sharpness", N_("Sharpness"), 0, 100, 1, false, "", 0],
];
const SLIDER_TIPS = {
  iso: N_("Brightness as a different ISO. Starts at the ISO the clip was shot at."),
  exposure: N_("Brightens or darkens in stops."),
  kelvin: N_("White balance color temperature."),
  tint: N_("Green to magenta balance."),
  contrast: N_("Contrast around middle gray."),
  highlights: N_("Brightens or darkens the bright parts."),
  shadows: N_("Brightens or darkens the dark parts."),
  saturation: N_("Strength of all colors."),
  colorBoost: N_("Strengthens muted colors more than strong ones."),
  sharpness: N_("20 leaves the image as it is."),
  nrLuma: N_("Smooths grain in the brightness. 0 is off."),
  nrChroma: N_("Smooths color noise. 0 is off."),
};

const dev = {
  path: null,
  clips: new Map(), // path -> { info, s, wb }
  frame: 0,
  view709: true,
  seq: 0,
  timer: 0,
  exporting: false,
  status: "",
  export: { width: 3840, mbps: null, cbr: false, output: "native", high: false },
  playing: false,
  playRun: 0,
  luts: [], // imported LUTs: { name, title, threeD, size }
  // Zoom: fit, or k CSS pixels per source pixel around the centre (cx, cy) in fractions.
  view: { fit: true, k: 0, cx: 0.5, cy: 0.5 },
  shown: null, // the crop the canvas shows, fractions x, y, w, h
};

// Collapsed settings sections, remembered across restarts.
const DEV_SECTIONS_KEY = "devSections";
let devCollapsed = {};
try { devCollapsed = JSON.parse(localStorage.getItem(DEV_SECTIONS_KEY) || "{}"); } catch { /* private mode */ }
function devSaveSections() {
  try { localStorage.setItem(DEV_SECTIONS_KEY, JSON.stringify(devCollapsed)); } catch { /* private mode */ }
}

// A collapsible section: a header button and a body. Returns the body.
function devSection(parent, id, title) {
  const wrap = document.createElement("section");
  wrap.className = "dev-group" + (devCollapsed[id] ? " collapsed" : "");
  const head = document.createElement("button");
  head.type = "button";
  head.className = "dev-sec dev-sec-btn";
  head.setAttribute("aria-expanded", String(!devCollapsed[id]));
  head.innerHTML = `<svg class="dev-chev" viewBox="0 0 16 16" aria-hidden="true"><path d="M5 6l3 3 3-3"/></svg><span>${esc(tr(title))}</span>`;
  const body = document.createElement("div");
  body.className = "dev-group-body";
  head.addEventListener("click", () => {
    devCollapsed[id] = !devCollapsed[id];
    wrap.classList.toggle("collapsed", !!devCollapsed[id]);
    head.setAttribute("aria-expanded", String(!devCollapsed[id]));
    devSaveSections();
  });
  wrap.append(head, body);
  parent.append(wrap);
  return body;
}

const devDemo = !window.__TAURI__;
const devHandlers = {};
const devEmit = (ev, p) => (devHandlers[ev] || []).forEach((f) => f(p));
function devListen(ev, fn) {
  if (devDemo) (devHandlers[ev] ||= []).push(fn);
  else api.listen(ev, fn);
}

async function devInvoke(cmd, a = {}) {
  if (!devDemo) return api.invoke(cmd, a);
  return devStandIn(cmd, a);
}

// Offline stand-in for the demo page: synthetic pictures only.
async function devStandIn(cmd, a) {
  const r3d = /\.r3d$/i.test(a.path || "");
  switch (cmd) {
    case "develop_info": {
      const j = jobs.find((x) => x.path === a.path) || { width: 8268, height: 4652, frames: 360 };
      const k = r3d ? 5600 : 4760;
      return {
        r3d, camera: r3d ? "R3D NE" : "NIKON ZR", width: j.width, height: j.height, frames: j.frames || 300, fps: 25,
        iso: 800, kelvin: k, hasAudio: true, renderer: "Metal (Apple GPU)",
        sidecar: r3d ? { name: a.path.split("/").pop().replace(/\.[^.]+$/, ".RMD"), exposure: 1, iso: 800, kelvin: null, tint: null } : null,
        outputPath: a.path.replace(/[^/]+$/, "Developed/") + a.path.split("/").pop().replace(/\.[^.]+$/, r3d ? "_Log3G10.mov" : "_NLog.mov"),
        asShot: { output: r3d ? "log3g10" : "nlog", kelvin: k, tint: 0, exposure: r3d ? 1 : 0, iso: 800, rollOff: "medium", toneMap: "medium", contrast: 0, highlights: 0, shadows: 0, saturation: 0, colorBoost: 0, sharpness: 20, nrLuma: 0, nrChroma: 0 },
      };
    }
    case "develop_preview": {
      await new Promise((r) => setTimeout(r, 60));
      const { width: w, height: h, settings: s } = a;
      const buf = new ArrayBuffer(8 + w * h * 4), dv = new DataView(buf);
      dv.setUint32(0, w, true); dv.setUint32(4, h, true);
      const px = new Uint8Array(buf, 8);
      const info = dev.clips.get(a.path)?.info;
      const gain = Math.pow(2, s.exposure) * (r3d ? s.iso / 800 : 1);
      const warm = Math.log2(s.kelvin / (info ? info.kelvin : 5600)) * 0.6;
      const tint = s.tint / 300;
      const sat = 1 + s.saturation / 100;
      const log = !a.view709;
      for (let y = 0; y < h; y++) {
        const fy = y / h;
        for (let x = 0; x < w; x++) {
          const fx = x / w;
          const hill = 0.58 + 0.07 * Math.sin(fx * 7 + a.frame / 60);
          const leaf = 0.5 + 0.5 * Math.sin(fx * 90) * Math.cos(fy * 70);
          let c = fy < hill ? [0.35 + 0.3 * (1 - fy), 0.5 + 0.25 * (1 - fy), 0.85] : [0.08 + 0.1 * leaf, 0.2 + 0.25 * leaf, 0.06 + 0.06 * leaf];
          c = [c[0] * Math.pow(2, warm), c[1] * Math.pow(2, -tint), c[2] * Math.pow(2, -warm)].map((v) => v * gain * 0.55);
          const l = 0.2627 * c[0] + 0.678 * c[1] + 0.0593 * c[2];
          c = c.map((v) => l + (v - l) * sat);
          const i = (y * w + x) * 4;
          for (let k = 0; k < 3; k++) {
            const v = Math.max(0, c[k]);
            const o = log ? 0.25 + 0.2 * Math.log2(v / 0.18 + 0.01) / 4 : Math.pow(v / (1 + v * 0.35), 1 / 2.4);
            px[i + k] = Math.max(0, Math.min(255, o * 255));
          }
          px[i + 3] = 255;
        }
      }
      return buf;
    }
    case "develop_auto_exposure": return 0.6;
    case "develop_play_start": dev.demoPlay = { ...a, at: performance.now() }; return null;
    case "develop_play_settings": if (dev.demoPlay) Object.assign(dev.demoPlay, a); return null;
    case "develop_play_stop": dev.demoPlay = null; return null;
    case "develop_play_frame": {
      const p = dev.demoPlay;
      if (!p) return new ArrayBuffer(12);
      const i = p.frame + Math.floor(((performance.now() - p.at) / 1000) * 25);
      const n = dev.clips.get(p.path)?.info.frames || 300;
      if (i >= n) { const b = new ArrayBuffer(12); new DataView(b).setUint32(8, 0xffffffff, true); return b; }
      const img = new Uint8Array(await devStandIn("develop_preview", { ...p, frame: i }));
      const out = new Uint8Array(12 + img.length - 8);
      out.set(img.subarray(0, 8), 0); new DataView(out.buffer).setUint32(8, i, true); out.set(img.subarray(8), 12);
      return out.buffer;
    }
    case "develop_default_kbps": return Math.round((100000 * a.width * a.height) / (3840 * 2160) * Math.max(1, a.fps / 25));
    case "develop_export": {
      dev.demoCancel = false;
      (async () => {
        for (const j of a.jobs) {
          for (let f = 0; f <= 1.0001 && !dev.demoCancel; f += 0.05) {
            await new Promise((r) => setTimeout(r, 80));
            devEmit("develop-progress", { path: j.path, fraction: f, fps: 31.4 });
          }
          devEmit("develop-done", { path: j.path, ok: !dev.demoCancel, output: j.path + ".mov", encoder: "hevc_videotoolbox", renderer: "Metal (Apple GPU)", seconds: 9.6, bytes: 1.2e8, error: dev.demoCancel ? "cancelled" : null });
        }
        devEmit("develop-finished", null);
      })();
      return null;
    }
    case "develop_cancel": dev.demoCancel = true; return null;
    case "lut_list": return dev.luts;
    case "lut_import": return { added: [{ name: "Demo Look", title: "Demo Look", threeD: true, size: 33 }], errors: [["broken.cube", "not a valid .cube LUT: no LUT_1D_SIZE or LUT_3D_SIZE"]] };
    case "lut_remove": return null;
  }
  return null;
}

const devEl = (id) => document.getElementById(id);
const devClip = () => dev.clips.get(dev.path);

function devClipsInQueue() {
  return jobs.filter((j) => j.ok && (isNev(j) || isR3d(j) || (isPrr(j) && j.codec === "ProRes RAW")));
}

async function devEnsure(path) {
  if (dev.clips.has(path)) return dev.clips.get(path);
  const info = await devInvoke("develop_info", { path });
  const c = { info, s: { ...info.asShot }, wb: "asShot" };
  dev.clips.set(path, c);
  return c;
}

function devShowTab(name) {
  const develop = name === "develop";
  if (!develop) devStop();
  document.body.classList.toggle("tab-develop", develop);
  devEl("develop").hidden = !develop;
  for (const b of devEl("tabs").querySelectorAll("button")) b.setAttribute("aria-checked", String(b.dataset.value === name));
  for (const b of devEl("tabs").querySelectorAll("button")) b.classList.toggle("active", b.dataset.value === name);
  if (develop) { devQueueKey = devClipsInQueue().map((j) => j.path).join("|"); devRender(); }
}

function devRender() {
  const list = devClipsInQueue();
  if (dev.path && !list.some((j) => j.path === dev.path)) dev.path = null;
  if (!dev.path && list.length) dev.path = list[0].path;
  devEl("dev-empty").hidden = list.length > 0;
  devEl("dev-work").hidden = list.length === 0;
  const ul = devEl("dev-list");
  ul.innerHTML = "";
  for (const j of list) {
    const b = document.createElement("button");
    b.className = "dev-clip" + (j.path === dev.path ? " on" : "");
    const c = dev.clips.get(j.path);
    const edited = c && JSON.stringify(c.s) !== JSON.stringify(c.info.asShot);
    b.innerHTML = `<span class="dev-clip-name">${esc(j.name)}</span><span class="dev-clip-sub">${esc(isR3d(j) ? "R3D NE" : isPrr(j) ? "ProRes RAW" : "N-RAW")}${edited ? " · " + esc(tr("Edited")) : ""}</span>`;
    b.addEventListener("click", () => { if (j.path !== dev.path) { devStop(); dev.view = { fit: true, k: 0, cx: 0.5, cy: 0.5 }; dev.shown = null; } dev.path = j.path; dev.frame = 0; devRender(); });
    ul.append(b);
  }
  if (!dev.path) return;
  devEnsure(dev.path).then(() => { devRenderControls(); devRenderZoom(); devSchedule(0); }).catch((e) => devError(String(e)));
}

let devQueueKey = "";
function devQueueChanged() {
  const key = devClipsInQueue().map((j) => j.path).join("|");
  if (key === devQueueKey) return;
  devQueueKey = key;
  if (!devEl("develop").hidden) devRender();
}

function devError(msg) {
  const m = /LUT_MISSING:(.*)/.exec(msg || "");
  if (m) msg = tr("The LUT {0} is no longer installed.", m[1]);
  devEl("dev-error").textContent = msg;
  devEl("dev-error").hidden = !msg;
}

const sliderToPos = (v, mn, mx, lg) => (lg ? Math.log(v / mn) / Math.log(mx / mn) : (v - mn) / (mx - mn)) * 1000;
const posToSlider = (p, mn, mx, lg, step) => {
  const f = p / 1000;
  const v = lg ? mn * Math.pow(mx / mn, f) : mn + (mx - mn) * f;
  return Math.round(v / step) * step;
};

// The tooltip of a control, with a note when its as shot value came from the RMD sidecar.
function devTip(c, key) {
  const sc = c.info.sidecar;
  const tip = SLIDER_TIPS[key] ? tr(SLIDER_TIPS[key]) : "";
  const from = { exposure: "exposure", iso: "iso", kelvin: "kelvin", tint: "tint" }[key];
  if (!sc || !from || sc[from] == null || !c.info.r3d) return tip;
  return [tip, tr("Starts at {0} from the sidecar {1}.", num(sc[from], 0, 2), sc.name)].filter(Boolean).join(" ");
}

function devSlider(c, [key, label, mn, mx, step, lg, unit, dig], disabled = false) {
  const row = document.createElement("div");
  row.className = "dev-row" + (disabled ? " off" : "");
  row.innerHTML = `<div class="dev-row-head"><span class="dev-label">${esc(tr(label))}</span>${devTip(c, key) ? `<span class="info" tabindex="0" role="img" aria-label="${esc(devTip(c, key))}">i</span>` : ""}<input class="mini-input dev-num" type="text" inputmode="decimal" aria-label="${esc(tr(label))}"><span class="dev-unit">${esc(unit)}</span></div><input type="range" class="range dev-range" min="0" max="1000" step="any" aria-label="${esc(tr(label))}">`;
  const range = row.querySelector(".dev-range"), numEl = row.querySelector(".dev-num");
  const show = () => {
    range.value = key === "iso" ? (ISO_STOPS.indexOf(snapIso(c.s[key])) * 1000) / (ISO_STOPS.length - 1) : sliderToPos(c.s[key], mn, mx, lg);
    range.style.setProperty("--p", `${range.value / 10}%`);
    numEl.value = num(c.s[key], dig, dig);
  };
  show();
  range.disabled = numEl.disabled = disabled;
  const iso = key === "iso";
  const set = (x) => { c.s[key] = Math.min(mx, Math.max(mn, iso && x > 0 ? snapIso(x) : x)); show(); devChanged(); };
  range.addEventListener("input", () => set(iso ? ISO_STOPS[Math.round((+range.value / 1000) * (ISO_STOPS.length - 1))] : posToSlider(+range.value, mn, mx, lg, step)));
  numEl.addEventListener("change", () => {
    const x = parseFloat(numEl.value.replace(",", "."));
    if (Number.isFinite(x)) set(x); else show();
  });
  if (iso && c.s[key] > 0 && snapIso(c.s[key]) !== c.s[key]) c.s[key] = snapIso(c.s[key]);
  show();
  range.addEventListener("dblclick", () => set(c.info.asShot[key]));
  return row;
}

function devSelect(label, options, value, onChange, tip) {
  const row = document.createElement("div");
  row.className = "dev-row";
  row.innerHTML = `<div class="dev-row-head"><span class="dev-label">${esc(tr(label))}</span>${tip ? `<span class="info" tabindex="0" role="img" aria-label="${esc(tr(tip))}">i</span>` : ""}</div>`;
  const sel = document.createElement("select");
  sel.className = "mini-select dev-select";
  sel.setAttribute("aria-label", tr(label));
  for (const [v, l] of options) {
    const o = document.createElement("option");
    o.value = v; o.textContent = tr(l);
    sel.append(o);
  }
  sel.value = value;
  sel.addEventListener("change", () => onChange(sel.value));
  row.append(sel);
  return row;
}

function devRenderControls() {
  const c = devClip();
  if (!c) return;
  const box = devEl("dev-controls");
  box.innerHTML = "";
  const i = c.info;
  devEl("dev-name").textContent = dev.path.split(/[\\/]/).pop();
  devEl("dev-meta").textContent = [i.camera, `${i.width} x ${i.height}`, `${num(i.fps, 0, 3)} fps`, i.iso ? `ISO ${i.iso}` : "", i.kelvin ? `${i.kelvin} K` : ""].filter(Boolean).join(" · ");
  devEl("dev-frame").max = Math.max(0, i.frames - 1);
  devEl("dev-frame").value = dev.frame;
  devFrameFill();
  if (i.r3d) {
    devSection(box, "exposure", N_("Exposure")).append(devSlider(c, R3D_SLIDERS[0]), devSlider(c, R3D_SLIDERS[1]));
    devSection(box, "wb", N_("White Balance")).append(devSlider(c, R3D_SLIDERS[2]), devSlider(c, R3D_SLIDERS[3]));
    const tone = devSection(box, "tone", N_("Tone"));
    tone.append(devSlider(c, R3D_SLIDERS[4]));
    tone.append(devSelect(N_("Highlight Roll Off"), DEV_ROLL, c.s.rollOff, (v) => { c.s.rollOff = v; devChanged(); }, N_("How bright highlights ease into white in Rec.709.")));
    tone.append(devSelect(N_("Output Tone Map"), DEV_TONE, c.s.toneMap, (v) => { c.s.toneMap = v; devChanged(); }, N_("Contrast of the Rec.709 picture.")));
  } else {
    const wb = devSection(box, "wb", N_("White Balance"));
    wb.append(devSelect(N_("White Balance"), DEV_WB.map(([v, l]) => [v, l]), c.wb, (v) => {
      c.wb = v;
      const p = DEV_WB.find((x) => x[0] === v);
      if (v === "asShot") { c.s.kelvin = i.asShot.kelvin; c.s.tint = i.asShot.tint; }
      else if (v !== "custom") { c.s.kelvin = p[2]; c.s.tint = 0; }
      devRenderControls();
      devChanged();
    }));
    const custom = c.wb === "custom";
    wb.append(devSlider(c, NRAW_SLIDERS[0], !custom), devSlider(c, NRAW_SLIDERS[1], !custom));
    const ex = devSection(box, "exposure", N_("Exposure"));
    for (const k of NRAW_SLIDERS.slice(2, 5)) ex.append(devSlider(c, k));
    const col = devSection(box, "color", N_("Color"));
    for (const k of NRAW_SLIDERS.slice(5)) col.append(devSlider(c, k));
  }
  const nr = devSection(box, "nr", N_("Noise Reduction"));
  for (const k of NR_SLIDERS) nr.append(devSlider(c, k));
  devRenderOutput();
  devRenderExport();
}

const devNative = (c) => (c.info.r3d ? "log3g10" : "nlog");
const devOutValue = (s) => (s.lut ? "lut:" + s.lut : s.output === "rec709" ? "rec709" : "native");

// Sets a clip's output from a dropdown value: native, rec709 or lut:<name>.
function devSetOutput(c, v) {
  if (v.startsWith("lut:")) { c.s.output = devNative(c); c.s.lut = v.slice(4); }
  else { c.s.output = v === "rec709" ? "rec709" : devNative(c); c.s.lut = null; }
}

async function devLoadLuts() {
  try { dev.luts = (await devInvoke("lut_list")) || []; } catch { dev.luts = []; }
}

async function devImportLuts(c) {
  let r;
  try { r = await devInvoke("lut_import"); } catch (e) { devError(tr("Could not import the LUT: {0}", String(e))); return; }
  if (!r) return;
  await devLoadLuts();
  if (devDemo) for (const a of r.added) if (!dev.luts.some((l) => l.name === a.name)) dev.luts.push(a);
  if (r.added.length && c) devSetOutput(c, "lut:" + r.added[r.added.length - 1].name);
  devError(r.errors.map(([f, e]) => tr("{0} is not a LUT that can be used: {1}", f, e)).join("\n"));
  devRenderOutput();
  devChanged();
}

async function devRemoveLut(name) {
  try { await devInvoke("lut_remove", { name }); } catch (e) { devError(String(e)); return; }
  dev.luts = dev.luts.filter((l) => l.name !== name);
  for (const c of dev.clips.values()) if (c.s.lut === name) devSetOutput(c, "native");
  devRenderOutput();
  devChanged();
}

function devRenderOutput() {
  const c = devClip();
  if (!c) return;
  let body = devEl("dev-output");
  if (!body) {
    body = devSection(devEl("dev-controls"), "output", N_("Output"));
    body.id = "dev-output";
  }
  body.innerHTML = "";
  if (c.s.lut && !dev.luts.some((l) => l.name === c.s.lut)) devSetOutput(c, "native");
  const native = c.info.r3d ? "Log3G10 / REDWideGamutRGB" : "N-Log / Rec.2020";
  const opts = [["native", native], ["rec709", "Rec.709"], ...dev.luts.map((l) => ["lut:" + l.name, tr("LUT: {0}", l.name)]), ["import", tr("Import LUT…")]];
  const row = devSelect(N_("Output"), opts, devOutValue(c.s), (v) => {
    if (v === "import") { devRenderOutput(); devImportLuts(c); return; }
    devSetOutput(c, v);
    devRenderOutput();
    devChanged();
  }, N_("Log keeps the most for grading later. Rec.709 is ready to watch. A LUT is applied to the log picture, and the export is tagged Rec.709."));
  body.append(row);
  if (c.s.lut) {
    const rm = document.createElement("button");
    rm.type = "button";
    rm.className = "btn ghost dev-lut-remove";
    rm.textContent = tr("Remove this LUT");
    const name = c.s.lut;
    rm.addEventListener("click", () => devRemoveLut(name));
    body.append(rm);
  }
}

function devRenderExport() {
  const c = devClip();
  if (!c) return;
  const box = devEl("dev-export");
  box.innerHTML = "";
  const fullH = Math.round(c.info.height / 2) * 2, fullW = Math.floor(c.info.width / 2) * 2;
  const sizes = DEV_SIZES.filter(([w]) => w < fullW).map(([w, l]) => [String(w), w ? l : tr("Full ({0})", `${fullW} x ${fullH}`)]);
  box.append(devSelect(N_("Size"), sizes, String(dev.export.width), (v) => { dev.export.width = +v; dev.export.mbps = null; devRenderExport(); }));
  box.append(devSelect(N_("Quality"), DEV_QUALITY, dev.export.high ? "high" : "standard", (v) => { dev.export.high = v === "high"; dev.export.mbps = null; devRenderExport(); }, N_("10-bit either way. High doubles the bitrate for the cleanest picture.")));
  const row = document.createElement("div");
  row.className = "dev-row";
  row.innerHTML = `<div class="dev-row-head"><span class="dev-label">${esc(tr("Bitrate"))}</span><span class="info" tabindex="0" role="img" aria-label="${esc(tr("10-bit HEVC. 100 Mb/s at 4K by default."))}">i</span><input class="mini-input dev-num" id="dev-mbps" type="text" inputmode="decimal" aria-label="${esc(tr("Bitrate"))}"><span class="dev-unit">Mb/s</span></div>
    <label class="dev-check"><input type="checkbox" id="dev-cbr" class="switch"><span>${esc(tr("Constant bitrate"))}</span></label>`;
  box.append(row);
  const mb = row.querySelector("#dev-mbps");
  const ew = dev.export.width || Math.floor(c.info.width / 2) * 2;
  const h = Math.round((ew * c.info.height) / c.info.width / 2) * 2;
  devInvoke("develop_default_kbps", { width: ew, height: h, fps: c.info.fps, high: dev.export.high }).then((k) => {
    if (dev.export.mbps == null) mb.value = num(k / 1000, 0, 0);
  });
  if (dev.export.mbps != null) mb.value = num(dev.export.mbps, 0, 1);
  mb.addEventListener("change", () => {
    const x = parseFloat(mb.value.replace(",", "."));
    dev.export.mbps = Number.isFinite(x) && x >= 5 && x <= 2000 ? x : null;
    devRenderExport();
  });
  const cbr = row.querySelector("#dev-cbr");
  cbr.checked = dev.export.cbr;
  cbr.addEventListener("change", () => { dev.export.cbr = cbr.checked; });
  const where = document.createElement("p");
  where.className = "hint";
  where.textContent = tr("Saved in a Developed folder next to each clip.");
  box.append(where);
  devEl("dev-export-one").disabled = dev.exporting;
  devEl("dev-export-all").disabled = dev.exporting;
  devEl("dev-cancel").hidden = !dev.exporting;
  devEl("dev-status").textContent = dev.status;
}

function devFrameFill() {
  const f = devEl("dev-frame");
  f.style.setProperty("--p", `${(100 * f.value) / Math.max(1, f.max)}%`);
  const c = devClip();
  if (c) devEl("dev-tc").textContent = `${+f.value + 1} / ${c.info.frames}` + (dev.playFps ? ` · ${fpsText(dev.playFps)} fps` : "");
}

function devChanged() {
  const pc = devClip();
  if (dev.playing && pc) devInvoke("develop_play_settings", { settings: pc.s, view709: (dev.view709 || pc.s.output === "rec709") && !pc.s.lut }).catch(() => {});
  else devSchedule(60);
  const on = devEl("dev-list").querySelector(".dev-clip.on .dev-clip-sub");
  const c = devClip();
  if (on && c) on.textContent = (c.info.r3d ? "R3D NE" : isPrr(jobs.find((j) => j.path === dev.path) || {}) ? "ProRes RAW" : "N-RAW") + (JSON.stringify(c.s) !== JSON.stringify(c.info.asShot) ? " · " + tr("Edited") : "");
}

function devSchedule(ms) {
  clearTimeout(dev.timer);
  dev.timer = setTimeout(devPreview, ms);
}

// Zoom ---------------------------------------------------------------------------
// Percentages are the clip's real pixels on the screen's real pixels, as in the Compress
// preview: 100% shows one source pixel per device pixel. Zoomed in, only the visible part
// of the frame is developed, at the size it covers on screen.
const DEV_MAX_ZOOM = 8; // 800%
const devDpr = () => window.devicePixelRatio || 1;
const devFitK = (c) => {
  const st = devEl("dev-stage");
  return Math.min(st.clientWidth / c.info.width, st.clientHeight / c.info.height);
};
const devZoomPct = (c) => (dev.view.fit ? devFitK(c) : dev.view.k) * devDpr() * 100;

// The whole frame's rectangle on the stage, in CSS pixels.
function devFrameRect(c) {
  const st = devEl("dev-stage");
  const k = dev.view.fit ? devFitK(c) : dev.view.k;
  const w = c.info.width * k, h = c.info.height * k;
  const cx = dev.view.fit ? 0.5 : dev.view.cx, cy = dev.view.fit ? 0.5 : dev.view.cy;
  return { x: st.clientWidth / 2 - cx * w, y: st.clientHeight / 2 - cy * h, w, h };
}

function devClampView(c) {
  const st = devEl("dev-stage"), f = devFrameRect(c);
  const hx = st.clientWidth / 2 / f.w, hy = st.clientHeight / 2 / f.h;
  dev.view.cx = hx >= 0.5 ? 0.5 : Math.min(1 - hx, Math.max(hx, dev.view.cx));
  dev.view.cy = hy >= 0.5 ? 0.5 : Math.min(1 - hy, Math.max(hy, dev.view.cy));
}

// The visible part of the frame in fractions, and its size on screen in device pixels.
function devVisible(c, max) {
  const st = devEl("dev-stage"), f = devFrameRect(c);
  const snap = (v) => Math.round(v * 8192) / 8192;
  const x0 = snap(Math.max(0, -f.x / f.w)), y0 = snap(Math.max(0, -f.y / f.h));
  const x1 = snap(Math.min(1, (st.clientWidth - f.x) / f.w)), y1 = snap(Math.min(1, (st.clientHeight - f.y) / f.h));
  const crop = [x0, y0, Math.max(1 / 8192, x1 - x0), Math.max(1 / 8192, y1 - y0)];
  let pw = Math.round(crop[2] * f.w * devDpr()), ph = Math.round(crop[3] * f.h * devDpr());
  const s = Math.min(1, max / Math.max(pw, ph));
  pw = Math.max(16, Math.round(pw * s)); ph = Math.max(16, Math.round(ph * s));
  return { crop, pw, ph };
}

// Puts the canvas where the crop it shows sits in the current view.
function devLayout() {
  const c = devClip(), cv = devEl("dev-canvas");
  if (!c || !dev.shown) return;
  const f = devFrameRect(c), [x, y, w, h] = dev.shown;
  Object.assign(cv.style, { left: `${f.x + x * f.w}px`, top: `${f.y + y * f.h}px`, width: `${w * f.w}px`, height: `${h * f.h}px` });
}

function devRenderZoom() {
  const c = devClip();
  if (!c) return;
  const pct = devZoomPct(c);
  const step = ["1", "2", "4"].find((v) => Math.abs(pct - Number(v) * 100) < 0.5);
  setSeg("dev-zoom", dev.view.fit ? "fit" : step || "", null);
  devEl("dev-stage").classList.toggle("zoomed", !dev.view.fit);
  devEl("dev-zoom-badge").textContent = `${Math.round(pct)}%`;
}

let devPlayRestart = 0;
function devViewChanged() {
  devLayout();
  devRenderZoom();
  if (dev.playing) {
    // Playback develops the visible part: start again from here once the view settles.
    clearTimeout(devPlayRestart);
    devPlayRestart = setTimeout(() => { if (dev.playing) { devStop(true); devPlay(); } }, 180);
  } else devSchedule(50);
}

// Zooms to `k` CSS pixels per source pixel (0 fits) around `anchor` (stage CSS pixels).
function devZoomTo(k, anchor) {
  const c = devClip();
  if (!c || !Number.isFinite(k)) return;
  const st = devEl("dev-stage"), fk = devFitK(c);
  k = Math.min(DEV_MAX_ZOOM / devDpr(), Math.max(fk, k));
  const ax = anchor ? anchor.x : st.clientWidth / 2, ay = anchor ? anchor.y : st.clientHeight / 2;
  const f = devFrameRect(c);
  const u = (ax - f.x) / f.w, v = (ay - f.y) / f.h;
  if (k <= fk * 1.001) dev.view = { fit: true, k: fk, cx: 0.5, cy: 0.5 };
  else {
    const w = c.info.width * k, h = c.info.height * k;
    dev.view = { fit: false, k, cx: (st.clientWidth / 2 - (ax - u * w)) / w, cy: (st.clientHeight / 2 - (ay - v * h)) / h };
    devClampView(c);
  }
  devViewChanged();
}

function devPan(dx, dy) {
  const c = devClip();
  if (!c || dev.view.fit) return;
  const f = devFrameRect(c);
  dev.view.cx -= dx / f.w;
  dev.view.cy -= dy / f.h;
  devClampView(c);
  devViewChanged();
}

function devBindStage() {
  const st = devEl("dev-stage");
  const local = (e) => { const r = st.getBoundingClientRect(); return { x: e.clientX - r.left, y: e.clientY - r.top }; };
  const k = () => (dev.view.fit ? devFitK(devClip()) : dev.view.k);
  // WebKit (macOS) delivers trackpad pinches as gesture events.
  let gestureStart = null;
  st.addEventListener("gesturestart", (e) => { e.preventDefault(); gestureStart = devClip() ? k() : null; });
  st.addEventListener("gesturechange", (e) => { e.preventDefault(); if (gestureStart != null) devZoomTo(gestureStart * e.scale, local(e)); });
  st.addEventListener("gestureend", (e) => { e.preventDefault(); gestureStart = null; });
  let drag = null;
  st.addEventListener("pointerdown", (e) => {
    if (e.button !== 0 || !devClip()) return;
    drag = { start: local(e), last: local(e), moved: false };
    st.setPointerCapture(e.pointerId);
    st.classList.toggle("panning", !dev.view.fit);
  });
  st.addEventListener("pointermove", (e) => {
    if (!drag) return;
    const p = local(e);
    if (Math.abs(p.x - drag.start.x) + Math.abs(p.y - drag.start.y) > 3) drag.moved = true;
    const dx = p.x - drag.last.x, dy = p.y - drag.last.y;
    drag.last = p;
    if (drag.moved) devPan(dx, dy);
  });
  const end = (e) => {
    if (!drag) return;
    // A click toggles between fit and 100%.
    if (!drag.moved) devZoomTo(dev.view.fit ? 1 / devDpr() : 0, local(e));
    drag = null;
    st.classList.remove("panning");
  };
  st.addEventListener("pointerup", end);
  st.addEventListener("pointercancel", () => { drag = null; st.classList.remove("panning"); });
  st.addEventListener("wheel", (e) => {
    e.preventDefault();
    if (!devClip()) return;
    // Pinch arrives as ctrl+wheel; a mouse wheel notch zooms too; trackpad scrolling pans.
    const notch = e.deltaMode !== 0 || (e.deltaX === 0 && Math.abs(e.deltaY) >= 40 && Number.isInteger(e.deltaY));
    if (e.ctrlKey || e.metaKey) devZoomTo(k() * Math.exp(-e.deltaY * 0.012), local(e));
    else if (notch) devZoomTo(k() * (e.deltaY > 0 ? 1 / 1.25 : 1.25), local(e));
    else devPan(-e.deltaX, -e.deltaY);
  }, { passive: false });
  devEl("dev-zoom").addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (b) devZoomTo(b.dataset.value === "fit" ? 0 : Number(b.dataset.value) / devDpr(), null);
  });
}

function devDraw(bytes, at, crop) {
  const dv = new DataView(bytes.buffer, bytes.byteOffset);
  const bw = dv.getUint32(0, true), bh = dv.getUint32(4, true);
  const cv = devEl("dev-canvas");
  if (cv.width !== bw || cv.height !== bh) { cv.width = bw; cv.height = bh; }
  cv.getContext("2d").putImageData(new ImageData(new Uint8ClampedArray(bytes.buffer, bytes.byteOffset + at, bw * bh * 4), bw, bh), 0, 0);
  dev.shown = crop;
  devLayout();
}

// Frames come over the bcframe protocol (much faster than command results).
const devFrameUrl = (p) => (/windows/i.test(navigator.userAgent) || /android/i.test(navigator.userAgent) ? `http://bcframe.localhost/${p}` : `bcframe://localhost/${p}`);
async function devNextFrame() {
  if (devDemo || dev.noProtocol) return devInvoke("develop_play_frame");
  try {
    const r = await fetch(devFrameUrl("play"), { cache: "no-store" });
    if (!r.ok) throw new Error(await r.text());
    return await r.arrayBuffer();
  } catch (e) {
    if (e instanceof TypeError) { dev.noProtocol = true; return devInvoke("develop_play_frame"); }
    throw e;
  }
}

const devBytes = (buf) => new Uint8Array(buf instanceof ArrayBuffer ? buf : buf.buffer || new Uint8Array(buf).buffer);

function devPlayIcon() {
  devEl("dev-play-ico").setAttribute("d", dev.playing ? "M4 2.5h3v11H4ZM9 2.5h3v11H9Z" : "M4 2.5v11L13.5 8Z");
  const l = tr(dev.playing ? "Pause" : "Play");
  devEl("dev-play").setAttribute("aria-label", l);
}

// Playback: frames come developed with the current settings, paced to the clip's frame rate
// (late frames are skipped). Pausing shows the frame again at full preview quality.
async function devPlay() {
  if (dev.playing) return devStop();
  const c = devClip();
  if (!c || dev.exporting) return;
  if (dev.frame >= c.info.frames - 1) dev.frame = 0;
  const run = ++dev.playRun;
  dev.playing = true;
  dev.playFps = 0;
  devPlayIcon();
  const { crop, pw, ph } = devVisible(c, 1280);
  try {
    await devInvoke("develop_play_start", { path: dev.path, frame: dev.frame, settings: c.s, crop, width: pw, height: ph, view709: (dev.view709 || c.s.output === "rec709") && !c.s.lut });
    let shown = 0, t0 = performance.now();
    while (dev.playing && run === dev.playRun) {
      const tw = performance.now();
      const bytes = devBytes(await devNextFrame());
      if (!dev.playing || run !== dev.playRun) break;
      const dv = new DataView(bytes.buffer, bytes.byteOffset);
      const idx = dv.getUint32(8, true);
      if (idx === 0xffffffff) break;
      if (dv.getUint32(0, true) === 0) continue;
      const t1 = performance.now();
      devDraw(bytes, 12, crop);
      if (dev.bench) { dev.bench.wait += t1 - tw; dev.bench.draw += performance.now() - t1; dev.bench.frames++; dev.bench.bytes += bytes.length; dev.bench.last = performance.now(); dev.bench.size = `${cv0()}`; }
      devError("");
      dev.frame = idx;
      devEl("dev-frame").value = idx;
      shown++;
      const el = (performance.now() - t0) / 1000;
      if (el > 0.5) { dev.playFps = shown / el; shown = 0; t0 = performance.now(); }
      devFrameFill();
    }
  } catch (e) {
    devError(String(e));
  }
  if (run === dev.playRun) devStop();
}

function devStop(quiet) {
  if (!dev.playing) return;
  dev.playing = false;
  dev.playRun++;
  dev.playFps = 0;
  devPlayIcon();
  devFrameFill();
  devInvoke("develop_play_stop").catch(() => {});
  if (!quiet) devSchedule(0);
}

function devStep(d) {
  const c = devClip();
  if (!c) return;
  devStop();
  dev.frame = Math.max(0, Math.min(c.info.frames - 1, dev.frame + d));
  devEl("dev-frame").value = dev.frame;
  devFrameFill();
  devSchedule(0);
}

async function devPreview() {
  const c = devClip();
  const stage = devEl("dev-stage");
  if (!c || !stage.clientWidth) return;
  if (dev.playing) return;
  const seq = ++dev.seq;
  // Fit: up to 1920 wide. Zoomed in: the visible part at its size on screen.
  const { crop, pw, ph } = devVisible(c, dev.view.fit ? 1920 : 4096);
  devEl("dev-loading").hidden = false;
  try {
    const view709 = (dev.view709 || c.s.output === "rec709") && !c.s.lut;
    const buf = await devInvoke("develop_preview", { path: dev.path, frame: dev.frame, settings: c.s, crop, width: pw, height: ph, view709 });
    if (seq !== dev.seq || dev.playing) return;
    devDraw(devBytes(buf), 8, crop);
    devError("");
  } catch (e) {
    if (seq === dev.seq) devError(String(e));
  } finally {
    if (seq === dev.seq) devEl("dev-loading").hidden = true;
  }
}

function devRequest(path) {
  const c = dev.clips.get(path);
  return { path, settings: c.s, width: dev.export.width, kbps: dev.export.mbps != null ? Math.round(dev.export.mbps * 1000) : null, cbr: dev.export.cbr, high: dev.export.high };
}

async function devExport(all) {
  if (dev.exporting) return;
  devStop();
  const paths = all ? devClipsInQueue().map((j) => j.path) : [dev.path];
  for (const p of paths) await devEnsure(p);
  const cur = devClip();
  const out = cur ? devOutValue(cur.s) : null;
  // "Export all" uses each clip's own settings, with the chosen output.
  if (all && out) for (const p of paths) devSetOutput(dev.clips.get(p), out);
  dev.exporting = true;
  dev.done = 0;
  dev.total = paths.length;
  dev.status = tr("Starting");
  devRenderExport();
  try {
    await devInvoke("develop_export", { jobs: paths.map(devRequest) });
  } catch (e) {
    dev.exporting = false;
    dev.status = String(e);
    devRenderExport();
  }
}

function devInit() {
  if (WEB) { devEl("tabs").hidden = true; return; }
  for (const b of devEl("tabs").querySelectorAll("button")) b.addEventListener("click", () => devShowTab(b.dataset.value));
  devEl("dev-play").addEventListener("click", () => devPlay());
  devBindStage();
  devLoadLuts();
  // The export settings collapse like the others; the export buttons stay in view.
  const exp = devEl("dev-export"), expHead = devEl("dev-export-head");
  const expBody = devSection(document.createElement("div"), "export", N_("Export"));
  const expWrap = expBody.parentElement;
  expHead.replaceWith(expWrap);
  expBody.append(exp);
  devEl("dev-prev").addEventListener("click", () => devStep(-1));
  devEl("dev-next").addEventListener("click", () => devStep(1));
  document.addEventListener("keydown", (e) => {
    if (devEl("develop").hidden || e.metaKey || e.ctrlKey || e.altKey) return;
    const t = e.target, tag = t.tagName;
    if (tag === "SELECT" || tag === "TEXTAREA" || (tag === "INPUT" && !(t.type === "range" && t.id === "dev-frame")) || (tag === "BUTTON" && e.key === " " && t.id !== "dev-play")) return;
    if (e.key === " ") { e.preventDefault(); if (t.id !== "dev-play") devPlay(); }
    else if (e.key === "ArrowLeft") { e.preventDefault(); devStep(e.shiftKey ? -10 : -1); }
    else if (e.key === "ArrowRight") { e.preventDefault(); devStep(e.shiftKey ? 10 : 1); }
    else if (e.key === "f") devZoomTo(0, null);
    else if (e.key === "z") devZoomTo(dev.view.fit ? 1 / devDpr() : 0, null);
    else if (e.key === "+" || e.key === "=") devZoomTo((dev.view.fit ? devFitK(devClip()) : dev.view.k) * 1.5, null);
    else if (e.key === "-") devZoomTo((dev.view.fit ? devFitK(devClip()) : dev.view.k) / 1.5, null);
  });
  devEl("dev-frame").addEventListener("pointerdown", () => devStop());
  devEl("dev-frame").addEventListener("input", () => { devStop(); dev.frame = +devEl("dev-frame").value; devFrameFill(); devSchedule(40); });
  devEl("dev-view").addEventListener("change", () => { dev.view709 = devEl("dev-view").checked; devChanged(); });
  devEl("dev-view").checked = dev.view709;
  devEl("dev-reset").addEventListener("click", () => {
    const c = devClip();
    if (!c) return;
    c.s = { ...c.info.asShot, output: c.s.output };
    c.wb = "asShot";
    devRenderControls();
    devChanged();
  });
  devEl("dev-auto").addEventListener("click", async () => {
    const c = devClip();
    if (!c) return;
    const btn = devEl("dev-auto");
    btn.disabled = true;
    try {
      c.s.exposure = await devInvoke("develop_auto_exposure", { path: dev.path, frame: dev.frame, settings: { ...c.s, exposure: 0 } });
      devRenderControls();
      devChanged();
    } catch (e) {
      devError(String(e));
    } finally {
      btn.disabled = false;
    }
  });
  devEl("dev-export-one").addEventListener("click", () => devExport(false));
  devEl("dev-export-all").addEventListener("click", () => devExport(true));
  devEl("dev-cancel").addEventListener("click", () => devInvoke("develop_cancel"));
  devEl("dev-add").addEventListener("click", () => devEl("btn-add").click());
  devListen("develop-progress", (p) => {
    const name = p.path.split(/[\\/]/).pop();
    dev.status = tr("Exporting {0}: {1}% at {2} fps", name, Math.round(p.fraction * 100), fpsText(p.fps));
    devEl("dev-status").textContent = dev.status;
  });
  devListen("develop-done", (d) => {
    dev.done++;
    dev.status = d.ok ? tr("Saved {0} ({1}, {2})", d.output.split(/[\\/]/).pop(), gb(d.bytes), duration(d.seconds)) : tr("Export failed: {0}", d.error || "");
    devEl("dev-status").textContent = dev.status;
  });
  devListen("develop-finished", () => {
    dev.exporting = false;
    devRenderExport();
  });
  new ResizeObserver(() => {
    const c = devClip();
    if (devEl("develop").hidden || !c) return;
    if (!dev.view.fit) { dev.view.k = Math.max(dev.view.k, devFitK(c)); devClampView(c); }
    devLayout();
    devRenderZoom();
    devSchedule(80);
  }).observe(devEl("dev-stage"));
  // Re-render in the new language.
  new MutationObserver(() => { if (!devEl("develop").hidden) devRender(); }).observe(document.documentElement, { attributes: true, attributeFilter: ["lang"] });
  if (new URLSearchParams(location.search).get("tab") === "develop") setTimeout(() => devShowTab("develop"), 600);
}

devInit();

const cv0 = () => { const c = devEl("dev-canvas"); return `${c.width}x${c.height}`; };
// Development benchmark (see develop_bench): play the first clip and report the timing.
(async () => {
  if (devDemo) return;
  const cfg = await devInvoke("develop_bench").catch(() => null);
  if (!cfg) return;
  const [secs, nr] = cfg;
  for (let i = 0; i < 100 && !devClipsInQueue().length; i++) await new Promise((r) => setTimeout(r, 200));
  devShowTab("develop");
  await new Promise((r) => setTimeout(r, 3000));
  const c = devClip();
  if (c && nr) { c.s.nrLuma = nr; c.s.nrChroma = nr; }
  dev.bench = { wait: 0, draw: 0, frames: 0, bytes: 0, t0: performance.now() };
  devPlay();
  await new Promise((r) => setTimeout(r, secs * 1000));
  const b = dev.bench, el = ((b.last || performance.now()) - b.t0) / 1000;
  const cv = devEl("dev-canvas");
  devStop();
  await devInvoke("develop_bench_done", { report: `${b.frames} frames in ${el.toFixed(2)}s = ${(b.frames / el).toFixed(1)} fps; frames ${b.size}; avg wait+transfer ${(b.wait / b.frames).toFixed(1)} ms, draw ${(b.draw / b.frames).toFixed(1)} ms, ${(b.bytes / b.frames / 1e6).toFixed(1)} MB/frame` });
})();
