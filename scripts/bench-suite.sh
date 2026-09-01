#!/usr/bin/env bash
# Benchmark the canonical fixture suite.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cache_mode=warm
runs=10
warmups=3
root=

usage() {
	cat <<'EOF'
Usage: scripts/bench-suite.sh [OPTIONS] [DIRECTORY]

Benchmark the Linux, flat-1m, and wide-400k fixtures.

Options:
  --cache MODE   warm, cold, or both (default: warm)
  --runs N       measured runs per fixture (default: 10)
  --warmups N    discarded warm-cache runs per fixture (default: 3)
  -h, --help     show this help
EOF
}

while (($#)); do
	case "$1" in
		--cache)
			[[ $# -ge 2 ]] || { echo "missing value for --cache" >&2; exit 2; }
			cache_mode=$2
			shift 2
			;;
		--runs)
			[[ $# -ge 2 ]] || { echo "missing value for --runs" >&2; exit 2; }
			runs=$2
			shift 2
			;;
		--warmups)
			[[ $# -ge 2 ]] || { echo "missing value for --warmups" >&2; exit 2; }
			warmups=$2
			shift 2
			;;
		-h|--help)
			usage
			exit 0
			;;
		--)
			shift
			[[ $# -eq 1 && -z $root ]] ||
				{ echo "expected one fixture directory" >&2; exit 2; }
			root=$1
			shift
			;;
		-*)
			echo "unknown option: $1" >&2
			exit 2
			;;
		*)
			[[ -z $root ]] || { echo "expected one fixture directory" >&2; exit 2; }
			root=$1
			shift
			;;
	esac
done

case "$cache_mode" in
	warm) modes=(warm) ;;
	cold) modes=(cold) ;;
	both) modes=(warm cold) ;;
	*) echo "--cache must be warm, cold, or both" >&2; exit 2 ;;
esac
root="${root:-$here/bench-data}"

targets=(
	"$here/test-data/linux-7.2.2"
	"$root/flat-1m"
	"$root/wide-400k"
)

for mode in "${modes[@]}"; do
	for target in "${targets[@]}"; do
		[[ -d $target ]] ||
			{ echo "missing fixture: $target (run scripts/make-fixtures.sh)" >&2; exit 1; }
		printf '## %s (%s-cache)\n\n' "$(basename "$target")" "$mode"
		"$here/scripts/bench.sh" --cache "$mode" --runs "$runs" --warmups "$warmups" "$target"
		printf '\n'
	done
done
