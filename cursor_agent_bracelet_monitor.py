#!/usr/bin/env python3
"""Watch Cursor agent activity and trigger the Pavlok bracelet daemon.

This is the bracelet-enabled sibling of cursor_agent_monitor.py. It uses the
same Cursor detection logic, but threshold notifications are sent to the local
Pavlok HTTP daemon instead of only being printed in the terminal.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from urllib.parse import urlencode
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

from cursor_agent_monitor import (
    compact_status_line,
    cursor_data_dir,
    frontmost_agent_key,
    make_status,
    status_change_key,
    write_status_file,
)


DEFAULT_INTERVAL_SECONDS = 0.1
DEFAULT_COOLDOWN_SECONDS = 10.0
DEFAULT_BRACELET_BASE_URL = "http://127.0.0.1:8765"
DEFAULT_COMMANDS = os.environ.get("COMMANDS", "zap,beep")
DEFAULT_COMMAND = os.environ.get("COMMAND", DEFAULT_COMMANDS)
DEFAULT_INTENSITY = 100
DEFAULT_COUNT = 1
DEFAULT_NUM_ZAPS = 10
DEFAULT_NUM_BEEPS = 10
DEFAULT_ZAP_INTERVAL_SECONDS = 0.05
DEFAULT_BEEP_INTERVAL_SECONDS = 0.05
DEFAULT_BRACELET_TIMEOUT_SECONDS = 2.0


def bracelet_url(base_url: str, command: str, intensity: int, count: int) -> str:
    query = urlencode(
        {
            "intensity": max(1, min(intensity, 100)),
            "count": max(1, min(count, 7)),
        }
    )
    return f"{base_url.rstrip('/')}/stim/{command}?{query}"


def send_bracelet_command(url: str, timeout: float) -> dict[str, Any]:
    request = Request(url, method="POST")
    try:
        with urlopen(request, timeout=max(timeout, 0.1)) as response:
            body = response.read(2048).decode("utf-8", "replace")
            return {
                "ok": True,
                "status": response.status,
                "body": body,
            }
    except HTTPError as error:
        body = error.read(2048).decode("utf-8", "replace")
        return {
            "ok": False,
            "status": error.code,
            "error": body or error.reason,
        }
    except (OSError, URLError) as error:
        return {
            "ok": False,
            "status": None,
            "error": str(error),
        }


def bracelet_result_line(status: dict[str, Any], command: str, result: dict[str, Any]) -> str:
    ok = str(bool(result.get("ok"))).lower()
    http_status = result.get("status")
    base = (
        f"{status['checked_at']} bracelet_command={command} "
        f"bracelet_command_sent={ok} http_status={http_status or '-'}"
    )
    if result.get("ok"):
        return base
    return f"{base} error={result.get('error') or '-'}"


def command_send_count(command: str, num_zaps: int, num_beeps: int) -> int:
    if command == "zap":
        return max(num_zaps, 0)
    if command == "beep":
        return max(num_beeps, 0)
    return 1


def send_command_sequence(
    base_url: str,
    commands: list[str],
    intensity: int,
    count: int,
    num_zaps: int,
    num_beeps: int,
    zap_interval_seconds: float,
    beep_interval_seconds: float,
    timeout: float,
) -> list[tuple[str, dict[str, Any]]]:
    results: list[tuple[str, dict[str, Any]]] = []
    for command in commands:
        sends = command_send_count(command, num_zaps, num_beeps)
        for send_index in range(sends):
            result = send_bracelet_command(
                bracelet_url(base_url, command, intensity, count),
                timeout,
            )
            label = f"{command}[{send_index + 1}/{sends}]" if sends > 1 else command
            results.append((label, result))
            if command == "zap" and send_index < sends - 1:
                time.sleep(max(zap_interval_seconds, 0.0))
            if command == "beep" and send_index < sends - 1:
                time.sleep(max(beep_interval_seconds, 0.0))
    return results


def parse_commands(raw: str) -> list[str]:
    commands = [item.strip().lower() for item in raw.split(",") if item.strip()]
    invalid = [command for command in commands if command not in {"beep", "vibe", "zap"}]
    if invalid:
        raise argparse.ArgumentTypeError(
            f"unsupported command(s): {', '.join(invalid)}; use beep, vibe, or zap"
        )
    if not commands:
        raise argparse.ArgumentTypeError("at least one command is required")
    return commands


def running_agent_key(status: dict[str, Any]) -> tuple[str, str] | None:
    agent = status.get("frontmost_agent")
    if not isinstance(agent, dict) or not agent.get("running"):
        return None
    return frontmost_agent_key(status)


def agent_key(agent: dict[str, Any]) -> tuple[str, str] | None:
    workspace_db = agent.get("workspace_db")
    composer_id = agent.get("composer_id")
    if not isinstance(workspace_db, str) or not isinstance(composer_id, str):
        return None
    return workspace_db, composer_id


def visible_running_agent_keys(status: dict[str, Any]) -> set[tuple[str, str]]:
    visible_agents = status.get("visible_agents")
    if not isinstance(visible_agents, list):
        return set()
    keys: set[tuple[str, str]] = set()
    for agent in visible_agents:
        if not isinstance(agent, dict):
            continue
        key = agent_key(agent)
        if key is None:
            continue
        if agent.get("running"):
            keys.add(key)
    return keys


def visible_stopped_agent_keys(status: dict[str, Any]) -> set[tuple[str, str]]:
    visible_agents = status.get("visible_agents")
    if not isinstance(visible_agents, list):
        return set()
    keys: set[tuple[str, str]] = set()
    for agent in visible_agents:
        if not isinstance(agent, dict):
            continue
        key = agent_key(agent)
        if key is None:
            continue
        if not agent.get("running"):
            keys.add(key)
    return keys


@dataclass
class PerAgentAgeCooldownNotifier:
    delay_seconds: float
    running_since_by_key: dict[tuple[str, str], float] | None = None
    cooldown_until_by_key: dict[tuple[str, str], float] | None = None

    def __post_init__(self) -> None:
        if self.running_since_by_key is None:
            self.running_since_by_key = {}
        if self.cooldown_until_by_key is None:
            self.cooldown_until_by_key = {}

    def observe(self, status: dict[str, Any], monotonic_now: float) -> bool:
        assert self.running_since_by_key is not None
        assert self.cooldown_until_by_key is not None

        for key in visible_running_agent_keys(status):
            self.running_since_by_key.setdefault(key, monotonic_now)
        for key in visible_stopped_agent_keys(status):
            self.running_since_by_key.pop(key, None)
            self.cooldown_until_by_key.pop(key, None)

        current_key = running_agent_key(status)
        if current_key is None or not status.get("running"):
            return False

        running_since = self.running_since_by_key.setdefault(current_key, monotonic_now)
        if monotonic_now - running_since < max(self.delay_seconds, 0.0):
            return False
        if monotonic_now < self.cooldown_until_by_key.get(current_key, 0.0):
            return False

        self.cooldown_until_by_key[current_key] = monotonic_now + max(self.delay_seconds, 0.0)
        return True


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--once", action="store_true", help="print one snapshot and exit")
    parser.add_argument("--json", action="store_true", help="print JSON instead of compact status lines")
    parser.add_argument("--quiet", action="store_true", help="do not print terminal notifications")
    parser.add_argument(
        "--print-every-poll",
        action="store_true",
        help="debug mode: print raw status on every poll instead of threshold notifications",
    )
    parser.add_argument(
        "--print-on-change",
        action="store_true",
        help="debug mode: print raw status whenever it changes",
    )
    parser.add_argument("--interval", type=float, default=DEFAULT_INTERVAL_SECONDS, help="poll interval in seconds")
    parser.add_argument(
        "--threshold",
        type=float,
        default=0.0,
        help="deprecated compatibility option; use --wait for the 10s running delay and cooldown",
    )
    parser.add_argument(
        "--wait",
        type=float,
        default=DEFAULT_COOLDOWN_SECONDS,
        help="seconds the agent must be running before firing, and cooldown seconds after firing",
    )
    parser.add_argument(
        "--status-file",
        type=Path,
        help="write the latest JSON snapshot to this file on every poll",
    )
    parser.add_argument(
        "--cursor-data-dir",
        type=Path,
        default=cursor_data_dir(),
        help="Cursor application support directory",
    )
    parser.add_argument(
        "--workspace",
        help="only consider a workspace path, DB path, or workspace-storage id containing this value",
    )
    parser.add_argument(
        "--all-workspaces",
        action="store_true",
        help="consider all stored Cursor workspaces instead of the most recently active one",
    )
    parser.add_argument(
        "--ignore-frontmost",
        action="store_true",
        help="ignore whether Cursor is the frontmost macOS app",
    )
    parser.add_argument(
        "--require-cursor-frontmost",
        action="store_true",
        help="compatibility no-op; Cursor frontmost is required by default",
    )
    parser.add_argument(
        "--max-log-bytes",
        type=int,
        default=2_000_000,
        help="bytes to read from the end of each renderer.log",
    )
    parser.add_argument(
        "--stale-after-seconds",
        type=float,
        default=6 * 60 * 60,
        help="ignore active wake-lock events older than this",
    )
    parser.add_argument(
        "--bracelet-base-url",
        default=DEFAULT_BRACELET_BASE_URL,
        help="base URL for the local Pavlok daemon",
    )
    parser.add_argument(
        "--commands",
        type=parse_commands,
        default=parse_commands(DEFAULT_COMMAND),
        help="comma-separated bracelet commands to send; default is zap,beep",
    )
    parser.add_argument(
        "--intensity",
        type=int,
        default=DEFAULT_INTENSITY,
        help="bracelet command intensity, clamped to 1..100; default is 100",
    )
    parser.add_argument(
        "--count",
        type=int,
        default=DEFAULT_COUNT,
        help="bracelet command repeat count, clamped to 1..7",
    )
    parser.add_argument(
        "--num-zaps",
        type=int,
        default=DEFAULT_NUM_ZAPS,
        help="number of zap commands to send each time zap is triggered",
    )
    parser.add_argument(
        "--num-beeps",
        type=int,
        default=DEFAULT_NUM_BEEPS,
        help="number of beep commands to send each time beep is triggered",
    )
    parser.add_argument(
        "--zap-interval",
        type=float,
        default=DEFAULT_ZAP_INTERVAL_SECONDS,
        help="seconds between repeated zap commands",
    )
    parser.add_argument(
        "--beep-interval",
        type=float,
        default=DEFAULT_BEEP_INTERVAL_SECONDS,
        help="seconds between repeated beep commands",
    )
    parser.add_argument(
        "--bracelet-url",
        help="full URL to POST when the Cursor agent signal is true; overrides --commands",
    )
    parser.add_argument(
        "--bracelet-timeout",
        type=float,
        default=DEFAULT_BRACELET_TIMEOUT_SECONDS,
        help="seconds to wait for the bracelet HTTP request",
    )
    parser.add_argument(
        "--print-bracelet-result",
        action="store_true",
        help="print whether the bracelet HTTP request succeeded after each trigger",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    data_dir = args.cursor_data_dir.expanduser()
    if not data_dir.exists():
        print(f"Cursor data directory not found: {data_dir}", file=sys.stderr)
        return 2

    require_cursor_frontmost = not args.ignore_frontmost
    last_change_key: str | None = None
    notifier = PerAgentAgeCooldownNotifier(max(args.wait, 0.0))

    while True:
        status = make_status(
            data_dir=data_dir,
            require_cursor_frontmost=require_cursor_frontmost,
            workspace_filter=args.workspace,
            all_workspaces=args.all_workspaces,
            max_log_bytes=args.max_log_bytes,
            stale_after_seconds=args.stale_after_seconds,
        )

        if args.status_file:
            write_status_file(args.status_file.expanduser(), status)

        rendered = json.dumps(status, sort_keys=True) if args.json else compact_status_line(status)
        change_key = status_change_key(status)
        should_print = args.once or args.print_every_poll or (args.print_on_change and change_key != last_change_key)
        if not args.quiet and should_print:
            print(rendered, flush=True)
            last_change_key = change_key
        elif not args.once and not args.print_on_change and not args.print_every_poll:
            if notifier.observe(status, time.monotonic()):
                if args.bracelet_url:
                    result = send_bracelet_command(args.bracelet_url, args.bracelet_timeout)
                    command_results = [("custom", result)]
                else:
                    command_results = send_command_sequence(
                        base_url=args.bracelet_base_url,
                        commands=args.commands,
                        intensity=args.intensity,
                        count=args.count,
                        num_zaps=args.num_zaps,
                        num_beeps=args.num_beeps,
                        zap_interval_seconds=args.zap_interval,
                        beep_interval_seconds=args.beep_interval,
                        timeout=args.bracelet_timeout,
                    )
                for command, result in command_results:
                    if not result.get("ok"):
                        print(bracelet_result_line(status, command, result), file=sys.stderr, flush=True)
                    elif not args.quiet and args.print_bracelet_result:
                        print(bracelet_result_line(status, command, result), flush=True)

        if args.once:
            return 0
        time.sleep(max(args.interval, 0.05))


if __name__ == "__main__":
    raise SystemExit(main())
