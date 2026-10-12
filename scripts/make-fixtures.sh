#!/usr/bin/env bash
# Generate deterministic benchmark fixtures.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
root="${1:-$here/bench-data}"
jobs="${JOBS:-$(nproc)}"

[[ $jobs =~ ^[1-9][0-9]*$ ]] ||
	{ echo "JOBS must be a positive integer" >&2; exit 2; }

for command in awk find mkdir nproc realpath seq touch wc xargs; do
	command -v "$command" >/dev/null 2>&1 ||
		{ echo "required command not found: $command" >&2; exit 1; }
done

root="$(realpath -m -- "$root")"
flat="$root/flat-1m"
wide="$root/wide-400k"

for fixture in "$flat" "$wide"; do
	[[ ! -e $fixture ]] ||
		{ echo "fixture already exists: $fixture (remove it before regenerating)" >&2; exit 1; }
done
mkdir -p "$root"

names() {
	local directory=$1 count=$2 width=$3
	seq 0 "$((count - 1))" |
		awk -v directory="$directory" -v width="$width" \
			'{ printf "%s/file%0*d%c", directory, width, $1, 0 }'
}

echo "creating flat-1m (1,000,000 files)..." >&2
mkdir "$flat"
names "$flat" 1000000 7 | xargs -0 -r -P "$jobs" -n 256 touch --

echo "creating wide-400k (100,000 directories x 4 files)..." >&2
mkdir "$wide"
# Variables in the child script are expanded by sh, not by this shell.
# shellcheck disable=SC2016
seq 0 99999 |
	awk -v directory="$wide" '{ printf "%s/dir%05d%c", directory, $1, 0 }' |
	xargs -0 -r -P "$jobs" -n 32 sh -c '
		for directory do
			mkdir -- "$directory"
			touch -- "$directory/file0" "$directory/file1" "$directory/file2" "$directory/file3"
		done
	' rdu-fixture

flat_files="$(find "$flat" -type f -print | wc -l)"
wide_dirs="$(find "$wide" -mindepth 1 -type d -print | wc -l)"
wide_files="$(find "$wide" -type f -print | wc -l)"
[[ $flat_files -eq 1000000 && $wide_dirs -eq 100000 && $wide_files -eq 400000 ]] ||
	{ echo "fixture verification failed" >&2; exit 1; }

printf 'created %s (%s files) and %s (%s dirs, %s files)\n' \
	"$flat" "$flat_files" "$wide" "$wide_dirs" "$wide_files"
