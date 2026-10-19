#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

if [[ $# -ne 2 ]]; then
	echo "Usage: $0 DIST_DIRECTORY REPOSITORY_DIRECTORY" >&2
	exit 2
fi
dist="$(realpath "$1")"
repo="$(realpath "$2")"
work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT

gpg --batch --dearmor --output "$work/keyring.gpg" "$repo/rdu.asc"
gpgv --keyring "$work/keyring.gpg" "$repo/apt/dists/stable/InRelease"
gpgv --keyring "$work/keyring.gpg" "$repo/apt/dists/stable/Release.gpg" \
	"$repo/apt/dists/stable/Release"
gpgv --keyring "$work/keyring.gpg" "$repo/rpm/repodata/repomd.xml.asc" \
	"$repo/rpm/repodata/repomd.xml"

mkdir -p "$work/rpmdb"
rpmkeys --dbpath "$work/rpmdb" --import "$repo/rdu.asc"
for package in "$dist/"*.rpm; do
	rpmkeys --dbpath "$work/rpmdb" --checksig --verbose "$package" |
		grep -E 'Signature.*: OK'
	[[ "$(rpm --dbpath "$work/rpmdb" -qp --qf '%{NAME}' "$package")" == rdu ]]
	rpm --dbpath "$work/rpmdb" -qpl "$package" | grep -Fx /usr/bin/rdu
done
for package in "$dist/"*.deb; do
	[[ "$(dpkg-deb --field "$package" Package)" == rdu ]]
	dpkg-deb --extract "$package" "$work/extracted"
	test -x "$work/extracted/usr/bin/rdu"
	cmp "$work/extracted/usr/share/licenses/rdu/LICENSE" \
		"$(dirname "${BASH_SOURCE[0]}")/../LICENSE"
done

# Ask APT to verify the signed metadata and download each architecture's package.
chmod 755 "$work"
for arch in amd64 arm64; do
	state="$work/$arch"
	mkdir -p "$state/lists/partial" "$state/cache/archives/partial" "$state/download"
	printf 'deb [arch=%s signed-by=%s] file:%s/apt stable main\n' \
		"$arch" "$work/keyring.gpg" "$repo" > "$state/sources.list"
	options=(
		-o "Dir::Etc::sourcelist=$state/sources.list"
		-o Dir::Etc::sourceparts=-
		-o "Dir::State=$state"
		-o "Dir::State::status=/dev/null"
		-o "Dir::Cache=$state/cache"
		-o "APT::Architecture=$arch"
		-o "APT::Architectures::=$arch"
		-o APT::Sandbox::User=root
		-o APT::Get::List-Cleanup=0
	)
	apt-get "${options[@]}" update --error-on=any
	(cd "$state/download" && apt-get "${options[@]}" download "rdu:$arch")
done
echo "package repository tests passed"
