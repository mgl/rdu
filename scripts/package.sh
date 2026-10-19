#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
	echo "Usage: $0 VERSION DIST_DIRECTORY" >&2
	exit 2
fi
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PACKAGE_VERSION="${1#v}"
dist="$(realpath "$2")"
export RPM_SIGNING_KEY_FILE="${RPM_SIGNING_KEY_FILE:-}"
work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT

for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
	case "$target" in
		x86_64-*) export PACKAGE_ARCH=amd64 ;;
		aarch64-*) export PACKAGE_ARCH=arm64 ;;
	esac
	export PACKAGE_ROOT="$work/$target"
	mkdir -p "$PACKAGE_ROOT"
	tar -xzf "$dist/rdu-$target.tar.gz" -C "$PACKAGE_ROOT" rdu LICENSE README.md
	for format in deb rpm; do
		nfpm package --config "$here/.github/nfpm.yaml" \
			--packager "$format" --target "$dist/"
	done
done
