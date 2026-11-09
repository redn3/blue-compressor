#!/usr/bin/env python3
"""Fail unless the GitHub Environment named release requires a reviewer.

The release workflow runs this before signing or publishing. It does not
print the response body. A missing environment, a missing rule, or an
unreadable rule is a failure.
"""
import json
import os
import sys
import urllib.error
import urllib.request


def reviewers_required(data):
    """True when protection_rules has required_reviewers and at least one reviewer."""
    if not isinstance(data, dict):
        return False
    rules = data.get("protection_rules") or []
    if not isinstance(rules, list):
        return False
    for rule in rules:
        if not isinstance(rule, dict):
            continue
        if rule.get("type") != "required_reviewers":
            continue
        reviewers = rule.get("reviewers") or []
        if isinstance(reviewers, list) and len(reviewers) >= 1:
            return True
    return False


def main():
    repo = os.environ.get("GITHUB_REPOSITORY", "").strip()
    if not repo or "/" not in repo:
        raise SystemExit("GITHUB_REPOSITORY is not set")
    url = f"https://api.github.com/repos/{repo}/environments/release"
    req = urllib.request.Request(
        url,
        headers={
            "Accept": "application/vnd.github+json",
            "User-Agent": "blue-compressor-release",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as response:
            data = json.load(response)
    except urllib.error.HTTPError as exc:
        raise SystemExit(f"release environment is not configured ({exc.code})") from exc
    except urllib.error.URLError as exc:
        raise SystemExit("release environment could not be read") from exc
    if not reviewers_required(data):
        raise SystemExit("release environment must require at least one reviewer")
    print("release environment requires a reviewer")


if __name__ == "__main__":
    sys.exit(main())
