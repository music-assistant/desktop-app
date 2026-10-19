"""Offline tests for release-note contributor selection."""

import importlib.util
import subprocess
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("notes", Path(__file__).with_name("generate-release-notes.py"))
notes = importlib.util.module_from_spec(spec)
spec.loader.exec_module(notes)

REPO = "music-assistant/desktop-app"
A = "a" * 40
B = "b" * 40
C = "c" * 40


def pr(number, login, sha=B, *, merged=True, kind="User", base="main", repository=REPO, labels=None):
    return {"number": number, "merged_at": "2026-10-16T00:00:00Z" if merged else None,
            "merge_commit_sha": sha, "user": {"login": login, "type": kind},
            "base": {"ref": base, "repo": {"full_name": repository}},
            "labels": labels or []}


class NotesTests(unittest.TestCase):
    def test_release_tag_must_match_target_when_it_exists(self):
        for code, sha, rejected in [(1, "", False), (0, B, False), (0, A, True)]:
            with self.subTest(code=code, sha=sha), patch.object(
                notes.subprocess, "run",
                return_value=subprocess.CompletedProcess([], code, stdout=sha, stderr=""),
            ):
                if rejected:
                    with self.assertRaisesRegex(ValueError, "does not match"):
                        notes.validate_release_tag("0.7.2", B)
                else:
                    notes.validate_release_tag("0.7.2", B)

    def test_contributors_all_associations_chore_bots_and_strict_merges(self):
        items = [pr(1, "RyanMac8", labels=[{"name": "chore"}]),
                 pr(2, "ryanmac8", sha=C), pr(3, "Other", sha=A),
                 pr(4, "unmerged", merged=False), pr(5, "outside", sha=C),
                 pr(6, "wrong-branch", base="feature"),
                 pr(7, "wrong-repo", repository="elsewhere/desktop-app"),
                 pr(8, "robot", kind="Bot"), pr(9, "dependabot[bot]"),
                 pr(10, "musicassistant-bot"), pr(11, "github-actions"),
                 pr(12, "malicious@mention"), pr(13, "Other", sha=A)]
        with patch.object(notes, "api_pages", side_effect=lambda _: iter(items)) as pages:
            self.assertEqual(notes.contributors(REPO, {A, B}), ["Other", "RyanMac8"])
            self.assertEqual(pages.call_count, 2)

    def test_api_pagination_and_failures_are_not_silent(self):
        for endpoint in ("repos/example/commits/sha/pulls", "repos/example/releases"):
            with self.subTest(endpoint=endpoint), patch.object(
                notes, "api_json", side_effect=[[1] * 100, [2] * 100, [3] * 51]
            ) as api:
                self.assertEqual(len(list(notes.api_pages(endpoint))), 251)
                self.assertEqual(api.call_count, 3)
        with patch.object(notes, "api_json", side_effect=RuntimeError("API failed")):
            with self.assertRaisesRegex(RuntimeError, "API failed"):
                list(notes.api_pages("repos/example/commits/sha/pulls"))

    def test_generation_uses_exact_range_and_same_previous_tag(self):
        commands = []
        def git(*args):
            commands.append(args)
            if args[0] == "rev-parse":
                return {"HEAD^{commit}": B, "0.7.1^{commit}": A}[args[-1]]
            return B + "\n" + C

        def api(*args):
            commands.append(args)
            return {"body": "## What's Changed\n\n* A feature"}

        with patch.object(notes, "git", side_effect=git), \
             patch.object(notes, "ancestor", return_value=True), \
             patch.object(notes, "api_json", side_effect=api), \
             patch.object(notes, "contributors", return_value=["Human"]):
            body, previous, authors = notes.generate(REPO, "0.7.2", "HEAD", "0.7.1")
        self.assertEqual(previous, "0.7.1")
        self.assertEqual(authors, ["Human"])
        self.assertIn(("rev-list", f"{A}..{B}"), commands)
        self.assertIn("-f", commands[-1])
        self.assertIn("previous_tag_name=0.7.1", commands[-1])
        self.assertEqual(body, "## What's Changed\n\n* A feature\n\n## Thanks to our contributors\n\n@Human\n")

    def test_no_authors_omits_section_and_first_release_uses_entire_history(self):
        with patch.object(notes, "resolve_commit", return_value=B), \
             patch.object(notes, "ancestor", return_value=True), \
             patch.object(notes, "previous_release", return_value=(None, None)), \
             patch.object(notes, "git", return_value=A + "\n" + B) as git, \
             patch.object(notes, "contributors", return_value=[]), \
             patch.object(notes, "api_json", return_value={"body": "Original"}) as api:
            body, previous, _ = notes.generate(REPO, "0.7.2", "HEAD")
        self.assertEqual((body, previous), ("Original\n", None))
        git.assert_called_once_with("rev-list", B)
        self.assertNotIn("previous_tag_name", str(api.call_args))

    def test_previous_release_skips_drafts_prereleases_and_nonancestors(self):
        releases = [{"tag_name": "0.7.2", "draft": True},
                    {"tag_name": "0.7.1-rc1"},
                    {"tag_name": "0.7.1", "published_at": "2026-10-15"},
                    {"tag_name": "0.7.0", "published_at": "2026-10-11"}]
        with patch.object(notes, "api_pages", return_value=iter(releases)), \
             patch.object(notes, "resolve_commit", side_effect=lambda tag: {"0.7.1": A, "0.7.0": C}[tag]), \
             patch.object(notes, "ancestor", side_effect=lambda sha, _: sha == A):
            self.assertEqual(notes.previous_release(REPO, B), ("0.7.1", A))

    def test_existing_release_without_ancestor_fails_instead_of_mismatching_notes(self):
        with patch.object(notes, "api_pages", return_value=iter([{"tag_name": "0.7.1"}])), \
             patch.object(notes, "resolve_commit", return_value=A), \
             patch.object(notes, "ancestor", return_value=False):
            with self.assertRaisesRegex(ValueError, "No published stable release"):
                notes.previous_release(REPO, B)


if __name__ == "__main__":
    unittest.main()
