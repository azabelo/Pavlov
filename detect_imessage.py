#!/usr/bin/env python3
"""Notify when distracting front-visible apps persist past a threshold.

Conditions:
- Messages/iMessage is the frontmost app while Do Not Disturb/Focus is active.
- A YouTube tab is frontmost in Google Chrome or Safari.
"""

from __future__ import annotations

import argparse
import json
import plistlib
import re
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any


DEFAULT_INTERVAL_SECONDS = 0.5
DEFAULT_THRESHOLD_SECONDS = 3.0
DEFAULT_WAIT_SECONDS = 5.0

MESSAGES_BUNDLE_IDS = {"com.apple.MobileSMS", "com.apple.iChat"}
CHROME_BUNDLE_ID = "com.google.Chrome"
SAFARI_BUNDLE_ID = "com.apple.Safari"
YOUTUBE_RE = re.compile(r"(^|\.)youtube\.com$|(^|\.)youtu\.be$", re.IGNORECASE)


@dataclass
class TriggerState:
    active_since: float | None = None
    active_key: tuple[str, str] | None = None
    cooldown_until: float = 0.0

    def observe(
        self,
        status: dict[str, Any],
        monotonic_now: float,
        threshold_seconds: float,
        wait_seconds: float,
    ) -> float | None:
        if monotonic_now < self.cooldown_until:
            self.active_since = None
            self.active_key = None
            return None

        current_key = condition_key(status)
        if current_key is None:
            self.active_since = None
            self.active_key = None
            return None

        if current_key != self.active_key:
            self.active_key = current_key
            self.active_since = monotonic_now

        assert self.active_since is not None
        active_for = monotonic_now - self.active_since
        if active_for < threshold_seconds:
            return None

        self.cooldown_until = monotonic_now + wait_seconds
        self.active_since = None
        self.active_key = None
        return active_for


def now_iso() -> str:
    return datetime.now().astimezone().isoformat(timespec="seconds")


def run_command(args: list[str], timeout: float = 1.5) -> str:
    try:
        completed = subprocess.run(
            args,
            check=False,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except (OSError, subprocess.SubprocessError):
        return ""
    if completed.returncode != 0:
        return ""
    return completed.stdout.strip()


def frontmost_bundle_id() -> str | None:
    system_events_bundle = run_command(
        [
            "osascript",
            "-e",
            'tell application "System Events" to get bundle identifier of first process whose frontmost is true',
        ],
        timeout=3.0,
    )
    if system_events_bundle:
        return system_events_bundle

    front = run_command(["lsappinfo", "front"])
    match = re.search(r"ASN:[^:\s]+-[^:\s]+:", front)
    if not match:
        return None

    info = run_command(["lsappinfo", "info", match.group(0)])
    match = re.search(r'bundleID="([^"]+)"', info)
    return match.group(1) if match else None


def browser_tab(bundle_id: str) -> dict[str, str | None]:
    if bundle_id == CHROME_BUNDLE_ID:
        url = run_command(
            [
                "osascript",
                "-e",
                'tell application "Google Chrome" to if (count of windows) > 0 then get URL of active tab of front window',
            ],
            timeout=3.0,
        )
        title = run_command(
            [
                "osascript",
                "-e",
                'tell application "Google Chrome" to if (count of windows) > 0 then get title of active tab of front window',
            ],
            timeout=3.0,
        )
        return {"url": url or None, "title": title or None}

    if bundle_id == SAFARI_BUNDLE_ID:
        url = run_command(
            [
                "osascript",
                "-e",
                'tell application "Safari" to if (count of windows) > 0 then get URL of current tab of front window',
            ],
            timeout=3.0,
        )
        title = run_command(
            [
                "osascript",
                "-e",
                'tell application "Safari" to if (count of windows) > 0 then get name of current tab of front window',
            ],
            timeout=3.0,
        )
        return {"url": url or None, "title": title or None}

    return {"url": None, "title": None}


def app_window_count(app_name: str) -> int | None:
    raw = run_command(
        ["osascript", "-e", f'tell application "{app_name}" to if it is running then get count of windows'],
        timeout=3.0,
    )
    if not raw:
        return None
    try:
        return int(raw)
    except ValueError:
        return None


def hostname_from_url(url: str) -> str:
    match = re.match(r"^[a-z][a-z0-9+.-]*://([^/:?#]+)", url, re.IGNORECASE)
    return match.group(1).lower() if match else ""


def is_youtube_tab(tab: dict[str, str | None]) -> bool:
    url = tab.get("url") or ""
    title = tab.get("title") or ""
    host = hostname_from_url(url)
    if host and YOUTUBE_RE.search(host):
        return True
    return "youtube" in title.lower()


def focus_active_via_private_framework() -> bool | None:
    try:
        import Foundation
        import objc
    except Exception:
        return None

    try:
        bundle = Foundation.NSBundle.bundleWithPath_(
            "/System/Library/PrivateFrameworks/DoNotDisturb.framework"
        )
        if bundle is None or not bundle.load():
            return None
        service_class = objc.lookUpClass("DNDStateService")
        service = service_class.alloc().init()
        state = service.queryCurrentStateWithError_(None)
    except Exception:
        return None

    if isinstance(state, tuple):
        state = state[0]
    if state is None:
        return False

    try:
        return bool(state.isActive())
    except Exception:
        return True


def focus_active_via_legacy_preferences() -> bool | None:
    by_host_dir = Path.home() / "Library" / "Preferences" / "ByHost"
    for path in by_host_dir.glob("com.apple.notificationcenterui*.plist"):
        try:
            data = plistlib.loads(path.read_bytes())
        except (OSError, plistlib.InvalidFileException):
            continue
        for key in ("doNotDisturb", "dndEnabled"):
            if isinstance(data.get(key), bool):
                return bool(data[key])

    return None


def focus_active() -> tuple[bool, str]:
    private_result = focus_active_via_private_framework()
    if private_result is not None:
        return private_result, "private_framework"

    legacy_result = focus_active_via_legacy_preferences()
    if legacy_result is not None:
        return legacy_result, "legacy_preferences"

    return False, "unavailable"


def make_status() -> dict[str, Any]:
    bundle_id = frontmost_bundle_id()
    dnd_active, dnd_source = focus_active()
    tab = browser_tab(bundle_id or "")
    messages_frontmost = bundle_id in MESSAGES_BUNDLE_IDS
    messages_window_count = app_window_count("Messages") if messages_frontmost else None
    messages_front_visible = bool(messages_frontmost and (messages_window_count is None or messages_window_count > 0))
    youtube_frontmost = bool(bundle_id in {CHROME_BUNDLE_ID, SAFARI_BUNDLE_ID} and is_youtube_tab(tab))
    messages_with_dnd = bool(messages_front_visible and dnd_active)

    if messages_with_dnd:
        state = "messages_with_dnd"
    elif youtube_frontmost:
        state = "youtube_frontmost"
    else:
        state = "idle"

    return {
        "checked_at": now_iso(),
        "state": state,
        "condition": messages_with_dnd or youtube_frontmost,
        "frontmost_bundle_id": bundle_id,
        "dnd_active": dnd_active,
        "dnd_source": dnd_source,
        "messages_frontmost": messages_frontmost,
        "messages_front_visible": messages_front_visible,
        "messages_window_count": messages_window_count,
        "messages_with_dnd": messages_with_dnd,
        "browser_tab": tab,
        "youtube_frontmost": youtube_frontmost,
    }


def condition_key(status: dict[str, Any]) -> tuple[str, str] | None:
    if status.get("messages_with_dnd"):
        return "messages_with_dnd", str(status.get("frontmost_bundle_id") or "")
    if status.get("youtube_frontmost"):
        tab = status.get("browser_tab")
        url = tab.get("url") if isinstance(tab, dict) else ""
        return "youtube_frontmost", str(url or status.get("frontmost_bundle_id") or "")
    return None


def compact_status_line(status: dict[str, Any]) -> str:
    tab = status.get("browser_tab")
    url = tab.get("url") if isinstance(tab, dict) else None
    return (
        f"{status['checked_at']} state={status['state']} "
        f"condition={str(status['condition']).lower()} "
        f"dnd_active={str(status['dnd_active']).lower()} "
        f"frontmost_bundle={status.get('frontmost_bundle_id') or '-'} "
        f"url={url or '-'}"
    )


def notification_line(status: dict[str, Any], active_for: float, threshold: float) -> str:
    if status.get("messages_with_dnd"):
        label = "messages_with_dnd"
        detail = f"frontmost_bundle={status.get('frontmost_bundle_id') or '-'}"
    else:
        label = "youtube_frontmost"
        tab = status.get("browser_tab")
        url = tab.get("url") if isinstance(tab, dict) else None
        detail = f"url={url or '-'}"

    return (
        f"{status['checked_at']} notification={label} "
        f"active_for={active_for:.1f}s threshold={threshold:g}s {detail}"
    )


def status_change_key(status: dict[str, Any]) -> str:
    comparable = dict(status)
    comparable.pop("checked_at", None)
    return json.dumps(comparable, sort_keys=True)


def write_status_file(path: Path, status: dict[str, Any]) -> None:
    tmp_path = path.with_suffix(path.suffix + ".tmp")
    tmp_path.write_text(json.dumps(status, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    tmp_path.replace(path)


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
    parser.add_argument(
        "--interval",
        type=float,
        default=DEFAULT_INTERVAL_SECONDS,
        help="poll interval in seconds",
    )
    parser.add_argument(
        "--threshold",
        type=float,
        default=DEFAULT_THRESHOLD_SECONDS,
        help="seconds the condition must stay true before notification",
    )
    parser.add_argument(
        "--wait",
        type=float,
        default=DEFAULT_WAIT_SECONDS,
        help="seconds to wait after a notification before checking again",
    )
    parser.add_argument(
        "--status-file",
        type=Path,
        help="write the latest JSON snapshot to this file on every poll",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    threshold_seconds = max(args.threshold, 0.0)
    wait_seconds = max(args.wait, 0.0)
    last_change_key: str | None = None
    trigger = TriggerState()

    while True:
        status = make_status()

        if args.status_file:
            write_status_file(args.status_file.expanduser(), status)

        rendered = json.dumps(status, sort_keys=True) if args.json else compact_status_line(status)
        change_key = status_change_key(status)
        should_print = args.once or args.print_every_poll or (args.print_on_change and change_key != last_change_key)

        if not args.quiet and should_print:
            print(rendered, flush=True)
            last_change_key = change_key
        elif not args.quiet and not args.once and not args.print_on_change and not args.print_every_poll:
            active_for = trigger.observe(status, time.monotonic(), threshold_seconds, wait_seconds)
            if active_for is not None:
                print(notification_line(status, active_for, threshold_seconds), flush=True)

        if args.once:
            return 0
        time.sleep(max(args.interval, 0.2))


if __name__ == "__main__":
    raise SystemExit(main())
