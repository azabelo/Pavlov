#!/usr/bin/env python3
"""Watch whether the frontmost Cursor agent tab is currently generating.

The monitor uses Cursor's local workspace state plus renderer wake-lock logs.
It does not need screen recording or Accessibility permissions.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sqlite3
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from urllib.parse import quote, unquote, urlparse


CURSOR_BUNDLE_ID = "com.todesktop.230313mzl4w4u92"
DEFAULT_INTERVAL_SECONDS = 0.5
DEFAULT_THRESHOLD_SECONDS = 3.0
DEFAULT_WAIT_SECONDS = 5.0
WAKEL0CK_RE = re.compile(
    r"^(?P<ts>\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{3}) "
    r"\[[^\]]+\] \[ComposerWakelockManager\] "
    r"(?P<action>Acquired|Released) wakelock id=(?P<id>\d+) "
    r'reason="(?P<reason>[^"]+)" composerId=(?P<composer_id>[0-9a-f-]+)'
)


@dataclass
class WakelockEvent:
    composer_id: str
    action: str
    reason: str
    timestamp: datetime
    source: str

    @property
    def is_active(self) -> bool:
        return self.action == "Acquired"


@dataclass
class VisibleComposer:
    composer_id: str
    workspace: str
    workspace_db: str
    workspace_db_mtime: float
    mode: str
    created_at: int | None


@dataclass
class ThresholdNotifier:
    threshold_seconds: float
    wait_seconds: float
    running_since: float | None = None
    running_key: tuple[str, str] | None = None
    cooldown_until: float = 0.0

    def observe(self, status: dict[str, Any], monotonic_now: float) -> float | None:
        if monotonic_now < self.cooldown_until:
            self.running_since = None
            self.running_key = None
            return None

        current_key = frontmost_agent_key(status) if status.get("running") else None
        if current_key is None:
            self.running_since = None
            self.running_key = None
            return None

        if current_key != self.running_key:
            self.running_key = current_key
            self.running_since = monotonic_now

        assert self.running_since is not None
        running_for = monotonic_now - self.running_since
        if running_for < max(self.threshold_seconds, 0.0):
            return None

        self.cooldown_until = monotonic_now + max(self.wait_seconds, 0.0)
        self.running_since = None
        self.running_key = None
        return running_for


def local_tz() -> timezone:
    return datetime.now().astimezone().tzinfo or timezone.utc


def parse_log_timestamp(raw: str) -> datetime:
    return datetime.strptime(raw, "%Y-%m-%d %H:%M:%S.%f").replace(tzinfo=local_tz())


def now_iso() -> str:
    return datetime.now().astimezone().isoformat(timespec="seconds")


def decode_db_value(value: Any) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", "replace")
    return str(value)


def read_sqlite_items(db_path: Path, keys: list[str] | None = None) -> dict[str, str]:
    if not db_path.exists():
        return {}

    quoted = quote(str(db_path), safe="/")
    uri = f"file:{quoted}?mode=ro"
    where = ""
    params: list[str] = []
    if keys:
        where = " where key in ({})".format(",".join("?" for _ in keys))
        params = keys

    try:
        with sqlite3.connect(uri, uri=True, timeout=0.2) as conn:
            rows = conn.execute(f"select key, value from ItemTable{where}", params).fetchall()
    except sqlite3.Error:
        return {}

    return {str(key): decode_db_value(value) for key, value in rows}


def parse_json(raw: str, fallback: Any) -> Any:
    if not raw:
        return fallback
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        return fallback


def cursor_data_dir() -> Path:
    return Path.home() / "Library" / "Application Support" / "Cursor"


def workspace_label(workspace_dir: Path) -> str:
    workspace_json = workspace_dir / "workspace.json"
    if workspace_json.exists():
        try:
            data = json.loads(workspace_json.read_text(encoding="utf-8"))
            folder = data.get("folder")
            if isinstance(folder, str):
                parsed = urlparse(folder)
                if parsed.scheme == "file":
                    return unquote(parsed.path)
                if parsed.scheme:
                    return folder
                return folder
        except (OSError, json.JSONDecodeError):
            pass
    return f"workspaceStorage/{workspace_dir.name}"


def folder_uri_to_label(folder_uri: str) -> str:
    parsed = urlparse(folder_uri)
    if parsed.scheme == "file":
        return unquote(parsed.path)
    if parsed.scheme:
        return folder_uri
    return folder_uri


def recent_workspace_label(data_dir: Path) -> str | None:
    db_path = data_dir / "User" / "globalStorage" / "state.vscdb"
    items = read_sqlite_items(db_path, ["history.recentlyOpenedPathsList"])
    recent = parse_json(items.get("history.recentlyOpenedPathsList", ""), {})
    entries = recent.get("entries") if isinstance(recent, dict) else None
    if not isinstance(entries, list):
        return None
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        folder_uri = entry.get("folderUri")
        if isinstance(folder_uri, str):
            return folder_uri_to_label(folder_uri)
    return None


def is_truthy_json(raw: str, default: bool) -> bool:
    parsed = parse_json(raw, default)
    if isinstance(parsed, bool):
        return parsed
    if isinstance(parsed, str):
        return parsed.lower() == "true"
    return bool(parsed)


def frontmost_composer_ids_from_pane(items: dict[str, str], active_panel_id: str) -> set[str]:
    ids: set[str] = set()
    suffix = ""
    prefix = "workbench.panel.aichat."
    if active_panel_id.startswith(prefix):
        suffix = active_panel_id[len(prefix) :]

    if not suffix:
        return ids

    pane_state = parse_json(items.get(f"workbench.panel.composerChatViewPane.{suffix}", ""), {})
    if not isinstance(pane_state, dict):
        return ids

    for view_id, view_state in pane_state.items():
        marker = "workbench.panel.aichat.view."
        if not isinstance(view_id, str) or not view_id.startswith(marker):
            continue
        if isinstance(view_state, dict) and view_state.get("isHidden") is True:
            continue
        ids.add(view_id[len(marker) :])
    return ids


def active_panel_is_visible(items: dict[str, str], active_panel_id: str) -> bool:
    if not active_panel_id:
        return False
    view_state = parse_json(items.get("workbench.auxiliarybar.viewContainersWorkspaceState", ""), [])
    if not isinstance(view_state, list):
        return True
    for entry in view_state:
        if isinstance(entry, dict) and entry.get("id") == active_panel_id:
            return entry.get("visible") is not False
    return True


def global_chat_visible(data_dir: Path) -> bool:
    db_path = data_dir / "User" / "globalStorage" / "state.vscdb"
    items = read_sqlite_items(
        db_path,
        ["cursor/globalLayoutState", "agentLayout.shared.v6"],
    )

    layout = parse_json(items.get("cursor/globalLayoutState", ""), {})
    if isinstance(layout, dict) and layout.get("chatVisible") is False:
        return False

    shared = parse_json(items.get("agentLayout.shared.v6", ""), {})
    if isinstance(shared, dict):
        visible_values = [
            shared.get("auxiliaryBarVisible"),
            shared.get("sidebarVisible"),
            shared.get("panelVisible"),
        ]
        if any(value is True for value in visible_values):
            return True

    return True


def workspace_matches_filter(label: str, db_path: Path, workspace_filter: str | None) -> bool:
    if not workspace_filter:
        return True
    needle = os.path.expanduser(workspace_filter)
    candidates = [label, str(db_path), db_path.parent.name]
    if label.startswith("/"):
        try:
            candidates.append(str(Path(label).resolve()))
        except OSError:
            pass
    try:
        resolved_needle = str(Path(needle).resolve())
    except OSError:
        resolved_needle = needle
    if resolved_needle != needle:
        candidates.append(resolved_needle)
    return any(needle in candidate or candidate == needle for candidate in candidates)


def find_visible_composers(data_dir: Path, workspace_filter: str | None = None) -> list[VisibleComposer]:
    if not global_chat_visible(data_dir):
        return []

    storage_dir = data_dir / "User" / "workspaceStorage"
    visible: list[VisibleComposer] = []
    for db_path in sorted(storage_dir.glob("*/state.vscdb")):
        workspace_dir = db_path.parent
        label = workspace_label(workspace_dir)
        if not workspace_matches_filter(label, db_path, workspace_filter):
            continue

        items = read_sqlite_items(db_path)
        composer_data = parse_json(items.get("composer.composerData", ""), {})
        if not isinstance(composer_data, dict):
            continue

        all_composers = composer_data.get("allComposers")
        if not isinstance(all_composers, list):
            continue

        composers_by_id = {
            composer.get("composerId"): composer
            for composer in all_composers
            if isinstance(composer, dict) and isinstance(composer.get("composerId"), str)
        }
        selected_ids = {
            cid
            for cid in composer_data.get("selectedComposerIds", [])
            if isinstance(cid, str)
        }

        active_panel_id = items.get("workbench.auxiliarybar.activepanelid", "")
        if not active_panel_id.startswith("workbench.panel.aichat."):
            continue
        if not active_panel_is_visible(items, active_panel_id):
            continue
        if is_truthy_json(items.get("workbench.auxiliaryBar.hidden", "false"), False):
            continue

        frontmost_ids = frontmost_composer_ids_from_pane(items, active_panel_id)
        candidate_ids = frontmost_ids
        if not candidate_ids and len(selected_ids) == 1:
            candidate_ids = selected_ids
        for composer_id in candidate_ids:
            composer = composers_by_id.get(composer_id)
            if not composer:
                continue
            if selected_ids and composer_id not in selected_ids:
                continue
            mode = str(composer.get("unifiedMode") or composer.get("forceMode") or "")
            if mode != "agent":
                continue
            if composer.get("isArchived") is True:
                continue
            visible.append(
                VisibleComposer(
                    composer_id=composer_id,
                    workspace=label,
                    workspace_db=str(db_path),
                    workspace_db_mtime=db_path.stat().st_mtime,
                    mode=mode,
                    created_at=composer.get("createdAt") if isinstance(composer.get("createdAt"), int) else None,
                )
            )

    return visible


def renderer_logs(data_dir: Path) -> list[Path]:
    return sorted((data_dir / "logs").glob("*/window*/renderer*.log"))


def parse_wakelock_line(line: str, source: Path) -> WakelockEvent | None:
    match = WAKEL0CK_RE.search(line)
    if not match:
        return None
    return WakelockEvent(
        composer_id=match.group("composer_id"),
        action=match.group("action"),
        reason=match.group("reason"),
        timestamp=parse_log_timestamp(match.group("ts")),
        source=str(source),
    )


def scan_log_tail(path: Path, max_bytes: int) -> list[WakelockEvent]:
    try:
        size = path.stat().st_size
        with path.open("rb") as handle:
            if size > max_bytes:
                handle.seek(size - max_bytes)
                handle.readline()
            raw_lines = handle.readlines()
    except OSError:
        return []

    events: list[WakelockEvent] = []
    for raw in raw_lines:
        event = parse_wakelock_line(raw.decode("utf-8", "replace"), path)
        if event:
            events.append(event)
    return events


def newest_events_by_composer(data_dir: Path, max_bytes_per_log: int) -> dict[str, WakelockEvent]:
    newest: dict[str, WakelockEvent] = {}
    for log_path in renderer_logs(data_dir):
        for event in scan_log_tail(log_path, max_bytes_per_log):
            current = newest.get(event.composer_id)
            if current is None or event.timestamp >= current.timestamp:
                newest[event.composer_id] = event
    return newest


def newest_workspace_composers(composers: list[VisibleComposer]) -> list[VisibleComposer]:
    if not composers:
        return []
    newest_mtime = max(composer.workspace_db_mtime for composer in composers)
    return [
        composer
        for composer in composers
        if composer.workspace_db_mtime == newest_mtime
    ]


def frontmost_bundle_id() -> str | None:
    try:
        front = subprocess.run(
            ["lsappinfo", "front"],
            check=True,
            capture_output=True,
            text=True,
            timeout=2.0,
        ).stdout.strip()
        match = re.search(r"ASN:[^:\s]+-[^:\s]+:", front)
        if not match:
            return None
        info = subprocess.run(
            ["lsappinfo", "info", match.group(0)],
            check=True,
            capture_output=True,
            text=True,
            timeout=2.0,
        ).stdout
    except (subprocess.SubprocessError, OSError):
        return None

    match = re.search(r'bundleID="([^"]+)"', info)
    return match.group(1) if match else None


def make_status(
    data_dir: Path,
    require_cursor_frontmost: bool,
    workspace_filter: str | None,
    all_workspaces: bool,
    max_log_bytes: int,
    stale_after_seconds: float,
) -> dict[str, Any]:
    front_bundle = frontmost_bundle_id()
    cursor_frontmost = front_bundle == CURSOR_BUNDLE_ID

    recent_workspace = recent_workspace_label(data_dir)
    effective_workspace_filter = workspace_filter

    visible_composers = find_visible_composers(data_dir, effective_workspace_filter)
    if not effective_workspace_filter and not all_workspaces:
        visible_composers = newest_workspace_composers(visible_composers)
    newest_events = newest_events_by_composer(data_dir, max_log_bytes)
    visible_states: list[dict[str, Any]] = []

    cutoff = datetime.now().astimezone().timestamp() - stale_after_seconds
    for composer in visible_composers:
        event = newest_events.get(composer.composer_id)
        stale = False
        running = False
        waiting_for_user = False
        if event:
            stale = event.timestamp.timestamp() < cutoff
            running = event.is_active and not stale
            waiting_for_user = event.reason == "user-approval-requested" and not stale

        visible_states.append(
            {
                "composer_id": composer.composer_id,
                "workspace": composer.workspace,
                "workspace_db": composer.workspace_db,
                "workspace_db_mtime": composer.workspace_db_mtime,
                "mode": composer.mode,
                "running": running,
                "waiting_for_user": waiting_for_user,
                "last_event": None
                if not event
                else {
                    "action": event.action,
                    "reason": event.reason,
                    "timestamp": event.timestamp.isoformat(timespec="milliseconds"),
                    "source": event.source,
                    "stale": stale,
                },
            }
        )

    frontmost_ok = cursor_frontmost or not require_cursor_frontmost
    running_visible_agents = [
        item for item in visible_states if frontmost_ok and item["running"]
    ]
    waiting_visible_agents = [
        item for item in visible_states if frontmost_ok and item["waiting_for_user"]
    ]

    if require_cursor_frontmost and not cursor_frontmost:
        state = "cursor_not_frontmost"
    elif running_visible_agents:
        state = "running"
    elif waiting_visible_agents:
        state = "waiting_for_user"
    elif visible_states:
        state = "visible_idle"
    else:
        state = "no_visible_agent"

    return {
        "checked_at": now_iso(),
        "state": state,
        "running": bool(running_visible_agents),
        "waiting_for_user": bool(waiting_visible_agents),
        "cursor_frontmost": cursor_frontmost,
        "cursor_app_frontmost": cursor_frontmost,
        "frontmost_bundle_id": front_bundle,
        "workspace_filter": effective_workspace_filter,
        "recent_workspace": recent_workspace,
        "frontmost_agent": visible_states[0] if visible_states else None,
        "visible_agents": visible_states,
    }


def compact_status_line(status: dict[str, Any]) -> str:
    visible = status.get("visible_agents") or []
    first = visible[0] if visible else {}
    workspace = first.get("workspace", "-")
    composer = str(first.get("composer_id", "-"))[:8]
    return (
        f"{status['checked_at']} state={status['state']} "
        f"running={str(status['running']).lower()} "
        f"cursor_app_frontmost={str(status['cursor_app_frontmost']).lower()} "
        f"workspace={workspace} composer={composer}"
    )


def frontmost_agent_key(status: dict[str, Any]) -> tuple[str, str] | None:
    agent = status.get("frontmost_agent")
    if not isinstance(agent, dict):
        return None
    workspace_db = agent.get("workspace_db")
    composer_id = agent.get("composer_id")
    if not isinstance(workspace_db, str) or not isinstance(composer_id, str):
        return None
    return workspace_db, composer_id


def notification_line(status: dict[str, Any], running_for: float, threshold: float) -> str:
    agent = status.get("frontmost_agent")
    if not isinstance(agent, dict):
        agent = {}
    workspace = agent.get("workspace", "-")
    composer = str(agent.get("composer_id", "-"))[:8]
    return (
        f"{status['checked_at']} notification=frontmost_agent_running "
        f"running_for={running_for:.1f}s threshold={threshold:g}s "
        f"workspace={workspace} composer={composer}"
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
    parser.add_argument("--interval", type=float, default=DEFAULT_INTERVAL_SECONDS, help="poll interval in seconds")
    parser.add_argument(
        "--threshold",
        type=float,
        default=DEFAULT_THRESHOLD_SECONDS,
        help="seconds the frontmost agent tab must run continuously before notification",
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
        help="compatibility no-op; Cursor app focus is ignored unless --require-cursor-frontmost is set",
    )
    parser.add_argument(
        "--require-cursor-frontmost",
        action="store_true",
        help="only report running agents while Cursor is the frontmost macOS app",
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
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    data_dir = args.cursor_data_dir.expanduser()
    if not data_dir.exists():
        print(f"Cursor data directory not found: {data_dir}", file=sys.stderr)
        return 2

    require_cursor_frontmost = args.require_cursor_frontmost and not args.ignore_frontmost
    threshold_seconds = max(args.threshold, 0.0)
    wait_seconds = max(args.wait, 0.0)
    last_change_key: str | None = None
    notifier = ThresholdNotifier(threshold_seconds, wait_seconds)

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
        elif not args.quiet and not args.once and not args.print_on_change and not args.print_every_poll:
            running_for = notifier.observe(status, time.monotonic())
            if running_for is not None:
                print(notification_line(status, running_for, threshold_seconds), flush=True)

        if args.once:
            return 0
        time.sleep(max(args.interval, 0.2))


if __name__ == "__main__":
    raise SystemExit(main())
