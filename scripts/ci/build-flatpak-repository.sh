#!/usr/bin/env bash
set -euo pipefail

cd public
mkdir -p flatpak
if [ ! -d flatpak/repo/objects ]; then
  rm -rf flatpak/repo
  ostree init --mode=archive-z2 --repo=flatpak/repo
fi

shopt -s nullglob
bundles=(../release-assets/*.flatpak)
if [ ${#bundles[@]} -eq 0 ]; then
  echo "No Flatpak bundles found in release assets" >&2
  exit 1
fi

for bundle in "${bundles[@]}"; do
  flatpak build-import-bundle --gpg-sign="$GPG_KEY_ID" flatpak/repo "$bundle"
done
flatpak build-update-repo --gpg-sign="$GPG_KEY_ID" --generate-static-deltas flatpak/repo

gpg_key=$(gpg --batch --export "$GPG_KEY_ID" | base64 -w0)
cat > flatpak/music-assistant.flatpakrepo <<FLATPAKREPO
[Flatpak Repo]
Title=Music Assistant
Url=$PAGES_BASE_URL/flatpak/repo
Homepage=https://www.music-assistant.io/
Comment=Music Assistant Companion Flatpak repository
Description=Music Assistant Companion Flatpak repository
Icon=$PAGES_BASE_URL/packages/icon.png
GPGKey=$gpg_key
FLATPAKREPO

cat > flatpak/io.music_assistant.Companion.flatpakref <<FLATPAKREF
[Flatpak Ref]
Title=Music Assistant Companion
Name=io.music_assistant.Companion
Branch=stable
Url=$PAGES_BASE_URL/flatpak/repo
SuggestRemoteName=music-assistant
RuntimeRepo=https://dl.flathub.org/repo/flathub.flatpakrepo
IsRuntime=false
GPGKey=$gpg_key
FLATPAKREF
