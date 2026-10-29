// Checks that every language in locales.js has every UI string, with the same {0}
// placeholders and the same markup. Run: node app/check-locales.mjs
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const dir = dirname(fileURLToPath(import.meta.url));
const read = (f) => readFileSync(join(dir, "ui", f), "utf8");
const js = read("app.js") + read("i18n.js") + read("develop.js");
const html = read("index.html");
const ctx = { window: {} };
vm.runInNewContext(read("locales.js"), ctx);
const locales = ctx.window.BC_LOCALES;

const unq = (s) => JSON.parse(`"${s}"`);
const unesc = (s) => s.replace(/&quot;/g, '"').replace(/&amp;/g, "&").replace(/&lt;/g, "<").replace(/&gt;/g, ">");
const keys = new Set();
const lit = '"((?:[^"\\\\]|\\\\.)*)"';
for (const m of js.matchAll(new RegExp(`\\b(?:tr|N_)\\(\\s*${lit}`, "g"))) keys.add(unq(m[1]));
for (const m of js.matchAll(new RegExp(`\\btp\\(\\s*${lit}\\s*,\\s*${lit}`, "g"))) { keys.add(unq(m[1])); keys.add(unq(m[2])); }
for (const m of html.matchAll(/data-i18n(?:-(?:title|aria-label|placeholder|html))?="([^"]*)"/g)) keys.add(unesc(m[1]));

const holders = (s) => [...s.matchAll(/\{\d+\}/g)].map((m) => m[0]).sort().join();
const tags = (s) => [...s.matchAll(/<\/?(\w+)/g)].map((m) => m[0]).sort().join();
const KEEP = ["R3D NE", "N-RAW", "Log3G10", "bpp"];
let bad = 0;
const fail = (msg) => { bad++; console.error(msg); };
const en = locales.en || {};
for (const [lang, dict] of Object.entries(locales)) {
  for (const k of keys) {
    if (!(k in dict)) { fail(`${lang}: missing ${JSON.stringify(k)}`); continue; }
    const src = en[k] ?? k, v = dict[k];
    if (holders(src) !== holders(v)) fail(`${lang}: placeholders differ in ${JSON.stringify(k)}`);
    if (tags(src) !== tags(v)) fail(`${lang}: markup differs in ${JSON.stringify(k)}`);
    for (const w of KEEP) if (src.includes(w) && !v.includes(w)) fail(`${lang}: "${w}" must stay untranslated in ${JSON.stringify(k)}`);
    if (v.includes("\u2014")) fail(`${lang}: em dash in ${JSON.stringify(k)}`);
  }
  for (const k of Object.keys(dict)) if (!keys.has(k)) fail(`${lang}: unused ${JSON.stringify(k)}`);
}
console.log(`${Object.keys(locales).length} languages, ${keys.size} strings${bad ? `, ${bad} problems` : ", all complete"}`);
process.exit(bad ? 1 : 0);
