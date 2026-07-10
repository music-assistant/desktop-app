#!/usr/bin/env bash
set -euo pipefail

cd public
mkdir -p apt/pool/main apt/dists/stable/main

shopt -s nullglob
debs=(../release-assets/*.deb)
if [ ${#debs[@]} -eq 0 ]; then
  echo "No DEB packages found in release assets" >&2
  exit 1
fi

for deb in "${debs[@]}"; do
  arch=$(dpkg-deb -f "$deb" Architecture)
  mkdir -p "apt/pool/main/$arch" "apt/dists/stable/main/binary-$arch"
  cp "$deb" "apt/pool/main/$arch/"
done

cd apt
for arch_dir in dists/stable/main/binary-*; do
  arch=${arch_dir##*-}
  apt-ftparchive packages "pool/main/$arch" > "$arch_dir/Packages"
  gzip -k -f "$arch_dir/Packages"
done

apt-ftparchive \
  -o APT::FTPArchive::Release::Origin="Music Assistant" \
  -o APT::FTPArchive::Release::Label="Music Assistant" \
  -o APT::FTPArchive::Release::Suite="stable" \
  -o APT::FTPArchive::Release::Codename="stable" \
  -o APT::FTPArchive::Release::Components="main" \
  -o APT::FTPArchive::Release::Architectures="amd64 arm64" \
  release dists/stable > dists/stable/Release

gpg --batch --yes --pinentry-mode loopback --passphrase "$GPG_PASSPHRASE" \
  --local-user "$GPG_KEY_ID" --clearsign \
  --output dists/stable/InRelease dists/stable/Release

gpg --batch --yes --pinentry-mode loopback --passphrase "$GPG_PASSPHRASE" \
  --local-user "$GPG_KEY_ID" --detach-sign --armor \
  --output dists/stable/Release.gpg dists/stable/Release

gpg --batch --armor --export "$GPG_KEY_ID" > music-assistant.asc
