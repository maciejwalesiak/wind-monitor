# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What it is

A Rust service (`wind-monitor`, one crate with lib + bin) that polls Windguru micro forecasts for configured spots and sends a push alert through ntfy when a spot's wind criteria are met within `days_ahead`. It ships as a static musl binary running under systemd (`deploy/wind-monitor.service`). The original design spec is `wind-monitoring-service-PLAN.md`; `README.md` is the user-facing reference (install, config keys, CLI).

## Commands

```sh
cargo test                           # unit tests + tests/fixtures_parse.rs
cargo test sector_wraps              # single test by name filter
cargo clippy --all-targets && cargo fmt --check
cargo run -- --config config.example.toml --once   # live fetch, prints matches, never notifies or writes state
cargo build --release               # static musl binary by default -> target/x86_64-unknown-linux-musl/release/wind-monitor
```

`.cargo/config.toml` makes `x86_64-unknown-linux-musl` the default target (tests run as musl binaries too) and sets `CC_x86_64_unknown_linux_musl=gcc` for ring's C code. `rust-toolchain.toml` adds the target via rustup. Output is never in `target/release/`. A glibc build there caused a `GLIBC_2.34 not found` error on the VPS.

In this dev environment `/workspace/target` is root-owned. Set `CARGO_TARGET_DIR` to a writable directory.

## Architecture

The pipeline is fetch → parse → per-hour timeline → windows → dedup → notify. `service.rs` drives one cycle; `main.rs` handles only the CLI, logging, the sleep+jitter loop and SIGTERM.

- `windguru.rs`: fetching and parsing. The response is an HTML page wrapping `<pre>`, with one block per model. Columns are located by **header name**, not by position. Rows carry only weekday, day of month and hour, in the block's `(UTC+N)` offset, so dates are rebuilt by searching forward from the model's init date (or the previous row) for a matching day and weekday; this handles month and year rollover. `-` parses as `None`. When the `WDEG` column is absent, direction falls back to the `WDIRN` compass text.
- `criteria.rs`: `timeline()` resolves the model **per hour** from the spot's priority list. A row covers `[t, next_row)` if that step is 3 h or less, otherwise only its own hour, so night gaps (Zephr-HD) and 3-hourly tails are both handled. A row with a missing speed falls through to the next model. `find_windows()` groups contiguous matching hours: strictly `> min_speed_kn`, and inside the sector if one is set.
- `state.rs`: dedup. A new window matches a previously alerted one if they overlap or their starts are less than 3 h apart. It re-alerts on a start shift of 3 h or more, or a peak change of 5 kn or more. The previous start is clamped to `horizon_start` (the current hour); windows already under way only re-alert when the peak **rises** by 5 kn or more. Otherwise a running window would re-alert every hour. Saves are atomic (temp file + fsync + rename).
- `notify.rs`: the `Notifier` trait (async-trait, `Vec<Box<dyn Notifier>>` built from config) and the message formatting. ntfy titles containing non-ASCII characters are sent RFC 2047-encoded, because HTTP headers must be ASCII. Errors strip the URL, since it contains the secret topic.
- `config.rs`: serde with `deny_unknown_fields`. `Sector::contains` handles wraparound through north.

## Constraints

- Use `reqwest` 0.12 with `default-features = false, features = ["rustls-tls"]` (ring + webpki-roots). Don't enable native-tls, and don't move to reqwest 0.13's default rustls: it uses aws-lc-rs and the system cert store, which make the static musl build harder.
- Never scrape the main windguru.cz site. Use only micro.windguru.cz.
- Test fixtures in `tests/fixtures/` are real responses from 2026-09-30. When the feed format changes, save a new live response rather than hand-editing.
- The thresholds and sectors in `config.example.toml` are placeholders. The user still has to supply the real values.
