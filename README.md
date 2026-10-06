# wind-monitor

A service that monitors wind conditions at given spot(s).

`wind-monitor` polls [Windguru micro](https://micro.windguru.cz/help.php) forecasts for your spots and sends a push notification to your phone via [ntfy](https://ntfy.sh) when the forecast wind meets your criteria within the next few days. It ships as a single static Linux binary running under systemd.

## How it works

Every `check_interval_min` minutes (plus a little random jitter) it runs one check:

1. It fetches each spot once with all models: `https://micro.windguru.cz/?s=<id>&m=all&v=WSPD,GUST,WDIRN,WDEG`.
2. For every hour from now until `days_ahead` days out, it takes the first model in the spot's `models` list that has a forecast for that hour. Near-term hours come from high-resolution models and later days fall back to GFS or IFS. Models that output every 3 hours cover their whole 3-hour step.
3. An hour **matches** when:
   - the mean wind is strictly greater than `min_speed_kn`, and
   - if a `sector` is set, the direction is inside it.
   - if `daylight_only` is set, it is light at the spot (from civil dawn to civil dusk).
4. A run of at least `min_consecutive_hours` matching hours is a **window**. Each window triggers one alert. It alerts again only when:
   - the window's start moves by 3 h or more,
   - its peak speed changes by 5 kn or more, or
   - it is a new window.

   A window that has already started only re-alerts if its peak rises by 5 kn or more.
5. Alerted windows are stored in `/var/lib/wind-monitor/state.json`. The file is written atomically, and windows that have ended are pruned.

If a fetch or parse fails, the error is logged and that spot backs off: the wait doubles each time, up to 6 h, and no alerts are sent for it meanwhile.

An alert looks like this:

```
Chałupy: 18 kn WSW
Thu 01.10 10:00–13:00 (3 h)
Wind: max 18 kn, mean 16.3 kn
Gusts: up to 24 kn
Direction: WSW (255°)
Model: ICON 2.2 km, GFS 13 km
```

## Build

You need Rust (via rustup) and gcc on an x86_64 Linux host. `ring`, the TLS crypto library, compiles C.

```sh
cargo build --release
file target/x86_64-unknown-linux-musl/release/wind-monitor   # "static-pie linked"
ldd  target/x86_64-unknown-linux-musl/release/wind-monitor   # "statically linked"
```

The static musl target is the default:
- `.cargo/config.toml` sets the build target and points `ring`'s C build at the host `gcc`.
- `rust-toolchain.toml` makes rustup install the `x86_64-unknown-linux-musl` target automatically.

The binary is therefore **not** in `target/release/`; it's in `target/x86_64-unknown-linux-musl/release/`. To build a glibc binary for local use, pass `--target x86_64-unknown-linux-gnu`.

On macOS, cross-compile with `cargo zigbuild --release` (cargo-zigbuild) or with `cross`. Both set their own C compiler, which overrides the gcc default.

TLS uses rustls with bundled Mozilla root certificates. There is no OpenSSL and no dependency on the system certificate store.

Run the tests with:

```sh
cargo test
```

The parser tests use real Windguru responses stored in `tests/fixtures/`.

## Install on the VPS (Ubuntu)

```sh
scp target/x86_64-unknown-linux-musl/release/wind-monitor deploy/wind-monitor.service config.example.toml vps:
ssh vps
sudo install -m 0755 wind-monitor /usr/local/bin/wind-monitor
sudo install -d -m 0755 /etc/wind-monitor
sudo install -m 0600 config.example.toml /etc/wind-monitor/config.toml
sudo editor /etc/wind-monitor/config.toml      # set the topic, thresholds and sectors
sudo install -m 0644 wind-monitor.service /etc/systemd/system/wind-monitor.service

sudo wind-monitor --config /etc/wind-monitor/config.toml --once         # dry run: prints matches, sends nothing
sudo wind-monitor --config /etc/wind-monitor/config.toml --test-notify

sudo systemctl daemon-reload
sudo systemctl enable --now wind-monitor
journalctl -u wind-monitor -f
```

The unit runs as a transient user (`DynamicUser=yes`) with a read-only filesystem apart from its state directory (`StateDirectory=wind-monitor`). The config stays root-only and reaches the service through `LoadCredential`, which needs systemd 247 or newer (Ubuntu 22.04+). Set `RUST_LOG=debug` in the unit for more detail.

## Phone setup (ntfy on iPhone)

1. Pick a long random topic name, e.g. `openssl rand -hex 16`. Anyone who knows it can read your alerts, so treat it like a password.
2. Put it in `[notify.ntfy] topic` in the config.
3. Install **ntfy** from the App Store, tap **+**, and subscribe to the same topic on `ntfy.sh`, or on your own server if you changed `server`.
4. Run `wind-monitor --test-notify` and check the notification arrives.

## Config reference

The default path is `/etc/wind-monitor/config.toml`; override it with `--config <path>`. See `config.example.toml` for a full example. Unknown keys are rejected.

| Key | Default | Meaning |
|---|---|---|
| `check_interval_min` | `30` | Minutes between checks. Up to 10% (max 5 min) of random jitter is added. |
| `timezone` | `"Europe/Warsaw"` | IANA timezone used for times in alerts. |
| `state_file` | `/var/lib/wind-monitor/state.json` | Where alert dedup state is kept. The `--state-file` flag overrides it. |
| `failure_notice_after` | off | Send a "feed broken" notice after this many consecutive failed fetches of a spot. |
| `[notify.ntfy] server` | `"https://ntfy.sh"` | ntfy server URL. |
| `[notify.ntfy] topic` | required | Secret topic name. |
| `[notify.ntfy] token` | none | Bearer token for self-hosted servers with access control. |

Each `[[spot]]`:

| Key | Default | Meaning |
|---|---|---|
| `id` | required | Windguru spot id (the number in the spot's URL). |
| `name` | required | Name used in alerts. |
| `days_ahead` | required | Only hours between now and now + `days_ahead` days count. |
| `min_speed_kn` | required | Mean wind must be **strictly greater** than this. |
| `sector` | none | `{ from = 300, to = 60 }`: allowed direction range in degrees, read clockwise, may wrap through north. Wind direction is where it blows *from*. |
| `min_consecutive_hours` | `1` | Minimum run of matching hours for a window. |
| `daylight_only` | `false` | Only count hours when it is light at the spot. Light means the sun is above −6° (civil twilight), at the coordinates from the Windguru feed, judged at the middle of each hour. Dark hours never match, so a window running into the night is cut at dusk, and `min_consecutive_hours` counts only light hours. |
| `models` | required | Model priority list, using the names exactly as Windguru prints them (case and spacing don't matter). |

Model names seen for Polish spots include `ICON 2.2 km`, `HARM-DK 2 km`, `MET Nordic 1 km`, `ALADIN 2.3 km`, `HARM-FI 2.5 km`, `HARMONIE 5 km`, `ICON 7 km`, `WRF 9 km`, `Zephr-HD 2.6 km`, `IFS-HRES 9 km`, `ICON 13 km`, `GFS 13 km` and `GDPS 15 km`. Run `wind-monitor --once` to see which ones your spot has. A configured model that is missing from the feed is logged as a warning.

## CLI

```
wind-monitor [--config <path>] [--state-file <path>]   run the service
wind-monitor --once                   one check, print matching windows, no notifications, state untouched
wind-monitor --test-notify            send a test notification
```

`--state-file` overrides `state_file` from the config. Under systemd, a path outside `/var/lib/wind-monitor` also needs a `ReadWritePaths=` entry for its directory, because the unit uses `ProtectSystem=strict` and the state is saved via a temp file and rename in that directory.

## Notes

- Only the free micro feed is used. Don't point this at the main windguru.cz site; scraping it is reportedly against their terms.
- Some high-resolution models (e.g. Zephr-HD) skip night hours. Those hours fall through to the next model in your list.
