#!/usr/bin/env bash
set -euo pipefail

cd public
mkdir -p rpm/x86_64 rpm/aarch64

shopt -s nullglob
rpms=(../release-assets/*.rpm)
if [ ${#rpms[@]} -eq 0 ]; then
  echo "No RPM packages found in release assets" >&2
  exit 1
fi

cat > ~/.rpmmacros <<RPMMACROS
%_signature gpg
%_gpg_name $GPG_KEY_ID
%__gpg_sign_cmd %{__gpg} --batch --yes --pinentry-mode loopback --passphrase $GPG_PASSPHRASE --no-verbose --no-armor --no-secmem-warning -u "%{_gpg_name}" -sbo %{__signature_filename} %{__plaintext_filename}
RPMMACROS

for rpm in "${rpms[@]}"; do
  rpmsign --addsign "$rpm"
  arch=$(rpm -qp --queryformat '%{ARCH}' "$rpm")
  case "$arch" in
    x86_64) cp "$rpm" rpm/x86_64/ ;;
    aarch64) cp "$rpm" rpm/aarch64/ ;;
    *) echo "Unsupported RPM architecture: $arch" >&2; exit 1 ;;
  esac
done

gpg --batch --armor --export "$GPG_KEY_ID" > rpm/RPM-GPG-KEY-music-assistant
for arch in x86_64 aarch64; do
  createrepo_c "rpm/$arch"
  gpg --batch --yes --pinentry-mode loopback --passphrase "$GPG_PASSPHRASE" \
    --local-user "$GPG_KEY_ID" --detach-sign --armor \
    --output "rpm/$arch/repodata/repomd.xml.asc" \
    "rpm/$arch/repodata/repomd.xml"
done
