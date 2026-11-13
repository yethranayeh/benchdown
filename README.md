# benchdown

benchdown measures how much processor energy a command uses, the way hyperfine
measures how long it takes. It alternates measured idle and work blocks in a
seeded order, subtracts each work block's preceding idle, and reports the median
per command with its spread.

> [!IMPORTANT]
> **Experimental.** Commands, output and the JSON schema may change between
> releases.

## Install

Rust 1.85 or newer, on macOS:

```sh
cargo install --path .
```

## Usage

```sh
benchdown run 'cargo build'
```

A run needs `sudo` for `powermetrics`, a machine on AC power and 2 GB of free
disk. It writes `power.plist`, `blocks.tsv`, `analysis.json` and `meta.json` to
`./benchdown-<date>/`, or to `--out <dir>`.

`--export-json <file>` writes the results as hyperfine-shaped JSON (schema version 2).

It reports whole-machine processor energy above an idle baseline, not the energy
of one process.

The energy method is adapted from [Zinn's measurement harness](https://github.com/yethranayeh/zinn) (MIT).

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

## Contributing

Bug reports, feature requests and pull requests are welcome. Unless you
explicitly state otherwise, any contribution intentionally submitted for
inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
