---
title: benchdown
---

# How much energy did that command use?

benchdown measures how much processor energy a command uses, the way
[hyperfine](https://github.com/sharkdp/hyperfine) measures how long it takes.

```sh
cargo install benchdown
benchdown run 'cargo build'
```

You get `12.4 J ± 0.8`, not `4.2 s ± 0.1`. Both matter, and only one of them
shows up in a benchmark table.

## How it measures

It alternates measured idle and work blocks in a seeded order, subtracts each
work block's preceding idle, and reports the median per command with its spread.
It also checks the machine before it starts — AC power, free disk, load — and
drops rounds that break the rules instead of averaging them in.

## Sensors

- **`powermetrics`** (macOS default) — processor, GPU and ANE power. Needs
  `sudo`.
- **`smc`** (macOS) — whole-system power without `sudo`, display and
  peripherals included.
- **`rapl`** (Linux default) — Intel or AMD package energy from the RAPL
  counters.
- **`energibridge`** — delegates the measurement to
  [EnergiBridge](https://github.com/tdurieux/EnergiBridge).

## Where to get it

- [crates.io/crates/benchdown](https://crates.io/crates/benchdown) — the crate
- [docs.rs/benchdown](https://docs.rs/benchdown) — the API documentation
- [github.com/yethranayeh/benchdown](https://github.com/yethranayeh/benchdown) —
  source, issues and CI

benchdown is experimental: commands, output and the JSON schema may change
between releases.
