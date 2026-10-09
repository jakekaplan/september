"""Translation from Claude Code hook events to September messages."""

import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import september


class Translation(unittest.TestCase):
    def setUp(self) -> None:
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.cwd = folder.name
        self.event = {"session_id": "s-1", "cwd": self.cwd, "prompt_id": "p-1"}

    def test_a_prompt_is_one_user_message_with_folder_provenance(self) -> None:
        [message] = september.prompt_messages({**self.event, "prompt": "use Postgres later"})
        self.assertEqual(
            message["source"],
            {"harness": "claude", "session": "s-1", "entry": "p-1", "part": 0},
        )
        self.assertEqual((message["kind"], message["text"]), ("user", "use Postgres later"))
        self.assertEqual(message["project"], Path(self.cwd).name)
        self.assertEqual(message["branch"], "none")

    def test_a_tool_use_is_a_call_and_a_clipped_result_sharing_its_id(self) -> None:
        output = "a" * 20_000 + "b" * 20_000
        call, result = september.tool_messages(
            {
                **self.event,
                "tool_name": "Bash",
                "tool_input": {"command": "ls"},
                "tool_response": output,
                "tool_use_id": "t-1",
            }
        )
        self.assertEqual(call["source"]["entry"], "t-1:call")
        self.assertEqual(call["text"], 'Bash {"command": "ls"}')
        self.assertEqual(result["source"]["entry"], "t-1:result")
        self.assertEqual((call["call_id"], result["call_id"]), ("t-1", "t-1"))
        self.assertTrue(result["text"].startswith("a" * 15_000 + "\n[… 10000 characters"))
        self.assertTrue(result["text"].endswith("b" * 15_000))

    def test_septembers_own_tools_are_not_archived(self) -> None:
        event = {**self.event, "tool_name": "mcp__plugin_september_september__zoom"}
        self.assertEqual(september.tool_messages(event), [])

    def test_a_repeated_reply_keeps_its_entry_and_a_new_one_does_not(self) -> None:
        def entry(text: str) -> str:
            event = {**self.event, "last_assistant_message": text}
            return september.reply_messages(event)[0]["source"]["entry"]

        self.assertEqual(entry("done"), entry("done"))
        self.assertNotEqual(entry("done"), entry("done again"))
        self.assertEqual(september.reply_messages({**self.event, "last_assistant_message": " "}), [])

    def test_long_text_splits_into_numbered_parts_without_losing_bytes(self) -> None:
        text = "é" * 40_000
        parts = september.split(text)
        self.assertEqual(len(parts), 2)
        self.assertTrue(all(len(part.encode()) <= september.MAX_TEXT_BYTES for part in parts))
        self.assertEqual("".join(parts), text)
        messages = september.prompt_messages({**self.event, "prompt": text})
        self.assertEqual([m["source"]["part"] for m in messages], [0, 1])


class Upload(unittest.TestCase):
    def test_a_conflict_means_the_event_is_already_archived(self) -> None:
        item = {"kind": "assistant"}
        for status, warned in [(201, False), (200, False), (409, False), (400, True)]:
            with (
                mock.patch.object(september, "request", return_value=(status, None)),
                mock.patch.object(september, "warn") as warn,
            ):
                september.upload([item])
            self.assertEqual(warn.called, warned, status)


class SessionStart(unittest.TestCase):
    def setUp(self) -> None:
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        patch = mock.patch.dict(os.environ, {"CLAUDE_PLUGIN_DATA": folder.name})
        patch.start()
        self.addCleanup(patch.stop)

    def test_the_view_is_requested_small_enough_to_reach_claude_whole(self) -> None:
        view = "<chat>\n" + "x" * (september.VIEW_BYTES - 14) + "</chat>"
        calls = []

        def request(method: str, path: str, body: object = None) -> tuple[int, object]:
            calls.append((method, path))
            return 200, {"status": "ready", "view": view}

        with mock.patch.object(september, "request", request):
            output = september.session_start({"session_id": "s-1"})
        self.assertEqual(calls[0][0], "PUT")
        self.assertTrue(calls[0][1].endswith(f"?within={september.VIEW_BYTES}"))
        context = output["hookSpecificOutput"]["additionalContext"]
        self.assertTrue(context.endswith(view))
        self.assertLessEqual(len(context), september.CONTEXT_CHARS)


class Stamp(unittest.TestCase):
    def setUp(self) -> None:
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        patch = mock.patch.dict(os.environ, {"CLAUDE_PLUGIN_DATA": folder.name})
        patch.start()
        self.addCleanup(patch.stop)

    def test_zoom_calls_get_the_sessions_snapshot(self) -> None:
        september.remember("s-1", "snap-1")
        output = september.stamp({"session_id": "s-1", "tool_input": {"start": 0, "length": 4}})
        self.assertEqual(
            output,
            {
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "allow",
                    "updatedInput": {"start": 0, "length": 4, "snapshot": "snap-1"},
                }
            },
        )

    def test_a_session_without_a_snapshot_is_left_alone(self) -> None:
        self.assertIsNone(september.stamp({"session_id": "unknown", "tool_input": {}}))


if __name__ == "__main__":
    unittest.main()
