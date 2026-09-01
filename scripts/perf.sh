#!/usr/bin/env bash
# Measure rdu on a real directory.
set -euo pipefail

usage() {
	cat <<'EOF'
Usage: scripts/perf.sh [OPTIONS] [PATH]

Measure rdu on real files. PATH defaults to $HOME.

Options:
  --runs N       measured runs per mode (default: 5)
  --warmups N    discarded warm-cache runs per mode (default: 1)
  --cores LIST   extra summary runs, e.g. 1,2,4,8
  --binary FILE  benchmark an existing release binary
  --output FILE  write the Markdown report to FILE
  --syscalls     count syscalls once per mode with strace
  -h, --help     show this help
EOF
}

target="$HOME"
target_set=false
runs=5
warmups=1
cores=
binary=
output=
trace_syscalls=false

while (($#)); do
	case "$1" in
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
		--cores)
			[[ $# -ge 2 ]] || { echo "missing value for --cores" >&2; exit 2; }
			cores=$2
			shift 2
			;;
		--binary)
			[[ $# -ge 2 ]] || { echo "missing value for --binary" >&2; exit 2; }
			binary=$2
			shift 2
			;;
		--output)
			[[ $# -ge 2 ]] || { echo "missing value for --output" >&2; exit 2; }
			output=$2
			shift 2
			;;
		--syscalls)
			trace_syscalls=true
			shift
			;;
		-h|--help)
			usage
			exit 0
			;;
		--)
			shift
			[[ $# -eq 1 && $target_set == false ]] ||
				{ echo "expected one PATH" >&2; exit 2; }
			target=$1
			target_set=true
			shift
			;;
		-*)
			echo "unknown option: $1" >&2
			exit 2
			;;
		*)
			[[ $target_set == false ]] ||
				{ echo "expected one PATH" >&2; exit 2; }
			target=$1
			target_set=true
			shift
			;;
	esac
done

[[ $runs =~ ^[1-9][0-9]*$ ]] ||
	{ echo "--runs must be a positive integer" >&2; exit 2; }
[[ $warmups =~ ^[0-9]+$ ]] ||
	{ echo "--warmups must be a non-negative integer" >&2; exit 2; }

IFS=',' read -r -a core_list <<< "$cores"
for core in "${core_list[@]}"; do
	[[ -z $core || $core =~ ^[1-9][0-9]*$ ]] ||
		{ echo "--cores must be a comma-separated list of positive integers" >&2; exit 2; }
done

command -v /usr/bin/time >/dev/null 2>&1 ||
	{ echo "rdu perf: /usr/bin/time is required" >&2; exit 1; }
command -v realpath >/dev/null 2>&1 ||
	{ echo "rdu perf: realpath is required" >&2; exit 1; }
if [[ $trace_syscalls == true ]]; then
	command -v strace >/dev/null 2>&1 ||
		{ echo "rdu perf: strace is required with --syscalls" >&2; exit 1; }
fi

target="$(realpath -e -- "$target")" ||
	{ echo "rdu perf: cannot resolve target: $target" >&2; exit 1; }
[[ -d $target ]] ||
	{ echo "rdu perf: target is not a directory: $target" >&2; exit 1; }
case "$target" in
	/mnt|/mnt/*)
		echo "rdu perf: refusing to scan /mnt (WSL Windows mount)" >&2
		exit 2
		;;
esac
ignore_mnt_args=()
ignored_mnt=none
find_args=(-xdev)
if [[ "$target" == "/" ]]; then
	ignore_mnt_args=(--ignore-dirs /mnt)
	ignored_mnt=/mnt
	find_args+=(-path /mnt -prune -o)
fi

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ -n $binary ]]; then
	rdu="$(realpath -e -- "$binary")" ||
		{ echo "rdu perf: cannot resolve binary: $binary" >&2; exit 1; }
else
	rdu="$here/target/release/rdu"
	if [[ ! -x $rdu ]]; then
		echo "building the release binary first..." >&2
		cargo build --release --manifest-path "$here/Cargo.toml"
	fi
fi
[[ -x $rdu ]] || { echo "rdu perf: binary is not executable: $rdu" >&2; exit 1; }

tmp="$(mktemp -d "${TMPDIR:-/tmp}/rdu-perf.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
# Count the dataset once for untimed throughput numbers.
if find "$target" "${find_args[@]}" -mindepth 1 -printf '%y\n' \
	>"$tmp/find-types" 2>"$tmp/find-errors"; then
	:
else
	echo "warning: entry inventory is incomplete; throughput is a lower bound" >&2
fi
if [[ -s "$tmp/find-errors" ]]; then
	error_count="$(wc -l < "$tmp/find-errors")"
	printf 'warning: find reported %s unreadable paths; showing the first 10\n' \
		"$error_count" >&2
	awk 'NR <= 10 { print }' "$tmp/find-errors" >&2
fi
IFS=$'\t' read -r file_count dir_count entry_count < <(
	awk -F '\n' '
		{
			entries++
			if ($1 == "f") files++
			if ($1 == "d") dirs++
		}
		END { printf "%d\t%d\t%d\n", files, dirs, entries }
	' "$tmp/find-types"
)
results="$tmp/results.tsv"
summary="$tmp/summary.tsv"
: > "$results"
: > "$summary"

run_mode() {
	local label=$1
	shift
	local -a args=(--no-progress --no-color --non-interactive --no-cross "${ignore_mnt_args[@]}" "$@")
	local phase sample start_ns end_ns elapsed_ns user system rss status count syscalls=-

	for phase in warmup measured; do
		if [[ $phase == warmup ]]; then
			count=$warmups
		else
			count=$runs
		fi
		for ((i = 1; i <= count; i++)); do
			sample="$tmp/time"
			start_ns=$(date +%s%N)
			if /usr/bin/time -f '%e	%U	%S	%M' -o "$sample" \
				"$rdu" "${args[@]}" "$target" >/dev/null 2>"$tmp/stderr"; then
				:
			else
				status=$?
				cat "$tmp/stderr" >&2
				echo "rdu perf: $label failed (exit $status)" >&2
				return "$status"
			fi
			end_ns=$(date +%s%N)
			elapsed_ns=$((end_ns - start_ns))
			IFS=$'\t' read -r _ user system rss < "$sample"
			[[ -n ${rss:-} ]] ||
				{ echo "rdu perf: invalid time output for $label" >&2; return 1; }
			if [[ $phase == measured ]]; then
				printf '%s\t%s\t%s\t%s\t%s\n' \
					"$label" "$elapsed_ns" "$user" "$system" "$rss" >> "$results"
			fi
		done
	done

	if [[ $trace_syscalls == true ]]; then
		strace -f -qq -c -o "$tmp/strace" \
			"$rdu" "${args[@]}" "$target" >/dev/null 2>"$tmp/strace-stderr" ||
			{ cat "$tmp/strace-stderr" >&2; return 1; }
		syscalls="$(awk '$NF == "total" { print $4 }' "$tmp/strace")"
	fi

	awk -F '\t' -v label="$label" -v files="$file_count" \
		-v dirs="$dir_count" -v entries="$entry_count" -v syscalls="$syscalls" '
		{
			if ($1 != label) next
			wall = $2 / 1000000000
			n++
			walls[n] = wall
			elapsed += wall
			user += $3
			sys_time += $4
			if (n == 1 || $5 > max_rss) max_rss = $5
			if (n == 1 || wall < min_elapsed) min_elapsed = wall
			if (n == 1 || wall > max_elapsed) max_elapsed = wall
		}
		END {
			mean = elapsed / n
			for (i = 2; i <= n; i++) {
				value = walls[i]
				j = i - 1
				while (j && walls[j] > value) {
					walls[j + 1] = walls[j]
					j--
				}
				walls[j + 1] = value
			}
			median = (n % 2) ? walls[(n + 1) / 2] : (walls[n / 2] + walls[n / 2 + 1]) / 2
			if (elapsed > 0) {
				cpu = 100 * (user + sys_time) / elapsed
				file_throughput = files / median
				dir_throughput = dirs / median
				entry_throughput = entries / median
			} else {
				cpu = 0
				file_throughput = 0
				dir_throughput = 0
				entry_throughput = 0
			}
			printf "%s\t%.3f\t%.3f\t%.3f\t%.3f\t%.3f\t%.3f\t%.1f\t%.1f\t%.0f\t%.0f\t%.0f\t%s\n",
				label, median, mean, min_elapsed, max_elapsed, user / n, sys_time / n,
				cpu, max_rss / 1024, file_throughput, dir_throughput,
				entry_throughput, syscalls
		}
	' "$results" >> "$summary"
}

run_mode summary-parallel --summarize
run_mode summary-sequential --summarize --sequential
run_mode full-tree-parallel --top 0
for core in "${core_list[@]}"; do
	[[ -n $core ]] && run_mode "summary-${core}-cores" --summarize --max-cores "$core"
done

if [[ -n $output ]]; then
	exec > "$output"
fi

cat <<EOF
# rdu performance

- target: \`$target\`
- ignored: \`$ignored_mnt\`
- regular files: $file_count
- directories (excluding root): $dir_count
- entries (excluding root): $entry_count
- measured runs per mode: $runs
- discarded warmups per mode: $warmups
- host cores: $(nproc 2>/dev/null || printf 'unknown')

| Mode | Median wall (s) | Mean wall (s) | Min (s) | Max (s) | User CPU (s) | System CPU (s) | CPU use | Max RSS (MiB) | Files/s | Dirs/s | Entries/s | Syscalls |
|:--|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
EOF

while IFS=$'\t' read -r label median mean min max user system cpu rss files_per_second dirs_per_second entries_per_second syscalls; do
	printf '| `%s` | %.3f | %.3f | %.3f | %.3f | %.3f | %.3f | %.1f%% | %.1f | %.0f | %.0f | %.0f | %s |\n' \
		"$label" "$median" "$mean" "$min" "$max" "$user" "$system" "$cpu" "$rss" \
		"$files_per_second" "$dirs_per_second" "$entries_per_second" "$syscalls"
done < "$summary"
