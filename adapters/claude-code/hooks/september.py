#!/usr/bin/env python3
"""Claude Code hooks for September: capture messages and load memory.

Each subcommand reads one hook event as JSON on stdin and translates it into
September's harness-neutral HTTP API. Failures are reported on stderr and never
block Claude. Standard library only.
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path
from typing import Any

HARNESS = "claude"
SERVER = os.environ.get("SEPTEMBER_URL", "http://127.0.0.1:3000").rstrip("/")
# September's own MCP tools: stamped with the snapshot, never archived.
TOOL_PREFIX = "mcp__plugin_september_september__"
MAX_TEXT_BYTES = 65_536
# The gist clips tool output to its head and tail, this many characters in all.
TOOL_OUTPUT_CHARS = 30_000
READY_WAIT_SECONDS = 20.0

MEMORY_GUIDE = """\
September memory: the whole chat across your sessions and tools, oldest first, \
as one-line summaries inside <chat> tags:

  id+n|text   the n messages from id on, summarized (newlines as spaces)

Recent lines cover one message each; older lines cover more. The latest word \
on a thing is the truth. When you need anything from earlier work, find its \
latest mention here and zoom until you have it whole, before you act, guess or \
ask. zoom(start, length) opens line start+length into the two lines it was made \
from; length 1 gives the message whole. date(id) gives a message's date and \
time. Summaries keep little of tool output, so say in your reply what you \
learned that will matter later."""

# Claude Code saves hook context over 10,000 characters to a file and shows only
# a preview, so the guide and the view must fit within it together.
CONTEXT_CHARS = 10_000
VIEW_BYTES = CONTEXT_CHARS - len(MEMORY_GUIDE) - 64

UNREADY_GUIDE = """\
September memory is still summarizing earlier messages, so it is not loaded \
for this session. zoom and date may answer once it is ready."""


# Translating events into September messages.


def message(
    event: dict[str, Any],
    kind: str,
    entry: str,
    text: str,
    call_id: str | None = None,
) -> list[dict[str, Any]]:
    """One September message per part of `text`, with the session's provenance."""
    project, branch = provenance(event.get("cwd") or os.getcwd())
    now = int(time.time() * 1000)
    return [
        {
            "source": {
                "harness": HARNESS,
                "session": event["session_id"],
                "entry": entry,
                "part": index,
            },
            "project": project,
            "branch": branch,
            "timestamp_ms": now,
            "kind": kind,
            "call_id": call_id,
            "text": part,
        }
        for index, part in enumerate(split(text))
    ]


def prompt_messages(event: dict[str, Any]) -> list[dict[str, Any]]:
    entry = event.get("prompt_id") or digest(event["prompt"])
    return message(event, "user", entry, event["prompt"])


def tool_messages(event: dict[str, Any]) -> list[dict[str, Any]]:
    name = event["tool_name"]
    if name.startswith(TOOL_PREFIX):
        return []
    call_id = event["tool_use_id"]
    call = f"{name} {as_text(event.get('tool_input'))}"
    result = clip(as_text(event.get("tool_response")))
    return [
        *message(event, "tool_call", f"{call_id}:call", call, call_id),
        *message(event, "tool_result", f"{call_id}:result", result, call_id),
    ]


def reply_messages(event: dict[str, Any]) -> list[dict[str, Any]]:
    text = event.get("last_assistant_message") or ""
    if not text.strip():
        return []
    # A repeated Stop for the same reply is a duplicate, not a conflict.
    entry = f"{event.get('prompt_id') or 'none'}:reply:{digest(text)}"
    return message(event, "assistant", entry, text)


def as_text(value: Any) -> str:
    if isinstance(value, str):
        return value
    return json.dumps(value, ensure_ascii=False, sort_keys=True)


def clip(text: str) -> str:
    """Keep the head and tail of long tool output, saying what was cut."""
    if len(text) <= TOOL_OUTPUT_CHARS:
        return text
    half = TOOL_OUTPUT_CHARS // 2
    cut = len(text) - 2 * half
    return f"{text[:half]}\n[… {cut} characters clipped …]\n{text[-half:]}"


def split(text: str) -> list[str]:
    """Split text into parts of at most MAX_TEXT_BYTES UTF-8 bytes, losing nothing."""
    data = text.encode()
    parts = []
    while data:
        end = min(len(data), MAX_TEXT_BYTES)
        # Back up to a character boundary: continuation bytes are 0b10xxxxxx.
        while end < len(data) and data[end] & 0xC0 == 0x80:
            end -= 1
        parts.append(data[:end].decode())
        data = data[end:]
    return parts or [text]


def digest(text: str) -> str:
    return hashlib.sha256(text.encode()).hexdigest()[:16]


def provenance(cwd: str) -> tuple[str, str]:
    """The git project and branch for `cwd`, or its folder name and `none`."""
    root = git(cwd, "rev-parse", "--show-toplevel")
    branch = git(cwd, "rev-parse", "--abbrev-ref", "HEAD")
    project = Path(root or cwd).name or "unknown"
    return project, branch or "none"


def git(cwd: str, *args: str) -> str | None:
    try:
        result = subprocess.run(
            ["git", "-C", cwd, *args],
            capture_output=True,
            text=True,
            timeout=2,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    return result.stdout.strip() or None


# Talking to September.


def request(method: str, path: str, body: Any = None) -> tuple[int, Any]:
    data = None if body is None else json.dumps(body).encode()
    headers = {"Content-Type": "application/json"} if data else {}
    call = urllib.request.Request(f"{SERVER}{path}", data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(call, timeout=5) as response:
            raw = response.read()
            return response.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as error:
        return error.code, None


def upload(messages: list[dict[str, Any]]) -> None:
    for item in messages:
        status, _ = request("POST", "/v1/messages", item)
        # Entries are stable per event, so a conflict is an earlier upload of the
        # same event with another timestamp: it is already archived.
        if status not in (200, 201, 409):
            warn(f"September rejected a {item['kind']} message with HTTP {status}")


def ready_snapshot(snapshot: str) -> dict[str, Any] | None:
    """Freeze this interaction, sized for Claude, and wait briefly for summaries."""
    status, body = request("PUT", f"/v1/snapshots/{snapshot}?within={VIEW_BYTES}")
    deadline = time.monotonic() + READY_WAIT_SECONDS
    while status == 202 and time.monotonic() < deadline:
        time.sleep(0.5)
        status, body = request("GET", f"/v1/snapshots/{snapshot}")
    return body if status == 200 else None


# Remembering each session's snapshot for the zoom stamp.


def state_file(session_id: str) -> Path:
    data = os.environ.get("CLAUDE_PLUGIN_DATA") or Path.home() / ".claude/plugins/data/september"
    # Session IDs are UUIDs; keep only safe characters regardless.
    name = "".join(c for c in session_id if c.isalnum() or c == "-")
    return Path(data) / "sessions" / f"{name}.json"


def remember(session_id: str, snapshot: str) -> None:
    path = state_file(session_id)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"snapshot": snapshot}))


def recall(session_id: str) -> str | None:
    try:
        return json.loads(state_file(session_id).read_text())["snapshot"]
    except (OSError, ValueError, KeyError):
        return None


# Hook handlers.


def session_start(event: dict[str, Any]) -> dict[str, Any]:
    snapshot = str(uuid.uuid4())
    remember(event["session_id"], snapshot)
    ready = ready_snapshot(snapshot)
    context = f"{MEMORY_GUIDE}\n\n{ready['view']}" if ready else UNREADY_GUIDE
    return hook_output("SessionStart", additionalContext=context)


def stamp(event: dict[str, Any]) -> dict[str, Any] | None:
    snapshot = recall(event["session_id"])
    if snapshot is None:
        return None
    arguments = {**(event.get("tool_input") or {}), "snapshot": snapshot}
    return hook_output("PreToolUse", permissionDecision="allow", updatedInput=arguments)


def hook_output(event_name: str, **fields: Any) -> dict[str, Any]:
    return {"hookSpecificOutput": {"hookEventName": event_name, **fields}}


def warn(text: str) -> None:
    print(f"september: {text}", file=sys.stderr)


HANDLERS = {
    "session-start": session_start,
    "prompt": lambda event: upload(prompt_messages(event)),
    "tool": lambda event: upload(tool_messages(event)),
    "reply": lambda event: upload(reply_messages(event)),
    "stamp": stamp,
}


def main(argv: list[str]) -> int:
    if len(argv) != 2 or argv[1] not in HANDLERS:
        warn(f"usage: september.py {{{','.join(HANDLERS)}}}")
        return 0
    try:
        output = HANDLERS[argv[1]](json.load(sys.stdin))
    except (OSError, ValueError, KeyError, TypeError) as error:
        # Never block Claude: memory is best effort from the adapter's side.
        warn(f"{argv[1]} failed: {error.__class__.__name__}: {error}")
        return 0
    if output is not None:
        json.dump(output, sys.stdout)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
