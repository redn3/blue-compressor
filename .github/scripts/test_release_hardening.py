#!/usr/bin/env python3
"""Checks for the 1.0.1 release metadata, the updater manifest, and the release workflow."""
import base64
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / ".github" / "scripts"))

import release_environment  # noqa: E402

MANIFEST = ROOT / ".github" / "scripts" / "updater-manifest.py"
RELEASE = ROOT / ".github" / "workflows" / "release.yml"
TEST_BUILD = ROOT / ".github" / "workflows" / "test-build.yml"
SIGN_SCRIPT = ROOT / ".github" / "scripts" / "test_updater_sign.sh"
TAURI_CLI_SHA256 = "820ae3c7580ca637cad15f65638960ee2a96e5a491e224691c86d6f25e6c9e8a"
VERSION = "1.0.1"


def job_body(text, name):
    lines = text.splitlines()
    start = None
    for i, line in enumerate(lines):
        if line.startswith(f"  {name}:"):
            start = i
            break
    if start is None:
        raise AssertionError(f"missing job {name}")
    body = []
    for line in lines[start + 1 :]:
        if len(line) > 2 and line[2] != " " and line.endswith(":") and not line.startswith("   "):
            break
        body.append(line)
    return "\n".join(body)


class VersionTests(unittest.TestCase):
    def test_version_sources(self):
        cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
        self.assertIn(f'version = "{VERSION}"', cargo)
        conf = json.loads((ROOT / "app" / "src-tauri" / "tauri.conf.json").read_text(encoding="utf-8"))
        self.assertEqual(conf["version"], VERSION)
        self.assertEqual(conf["bundle"]["macOS"]["signingIdentity"], "-")
        pubkey = base64.b64decode(conf["plugins"]["updater"]["pubkey"], validate=True)
        self.assertIn(b"minisign public key", pubkey)
        lock = (ROOT / "Cargo.lock").read_text(encoding="utf-8")
        for name in ("blue-compressor-app", "bluecompress", "bluecompress-cli"):
            self.assertIn(f'name = "{name}"\nversion = "{VERSION}"', lock)
        notes = (ROOT / ".github" / "release-notes.md").read_text(encoding="utf-8")
        self.assertTrue(notes.startswith(f"**What's new in {VERSION}**"))
        readme = (ROOT / "README.md").read_text(encoding="utf-8")
        self.assertIn(f"v{VERSION}", readme)
        self.assertNotIn("v1.0.0", readme)


class ManifestTests(unittest.TestCase):
    def write_sigs(self, directory, present):
        version = "1.0.1"
        names = {
            "darwin-aarch64": f"Blue-Compressor_{version}_macOS_arm64.app.tar.gz",
            "darwin-x86_64": f"Blue-Compressor_{version}_macOS_x64.app.tar.gz",
            "windows-x86_64-nsis": f"Blue-Compressor_{version}_Windows_x64-setup.exe",
            "windows-x86_64-msi": f"Blue-Compressor_{version}_Windows_x64.msi",
            "linux-x86_64-appimage": f"Blue-Compressor_{version}_Linux_x64.AppImage",
        }
        for key in present:
            path = directory / (names[key] + ".sig")
            path.write_text(f"signature-for-{key}\n", encoding="utf-8")
        return names

    def run_manifest(self, sigdir, notes, out, allow_missing=False):
        env = os.environ.copy()
        if allow_missing:
            env["ALLOW_MISSING"] = "1"
        else:
            env.pop("ALLOW_MISSING", None)
        return subprocess.run(
            [
                sys.executable,
                str(MANIFEST),
                "v1.0.1",
                "https://github.com/redn3/blue-compressor/releases/download/v1.0.1",
                str(sigdir),
                str(notes),
                str(out),
            ],
            text=True,
            capture_output=True,
            env=env,
            check=False,
        )

    def test_manifest_maps_aliases_and_notes(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = pathlib.Path(tmp)
            self.write_sigs(
                directory,
                [
                    "darwin-aarch64",
                    "darwin-x86_64",
                    "windows-x86_64-nsis",
                    "windows-x86_64-msi",
                    "linux-x86_64-appimage",
                ],
            )
            notes = directory / "notes.md"
            notes.write_text(
                "**What's new in 1.0.1**\n- One change.\n\n**What's new in 1.0.0**\n- Old.\n",
                encoding="utf-8",
            )
            out = directory / "latest.json"
            result = self.run_manifest(directory, notes, out)
            self.assertEqual(result.returncode, 0, result.stderr)
            data = json.loads(out.read_text(encoding="utf-8"))
            self.assertEqual(data["version"], "1.0.1")
            self.assertEqual(data["notes"], "**What's new in 1.0.1**\n- One change.")
            self.assertRegex(data["pub_date"], r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$")
            self.assertEqual(len(data["platforms"]), 9)
            arm = data["platforms"]["darwin-aarch64"]
            self.assertEqual(arm["signature"], "signature-for-darwin-aarch64")
            self.assertTrue(arm["url"].endswith("/Blue-Compressor_1.0.1_macOS_arm64.app.tar.gz"))
            self.assertEqual(data["platforms"]["darwin-aarch64-app"], arm)
            self.assertEqual(data["platforms"]["windows-x86_64"], data["platforms"]["windows-x86_64-nsis"])
            self.assertEqual(data["platforms"]["linux-x86_64"], data["platforms"]["linux-x86_64-appimage"])

    def test_missing_signature_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = pathlib.Path(tmp)
            self.write_sigs(directory, ["darwin-aarch64"])
            notes = directory / "notes.md"
            notes.write_text("**What's new in 1.0.1**\n- One.\n", encoding="utf-8")
            result = self.run_manifest(directory, notes, directory / "latest.json")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("missing signatures", result.stderr)
            self.assertFalse((directory / "latest.json").exists())

    def test_allow_missing_omits_platforms(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = pathlib.Path(tmp)
            self.write_sigs(directory, ["linux-x86_64-appimage"])
            notes = directory / "notes.md"
            notes.write_text("**What's new in 1.0.1**\n- One.\n", encoding="utf-8")
            out = directory / "latest.json"
            result = self.run_manifest(directory, notes, out, allow_missing=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            data = json.loads(out.read_text(encoding="utf-8"))
            self.assertEqual(set(data["platforms"]), {"linux-x86_64-appimage", "linux-x86_64"})


class WorkflowTests(unittest.TestCase):
    def test_release_jobs_are_separated(self):
        text = RELEASE.read_text(encoding="utf-8")
        self.assertNotIn("Swatinem/rust-cache", text)
        self.assertNotIn("actions/cache@", text)
        self.assertIn(TAURI_CLI_SHA256, text)
        self.assertIn(TAURI_CLI_SHA256, SIGN_SCRIPT.read_text(encoding="utf-8"))
        for name in ("release-guard", "create-release", "build"):
            body = job_body(text, name)
            self.assertNotIn("environment:", body, name)
            self.assertNotIn("secrets.", body, name)
        build = job_body(text, "build")
        self.assertNotIn("TAURI_SIGNING_PRIVATE_KEY: ${{", build)
        self.assertIn("id-token: write", build)
        self.assertIn("attestations: write", build)
        sign = job_body(text, "sign")
        self.assertIn("environment: release", sign)
        self.assertIn("secrets.TAURI_SIGNING_PRIVATE_KEY", sign)
        self.assertIn("secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD", sign)
        self.assertNotIn("actions/checkout", sign)
        self.assertNotIn("cargo tauri build", sign)
        self.assertNotIn("cargo build", sign)
        self.assertNotIn("contents: write", sign)
        publish = job_body(text, "publish")
        self.assertIn("environment: release", publish)
        self.assertNotIn("TAURI_SIGNING", publish)
        self.assertNotIn("secrets.", publish)
        self.assertIn("updater-manifest.py", publish)
        self.assertIn("contents: write", publish)

    def test_test_builds_do_not_receive_the_updater_key(self):
        text = TEST_BUILD.read_text(encoding="utf-8")
        self.assertNotIn("secrets.TAURI_SIGNING_PRIVATE_KEY", text)
        self.assertNotIn("secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD", text)
        self.assertIn('signingIdentity": "-"', (ROOT / "app" / "src-tauri" / "tauri.conf.json").read_text(encoding="utf-8"))


class EnvironmentRuleTests(unittest.TestCase):
    def test_requires_a_named_reviewer_rule(self):
        self.assertFalse(release_environment.reviewers_required({}))
        self.assertFalse(release_environment.reviewers_required({"protection_rules": []}))
        self.assertFalse(
            release_environment.reviewers_required(
                {"protection_rules": [{"type": "wait_timer", "wait_timer": 5}]}
            )
        )
        self.assertFalse(
            release_environment.reviewers_required(
                {"protection_rules": [{"type": "required_reviewers", "reviewers": []}]}
            )
        )
        self.assertTrue(
            release_environment.reviewers_required(
                {
                    "protection_rules": [
                        {
                            "type": "required_reviewers",
                            "reviewers": [{"type": "User", "reviewer": {"login": "reviewer"}}],
                        }
                    ]
                }
            )
        )


if __name__ == "__main__":
    unittest.main()
