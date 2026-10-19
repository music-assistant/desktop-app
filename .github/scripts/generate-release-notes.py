#!/usr/bin/env python3
"""Preview GitHub's release notes with thanks for merged human PR authors."""

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

BOT_LOGINS = {"dependabot", "github-actions", "musicassistant-bot"}
STABLE_TAG = re.compile(r"v?\d+\.\d+\.\d+\Z")
LOGIN = re.compile(r"[A-Za-z0-9-]+\Z")


def run(*command):
    return subprocess.run(command, check=True, capture_output=True, text=True).stdout.strip()


def api_json(*args):
    return json.loads(run("gh", "api", *args))


def api_pages(endpoint):
    page = 1
    while True:
        items = api_json(f"{endpoint}{'&' if '?' in endpoint else '?'}per_page=100&page={page}")
        if not isinstance(items, list):
            raise ValueError(f"Expected a list from {endpoint}")
        yield from items
        if len(items) < 100:
            break
        page += 1


def git(*args):
    return run("git", *args)


def ancestor(older, newer):
    return subprocess.run(
        ["git", "merge-base", "--is-ancestor", older, newer],
        capture_output=True, check=False,
    ).returncode == 0


def resolve_commit(ref):
    return git("rev-parse", "--verify", "--end-of-options", f"{ref}^{{commit}}")


def validate_release_tag(tag, target):
    result = subprocess.run(
        ["git", "rev-parse", "--verify", "--quiet", "--end-of-options", f"refs/tags/{tag}^{{commit}}"],
        capture_output=True, text=True, check=False,
    )
    if result.returncode == 0 and result.stdout.strip() != target:
        raise ValueError("Existing release tag does not match the target commit")
    if result.returncode not in (0, 1):
        result.check_returncode()


def previous_release(repo, target):
    releases = api_pages(f"repos/{repo}/releases")
    candidates = []
    stable_releases = 0
    for release in releases:
        tag = release.get("tag_name", "")
        if release.get("draft") or release.get("prerelease") or not STABLE_TAG.fullmatch(tag):
            continue
        stable_releases += 1
        sha = resolve_commit(tag)
        if sha != target and ancestor(sha, target):
            candidates.append((release.get("published_at") or "", tag, sha))
    if candidates:
        _, tag, sha = max(candidates)
        return tag, sha
    if stable_releases:
        raise ValueError("No published stable release is an ancestor of the target")
    return None, None


def contributors(repo, commits):
    names = {}
    seen_prs = set()
    for sha in sorted(commits):
        for pr in api_pages(f"repos/{repo}/commits/{sha}/pulls"):
            number = pr.get("number")
            if number in seen_prs:
                continue
            seen_prs.add(number)
            base = pr.get("base") or {}
            base_repo = base.get("repo") or {}
            author = pr.get("user") or {}
            login = author.get("login") or ""
            lower = login.lower()
            if (not pr.get("merged_at") or pr.get("merge_commit_sha") not in commits
                    or base_repo.get("full_name", "").lower() != repo.lower()
                    or base.get("ref") != "main" or not LOGIN.fullmatch(login)
                    or author.get("type", "").lower() == "bot"
                    or lower.endswith("[bot]") or lower in BOT_LOGINS):
                continue
            names.setdefault(lower, login)
    return [names[key] for key in sorted(names)]


def generate(repo, tag, target, previous_tag=None, previous_sha=None):
    if not STABLE_TAG.fullmatch(tag):
        raise ValueError("Release tag must be a stable numeric version")
    target_sha = resolve_commit(target)
    validate_release_tag(tag, target_sha)
    if not ancestor(target_sha, "refs/remotes/origin/main"):
        raise ValueError("Target must be reachable from origin/main")
    if previous_tag is None:
        previous_tag, previous_sha = previous_release(repo, target_sha)
    elif previous_tag:
        previous_sha = resolve_commit(previous_tag)
    if previous_sha and not ancestor(previous_sha, target_sha):
        raise ValueError("Previous release must be an ancestor of the target")
    range_spec = f"{previous_sha}..{target_sha}" if previous_sha else target_sha
    commits = set(git("rev-list", range_spec).splitlines())
    authors = contributors(repo, commits)
    args = ["repos/" + repo + "/releases/generate-notes", "--method", "POST",
            "-f", f"tag_name={tag}", "-f", f"target_commitish={target_sha}"]
    if previous_tag:
        args += ["-f", f"previous_tag_name={previous_tag}"]
    body = api_json(*args)["body"].rstrip()
    if authors:
        body += "\n\n## Thanks to our contributors\n\n" + ", ".join(f"@{name}" for name in authors)
    return body + "\n", previous_tag, authors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--previous-tag", help="Override auto-selected previous published stable release")
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo):
        parser.error("Invalid repository")
    try:
        body, previous, authors = generate(args.repo, args.tag, args.target, args.previous_tag)
        Path(args.output).write_text(body, encoding="utf-8")
    except (subprocess.CalledProcessError, ValueError, KeyError, OSError) as error:
        parser.exit(1, f"Could not generate release notes: {error}\n")
    print(f"Preview saved to {args.output}; previous release: {previous or '(first release)'}; "
          f"human PR authors: {len(authors)}")


if __name__ == "__main__":
    main()
