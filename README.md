# rdu

[![CI](https://github.com/mgl/rdu/actions/workflows/ci.yml/badge.svg)](https://github.com/mgl/rdu/actions/workflows/ci.yml)

Fast disk usage analyzer for Linux.

`rdu` scans directory trees in parallel and displays the results in an
interactive terminal browser or as text and JSON. JSON reports are compatible
with [gdu](https://github.com/dundee/gdu).

## Installation

### Prebuilt Linux binaries

Download a static binary from the
[latest release](https://github.com/mgl/rdu/releases/latest). No Rust installation
is needed. Choose `x86_64` for most Intel/AMD machines or `aarch64` for ARM64.

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

Ensure `$HOME/.local/bin` is on your `PATH`, or run
`"$HOME/.local/bin/rdu"` directly. These binaries support both glibc- and
musl-based Linux distributions.

### Build from source

Requires Rust 1.99.1 or later.

```sh
cargo install --git https://github.com/mgl/rdu.git
```

## Quick start

Scan the current directory:

```sh
rdu
```

Pass a path to scan somewhere else:

```sh
rdu "$HOME"
```

When standard output is a terminal, `rdu` opens the interactive browser. It
prints text instead when output is redirected or piped. Use `-n` to force text
output in a terminal.

```sh
rdu -n "$HOME"             # list the path's direct children
rdu -n -s "$HOME"          # print one total
rdu -n --depth 2 /var      # show two directory levels
rdu -n --top 20 "$HOME"    # show the 20 largest files anywhere below the path
rdu -n "$HOME" | head     # piping also selects text output
```

`--summarize`, `--depth`, and `--top` select different output layouts and
cannot be combined. Entries are largest-first by default; `--reverse-sort`
lists them smallest-first.

## Sizes and display

By default, `rdu` reports disk usage: the storage blocks allocated by the
filesystem. Use `-a` to report apparent size (the file length) instead. Sparse
files can have a much larger apparent size than disk usage.

```sh
rdu -n -a "$HOME"
rdu -n -C "$HOME"          # include the number of items in each directory
rdu -n --si "$HOME"        # decimal kB, MB, GB units
rdu -n -k "$HOME"          # report every value in KiB
rdu -n --no-prefix "$HOME" # report raw bytes
rdu -n -c "$HOME"          # disable color
```

`--no-prefix` cannot be combined with `--si` or `--show-in-kib`. Color and scan
progress are enabled only when their respective output streams are terminals;
`--no-color` and `--no-progress` disable them explicitly.

## Filtering

### Directories

Skip exact directory paths with `-i`. Relative paths are resolved from the
directory being scanned, not from the shell's current directory.

```sh
rdu -i target,node_modules .
rdu -i /var/cache /var
rdu -H "$HOME"                         # skip directories beginning with a dot
```

Use `-I` for regular expressions. A pattern is matched against the complete
absolute directory path, so include `.*` when matching an arbitrary prefix or
suffix.

```sh
rdu -I '.*/(target|node_modules)' .
rdu -I '.*/cache(/.*)?' "$HOME"
```

`-X FILE` reads one regular expression per non-empty line. Lines are trimmed;
there is no special comment syntax.

```text
.*/target
.*/node_modules
```

```sh
rdu -X .rdu-ignore .
```

### File types and modification time

`-T` counts only the listed filename extensions, while `-E` excludes them.
Extensions are matched case-insensitively and should be written without a
leading dot. The two modes cannot be combined.

```sh
rdu -T rs,toml .             # only .rs and .toml files
rdu -E jpg,jpeg,png "$HOME"  # everything except these extensions
```

Time filters apply to file modification times. `--since` and `--until` accept
an RFC 3339 timestamp or a local `YYYY-MM-DD` date; date bounds include the
whole named day. Relative age filters accept compound durations using `y`,
`mo`, `w`, `d`, `h`, `m`, and `s`. A year is 365 days and a month is 30 days.

```sh
rdu --since 2026-01-01 .
rdu --until 2026-08-31T18:00:00+02:00 .
rdu --max-age 7d .           # files modified within the last seven days
rdu --min-age 1y2mo .        # files at least one year and two months old
rdu --max-age 2h30m .
```

Multiple time options form an intersection, allowing an absolute or relative
range to be narrowed further.

## Filesystems and parallelism

`rdu` counts each hard-linked inode once. Symlinks are measured as links by
default. `-L` reports the target size of file symlinks, while directory
symlinks are never followed. Use `-x` to stay on the filesystem where the scan
started.

The scanner normally uses several workers per available core to keep metadata
lookups in flight. Limit it with `-m N`, or use `--sequential` for rotating
disks. These two options cannot be combined.

```sh
rdu -x /
rdu -m 8 "$HOME"
rdu --sequential /mnt/hdd
```

Unreadable paths and files with invalid size metadata (such as allocated blocks
exceeding filesystem capacity on some Windows mounts) are marked in the results
and skipped. No apparent-size estimate is substituted for invalid disk usage.
After producing the available output, `rdu` reports that the scan was incomplete
and exits with a failure status.

## Saved reports

Write the complete scan as JSON with `-o`, then reopen it later in the browser
or render it as text without rescanning.

```sh
rdu -o report.json "$HOME"
rdu -f report.json
rdu -n --depth 2 -f report.json
```

Use `-` to stream a report through standard output or standard input:

```sh
rdu -o - "$HOME" >report.json
rdu -n -f - <report.json
```

Reports use the same format as [gdu](https://github.com/dundee/gdu). With
`--output-attrs`, an export can be limited to a comma-separated selection of
`name`, `asize`, `dsize`, `items`, `mtime`, and `notreg`. `name` is always
present; omitted numeric attributes are unavailable when the report is loaded
again.

```sh
rdu -o sizes.json --output-attrs name,asize,dsize "$HOME"
```

Scan filters and scanner options have no effect with `-f`; display and output
options still apply to the loaded tree.

## Browser controls

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

Marked entries show a red `D`, including children inside a marked folder.
Marking does not modify the filesystem or rescan it. The footer shows the
number of marked entries and their estimated potential space release without
double-counting marked descendants.

Press `x`, or quit with pending marks, to review the full paths. Type the exact
word `DELETE` and press `Enter` to permanently delete the batch. **There is no
Trash or undo. A marked folder is deleted with all its contents, including
entries hidden by scan filters or unreadable during scanning.** `Esc` cancels
the confirmation and keeps the marks; use `u` then `q` to quit without deleting.
Use `Page Up` / `Page Down` to scroll the review.

The space estimate uses scanned allocated sizes, regardless of apparent-size
display mode. It is not guaranteed free space: hard links, snapshots, shared
blocks, filters, and filesystem changes can affect the result. Deletion and the
subsequent rescan run in a background worker; the browser shows a working dialog
and prevents further changes until they finish. Failures are displayed explicitly;
partially deleted folders are not rolled back. Successful deletions are removed
from the batch, while failed and unattempted entries remain marked.

Deletion never follows symlinks, refuses traversal through symlinked parent
directories or onto other filesystems, and rejects marked entries whose identity
has changed. The scan root itself cannot be marked. Browsing a saved report
with `-f` is read-only and cannot delete filesystem entries.
Deletion requires Linux `openat2` support (kernel 5.6 or newer, permitted by
the process sandbox); if unavailable, marking fails with an explicit error.

Use `rdu --help` for the complete option reference.

## Benchmarks

`rdu` is optimized for metadata-heavy directory scans. The results below are
scanner-only wall-clock times on 8-core WSL2 with output discarded. `rdu` is the
baseline; percentages show how much slower each competitor was.

Warm cache: 10 measured runs after 3 warmups.

| Fixture | Entries | `rdu` | `pdu` | `diskus` | `dust` |
| --- | ---: | ---: | ---: | ---: | ---: |
| Linux 7.2.2 | 100,960 | 53.0 ms | 53.0 ms (+0%) | 66.2 ms (+25%) | 92.7 ms (+75%) |
| `flat-1m` | 1,000,001 | 576.2 ms | 920.5 ms (+60%) | 846.1 ms (+47%) | 1.788 s (+210%) |
| `wide-400k` | 500,001 | 230.0 ms | 356.9 ms (+55%) | 371.2 ms (+61%) | 540.8 ms (+135%) |

Cold cache: 3 measured runs with the page cache dropped before each run.

| Fixture | `rdu` | `pdu` | `diskus` | `dust` |
| --- | ---: | ---: | ---: | ---: |
| Linux 7.2.2 | 223.5 ms | 435.1 ms (+95%) | 311.8 ms (+40%) | 487.8 ms (+118%) |
| `flat-1m` | 3.261 s | 3.580 s (+10%) | 3.748 s (+15%) | 4.711 s (+44%) |
| `wide-400k` | 1.724 s | 4.088 s (+137%) | 2.596 s (+51%) | 4.329 s (+151%) |

Reproduce the benchmark suite:

```sh
./scripts/make-fixtures.sh
mise exec -- ./scripts/bench-suite.sh --cache both
```

## Development

### Publishing a release

Set the version in `Cargo.toml` and update `Cargo.lock`, then push a matching
`v`-prefixed tag (for example, `v0.1.2`). The release workflow tests and builds
static Linux x86-64 and ARM64 binaries on native runners, smoke-tests the
packaged executables, and publishes both archives and `SHA256SUMS` to GitHub
Releases. Pull requests changing the release workflow or build configuration
also run the packaging jobs without publishing.

### Local checks

The checked-in `rust-toolchain.toml` selects Rust and its formatting/linting
components for local builds and CI.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
shellcheck scripts/*.sh
bash scripts/test.sh
```

The same checks are available as `mise run fmt-check`, `mise run lint`,
`mise run test`, and `mise run scripts-check`. CI runs these checks separately
from the release-build benchmark.

Benchmark and performance scripts refresh the release build before measuring it.
Use `scripts/perf.sh --binary FILE` to measure a specific existing binary instead.
`mise.toml` pins optional benchmark tools; install only the tools you need, for
example `mise install cargo:hyperfine`. Cold-cache benchmarks drop the host page
cache and require passwordless sudo or `RDU_DROP_CACHES_CMD`.

## License

MIT. See [LICENSE](LICENSE).
