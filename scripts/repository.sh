#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
	echo "Usage: $0 DIST_DIRECTORY REPOSITORY_DIRECTORY SIGNING_FINGERPRINT" >&2
	exit 2
fi
dist="$(realpath "$1")"
mkdir -p "$2"
repo="$(realpath "$2")"
fingerprint="$3"

mkdir -p "$repo/apt/pool/main" "$repo/rpm"
cp "$dist/"*.deb "$repo/apt/pool/main/"
cp "$dist/"*.rpm "$repo/rpm/"
gpg --batch --armor --export "$fingerprint" > "$repo/rdu.asc"
[[ -s "$repo/rdu.asc" ]]

cd "$repo/apt"
for arch in amd64 arm64; do
	index="dists/stable/main/binary-$arch"
	mkdir -p "$index"
	apt-ftparchive --arch "$arch" packages pool > "$index/Packages"
	gzip -n -9 -c "$index/Packages" > "$index/Packages.gz"
done
# Do not include the previous Release files in their own checksums.
rm -f dists/stable/Release dists/stable/InRelease dists/stable/Release.gpg
apt-ftparchive \
	-o APT::FTPArchive::Release::Origin=rdu \
	-o APT::FTPArchive::Release::Label=rdu \
	-o APT::FTPArchive::Release::Suite=stable \
	-o APT::FTPArchive::Release::Codename=stable \
	-o APT::FTPArchive::Release::Architectures="amd64 arm64" \
	-o APT::FTPArchive::Release::Components=main \
	release dists/stable > "$repo/apt/Release.new"
mv "$repo/apt/Release.new" dists/stable/Release
gpg --batch --yes --pinentry-mode loopback --passphrase-fd 3 \
	--local-user "$fingerprint" --digest-algo SHA256 --clearsign \
	--output dists/stable/InRelease dists/stable/Release \
	3<<< "${SIGNING_PASSPHRASE:-}"
gpg --batch --yes --pinentry-mode loopback --passphrase-fd 3 \
	--local-user "$fingerprint" --digest-algo SHA256 --armor --detach-sign \
	--output dists/stable/Release.gpg dists/stable/Release \
	3<<< "${SIGNING_PASSPHRASE:-}"

cd "$repo"
createrepo_c --checksum sha256 rpm
gpg --batch --yes --pinentry-mode loopback --passphrase-fd 3 \
	--local-user "$fingerprint" --digest-algo SHA256 --armor --detach-sign \
	--output rpm/repodata/repomd.xml.asc rpm/repodata/repomd.xml \
	3<<< "${SIGNING_PASSPHRASE:-}"
touch .nojekyll
