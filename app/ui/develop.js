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
const DEV_SIZES = [[3840, "4K"], [1920, "1080p"]];

// True ISO values in third stops; the ISO slider and box only land on these.
const ISO_STOPS = [50, 64, 80, 100, 125, 160, 200, 250, 320, 400, 500, 640, 800, 1000, 1250, 1600, 2000, 2500, 3200, 4000, 5000, 6400];
const snapIso = (v) => ISO_STOPS.reduce((b, x) => (Math.abs(Math.log(x / v)) < Math.abs(Math.log(b / v)) ? x : b), ISO_STOPS[0]);

// Each slider: key, label, min, max, step, log scale, unit, format digits.
const R3D_SLIDERS = [
  ["iso", N_("ISO"), 50, 6400, 1, true, "", 0],
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
  export: { width: 3840, mbps: null, cbr: false, output: "native" },
};

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
        asShot: { output: r3d ? "log3g10" : "nlog", kelvin: k, tint: 0, exposure: r3d ? 1 : 0, iso: 800, rollOff: "medium", toneMap: "medium", contrast: 0, highlights: 0, shadows: 0, saturation: 0, colorBoost: 0, sharpness: 20 },
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
  }
  return null;
}

const devEl = (id) => document.getElementById(id);
const devClip = () => dev.clips.get(dev.path);

function devClipsInQueue() {
  return jobs.filter((j) => j.ok && (isNev(j) || isR3d(j)));
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
    b.innerHTML = `<span class="dev-clip-name">${esc(j.name)}</span><span class="dev-clip-sub">${esc(isR3d(j) ? "R3D NE" : "N-RAW")}${edited ? " · " + esc(tr("Edited")) : ""}</span>`;
    b.addEventListener("click", () => { dev.path = j.path; dev.frame = 0; devRender(); });
    ul.append(b);
  }
  if (!dev.path) return;
  devEnsure(dev.path).then(() => { devRenderControls(); devSchedule(0); }).catch((e) => devError(String(e)));
}

let devQueueKey = "";
function devQueueChanged() {
  const key = devClipsInQueue().map((j) => j.path).join("|");
  if (key === devQueueKey) return;
  devQueueKey = key;
  if (!devEl("develop").hidden) devRender();
}

function devError(msg) {
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
  const sec = (title) => {
    const h = document.createElement("div");
    h.className = "dev-sec";
    h.textContent = tr(title);
    box.append(h);
  };
  if (i.r3d) {
    sec(N_("Exposure"));
    box.append(devSlider(c, R3D_SLIDERS[0]), devSlider(c, R3D_SLIDERS[1]));
    sec(N_("White Balance"));
    box.append(devSlider(c, R3D_SLIDERS[2]), devSlider(c, R3D_SLIDERS[3]));
    sec(N_("Tone"));
    box.append(devSlider(c, R3D_SLIDERS[4]));
    box.append(devSelect(N_("Highlight Roll Off"), DEV_ROLL, c.s.rollOff, (v) => { c.s.rollOff = v; devChanged(); }, N_("How bright highlights ease into white in Rec.709.")));
    box.append(devSelect(N_("Output Tone Map"), DEV_TONE, c.s.toneMap, (v) => { c.s.toneMap = v; devChanged(); }, N_("Contrast of the Rec.709 picture.")));
  } else {
    sec(N_("White Balance"));
    box.append(devSelect(N_("White Balance"), DEV_WB.map(([v, l]) => [v, l]), c.wb, (v) => {
      c.wb = v;
      const p = DEV_WB.find((x) => x[0] === v);
      if (v === "asShot") { c.s.kelvin = i.asShot.kelvin; c.s.tint = i.asShot.tint; }
      else if (v !== "custom") { c.s.kelvin = p[2]; c.s.tint = 0; }
      devRenderControls();
      devChanged();
    }));
    const custom = c.wb === "custom";
    box.append(devSlider(c, NRAW_SLIDERS[0], !custom), devSlider(c, NRAW_SLIDERS[1], !custom));
    sec(N_("Exposure"));
    for (const k of NRAW_SLIDERS.slice(2, 5)) box.append(devSlider(c, k));
    sec(N_("Color"));
    for (const k of NRAW_SLIDERS.slice(5)) box.append(devSlider(c, k));
  }
  devRenderExport();
}

function devRenderExport() {
  const c = devClip();
  if (!c) return;
  const box = devEl("dev-export");
  box.innerHTML = "";
  const native = c.info.r3d ? "Log3G10 / REDWideGamutRGB" : "N-Log / Rec.2020";
  box.append(devSelect(N_("Output"), [["native", native], ["rec709", "Rec.709"]], c.s.output === "rec709" ? "rec709" : "native", (v) => {
    c.s.output = v === "rec709" ? "rec709" : c.info.r3d ? "log3g10" : "nlog";
    devChanged();
  }, N_("Log keeps the most for grading later. Rec.709 is ready to watch.")));
  box.append(devSelect(N_("Size"), DEV_SIZES.map(([w, l]) => [String(w), l]), String(dev.export.width), (v) => { dev.export.width = +v; dev.export.mbps = null; devRenderExport(); }));
  const row = document.createElement("div");
  row.className = "dev-row";
  row.innerHTML = `<div class="dev-row-head"><span class="dev-label">${esc(tr("Bitrate"))}</span><span class="info" tabindex="0" role="img" aria-label="${esc(tr("10-bit HEVC. 100 Mb/s at 4K by default."))}">i</span><input class="mini-input dev-num" id="dev-mbps" type="text" inputmode="decimal" aria-label="${esc(tr("Bitrate"))}"><span class="dev-unit">Mb/s</span></div>
    <label class="dev-check"><input type="checkbox" id="dev-cbr" class="switch"><span>${esc(tr("Constant bitrate"))}</span></label>`;
  box.append(row);
  const mb = row.querySelector("#dev-mbps");
  const h = Math.round((dev.export.width * c.info.height) / c.info.width / 2) * 2;
  devInvoke("develop_default_kbps", { width: dev.export.width, height: h, fps: c.info.fps }).then((k) => {
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
}

function devChanged() {
  devSchedule(60);
  const on = devEl("dev-list").querySelector(".dev-clip.on .dev-clip-sub");
  const c = devClip();
  if (on && c) on.textContent = (c.info.r3d ? "R3D NE" : "N-RAW") + (JSON.stringify(c.s) !== JSON.stringify(c.info.asShot) ? " · " + tr("Edited") : "");
}

function devSchedule(ms) {
  clearTimeout(dev.timer);
  dev.timer = setTimeout(devPreview, ms);
}

async function devPreview() {
  const c = devClip();
  const stage = devEl("dev-stage");
  if (!c || !stage.clientWidth) return;
  const seq = ++dev.seq;
  const r = window.devicePixelRatio || 1;
  const aspect = c.info.height / c.info.width;
  let w = stage.clientWidth, h = w * aspect;
  if (h > stage.clientHeight) { h = stage.clientHeight; w = h / aspect; }
  const pw = Math.max(16, Math.min(1920, Math.round(w * r))), ph = Math.max(16, Math.round(pw * aspect));
  devEl("dev-loading").hidden = false;
  try {
    const view709 = dev.view709 || c.s.output === "rec709";
    const buf = await devInvoke("develop_preview", { path: dev.path, frame: dev.frame, settings: c.s, crop: null, width: pw, height: ph, view709 });
    if (seq !== dev.seq) return;
    const bytes = new Uint8Array(buf instanceof ArrayBuffer ? buf : buf.buffer || new Uint8Array(buf).buffer);
    const dv = new DataView(bytes.buffer, bytes.byteOffset);
    const bw = dv.getUint32(0, true), bh = dv.getUint32(4, true);
    const cv = devEl("dev-canvas");
    cv.width = bw; cv.height = bh;
    cv.style.width = `${w}px`; cv.style.height = `${h}px`;
    cv.getContext("2d").putImageData(new ImageData(new Uint8ClampedArray(bytes.buffer, bytes.byteOffset + 8, bw * bh * 4), bw, bh), 0, 0);
    devError("");
  } catch (e) {
    if (seq === dev.seq) devError(String(e));
  } finally {
    if (seq === dev.seq) devEl("dev-loading").hidden = true;
  }
}

function devRequest(path) {
  const c = dev.clips.get(path);
  return { path, settings: c.s, width: dev.export.width, kbps: dev.export.mbps != null ? Math.round(dev.export.mbps * 1000) : null, cbr: dev.export.cbr };
}

async function devExport(all) {
  if (dev.exporting) return;
  const paths = all ? devClipsInQueue().map((j) => j.path) : [dev.path];
  for (const p of paths) await devEnsure(p);
  const out = devClip()?.s.output;
  // "Export all" uses each clip's own settings, with the chosen output space.
  if (all && out) for (const p of paths) {
    const c = dev.clips.get(p);
    c.s.output = out === "rec709" ? "rec709" : c.info.r3d ? "log3g10" : "nlog";
  }
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
  devEl("dev-frame").addEventListener("input", () => { dev.frame = +devEl("dev-frame").value; devFrameFill(); devSchedule(40); });
  devEl("dev-view").addEventListener("change", () => { dev.view709 = devEl("dev-view").checked; devSchedule(0); });
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
  new ResizeObserver(() => { if (!devEl("develop").hidden) devSchedule(80); }).observe(devEl("dev-stage"));
  // Re-render in the new language.
  new MutationObserver(() => { if (!devEl("develop").hidden) devRender(); }).observe(document.documentElement, { attributes: true, attributeFilter: ["lang"] });
  if (new URLSearchParams(location.search).get("tab") === "develop") setTimeout(() => devShowTab("develop"), 600);
}

devInit();
