#!/usr/bin/env bash
# Benchmark rdu against other disk-usage tools.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/common.sh
source "$here/scripts/common.sh"

usage() {
	cat <<'EOF'
Usage: scripts/bench.sh [OPTIONS] [PATH]

Benchmark PATH (default: $HOME).

Options:
  --cache MODE   warm or cold (default: warm)
  --runs N       measured runs (default: 10)
  --warmups N    discarded warm-cache runs (default: 3)
  -h, --help     show this help
EOF
}

cache_mode=warm
runs=10
warmups=3
target=
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
			[[ $# -eq 1 && -z $target ]] ||
				{ echo "expected one PATH" >&2; exit 2; }
			target=$1
			shift
			;;
		-*)
			echo "unknown option: $1" >&2
			exit 2
			;;
		*)
			[[ -z $target ]] || { echo "expected one PATH" >&2; exit 2; }
			target=$1
			shift
			;;
	esac
done

target="${target:-$HOME}"
[[ $cache_mode == warm || $cache_mode == cold ]] ||
	{ echo "--cache must be warm or cold" >&2; exit 2; }
validate_samples "$runs" "$warmups"

target="$(realpath -e -- "$target")"
case "$target" in
	/mnt|/mnt/*)
		echo "refusing to scan /mnt (WSL Windows mount)" >&2
		exit 2
		;;
esac
rdu="$here/target/release/rdu"

shell_quote() {
	printf "'%s'" "${1//\'/\'\\\'\'}"
}

target_arg="$(shell_quote "$target")"
rdu_command="$(shell_quote "$rdu") -npsc"
if [[ "$target" == "/" ]]; then
	rdu_command+=" --ignore-dirs /mnt"
fi

if ! command -v hyperfine >/dev/null 2>&1; then
	echo "hyperfine is required: mise install cargo:hyperfine" >&2
	exit 1
fi

build_release "$here"

# Skip competitors that are not installed.
commands=(-n "rdu" "$rdu_command $target_arg >/dev/null")
if [[ "$target" == "/" ]]; then
	echo "skipping competitors for /: they cannot be told to ignore /mnt" >&2
else
	for name in diskus dua du dust pdu ncdu gdu; do
		if command -v "$name" >/dev/null 2>&1; then
			cmd="$(shell_quote "$name")"
			case "$name" in
				du) cmd+=" -sx" ;;
				dua) cmd+=" aggregate --no-sort" ;;
				dust) cmd+=" -d0" ;;
				pdu) cmd+=" --threads=max --max-depth=1 --no-sort --silent-errors" ;;
				ncdu) cmd+=" -0 -o /dev/null" ;;
				gdu) cmd+=" -n -p -s" ;;
			esac
			commands+=(-n "$name" "$cmd $target_arg >/dev/null")
		else
			echo "skipping $name (not installed)" >&2
		fi
	done
fi

echo "$cache_mode-cache scan of $target" >&2
hyperfine_args=(--warmup "$warmups" --runs "$runs" --ignore-failure --export-markdown -)
if [[ $cache_mode == cold ]]; then
	echo "cold-cache scan: dropping the host page cache before every sample" >&2
	if [[ -n ${RDU_DROP_CACHES_CMD:-} ]]; then
		prepare=$RDU_DROP_CACHES_CMD
	elif sudo -n true >/dev/null 2>&1; then
		prepare="sudo -n sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'"
	else
		echo "cold-cache scan requires passwordless sudo or RDU_DROP_CACHES_CMD" >&2
		exit 1
	fi
	hyperfine_args+=(--prepare "$prepare")
fi
hyperfine "${hyperfine_args[@]}" "${commands[@]}"
