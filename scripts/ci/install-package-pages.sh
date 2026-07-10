#!/usr/bin/env bash
set -euo pipefail

touch public/.nojekyll
if [ ! -f app-icon.png ]; then
  echo "app-icon.png is required for package pages" >&2
  exit 1
fi

mkdir -p public/packages
cp -R packaging/pages/. public/
cp app-icon.png public/packages/icon.png
