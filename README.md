# rdu

[![CI](https://github.com/mgl/rdu/actions/workflows/ci.yml/badge.svg)](https://github.com/mgl/rdu/actions/workflows/ci.yml)

Fast disk usage analyzer for Linux.

`rdu` scans directory trees in parallel and displays the results in an
interactive terminal browser or as text and JSON. JSON reports are compatible
with [gdu](https://github.com/dundee/gdu).

## Installation

Requires Rust 1.98 or later.

```sh
cargo install --git https://github.com/mgl/rdu.git
```

## Usage

With a terminal on stdout, `rdu` opens the interactive browser by default:

```sh
rdu [PATH]
```

Use `-n` for non-interactive output:

```sh
rdu -n -s "$HOME"             # print one total
rdu -n --top 20 "$HOME"       # show the 20 largest files
rdu -n --depth 2 /var         # show two directory levels
rdu -n -a "$HOME"             # use apparent size
rdu -n -o report.json "$HOME" # write a JSON report
rdu -n -f report.json         # read a saved report
```

Use `rdu --help` for all options. `-L` follows file symlinks; directory
symlinks are not followed.

### Browser controls

`↑`/`↓` or `h`/`j`/`k`/`l` navigate. `s`, `n`, `C`, and `m` sort by size, name,
item count, and modification time. `/` filters by name, `a` toggles apparent
size, `?` shows help, and `q` quits.

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

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

## License

MIT. See [LICENSE](LICENSE).
