#!/usr/bin/env bash
set -euo pipefail

git fetch origin gh-pages || true
if git rev-parse --verify origin/gh-pages >/dev/null 2>&1; then
  git worktree add public origin/gh-pages
else
  mkdir -p public
  git -C public init
  git -C public remote add origin "$(git remote get-url origin)"
  git -C public checkout -b gh-pages
fi

# APT, RPM, and instruction pages are regenerated on every release. Keep the
# Flatpak OSTree repository so static deltas can be generated across releases.
rm -rf public/apt public/rpm public/packages
mkdir -p public/packages
