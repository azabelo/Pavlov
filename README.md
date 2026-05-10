# Pavlov

Low-latency local control experiments for Pavlok 3.

## Rust Headless Runner

`pavlovd-rs` is the local daemon. It sends Pavlok 3 stimuli directly over BLE,
accepts Pavlok API v5-compatible stimulus requests, and exposes safe
Webtool-compatible packet aliases. It uses `btleplug`, which uses CoreBluetooth
on macOS:

```sh
cargo build --release --bin pavlovd-rs
```

It preserves the daemon command shape:

```sh
target/release/pavlovd-rs scan --scan-timeout-ms 3000
target/release/pavlovd-rs once --name Pavlok-3-E14D --stim vibe --mode response
target/release/pavlovd-rs serve --name Pavlok-3-E14D --mode response
target/release/pavlovd-rs stdin --name Pavlok-3-E14D --mode response
```

The warm HTTP daemon listens on `127.0.0.1:8765` by default. It exposes the
low-latency local routes:

```sh
curl -s -X POST 'http://127.0.0.1:8765/stim/vibe?intensity=50&count=1'
curl -s -X POST 'http://127.0.0.1:8765/stim/beep?intensity=50&count=1'
curl -s -X POST 'http://127.0.0.1:8765/stim/zap?intensity=10'
```

It also accepts Pavlok API v5-compatible stimulus requests locally:

```sh
curl -s -X POST 'http://127.0.0.1:8765/api/v5/stimulus/send' \
  -H 'Content-Type: application/json' \
  -d '{"stimulus":{"stimulusType":"vibe","stimulusValue":50}}'
```

For client compatibility, the v5-compatible route accepts an `Authorization`
header if present but does not require or forward it. It validates
`stimulusType` as `zap`, `beep`, or `vibe`, and `stimulusValue` as `1..100`.

The safe Webtool packet aliases currently implemented are:

```sh
curl -s -X POST 'http://127.0.0.1:8765/webtool/testVibe'
curl -s -X POST 'http://127.0.0.1:8765/webtool/testBeep'
curl -s -X POST 'http://127.0.0.1:8765/webtool/testLeds'
curl -s -X POST 'http://127.0.0.1:8765/webtool/findPavlok'
curl -s -X POST 'http://127.0.0.1:8765/webtool/findCancel'
```

`/webtool/diagZap` is also available, but only when the daemon was started with
`--allow-zap`. Zap remains gated behind `--allow-zap`, and `response` remains
the default write mode because this Pavlok 3 did not advertise
`writeWithoutResponse` for the vibe/beep characteristics.

## BLE Packets

| Stimulus | Characteristic | UUID | Payload |
| --- | --- | --- | --- |
| `vibe` | `c_vibe` | `00001001-0000-1000-8000-00805f9b34fb` | `[0x80 | count, 0x02, intensity, on_ms, off_ms]` |
| `beep` | `c_beep` | `00001002-0000-1000-8000-00805f9b34fb` | `[0x80 | count, 0x00, intensity, on_ms, off_ms]` |
| `zap` | `c_zap` | `00001003-0000-1000-8000-00805f9b34fb` | `[0x89, intensity]` |

`intensity` is clamped to `1..100`, `count` to `1..7`, and `on_ms`/`off_ms` to
`1..255`. Zap requires `--allow-zap`.

For macOS Bluetooth privacy prompts, build the signed app bundle with:

```sh
cargo app-bundle
```

The Rust JSON keeps `trigger_to_write_issued`, `write_call_wall`,
`trigger_to_write_ack`, and `request_to_response`. `btleplug` exposes the write
as an async future rather than the exact CoreBluetooth delegate boundary, so
`write_call_wall` is the awaited Rust write duration.
