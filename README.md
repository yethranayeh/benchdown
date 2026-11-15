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

A run needs a machine on AC power and 2 GB of free disk, plus `sudo` when the
sensor is `powermetrics`. It writes `blocks.tsv`, `analysis.json` and `meta.json`
to `./benchdown-<date>/`, or to `--out <dir>`, with the raw samples in
`power.plist` (powermetrics) or `samples.tsv` (a counter sensor).

`--export-json <file>` writes the results as hyperfine-shaped JSON (schema version 2).

`--cwd <dir>` runs the measured commands in another directory. `--env KEY=VALUE`
(repeatable, last value wins) adds variables to their environment, and
`--env-clear` starts them with an empty environment before `--env` entries are
added. All three apply to measured commands only, not to `--prepare` commands.

`--shell=none` (or `-N`) runs each command directly instead of through a shell.
It splits the string into words with quotes and backslashes, so
`task create --title 'two words'` keeps its title as one argument, but it does
not expand variables or interpret pipes, redirections or other shell operators.
A string it cannot split is refused before the run starts.

`--sensor <sensor>` picks where the power samples come from:

- `powermetrics` (macOS default): processor, GPU and ANE power. Needs `sudo` and
  reports whole-machine processor energy above an idle baseline.
- `smc` (macOS): AppleSMC whole-system power, no `sudo`. There is no
  processor-only SMC key on Apple Silicon, so this measures the whole machine,
  display and peripherals included. It idles far above processor power, so the
  idle limit does not apply and only the work-spike rule can drop a round.
- `rapl` (Linux default): Intel or AMD package energy from the RAPL counters.
  Needs the `msr` module and read access to `/dev/cpu/*/msr`. Everything else
  the run checks — load, disk, power source, battery, machine model — comes
  from `/proc`, `/sys` and `uname`, so nothing has to be installed.

Every run records the sensor it used in `meta.json` and prints it above the
table. `benchdown analyze` takes the same `--sensor` so a log is analyzed with
the limits that suit it, and reads either a `power.plist` or a `samples.tsv`.

## Prepare a quiet session

`benchdown session` turns selected macOS settings off for the duration of a job
and restores them afterwards, including when the session is interrupted:

```sh
benchdown session --spotlight-off --time-machine-off --wifi-off --jobs jobs.json
```

A jobs file is a JSON array of `{ "name", "command", "estimateMinutes" }`; one
command can also follow `--`. Each system flag is independent, and a setting that
is already off is left alone. Spotlight is handled per volume, and the sealed
system volume, whose index is read-only, is skipped. The session asks for the
password once, runs each job in its own process group, samples load and the
busiest processes every five seconds, and writes `session.log`, `status.txt`,
`guard.log` and `samples.tsv` to `./benchdown-sessions/sessions/<time>/`.

A small root guard applies the changes and undoes them over a pipe, so a closed
terminal or a killed session still restores. A restore state file is written
before the first change and updated as changes land; `benchdown session restore`
replays it after a crash or power cut; to run a command named `restore`, put it
after `--`. `--dry-run` prints the plan without changing anything or asking for
a password.

The session passes its samples file to jobs as `BENCHDOWN_SESSION_SAMPLES`.
`benchdown run` reads it and drops any round whose block saw a one-minute load
above 3.5 or a background indexing, backup or analysis service at 20% CPU or
more. `benchdown analyze` takes the same file as an optional third path.

The energy method is adapted from [Zinn's measurement harness](https://github.com/yethranayeh/zinn) (MIT).

The RAPL register numbers and the AppleSMC keys come from
[EnergiBridge](https://github.com/tdurieux/EnergiBridge) (MIT). AppleSMC reads
use the [`smc`](https://crates.io/crates/smc) crate (WTFPL). See
[THIRD-PARTY.md](THIRD-PARTY.md).

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
