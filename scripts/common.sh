#!/usr/bin/env bash

validate_samples() {
	[[ $1 =~ ^[1-9][0-9]*$ ]] ||
		{ echo "--runs must be a positive integer" >&2; return 2; }
	[[ $2 =~ ^[0-9]+$ ]] ||
		{ echo "--warmups must be a non-negative integer" >&2; return 2; }
}

build_release() {
	local root=$1
	(
		cd "$root" || return
		cargo build --release --locked --target-dir "$root/target"
	)
}
