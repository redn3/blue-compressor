// Translations: tr(key, ...args) looks the English text up in the chosen language
// (locales.js) and fills in {0}, {1}... Numbers are formatted for the language.
"use strict";

const LANGS = [
  ["en", "English"], ["sv", "Svenska"], ["de", "Deutsch"], ["fr", "Français"], ["es", "Español"],
  ["pt-BR", "Português (Brasil)"], ["it", "Italiano"], ["nl", "Nederlands"], ["pl", "Polski"],
  ["ru", "Русский"], ["ja", "日本語"], ["ko", "한국어"],
  ["zh-Hans", "简体中文"], ["zh-Hant", "繁體中文"],
];
const LANG_KEY = "lang";
const I18N = { lang: "en", dict: {}, nf: new Map() };

// The OS or browser language, matched to one we have.
function detectLang(list) {
  const have = new Set(LANGS.map((l) => l[0]));
  for (const raw of list || []) {
    const l = String(raw || "");
    const low = l.toLowerCase();
    if (low.startsWith("zh")) {
      return /hant|tw|hk|mo/.test(low) ? "zh-Hant" : "zh-Hans";
    }
    if (low.startsWith("pt")) return "pt-BR";
    if (have.has(l)) return l;
    const base = low.split(/[-_]/)[0];
    if (have.has(base)) return base;
  }
  return "en";
}

function savedLang() {
  try { return localStorage.getItem(LANG_KEY) || ""; } catch { return ""; }
}

function num(x, minFrac = 0, maxFrac = 3) {
  const key = `${minFrac}|${maxFrac}`;
  let f = I18N.nf.get(key);
  if (!f) {
    f = new Intl.NumberFormat(I18N.lang, { minimumFractionDigits: minFrac, maximumFractionDigits: maxFrac, useGrouping: false });
    I18N.nf.set(key, f);
  }
  return f.format(x);
}

function tr(key, ...args) {
  const s = Object.prototype.hasOwnProperty.call(I18N.dict, key) ? I18N.dict[key] : key;
  return args.length ? s.replace(/\{(\d+)\}/g, (m, i) => {
    const v = args[Number(i)];
    return v === undefined ? m : typeof v === "number" ? num(v) : String(v);
  }) : s;
}

// Plural: `one` for 1 (as the language counts it), else `other`; {0} is n.
function tp(one, other, n, ...rest) {
  const form = new Intl.PluralRules(I18N.lang).select(n);
  return tr(form === "one" ? one : other, n, ...rest);
}

// Translates the page: data-i18n (text), data-i18n-title/-aria-label/-placeholder
// (attributes) hold the English; data-i18n-html names a key whose text has markup.
function translatePage(root = document) {
  for (const el of root.querySelectorAll("[data-i18n]")) el.textContent = tr(el.dataset.i18n);
  for (const a of ["title", "aria-label", "placeholder"]) {
    for (const el of root.querySelectorAll(`[data-i18n-${a}]`)) el.setAttribute(a, tr(el.getAttribute(`data-i18n-${a}`)));
  }
  for (const el of root.querySelectorAll("[data-i18n-html]")) {
    const key = el.dataset.i18nHtml;
    const en = (window.BC_LOCALES.en || {})[key];
    const html = I18N.dict[key] || en;
    if (html) el.innerHTML = html;
  }
  document.documentElement.lang = I18N.lang;
}

function setLang(choice) {
  const lang = choice && choice !== "auto" ? choice : detectLang(navigator.languages || [navigator.language]);
  I18N.lang = lang;
  I18N.dict = Object.assign({}, (window.BC_LOCALES || {}).en, (window.BC_LOCALES || {})[lang]);
  I18N.nf.clear();
  translatePage();
}

// The language picker: "Automatic" follows the system; the choice is remembered.
function initLangPicker() {
  const sel = document.getElementById("lang");
  if (!sel) return;
  const auto = document.createElement("option");
  auto.value = "auto";
  auto.dataset.i18n = "Automatic";
  auto.textContent = tr("Automatic");
  sel.append(auto);
  for (const [code, name] of LANGS) {
    const o = document.createElement("option");
    o.value = code;
    o.textContent = name;
    o.lang = code;
    sel.append(o);
  }
  sel.value = savedLang() || "auto";
  sel.addEventListener("change", () => {
    try {
      if (sel.value === "auto") localStorage.removeItem(LANG_KEY);
      else localStorage.setItem(LANG_KEY, sel.value);
    } catch { /* private mode */ }
    setLang(sel.value);
    if (window.bcRerender) window.bcRerender();
  });
}

setLang(savedLang() || "auto");
initLangPicker();
