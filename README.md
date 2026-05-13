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
target/release/pavlovd-rs monitor --intensity 100
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

## Signal Monitor

`monitor` watches local activity and sends a zap when a configured rule matches.
By default it posts to the already-running local daemon, so it does not open a
second BLE connection.
The default rules are:

| Rule | Scope | Cooldown |
| --- | --- | --- |
| Frontmost app contains `Messages` | Do Not Disturb only | 30 seconds |
| Frontmost app contains `Outlook` | Do Not Disturb only | 30 seconds |
| Browser URL is the YouTube homepage | Always | 30 seconds |
| Browser URL/title contains `linkedin` | Always | 30 seconds |

```sh
target/release/pavlovd-rs monitor \
  --intensity 100 \
  --cooldown-secs 30
```

By default, `monitor` sends zaps through the already-running local HTTP daemon
at `http://127.0.0.1:8765/stim/zap`, which avoids opening a second BLE
connection. For one-process diagnostic runs, pass `--direct-ble` with
`--allow-zap` and an explicit `--name` or `--uuid`.

Rules live in `~/.config/pavlov/rules.json` unless `--rules-file` is provided:

```sh
target/release/pavlovd-rs rules list
target/release/pavlovd-rs rules add-app Slack --scope dnd
target/release/pavlovd-rs rules add-app Discord --scope always
target/release/pavlovd-rs rules add-site reddit.com --scope dnd
target/release/pavlovd-rs rules add-site x.com --scope always
target/release/pavlovd-rs rules remove-site reddit.com --scope dnd
```

The monitor reads the frontmost app with AppleScript and checks Safari,
Chrome-family browsers, Arc, Dia, Opera, Edge, Brave, and Firefox window titles
for website rules. macOS does not provide a stable public Focus status API, so
Do Not Disturb detection uses best-effort fallbacks. If those do not work on
your macOS version, pass a command that returns or exits truthy when DND is on:

```sh
target/release/pavlovd-rs monitor \
  --dnd-command 'test -f /tmp/pavlov-dnd-on'
```

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

## Cursor Agent Monitor

`cursor_agent_monitor.py` watches Cursor's local workspace state and renderer
logs to detect whether the frontmost agent tab is actively generating.
It does not require macOS Screen Recording or Accessibility permissions.

Run one snapshot:

```bash
python3 cursor_agent_monitor.py --once
```

Run continuously in a terminal:

```bash
python3 cursor_agent_monitor.py
```

This prints one terminal notification after the frontmost agent tab has been
running for `3` consecutive seconds, then waits `5` seconds before checking for
that condition again.

Tune those values:

```bash
python3 cursor_agent_monitor.py --threshold 3 --wait 5
```

Run quietly in the background and keep the latest state as JSON:

```bash
nohup python3 cursor_agent_monitor.py --quiet --status-file .cursor-agent-status.json > .cursor-agent-monitor.log 2>&1 &
```

By default the monitor checks every `0.5s`, ignores whether the Cursor app is
the frontmost macOS app, and tracks the active/frontmost agent tab inside the
newest live-looking Cursor workspace state. Add `--print-every-poll` or
`--print-on-change` for debugging. Add `--require-cursor-frontmost` if you only
want `running=true` while Cursor itself has macOS focus. Add `--all-workspaces`
if you want to scan every stored Cursor workspace.

## Cursor Bracelet Monitor

`cursor_agent_bracelet_monitor.py` sends Pavlok commands through the local
daemon when a running Cursor agent crosses the configured delay. By default it
checks every `0.1s`, waits `10` seconds per running agent before firing, and
sends one `zap` command at `100%`.

For always-on background use, install the LaunchAgents:

```bash
mkdir -p ~/.pavlov ~/Library/LaunchAgents
install -m 755 cursor_agent_monitor.py ~/.pavlov/cursor_agent_monitor.py
install -m 755 cursor_agent_bracelet_monitor.py ~/.pavlov/cursor_agent_bracelet_monitor.py
cp LaunchAgents/com.pavlov.daemon.plist ~/Library/LaunchAgents/
cp LaunchAgents/com.pavlov.cursor-bracelet-monitor.plist ~/Library/LaunchAgents/
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/com.pavlov.daemon.plist
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/com.pavlov.cursor-bracelet-monitor.plist
```

## iMessage / YouTube Monitor

`detect_imessage` watches the frontmost app and prints one terminal
notification when either condition stays true for the threshold:

- Messages/iMessage is frontmost while Do Not Disturb/Focus is active.
- A YouTube tab is frontmost in Google Chrome or Safari.

Run continuously:

```bash
./detect_imessage --threshold 3 --wait 5
```

Debug one snapshot:

```bash
./detect_imessage --once --json
```

Like the Cursor monitor, it checks every `0.5s` by default, waits `5` seconds
after each notification before checking again, and supports `--print-every-poll`,
`--print-on-change`, and `--status-file`.
