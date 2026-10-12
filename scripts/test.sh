#!/usr/bin/env bash
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/common.sh
source "$here/scripts/common.sh"

expect_status() {
	local expected=$1 actual=0
	shift
	"$@" >/dev/null 2>&1 || actual=$?
	[[ $actual -eq $expected ]] ||
		{ printf 'expected exit %s, got %s: %s\n' "$expected" "$actual" "$*" >&2; exit 1; }
}

expect_status 0 validate_samples 1 0
expect_status 0 validate_samples 10 3
for runs in 0 -1 01 abc ""; do
	expect_status 2 validate_samples "$runs" 0
done
for warmups in -1 abc ""; do
	expect_status 2 validate_samples 1 "$warmups"
done

for script in bench.sh bench-suite.sh perf.sh; do
	expect_status 0 bash "$here/scripts/$script" --help
	expect_status 2 bash "$here/scripts/$script" --runs 0
	expect_status 2 bash "$here/scripts/$script" --warmups -1
	expect_status 2 bash "$here/scripts/$script" --runs
	expect_status 2 bash "$here/scripts/$script" --unknown
	expect_status 2 bash "$here/scripts/$script" -- path extra
done

# Mock Cargo to check build arguments and failure propagation without compiling.
(
	# Invoked indirectly by build_release.
	# shellcheck disable=SC2329
	cargo() {
		[[ $PWD == "$here" && $# -eq 5 &&
			$1 == build && $2 == --release && $3 == --locked &&
			$4 == --target-dir && $5 == "$here/target" ]]
	}
	expect_status 0 build_release "$here"
	expect_status 1 build_release "$here/Cargo.toml"
	# shellcheck disable=SC2329
	cargo() { return 7; }
	expect_status 7 build_release "$here"
)

echo "script tests passed"
