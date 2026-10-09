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
from datetime import datetime
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
# Claude Code writes the final reply to the transcript just after Stop fires.
REPLY_WAIT_SECONDS = 3.0

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
    timestamp_ms: int | None = None,
) -> list[dict[str, Any]]:
    """One September message per part of `text`, with the session's provenance."""
    project, branch = provenance(event.get("cwd") or os.getcwd())
    now = int(time.time() * 1000) if timestamp_ms is None else timestamp_ms
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


def reply_messages(event: dict[str, Any], entries: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Claude's text in transcript `entries`, including text between tool calls.

    Each text block keeps its transcript identity and time, so a block read twice
    is a duplicate. Subagent, error, and synthetic entries are not Claude's reply.
    """
    messages = []
    for entry in entries:
        body = entry.get("message") or {}
        content = body.get("content")
        if (
            entry.get("type") != "assistant"
            or entry.get("isSidechain")
            or entry.get("isApiErrorMessage")
            or body.get("model") == "<synthetic>"
            or not entry.get("uuid")
            or not isinstance(content, list)
        ):
            continue
        written = transcript_time(entry.get("timestamp"))
        for index, block in enumerate(content):
            text = block.get("text") if block.get("type") == "text" else None
            if text and text.strip():
                key = f"{entry['uuid']}:{index}"
                messages += message(event, "assistant", key, text, timestamp_ms=written)
    return messages


def transcript_time(stamp: Any) -> int | None:
    """Milliseconds for a transcript timestamp such as `2026-10-09T17:42:31.518Z`."""
    if not isinstance(stamp, str):
        return None
    try:
        # Python 3.9's parser does not accept a trailing Z.
        parsed = datetime.fromisoformat(stamp.replace("Z", "+00:00"))
    except ValueError:
        return None
    return int(parsed.timestamp() * 1000)


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


def upload(messages: list[dict[str, Any]]) -> bool:
    """Archive each message in order; whether all of them are archived."""
    archived = True
    for item in messages:
        status, _ = request("POST", "/v1/messages", item)
        # Entries are stable per event, so a conflict is an earlier upload of the
        # same event with another timestamp: it is already archived.
        if status not in (200, 201, 409):
            warn(f"September rejected a {item['kind']} message with HTTP {status}")
            archived = False
    return archived


def ready_snapshot(snapshot: str) -> dict[str, Any] | None:
    """Freeze this interaction, sized for Claude, and wait briefly for summaries."""
    status, body = request("PUT", f"/v1/snapshots/{snapshot}?within={VIEW_BYTES}")
    deadline = time.monotonic() + READY_WAIT_SECONDS
    while status == 202 and time.monotonic() < deadline:
        time.sleep(0.5)
        status, body = request("GET", f"/v1/snapshots/{snapshot}")
    return body if status == 200 else None


# Each session's state: its snapshot for the zoom stamp, and how much of its
# transcript has been archived.


def state_file(session_id: str) -> Path:
    data = os.environ.get("CLAUDE_PLUGIN_DATA") or Path.home() / ".claude/plugins/data/september"
    # Session IDs are UUIDs; keep only safe characters regardless.
    name = "".join(c for c in session_id if c.isalnum() or c == "-")
    return Path(data) / "sessions" / f"{name}.json"


def load_state(session_id: str) -> dict[str, Any]:
    try:
        state = json.loads(state_file(session_id).read_text())
    except (OSError, ValueError):
        return {}
    return state if isinstance(state, dict) else {}


def save_state(session_id: str, state: dict[str, Any]) -> None:
    path = state_file(session_id)
    path.parent.mkdir(parents=True, exist_ok=True)
    # Hooks for parallel tool calls run at once; never leave a half-written file.
    partial = path.with_suffix(f".{os.getpid()}.tmp")
    partial.write_text(json.dumps(state))
    os.replace(partial, path)


def read_transcript(path: str | None, start: int) -> tuple[list[dict[str, Any]], int]:
    """The complete transcript entries after byte `start`, and where they end."""
    if not path:
        return [], start
    try:
        transcript = open(path, "rb")
    except FileNotFoundError:
        # A new session's first prompt arrives before its transcript exists.
        return [], start
    with transcript:
        if start > os.fstat(transcript.fileno()).st_size:
            start = 0  # A different, shorter file: read it from the beginning.
        transcript.seek(start)
        data = transcript.read()
    # Claude Code may still be writing the last line; leave it for next time.
    complete = data.rfind(b"\n") + 1
    entries = []
    for line in data[:complete].splitlines():
        try:
            entry = json.loads(line)
        except ValueError:
            continue
        if isinstance(entry, dict):
            entries.append(entry)
    return entries, start + complete


def upload_replies(event: dict[str, Any], final: str = "") -> None:
    """Archive Claude's text written since the last upload, in transcript order.

    With the turn's `final` reply, wait briefly for the transcript to hold it. A
    reply that still is not there is archived by the next prompt or session end.
    """
    session = event["session_id"]
    start = load_state(session).get("transcript_read", 0)
    deadline = time.monotonic() + REPLY_WAIT_SECONDS
    while True:
        entries, end = read_transcript(event.get("transcript_path"), start)
        replies = reply_messages(event, entries)
        written = any(final.rstrip().endswith(reply["text"].rstrip()) for reply in replies)
        if not final.strip() or written or time.monotonic() >= deadline:
            break
        time.sleep(0.1)
    if upload(replies):
        save_state(session, {**load_state(session), "transcript_read": end})


# Hook handlers.


def session_start(event: dict[str, Any]) -> dict[str, Any]:
    snapshot = str(uuid.uuid4())
    state = {**load_state(event["session_id"]), "snapshot": snapshot}
    # A resumed transcript's earlier text was archived when it was written.
    if "transcript_read" not in state:
        path = event.get("transcript_path")
        state["transcript_read"] = os.path.getsize(path) if path and os.path.exists(path) else 0
    save_state(event["session_id"], state)
    ready = ready_snapshot(snapshot)
    context = f"{MEMORY_GUIDE}\n\n{ready['view']}" if ready else UNREADY_GUIDE
    return hook_output("SessionStart", additionalContext=context)


def stamp(event: dict[str, Any]) -> dict[str, Any] | None:
    snapshot = load_state(event["session_id"]).get("snapshot")
    if snapshot is None:
        return None
    arguments = {**(event.get("tool_input") or {}), "snapshot": snapshot}
    return hook_output("PreToolUse", permissionDecision="allow", updatedInput=arguments)


def hook_output(event_name: str, **fields: Any) -> dict[str, Any]:
    return {"hookSpecificOutput": {"hookEventName": event_name, **fields}}


def warn(text: str) -> None:
    print(f"september: {text}", file=sys.stderr)


def prompt(event: dict[str, Any]) -> None:
    # A reply the transcript did not yet hold at Stop comes before the new prompt.
    upload_replies(event)
    upload(prompt_messages(event))


def tool(event: dict[str, Any]) -> None:
    # Text Claude wrote before this call comes first, keeping the archive in order.
    upload_replies(event)
    upload(tool_messages(event))


HANDLERS = {
    "session-start": session_start,
    "prompt": prompt,
    "tool": tool,
    "reply": lambda event: upload_replies(event, event.get("last_assistant_message") or ""),
    "session-end": upload_replies,
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
