#!/usr/bin/env bash
# Signs a disposable fixture with the pinned Tauri CLI and checks the updater manifest.
# The key is created here and deleted with the temp directory. It is not a release key.
set -euo pipefail

root=$(cd -- "$(dirname -- "$0")/../.." && pwd)
hash="820ae3c7580ca637cad15f65638960ee2a96e5a491e224691c86d6f25e6c9e8a"
url="https://github.com/tauri-apps/tauri/releases/download/tauri-cli-v2.12.4/cargo-tauri-x86_64-unknown-linux-gnu.tgz"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

curl -fsSL -o "$work/cargo-tauri.tgz" "$url"
echo "$hash  $work/cargo-tauri.tgz" | sha256sum -c -
if tar -tzf "$work/cargo-tauri.tgz" | grep -E '(^/|(^|/)\.\.(/|$))'; then
  echo "refusing to extract the Tauri CLI archive"
  exit 1
fi
mkdir -p "$work/cli"
tar -xzf "$work/cargo-tauri.tgz" -C "$work/cli"
cli="$work/cli/cargo-tauri"
chmod +x "$cli"
test "$("$cli" --version)" = "tauri-cli 2.12.4"

key="$work/disposable.key"
"$cli" signer generate --ci --password 'disposable' --force --write-keys "$key" >/dev/null
test -f "$key"
test -f "${key}.pub"

# A file that would do something if it were executed. Signing must only read it.
marker="$work/executed"
fixture="$work/Blue-Compressor_1.0.1_macOS_arm64.app.tar.gz"
printf '#!/bin/sh\ntouch %s\n' "$marker" >"$fixture"
chmod +x "$fixture"
TAURI_SIGNING_PRIVATE_KEY="$(cat "$key")" \
  TAURI_SIGNING_PRIVATE_KEY_PASSWORD=disposable \
  "$cli" signer sign --app-version 1.0.1 "$fixture" >/dev/null
test ! -e "$marker"
sig="${fixture}.sig"
test -f "$sig"
python3 - "$sig" <<'PY'
import base64, pathlib, sys
text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").strip()
raw = base64.b64decode(text, validate=True)
if not raw.startswith(b"untrusted comment:"):
    raise SystemExit("signature is not a Tauri minisign box")
if b"\tversion:1.0.1" not in raw and b"version:1.0.1" not in raw:
    raise SystemExit("signature is not bound to 1.0.1")
PY

pubkey_box=$(mktemp)
base64 -d "${key}.pub" >"$pubkey_box"
minisign_sig=$(mktemp)
base64 -d "$sig" >"$minisign_sig"
minisign -V -p "$pubkey_box" -x "$minisign_sig" -m "$fixture" >/dev/null

# A second disposable key must not verify the same file.
other="$work/other.key"
"$cli" signer generate --ci --password 'disposable' --force --write-keys "$other" >/dev/null
base64 -d "${other}.pub" >"$work/other.pub"
if minisign -V -p "$work/other.pub" -x "$minisign_sig" -m "$fixture" >/dev/null 2>&1; then
  echo "a different key verified the signature"
  exit 1
fi

# Build a full manifest from signatures of the same shape as a release.
sigdir="$work/sigs"
mkdir -p "$sigdir"
ver=1.0.1
names=(
  "Blue-Compressor_${ver}_macOS_arm64.app.tar.gz"
  "Blue-Compressor_${ver}_macOS_x64.app.tar.gz"
  "Blue-Compressor_${ver}_Windows_x64-setup.exe"
  "Blue-Compressor_${ver}_Windows_x64.msi"
  "Blue-Compressor_${ver}_Linux_x64.AppImage"
)
for name in "${names[@]}"; do
  path="$work/$name"
  printf 'fixture %s\n' "$name" >"$path"
  TAURI_SIGNING_PRIVATE_KEY="$(cat "$key")" \
    TAURI_SIGNING_PRIVATE_KEY_PASSWORD=disposable \
    "$cli" signer sign --app-version "$ver" "$path" >/dev/null
  mv "${path}.sig" "$sigdir/${name}.sig"
done
notes="$work/notes.md"
cp "$root/.github/release-notes.md" "$notes"
out="$work/latest.json"
python3 "$root/.github/scripts/updater-manifest.py" "v$ver" \
  "https://github.com/redn3/blue-compressor/releases/download/v$ver" \
  "$sigdir" "$notes" "$out" >/dev/null
python3 - "$out" "$sigdir" <<'PY'
import base64, json, pathlib, sys
manifest = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
sigdir = pathlib.Path(sys.argv[2])
if manifest["version"] != "1.0.1":
    raise SystemExit("manifest version")
if not manifest["notes"].startswith("**What's new in 1.0.1**"):
    raise SystemExit("manifest notes")
arm = manifest["platforms"]["darwin-aarch64"]
sig = (sigdir / "Blue-Compressor_1.0.1_macOS_arm64.app.tar.gz.sig").read_text(encoding="utf-8").strip()
if arm["signature"] != sig:
    raise SystemExit("manifest signature does not match the signed file")
if not arm["url"].endswith("/Blue-Compressor_1.0.1_macOS_arm64.app.tar.gz"):
    raise SystemExit("manifest url")
raw = base64.b64decode(sig, validate=True)
if b"version:1.0.1" not in raw:
    raise SystemExit("manifest signature is not bound to 1.0.1")
print("updater signing fixture ok")
PY
