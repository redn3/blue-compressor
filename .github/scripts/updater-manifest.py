#!/usr/bin/env python3
"""Writes latest.json for the Tauri updater from the signatures of a release.

Usage: updater-manifest.py <version> <base download url> <folder with .sig files> <release notes> <out>

With ALLOW_MISSING=1 platforms without a signature are left out (test manifests).

The app asks https://github.com/redn3/blue-compressor/releases/latest/download/latest.json,
which GitHub serves from the newest published (not draft, not pre-release) release. Each
platform points at the release asset the app can install, with that file's signature.
"""
import datetime
import json
import os
import sys

version, base, sigdir, notes_file, out = sys.argv[1:6]
v = version.lstrip("v")
assets = {
    "darwin-aarch64": f"Blue-Compressor_{v}_macOS_arm64.app.tar.gz",
    "darwin-x86_64": f"Blue-Compressor_{v}_macOS_x64.app.tar.gz",
    "windows-x86_64-nsis": f"Blue-Compressor_{v}_Windows_x64-setup.exe",
    "windows-x86_64-msi": f"Blue-Compressor_{v}_Windows_x64.msi",
    "linux-x86_64-appimage": f"Blue-Compressor_{v}_Linux_x64.AppImage",
}
# The plain {os}-{arch} keys are what the updater falls back to.
aliases = {
    "darwin-aarch64-app": "darwin-aarch64",
    "darwin-x86_64-app": "darwin-x86_64",
    "windows-x86_64": "windows-x86_64-nsis",
    "linux-x86_64": "linux-x86_64-appimage",
}


def first_section(text):
    lines = []
    for i, line in enumerate(text.splitlines()):
        t = line.strip()
        if i > 0 and t.startswith("**What's new"):
            break
        if not t and lines:
            break
        if t:
            lines.append(t)
    return "\n".join(lines)


platforms = {}
missing = []
for key, name in assets.items():
    sig = os.path.join(sigdir, name + ".sig")
    if not os.path.exists(sig):
        missing.append(name + ".sig")
        continue
    with open(sig, encoding="utf-8") as f:
        platforms[key] = {"signature": f.read().strip(), "url": f"{base}/{name}"}
for key, target in aliases.items():
    if target in platforms:
        platforms[key] = platforms[target]
if missing and not os.environ.get("ALLOW_MISSING"):
    sys.exit("missing signatures: " + ", ".join(missing))

with open(notes_file, encoding="utf-8") as f:
    notes = first_section(f.read())
manifest = {
    "version": v,
    "notes": notes,
    "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "platforms": platforms,
}
with open(out, "w", encoding="utf-8") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")
print(f"{out}: {v}, {len(platforms)} platform entries")
