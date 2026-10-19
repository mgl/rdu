# rdu

[![CI](https://github.com/mgl/rdu/actions/workflows/ci.yml/badge.svg)](https://github.com/mgl/rdu/actions/workflows/ci.yml)

Parallel disk usage analysis for Linux, written in Rust.

`rdu` combines a metadata-oriented scanner with an interactive terminal browser,
scriptable text output, and [gdu-compatible](https://github.com/dundee/gdu) JSON
reports.

- Parallel traversal with configurable worker limits.
- Allocated and apparent sizes, hard-link deduplication, and filesystem boundaries.
- Path, extension, and modification-time filters.
- Interactive sorting, navigation, and explicitly confirmed batch deletion.
- Static x86-64 and ARM64 binaries..

[Usage](#usage) · [Installation](#installation) · [Scanning](#scanning) ·
[Terminal interface](#terminal-interface) · [Benchmarks](#benchmarks)

![rdu terminal browser showing directory sizes, usage bars, item counts, and navigation controls](assets/screenshot.png)

*Interactive view of a synthetic workspace, using apparent sizes and item counts.*

## Usage

```sh
rdu "$HOME"               # interactive browser
rdu -n --depth 2 /var     # directory totals, two levels deep
rdu -n --top 20 "$HOME"   # largest files across the tree
rdu -n -s .              # one total
rdu -o report.json .     # export a scan
rdu -f report.json       # browse a saved report
```

The default path is `.`. A terminal on stdout selects the interactive browser;
redirected or piped output selects text. Use `-n` to force text output.

Entries are sorted largest-first; `--reverse-sort` reverses the order.
`--summarize`, `--depth`, and `--top` are mutually exclusive layouts.
Run `rdu --help` for the complete option reference.

## Installation

Linux x86-64 and ARM64 are supported. Static binaries run on both glibc- and
musl-based distributions without a Rust toolchain.

### Repository signing key

Verify the APT/RPM signing key before adding either repository:

```text
6EA8 839A 32AB 3FD2 14A1 503E 5499 C701 15C9 2C2E
```

```sh
curl -fsSL https://mgl.github.io/rdu/rdu.asc |
  gpg --show-keys --with-fingerprint
```

### APT (Debian, Ubuntu, Mint)

Add the repository signing key and source once:

```sh
sudo install -d -m 755 /etc/apt/keyrings
curl -fsSL https://mgl.github.io/rdu/rdu.asc |
  sudo tee /etc/apt/keyrings/rdu.asc >/dev/null
sudo chmod 644 /etc/apt/keyrings/rdu.asc
echo "deb [signed-by=/etc/apt/keyrings/rdu.asc] https://mgl.github.io/rdu/apt stable main" |
  sudo tee /etc/apt/sources.list.d/rdu.list >/dev/null
sudo apt update
sudo apt install rdu
```

### DNF (Fedora, Rocky Linux, AlmaLinux)

Add the repository source once:

```sh
sudo tee /etc/yum.repos.d/rdu.repo >/dev/null <<'EOF'
[rdu]
name=rdu
baseurl=https://mgl.github.io/rdu/rpm
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=https://mgl.github.io/rdu/rdu.asc
EOF
sudo dnf install rdu
```

Both repositories integrate with system updates. APT verifies signed repository
metadata; DNF verifies metadata and package signatures.

### Static binaries

Download and verify an archive from the
[latest release](https://github.com/mgl/rdu/releases/latest):

```sh
# Use aarch64-unknown-linux-musl instead on ARM64.
target=x86_64-unknown-linux-musl
base=https://github.com/mgl/rdu/releases/latest/download
curl -fLO "$base/rdu-$target.tar.gz"
curl -fLO "$base/SHA256SUMS"
grep "  rdu-$target.tar.gz$" SHA256SUMS | sha256sum --check --strict -
tar -xzf "rdu-$target.tar.gz" rdu
install -Dm755 rdu "$HOME/.local/bin/rdu"
```

Add `$HOME/.local/bin` to `PATH` if needed.

### Containers

`ghcr.io/mgl/rdu` provides x86-64 and ARM64 images built from `scratch`.
Use `latest` for the current stable release or a version tag such as `0.1.4`.

Mount the scan root at `/data`. A read-only mount prevents host-file deletion:

```sh
docker run --rm -it --read-only --network none --cap-drop ALL \
  --user "$(id -u):$(id -g)" \
  -v "$PWD:/data:ro" ghcr.io/mgl/rdu:latest
```

For text output, omit `-it` and pass CLI options after the image:

```sh
docker run --rm --read-only --network none --cap-drop ALL \
  --user "$(id -u):$(id -g)" \
  -v "$PWD:/data:ro" ghcr.io/mgl/rdu:latest -n -s /data
```

Podman is also supported. On SELinux systems, `:ro,Z` grants access by relabeling
the mounted directory; do not apply it to system directories. Report paths are
container-local; saving JSON reports requires a writable output mount.

### Source

Requires Rust **1.99.1** or newer.

```sh
cargo install --git https://github.com/mgl/rdu.git
```

## Scanning

### Size accounting

By default, sizes reflect allocated filesystem blocks. `-a` selects apparent
size (file length), which can be substantially larger for sparse files.

```sh
rdu -n -a "$HOME"
rdu -n -C "$HOME"          # include the number of items in each directory
rdu -n --si "$HOME"        # decimal kB, MB, GB units
rdu -n -k "$HOME"          # report every value in KiB
rdu -n --no-prefix "$HOME" # report raw bytes
rdu -n -c "$HOME"          # disable color
```

`--no-prefix` conflicts with `--si` and `--show-in-kib`. Color and progress are
enabled only on their respective terminal streams; disable them explicitly with
`--no-color` and `--no-progress`.

### Directory filters

`-i` excludes exact directory paths. Relative paths resolve against the scan
root, not the shell's working directory.

```sh
rdu -i target,node_modules .
rdu -i /var/cache /var
rdu -H "$HOME"                         # skip directories beginning with a dot
```

`-I` matches regular expressions against complete absolute directory paths.
Use `.*` for an arbitrary prefix or suffix.

```sh
rdu -I '.*/(target|node_modules)' .
rdu -I '.*/cache(/.*)?' "$HOME"
```

`-X FILE` loads one regular expression per non-empty line. Whitespace is trimmed;
comments have no special syntax.

```text
.*/target
.*/node_modules
```

```sh
rdu -X .rdu-ignore .
```

### Extension and time filters

`-T` counts only the listed filename extensions, while `-E` excludes them.
Extensions are matched case-insensitively and should be written without a
leading dot. The two modes cannot be combined.

```sh
rdu -T rs,toml .             # only .rs and .toml files
rdu -E jpg,jpeg,png "$HOME"  # everything except these extensions
```

Time filters use file modification times. `--since` and `--until` accept RFC 3339
timestamps or local `YYYY-MM-DD` dates, inclusive of the entire named day.
Relative ages accept compound durations in `y`, `mo`, `w`, `d`, `h`, `m`, and `s`;
a year is 365 days and a month is 30 days.

```sh
rdu --since 2026-01-01 .
rdu --until 2026-08-31T18:00:00+02:00 .
rdu --max-age 7d .           # files modified within the last seven days
rdu --min-age 1y2mo .        # files at least one year and two months old
rdu --max-age 2h30m .
```

Multiple time filters are intersected.

### Filesystem semantics

- Each hard-linked inode is counted once.
- Symlinks are measured as links by default. `-L` measures file-symlink targets;
  directory symlinks are never followed.
- `-x` restricts traversal to the scan root's filesystem.

The scanner uses several workers per available core to overlap metadata lookups.
`-m N` caps workers; `--sequential` disables parallel traversal and can be useful
on rotating disks. These options are mutually exclusive.

```sh
rdu -x /
rdu -m 8 "$HOME"
rdu --sequential /mnt/hdd
```

Unreadable paths and invalid size metadata are flagged and skipped. Allocated
size is never replaced with an apparent-size estimate. An incomplete scan still
produces the available results, then exits with a failure status.

## JSON reports

Export with `-o`; load with `-f` to browse or render without rescanning.

```sh
rdu -o report.json "$HOME"
rdu -f report.json
rdu -n --depth 2 -f report.json
```

`-` selects stdout or stdin:

```sh
rdu -o - "$HOME" >report.json
rdu -n -f - <report.json
```

The format is compatible with [gdu](https://github.com/dundee/gdu).
`--output-attrs` selects a comma-separated subset of `name`, `asize`, `dsize`,
`items`, `mtime`, and `notreg`. `name` is always included; omitted numeric
attributes remain unavailable when loading the report.

```sh
rdu -o sizes.json --output-attrs name,asize,dsize "$HOME"
```

With `-f`, scan filters and scanner options are ignored. Display and output
options still apply. Saved reports are read-only.

## Terminal interface

| Key | Action |
| --- | --- |
| `↑` / `k`, `↓` / `j` | Move the selection |
| `g`, `G` | Jump to the first or last entry |
| `Page Up`, `Page Down` | Move ten entries |
| `→` / `l` / `Enter` | Enter the selected directory |
| `←` / `h` | Return to the parent directory |
| `s`, `n`, `C`, `m` | Sort by size, name, item count, or modification time |
| Repeat a sort key | Reverse that sort order |
| `a` | Toggle apparent size and disk usage |
| `/` | Filter names; `Enter` keeps the filter and `Esc` clears it |
| `d` / `Delete` | Mark or unmark the selected file/folder for permanent deletion |
| `x` | Review and apply marked deletions |
| `u` | Clear all deletion marks |
| `?` | Show help |
| `q`, `Ctrl-C` | Quit; review pending deletions first if any are marked |

### Deletion

`d` marks entries without modifying the filesystem. Marks appear as a red `D`,
including descendants of marked directories. The footer estimates allocated
space without double-counting marked descendants.

Press `x`, or quit with pending marks, to review full paths. Type `DELETE` and
press `Enter` to confirm. `Page Up` / `Page Down` scroll the review; `Esc` cancels
confirmation but retains marks. Use `u`, then `q`, to exit without deleting.

**Deletion is permanent. There is no Trash or undo. Marked directories are
removed recursively, including contents hidden by filters or unreadable during
the scan.**

- Deletion never follows symlinks, crosses filesystems, traverses symlinked
  parents, or accepts marked entries whose identity has changed.
- The scan root cannot be marked. Saved reports cannot modify the filesystem.
- Deletion requires Linux `openat2` (kernel 5.6+) and permission to use it in the
  process sandbox. Unsupported environments fail explicitly when marking.
- Deletion and rescan run in a background worker; further changes are blocked
  until completion. Failures are displayed. Successful entries leave the batch;
  failed and unattempted entries remain marked. Partial deletions are not rolled
  back.

Space estimates use scanned allocated sizes, even in apparent-size mode. They do
not guarantee reclaimed space: hard links, snapshots, shared blocks, filters,
and concurrent filesystem changes affect the result.

## Benchmarks

Scanner-only wall-clock times on 8-core WSL2, with output discarded.
Percentages are relative to `rdu`; results depend on hardware, filesystem,
directory layout, and cache state.

Warm cache (`rdu` v0.1.4): mean of 10 measured runs after 3 warmups.
Linux 7.2.2 has 100,960 entries, `flat-1m` has 1,000,001, and `wide-400k`
has 500,001.

| Tool | Linux 7.2.2 | `flat-1m` | `wide-400k` |
| --- | ---: | ---: | ---: |
| `rdu` | 73.0 ms | 673.7 ms | 397.8 ms |
| `pdu` | 94.6 ms (+30%) | 964.2 ms (+43%) | 481.1 ms (+21%) |
| `diskus` | 94.1 ms (+29%) | 998.7 ms (+48%) | 705.0 ms (+77%) |
| `dust` | 113.5 ms (+55%) | 2.010 s (+198%) | 673.2 ms (+69%) |
| `dua` | 868.7 ms (+1090%) | 12.687 s (+1783%) | 6.732 s (+1592%) |
| `du` | 572.7 ms (+684%) | 3.168 s (+370%) | 5.493 s (+1281%) |
| `ncdu` | 308.4 ms (+322%) | 2.303 s (+242%) | 2.196 s (+452%) |
| `gdu` | 189.7 ms (+160%) | 6.349 s (+843%) | 916.9 ms (+131%) |

Hyperfine flagged a slow first measured run for `dua` on `flat-1m`; it remains
included in the mean.

Historical cold-cache results (not rerun for `rdu` v0.1.4): 3 measured runs with
the page cache dropped before each run.

| Fixture | `rdu` | `pdu` | `diskus` | `dust` |
| --- | ---: | ---: | ---: | ---: |
| Linux 7.2.2 | 223.5 ms | 435.1 ms (+95%) | 311.8 ms (+40%) | 487.8 ms (+118%) |
| `flat-1m` | 3.261 s | 3.580 s (+10%) | 3.748 s (+15%) | 4.711 s (+44%) |
| `wide-400k` | 1.724 s | 4.088 s (+137%) | 2.596 s (+51%) | 4.329 s (+151%) |

Reproduce the warm-cache benchmark suite (skip fixture creation if they already
exist). Hyperfine 2.0 requires an explicit shell for the scripts' redirections:

```sh
./scripts/make-fixtures.sh
mise exec -- bash -c '
  hyperfine() { command hyperfine --shell=sh "$@"; }
  export -f hyperfine
  ./scripts/bench-suite.sh --cache warm
'
```

Cold-cache testing was skipped to avoid clearing the host-wide page cache.
Use `--cache both` instead of `--cache warm` only when that disruption is
acceptable and cache-dropping permissions are available.

## Development

[`rust-toolchain.toml`](rust-toolchain.toml) pins the toolchain and components
used locally and in CI.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
shellcheck scripts/*.sh
bash scripts/test.sh
```

Mise equivalents: `mise run fmt-check`, `mise run lint`, `mise run test`, and
`mise run scripts-check`. CI runs checks separately from release-build benchmarks.

Benchmark scripts rebuild in release mode before measuring.
`scripts/perf.sh --binary FILE` measures an existing binary instead.
[`mise.toml`](mise.toml) pins optional tools, such as `cargo:hyperfine`.
Cold-cache runs drop the host page cache and require passwordless sudo or
`RDU_DROP_CACHES_CMD`.

## License

MIT. See [LICENSE](LICENSE).
