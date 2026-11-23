"""Claude Agent SDK 历史读取器的回归样例：只读取临时日志和进程登记，不连接或启动 Claude。

用户要求改用 Claude Agent SDK 对接后，旧 daemon 后台任务与终端画面识别的测试随旧代码一并删除；
执行状态、审批、问答和消息回执由 Rust 对端的 stream-json 连接提供，对应测试在 claude_sdk/ 内。"""
import json
import contextlib
import io
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import MagicMock, patch


source = Path(__file__).resolve().parents[1] / "crates/elves-harness/src/claude_shared.py"
namespace = {"__name__": "claude_shared_history_test"}
exec(compile(source.read_text(encoding="utf-8"), str(source), "exec"), namespace)
SdkReader = namespace["SdkReader"]

NativeDiscovery = namespace["NativeDiscovery"]
trust_source = source.with_name("claude_project_trust.py")
trust_namespace = {"__name__": "claude_project_trust_test"}
exec(compile(trust_source.read_text(encoding="utf-8"), str(trust_source), "exec"), trust_namespace)


class SharedQueueHistoryTests(unittest.TestCase):


    dialog_text = ("Switch model?\nYour next response will be slower and use more tokens\n"
                   "This conversation is cached for the current model. Switching to Opus 5.5 means "
                   "the full history gets re-read.\n❯ 1. Yes, switch to Opus 5.5\n  2. No, go back\n")

    def waiting_dialog(self):
        native = SdkReader("unused", "test-session")
        native.initialized = True
        native.model = "sonnet"
        native.messages = [{"type": "system", "subtype": "turn_duration", "uuid": "old-done"}]
        native.setting_catalog = [{"value": "opus", "displayName": "Opus 5.5"},
                                  {"value": "sonnet", "displayName": "Sonnet 5.5"}]
        native.actual_settings = {"model": "sonnet"}
        native.job = {"short": "saved-job", "sessionId": "test-session", "state": "done", "tempo": "idle",
                      "sessionStatus": "waiting", "sessionStatusAtMs": 2000, "needs": "dialog open"}
        native.lookup = lambda: native.job
        native.read_messages = lambda: None
        native.context_configuration = lambda: {}
        native.screen.feed(self.dialog_text)
        return native


    def test_native_setting_result_roundtrips_through_history_and_reconnection(self):
        rows = [
            {"type": "assistant", "uuid": "previous", "timestamp": 1000, "message": {"model": "sonnet"}},
            {"type": "user", "uuid": "command", "timestamp": 2000, "entrypoint": "cli", "message": {
                "content": "<command-name>/model</command-name><command-message>model</command-message><command-args>opus</command-args>"}},
            {"type": "user", "uuid": "result", "timestamp": 2000, "entrypoint": "cli", "parentUuid": "command",
             "message": {"content": "<local-command-stdout>Set model to `Opus 5.5` and saved as your default for new sessions</local-command-stdout>"}},
        ]
        native = self.read(rows)
        self.assertEqual(native.settings_projection()["model"], "opus")
        self.assertEqual(native.setting_changes["model"]["state"], "applied")
        resumed = SdkReader("unused", "test-session")
        resumed.restore_settings(native.control_projection())
        self.assertEqual(resumed.settings_projection(), native.settings_projection())
        quote = {**rows[-1], "uuid": "quoted", "promptSource": "typed", "origin": {"kind": "human"}}
        rows.append(quote)
        self.assertEqual(self.read(rows).settings_projection()["model"], "opus")


    def test_goal_statistics_use_only_native_numbers_and_reset_on_replacement(self):
        start = {"type": "attachment", "uuid": "goal", "timestamp": 1000,
                 "attachment": {"type": "goal_status", "condition": "目标", "met": False, "sentinel": True}}
        verdict = {"type": "attachment", "uuid": "verdict", "timestamp": 30000,
                   "attachment": {"type": "goal_status", "condition": "目标", "met": True,
                                  "tokens": 134, "durationMs": 28290, "iterations": 1}}
        native = self.read([start, verdict])
        self.assertEqual((native.goal["tokensUsed"], native.goal["timeUsedSeconds"], native.goal["iterations"]),
                         (134, 28.29, 1))
        native.observe_control_record({**start, "uuid": "replacement"})
        self.assertNotIn("tokensUsed", native.goal)
        native.observe_control_record({**verdict, "attachment": {**verdict["attachment"],
            "tokens": True, "durationMs": -1, "iterations": 9007199254740992}})
        self.assertNotIn("tokensUsed", native.goal)
        self.assertNotIn("timeUsedSeconds", native.goal)
        self.assertNotIn("iterations", native.goal)


    def test_compaction_model_never_uses_current_session_selection(self):
        native = self.native_active()
        native.model = "current-session-model"
        native.observe_control_record({"type": "system", "subtype": "compact_boundary",
            "uuid": "without-model", "timestamp": 3000, "compactMetadata": {"trigger": "manual"}})
        self.assertNotIn("compactionModel", native.compaction_events["without-model"])
        native.observe_control_record({"type": "system", "subtype": "compact_boundary",
            "uuid": "explicit-model", "timestamp": 4000,
            "compactMetadata": {"trigger": "manual", "model": "explicit-compression-model"}})
        self.assertEqual(native.compaction_events["explicit-model"]["compactionModel"], "explicit-compression-model")
        native.observe_control_record({"type": "system", "subtype": "compact_boundary",
            "uuid": "invalid-model", "timestamp": 5000,
            "compactMetadata": {"trigger": "manual", "compactionModel": "bad\nmodel"}})
        self.assertNotIn("compactionModel", native.compaction_events["invalid-model"])


    def test_native_goal_records_distinguish_clear_from_evaluated_completion(self):
        def goal(identity, at, met=False, sentinel=True):
            return {"type": "attachment", "uuid": identity, "timestamp": at,
                    "version": "2.1.322", "attachment": {"type": "goal_status",
                    "condition": "完成验收", "met": met, "sentinel": sentinel}}
        native = self.read([goal("start", 1000), goal("clear", 2000, True)])
        self.assertIsNone(native.goal, "clear 的 met=true sentinel 不能显示目标已完成")
        self.assertEqual(native.goal_revision, "clear")
        native = self.read([goal("start", 1000), goal("verdict", 2000, True, False),
                            goal("end", 2001, True)])
        self.assertEqual(native.goal["status"], "completed")
        self.assertEqual(native.goal["nativeId"], "start")
        self.assertTrue(native.native_capabilities["goal"])

    def test_native_control_records_preserve_real_quotes_and_clean_command_output(self):
        command = {"type": "user", "uuid": "command", "entrypoint": "cli",
                   "message": {"content": "<command-name>/goal</command-name><command-message>goal</command-message><command-args>完成验收</command-args>"}}
        native = self.read([command, {"type": "user", "uuid": "output", "parentUuid": "command",
                            "turnOrigin": "human", "message": {"content": "<local-command-stdout>Goal set: 完成验收</local-command-stdout>"}},
                            {**command, "uuid": "quote", "origin": {"kind": "human"}, "promptSource": "typed"}])
        self.assertEqual(native.messages[0]["nativeControlCommand"]["value"], "完成验收")
        self.assertTrue(namespace["real_user"](native.messages[0]))
        self.assertFalse(namespace["real_user"](native.messages[1]))
        self.assertNotIn("nativeControlCommand", native.messages[2])
        self.assertTrue(namespace["real_user"](native.messages[2]))

    def test_compaction_boundary_survives_and_only_finishes_a_newer_native_operation(self):
        native = self.read([{"type": "user", "uuid": "u", "message": {"content": "问题"}}])
        native.native_commands = [{"id": "request", "name": "compact", "argument": "",
                                   "state": "pending", "issuedAt": 2000, "baseline": "old"}]
        native.compaction = {"state": "pending", "itemId": "request"}
        old = {"type": "system", "subtype": "compact_boundary", "uuid": "old", "timestamp": 1000}
        native.observe_control_record(old)
        self.assertEqual(native.native_commands[0]["state"], "pending")
        self.assertEqual(native.compaction["state"], "pending", "旧边界不能清掉正在等待的新请求")
        native.observe_control_record({**old, "uuid": "new", "timestamp": 3000})
        self.assertEqual(native.native_commands[0]["state"], "completed")
        self.assertEqual(native.compaction, {"state": "idle", "itemId": "new"})
        reader = self.read([old])
        self.assertEqual(reader.messages[0]["subtype"], "compact_boundary")


    def test_completed_native_job_clears_stale_working_and_permission_states(self):
        native = SdkReader("unused", "test-session")
        native.initialized = True
        native.read_messages = lambda: None
        native.context_configuration = lambda: {}
        native.messages = [{"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": "old-tool", "name": "Write", "input": {}}]}}]
        for tempo in ["active", "blocked"]:
            native.lookup = lambda: {"short": "saved-job", "sessionId": "test-session",
                                     "state": "done", "tempo": tempo, "needs": "permission"}
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                native.poll(force=True)
            frame = json.loads(output.getvalue())
            self.assertEqual(frame["native_status"]["tempo"], "idle")
            self.assertEqual(frame["requests"], {})


    def test_history_replacement_with_same_length_reloads_native_records(self):
        native = self.read([{"type": "user", "uuid": "user-one", "message": {"content": "first"}}])
        original_size = native.log_path.stat().st_size
        replacement = native.log_path.with_suffix(".replacement")
        replacement.write_text(json.dumps(
            {"type": "user", "uuid": "user-two", "message": {"content": "other"}}) + "\n", encoding="utf-8")
        self.assertEqual(replacement.stat().st_size, original_size)
        replacement.replace(native.log_path)
        native.read_messages()
        self.assertEqual([row["uuid"] for row in native.messages], ["user-two"])
        with native.log_path.open("a", encoding="utf-8") as stream:
            stream.write(json.dumps({"type": "assistant", "uuid": "reply",
                                      "message": {"content": [{"type": "text", "text": "new"}]}}) + "\n")
        native.read_messages()
        self.assertEqual([row["uuid"] for row in native.messages], ["user-two", "reply"])
        native.read_messages()
        self.assertEqual(len(native.messages), 2)

    def test_invalid_metadata_does_not_prevent_later_history_from_loading(self):
        native = self.read([
            None, ["auxiliary"], {"type": "user", "uuid": "diagnostic", "message": None},
            {"type": "user", "uuid": "valid", "timestamp": "invalid-time", "message": {"content": "问题"}},
            {"type": "assistant", "uuid": "answer", "timestamp": 1794416642798,
             "message": {"content": [{"type": "text", "text": "回复"}]}},
        ])
        self.assertEqual([row["uuid"] for row in native.messages], ["valid", "answer"])
        self.assertEqual(native.messages[0]["timestampMs"], 0)
        self.assertEqual(native.messages[1]["timestampMs"], 1794416642798)

    def native_active(self):
        native = SdkReader("unused", "test-session")
        native.messages = [{"type": "user", "uuid": "active-user", "message": {"content": "正在处理"}}]
        native.read_messages = lambda: None
        native.poll = lambda **kwargs: None
        native.lookup = lambda: {"short": "saved-job", "sessionId": "test-session",
                                 "tempo": "active"}
        return native


    # 真实 CLI 2.1.325（本地模拟 API + 并行 MCP / 读取 / 搜索）的工具分组行；末尾展开提示可有可无。
    # 过去式来自 CLI 源码，尚未在真实画面观察到。
    TOOL_GROUP_LINES = (
        "Calling playwright 3 times…",
        "Calling fake 3 times… (ctrl+o to expand)",
        "Reading 2 files… (ctrl+o to expand)",
        "Searching for 1 pattern, reading 2 files, calling fake, other 3 times… (ctrl+o to expand)",
        "Called fake 3 times (ctrl+o to expand)",
        "Searched for 1 pattern, read 2 files, called fake, other 3 times",
        "Listing 2 directories…",
    )


    def test_native_skill_wrapper_confirms_the_original_slash_input_once(self):
        text = ("<command-message>simplify</command-message>\n"
                "<command-name>/simplify</command-name>\n<command-args>probe</command-args>")
        user = {"type": "user", "uuid": "skill-user", "entrypoint": "sdk-cli", "promptId": "skill-prompt",
                "message": {"content": text}}
        native = self.read([user], pending_text="/simplify probe")
        self.assertEqual(native.messages[0]["message"]["content"], "/simplify probe")
        self.assertEqual(native.clients, {"skill-user": "web-client"})
        self.assertEqual(native.pending, [])
        native.read_messages()
        self.assertEqual(native.messages[0]["message"]["content"], "/simplify probe")
        spaced = self.read([user], pending_text="/simplify   probe")
        self.assertEqual(spaced.messages[0]["message"]["content"], "/simplify probe")
        self.assertEqual(spaced.clients, {"skill-user": "web-client"})
        self.assertEqual(spaced.pending, [])
        companion = {"type": "user", "uuid": "skill-body", "parentUuid": "skill-user",
                     "promptId": "skill-prompt", "isMeta": True, "turnCompanion": True,
                     "message": {"content": [{"type": "text", "text": "Native skill body"}]}}
        reopened = self.read([user, companion])
        self.assertEqual(reopened.messages[0]["message"]["content"], "/simplify probe",
                         "重读原生日志也按同一展开关系恢复显示，不依赖网页临时缓存")
        self.assertFalse(namespace["real_user"](reopened.messages[1]))

    def test_native_skill_wrapper_does_not_rewrite_literal_quotes_or_unrelated_companions(self):
        text = ("<command-message>simplify</command-message>\n"
                "<command-name>/simplify</command-name>\n<command-args>probe</command-args>")
        user = {"type": "user", "uuid": "quoted-user", "entrypoint": "sdk-cli", "promptId": "quote-prompt",
                "message": {"content": text}}
        quote = self.read([user], pending_text=text)
        self.assertEqual(quote.messages[0]["message"]["content"], text)
        self.assertEqual(quote.clients, {"quoted-user": "web-client"})
        self.assertIsNone(quote.messages[0].get("nativeSkillCommand"))
        unrelated = {"type": "user", "uuid": "other-body", "parentUuid": "other-user",
                     "promptId": "quote-prompt", "isMeta": True, "turnCompanion": True,
                     "message": {"content": "Unrelated metadata"}}
        native = self.read([user, unrelated])
        self.assertEqual(native.messages[0]["message"]["content"], text)
        self.assertEqual(native.clients, {})


    def test_preview_basis_changes_for_real_text_not_tool_metadata_or_synthetic_placeholders(self):
        native = self.native_active()
        basis = native.preview_context()[1]
        native.messages.append({"type": "assistant", "uuid": "tool-row", "message": {
            "id": "tool-call", "content": [{"type": "tool_use", "id": "tool", "name": "Read", "input": {}}]}})
        self.assertEqual(native.preview_context()[1], basis)
        native.messages.append({"type": "assistant", "uuid": "placeholder", "isApiErrorMessage": False,
                                "message": {"id": "placeholder", "model": "<synthetic>",
                                            "content": [{"type": "text", "text": "No response requested."}]}})
        self.assertEqual(native.preview_context()[1], basis)
        native.messages.append({"type": "assistant", "uuid": "real-text", "message": {
            "id": "real-text", "model": "real-model",
            "content": [{"type": "text", "text": "No response requested."}]}})
        self.assertNotEqual(native.preview_context()[1], basis, "真实正文不能按同名文本隐藏")


    def read(self, rows, after=0, job_id="", pending_text="待接收消息"):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        log = Path(folder.name) / "native.jsonl"
        log.write_text("".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows),
                       encoding="utf-8")
        native = SdkReader("unused", "test-session")
        native.log_path = log
        native.pending = [{"clientUserMessageId": "web-client", "_nativeText": pending_text,
                           "_after": after}]
        native.read_messages()
        return native

    def test_parallel_tool_results_stay_on_the_current_branch(self):
        # 2.1.323 实测：并行 TaskCreate 的两条结果分别挂在各自的 tool_use 行下，只有后一条在当前链上。
        def use(uuid, parent, tool):
            return {"type": "assistant", "uuid": uuid, "parentUuid": parent,
                    "message": {"content": [{"type": "tool_use", "id": tool, "name": "TaskCreate", "input": {}}]}}

        def result(uuid, parent, tool):
            return {"type": "user", "uuid": uuid, "parentUuid": parent,
                    "message": {"content": [{"type": "tool_result", "tool_use_id": tool, "content": "ok"}]}}

        native = self.read([
            {"type": "user", "uuid": "u0", "message": {"content": "创建任务"}},
            use("a1", "u0", "t1"), use("a2", "a1", "t2"),
            result("r1", "a1", "t1"), result("r2", "a2", "t2"),
            {"type": "assistant", "uuid": "a3", "parentUuid": "r2", "message": {"content": [{"type": "text", "text": "完成"}]}},
            # 被回退的旧分支：同样挂在当前链上的提示之后，但答复的调用不在当前分支。
            use("old-a", "u0", "t-old"), result("old-r", "old-a", "t-old"),
            {"type": "user", "uuid": "u1", "parentUuid": "a3", "message": {"content": "继续"}},
        ], pending_text="无关")
        self.assertEqual([row["uuid"] for row in native.messages], ["u0", "a1", "a2", "r1", "r2", "a3", "u1"])

    def test_tool_results_forward_only_render_metadata_for_their_single_call(self):
        patch_rows = [{"oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1, "lines": ["-旧", "+新"]}]
        native = self.read([
            {"type": "user", "uuid": "u", "message": {"content": "改文件"}},
            {"type": "assistant", "uuid": "a", "parentUuid": "u", "message": {"id": "m", "content": [
                {"type": "tool_use", "id": "e1", "name": "Edit", "input": {"file_path": "/w/a"}},
                {"type": "tool_use", "id": "b1", "name": "Bash",
                 "input": {"command": "sleep 9", "run_in_background": True}}]}},
            {"type": "user", "uuid": "r1", "parentUuid": "a", "message": {"content": [
                {"type": "tool_result", "tool_use_id": "e1", "content": "ok"}]},
             "toolUseResult": {"filePath": "/w/a", "oldString": "旧", "newString": "新",
                               "originalFile": "旧\n" * 1000, "structuredPatch": patch_rows, "userModified": False}},
            {"type": "user", "uuid": "r2", "parentUuid": "r1", "message": {"content": [
                {"type": "tool_result", "tool_use_id": "b1", "content": "running"}]},
             "toolUseResult": {"stdout": "x" * 1000, "stderr": "", "interrupted": False, "backgroundTaskId": "bg1"}},
            {"type": "user", "uuid": "r3", "parentUuid": "r2", "message": {"content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "a"},
                {"type": "tool_result", "tool_use_id": "t2", "content": "b"}]},
             "toolUseResult": {"backgroundTaskId": "ambiguous"}},
        ])
        rows = {row["uuid"]: row for row in native.messages}
        self.assertEqual(rows["r1"]["nativeToolResult"], {"toolUseId": "e1", "filePath": "/w/a",
                                                          "fileChange": "update", "structuredPatch": patch_rows})
        self.assertEqual(rows["r2"]["nativeToolResult"], {"toolUseId": "b1", "backgroundTaskId": "bg1"})
        self.assertNotIn("nativeToolResult", rows["r3"], "多个结果共用元数据时归属不明，不能转发")
        self.assertFalse(any("toolUseResult" in row for row in native.messages), "原始 originalFile/stdout 不随历史下发")
        tool_result = namespace["native_tool_result"]
        single = {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "w"}]}}
        oversized = [{"oldStart": 0, "oldLines": 0, "newStart": 1, "newLines": 1,
                      "lines": ["+" + "x" * namespace["NATIVE_PATCH_LIMIT"]]}]
        self.assertEqual(tool_result({**single, "toolUseResult": {"filePath": "/w/b", "type": "update",
                                                                   "structuredPatch": oversized}}),
                         {"toolUseId": "w", "filePath": "/w/b", "fileChange": "update"})
        self.assertEqual(tool_result({**single, "toolUseResult": {"task": {"id": 7, "subject": "任务"}}}),
                         {"toolUseId": "w", "createdTaskId": "7"})
        self.assertEqual(tool_result({**single, "toolUseResult": {"success": False, "taskId": "1"}}),
                         {"toolUseId": "w", "success": False})
        self.assertIsNone(tool_result({**single, "toolUseResult": {"structuredPatch": [{"oldStart": True}],
                                                                    "filePath": 3}}))

    def test_question_answers_forward_reply_text_and_only_own_upload_images(self):
        # 2026-11-15 实测：提问等待时从网页输入框发出的文字和截图被原生记为 AskUserQuestion 第一题的回答，
        # 网页须在正文看到这段回复；截图只认本用户上传目录内的直接文件，其余“@”行按原文保留。
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        root = Path(folder.name)
        uploads = root / "remote-attachments" / "web-client"
        other = root / "remote-attachments" / "other-client"
        uploads.mkdir(parents=True)
        other.mkdir()
        (uploads / "0.jpg").write_bytes(b"\xff\xd8\xff")
        (other / "0.png").write_bytes(b"other")
        outside = root / "secret.png"
        outside.write_bytes(b"secret")
        (uploads / "1.png").symlink_to(outside)
        questions = [{"question": "需求 b 怎么做？", "header": "需求 b", "options": [{"label": "完整分层"}]},
                     {"question": "需求 a 现在提交吗？", "header": "需求 a", "options": [{"label": "现在提交"}]},
                     {"question": "保留测试吗？", "header": "测试", "options": [{"label": "保留"}]}]
        kept = [f"@{uploads / '1.png'}", f"@{other / '0.png'}", f"@{outside}"]
        answer = "\n".join(["按截图处理", f"@{uploads / '0.jpg'}", *kept, f"@{uploads / '0.jpg'}"])
        rows = [
            {"type": "user", "uuid": "u", "message": {"content": "开始"}},
            {"type": "assistant", "uuid": "a", "parentUuid": "u", "message": {"id": "m", "content": [
                {"type": "tool_use", "id": "q1", "name": "AskUserQuestion", "input": {"questions": questions}}]}},
            {"type": "user", "uuid": "r", "parentUuid": "a", "message": {"content": [
                {"type": "tool_result", "tool_use_id": "q1", "content": "The user answered: ..."}]},
             "toolUseResult": {"questions": questions, "answers": {questions[0]["question"]: answer,
                                                                    questions[2]["question"]: "保留"}}},
        ]
        log = root / "native.jsonl"
        log.write_text("".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows), encoding="utf-8")
        native = SdkReader("unused", "test-session")
        native.root, native.log_path = root, log
        native.read_messages()
        result = next(row for row in native.messages if row["uuid"] == "r")["nativeToolResult"]
        jpeg = "data:image/jpeg;base64," + namespace["base64"].b64encode(b"\xff\xd8\xff").decode()
        self.assertEqual(result, {"toolUseId": "q1", "questionAnswers": [
            {"question": questions[0]["question"], "answer": "\n".join(["按截图处理", *kept]),
             "images": [{"url": jpeg}]},
            {"question": questions[1]["question"], "answer": None, "images": []},
            {"question": questions[2]["question"], "answer": "保留", "images": []}]})
        # 子代理只读视图没有配置目录：不读取任何图片，回答原文完整保留。
        bare = namespace["native_tool_result"](rows[2])["questionAnswers"][0]
        self.assertEqual((bare["answer"], bare["images"]), (answer, []))
        self.assertIsNone(namespace["native_tool_result"]({**rows[2], "toolUseResult": {"questions": "坏数据",
                                                                                        "answers": {}}}))

    def test_task_notifications_start_auto_turns_and_are_never_user_messages(self):
        origin = {"kind": "task-notification", "producer": "session-task"}
        monitor = {"type": "user", "uuid": "n1", "parentUuid": "d1", "origin": origin, "message": {"content": (
            "<task-notification>\n<task-id>bzb25w5se</task-id>\n<summary>Monitor event: \"等待测试\"</summary>\n"
            "<event>EXIT=0 &amp; 完成</event>\n</task-notification>")}}
        command = {"type": "user", "uuid": "n2", "parentUuid": "d2", "origin": origin, "message": {"content": [
            {"type": "text", "text": "<task-notification><task-id>bg1</task-id><tool-use-id>toolu_1</tool-use-id>"
                                     "<status>completed</status><summary>Background command \"make\" completed "
                                     "(exit code 0)</summary></task-notification>"}]}}
        quoted = {"type": "user", "uuid": "q", "message": {"content": monitor["message"]["content"]}}
        parse, real_user = namespace["native_task_notification"], namespace["real_user"]
        self.assertEqual(parse(monitor), {"taskId": "bzb25w5se", "summary": "Monitor event: \"等待测试\"",
                                          "event": "EXIT=0 & 完成"})
        self.assertEqual(parse(command), {"taskId": "bg1", "summary": "Background command \"make\" completed (exit code 0)",
                                          "status": "completed", "toolUseId": "toolu_1"})
        self.assertIsNone(parse(quoted), "用户引用相同 XML 仍是普通正文")
        self.assertFalse(real_user(monitor), "没有状态的监控事件也不能当作可编辑的用户消息")
        self.assertTrue(real_user(quoted))
        finished = namespace["native_turn_finished"]
        done = [{"type": "user", "uuid": "u", "message": {"content": "跑测试"}},
                {"type": "system", "uuid": "d1", "subtype": "turn_duration"}]
        self.assertTrue(finished(done))
        self.assertFalse(finished(done + [monitor]), "通知开启的新一轮尚未结束")

        native = self.read([*done, monitor], pending_text="不相关")
        row = native.messages[-1]
        self.assertEqual(row["nativeTaskNotification"]["taskId"], "bzb25w5se")
        self.assertFalse(row["nativeEditable"])
        self.assertIsNone(native.preview_context(), "自动续跑轮次无法在终端定位回复区，不做实时预览")
        self.assertEqual(native.clients, {}, "通知不能确认网页待发送消息")

    def test_scheduled_task_prompt_is_an_auto_turn_not_a_user_message(self):
        # 2.1.323 实测：system/scheduled_task_fire 之后紧跟以它为父节点、没有 origin 的 user 提示。
        native = self.read([
            {"type": "user", "uuid": "u0", "message": {"content": "创建定时任务"}},
            {"type": "assistant", "uuid": "a0", "parentUuid": "u0", "message": {"content": [{"type": "text", "text": "已创建"}]}},
            {"type": "system", "uuid": "d0", "parentUuid": "a0", "subtype": "turn_duration"},
            {"type": "system", "uuid": "f1", "parentUuid": "d0", "subtype": "scheduled_task_fire",
             "content": "Running scheduled task (Nov 14 12:27am)"},
            {"type": "user", "uuid": "p1", "parentUuid": "f1", "message": {"content": "只回复“定时触发成功”。"}},
            {"type": "assistant", "uuid": "a1", "parentUuid": "p1", "message": {"content": [{"type": "text", "text": "定时触发成功"}]}},
            {"type": "user", "uuid": "p2", "parentUuid": "a1", "message": {"content": "只回复“定时触发成功”。"}},
        ], pending_text="无关")
        rows = {row["uuid"]: row for row in native.messages}
        self.assertEqual(rows["p1"]["nativeTaskNotification"], {"taskId": "scheduled", "kind": "schedule",
                                                                "summary": "只回复“定时触发成功”。",
                                                                "event": "Running scheduled task (Nov 14 12:27am)"})
        self.assertFalse(rows["p1"]["nativeEditable"], "定时任务提示不能作为可编辑的用户消息")
        self.assertNotIn("nativeTaskNotification", rows["p2"], "用户手动输入相同文字仍是用户消息")
        self.assertTrue(rows["p2"]["nativeEditable"])

    def test_local_commands_and_system_notices_become_events_not_user_messages(self):
        command_text = ("<command-name>/resume</command-name>\n            <command-message>resume</command-message>\n"
                        "            <command-args></command-args>")
        rows = [
            {"type": "user", "uuid": "d", "message": {"content": "开始"}},
            {"type": "user", "uuid": "c0", "parentUuid": "d", "isMeta": True, "entrypoint": "cli", "promptId": "p1",
             "message": {"content": "<local-command-caveat>Caveat: run directly</local-command-caveat>"}},
            {"type": "user", "uuid": "c1", "parentUuid": "c0", "entrypoint": "cli", "promptId": "p1",
             "message": {"content": command_text}},
            {"type": "user", "uuid": "c2", "parentUuid": "c1", "entrypoint": "cli", "promptId": "p1",
             "message": {"content": "<local-command-stdout>\x1b[2m已在后台运行\x1b[22m</local-command-stdout>"}},
            {"type": "user", "uuid": "q", "parentUuid": "c2", "entrypoint": "cli", "promptId": "p2",
             "message": {"content": command_text}},
            {"type": "system", "uuid": "s1", "parentUuid": "q", "subtype": "local_command", "level": "info",
             "content": "<command-name>/skills</command-name>\n<command-message>skills</command-message>\n"
                        "<command-args></command-args>"},
            {"type": "system", "uuid": "s2", "parentUuid": "s1", "subtype": "local_command", "level": "info",
             "content": "<local-command-stdout>No changes</local-command-stdout>",
             "commandRun": {"command": "skills", "args": ""}},
            {"type": "system", "uuid": "s3", "parentUuid": "s2", "subtype": "local_command",
             "content": "<command-name>/model</command-name>\n<command-message>model</command-message>\n"
                        "<command-args>opus</command-args>"},
            {"type": "system", "uuid": "s4", "parentUuid": "s3", "subtype": "away_summary",
             "content": "正在修复渲染。 (disable recaps in /config)"},
            {"type": "system", "uuid": "s5", "parentUuid": "s4", "subtype": "stop_hook_summary", "hookErrors": [],
             "preventedContinuation": False, "stopReason": ""},
            {"type": "system", "uuid": "s6", "parentUuid": "s5", "subtype": "stop_hook_summary",
             "hookErrors": ["exit 2"], "preventedContinuation": True, "stopReason": "测试未通过"},
        ]
        native = self.read(rows, pending_text="无关")
        rows = {row["uuid"]: row for row in native.messages}
        self.assertEqual(rows["c1"]["nativeLocalCommand"], {"name": "/resume", "args": ""})
        self.assertFalse(rows["c1"]["nativeEditable"], "终端本地命令不能作为可编辑的用户消息")
        self.assertEqual(rows["c2"]["nativeLocalOutput"], {"commandId": "c1", "stream": "stdout", "text": "已在后台运行"})
        self.assertFalse(rows["c2"]["nativeEditable"])
        self.assertNotIn("nativeLocalCommand", rows["q"], "没有原生 caveat 的同样标签按用户正文处理")
        self.assertTrue(rows["q"]["nativeEditable"])
        self.assertEqual(rows["s1"]["nativeLocalCommand"]["name"], "/skills")
        self.assertEqual(rows["s2"]["nativeLocalOutput"]["text"], "No changes")
        self.assertNotIn("s3", rows, "模型命令另有专门展示")
        self.assertEqual(rows["s4"]["nativeSystemNotice"], {"kind": "awaySummary", "text": "正在修复渲染。"})
        self.assertNotIn("s5", rows, "无输出、未阻止的 Stop Hook 汇总不展示")
        self.assertEqual(rows["s6"]["nativeSystemNotice"],
                         {"kind": "stopHook", "prevented": True, "reason": "测试未通过", "errors": ["exit 2"]})


    def test_native_interruption_does_not_hide_human_quotes_or_unmarked_identical_text(self):
        marker = "[Request interrupted by user]"
        for content, metadata, text in [
            (marker, {}, marker),
            ([{"type": "text", "text": marker}], {}, marker),
            ([{"type": "text", "text": "请解释 " + marker}], {"interruptedMessageId": "response-id"}, "请解释 " + marker),
        ]:
            with self.subTest(content=content):
                native = self.read([{"type": "user", "uuid": "literal", **metadata,
                                     "message": {"content": content}}], pending_text=text)
                self.assertTrue(namespace["real_user"](native.messages[0]))
                self.assertEqual(native.clients, {"literal": "web-client"})
                self.assertEqual(native.pending, [])

    def test_native_interruption_requires_a_real_response_reference_and_single_text_control(self):
        marker = {"type": "text", "text": "[Request interrupted by user]"}
        for reference, content in [("", [marker]), ("response", [marker, {"type": "image"}]),
                                   ("response", "引用 [Request interrupted by user]")]:
            self.assertFalse(namespace["native_interruption"](
                {"type": "user", "interruptedMessageId": reference, "message": {"content": content}}))
        for message in (None, "API diagnostics"):
            self.assertFalse(namespace["native_interruption"]({"type": "api-request-blob", "message": message}))

    def tool_interruption_rows(self):
        return [
            {"type": "user", "uuid": "question", "message": {"content": "原问题"}},
            {"type": "assistant", "uuid": "tool-call", "parentUuid": "question",
             "message": {"id": "tool-response", "content": [
                 {"type": "tool_use", "id": "denied-tool", "name": "Bash", "input": {}}]}},
            {"type": "user", "uuid": "denial", "parentUuid": "tool-call", "promptId": "native-stop",
             "toolDenialKind": "user-rejected", "sourceToolAssistantUUID": "tool-call",
             "message": {"content": [{"type": "tool_result", "tool_use_id": "denied-tool",
                                      "is_error": True, "content": "原生拒绝结果"}]}},
            {"type": "user", "uuid": "tool-control", "parentUuid": "denial", "promptId": "native-stop",
             "entrypoint": "cli", "message": {"role": "user", "content": [
                 {"type": "text", "text": "[Request interrupted by user for tool use]"}]}},
        ]

    def test_native_tool_interruption_uses_denial_ancestry_without_rewriting_history_or_consuming_input(self):
        marker = "[Request interrupted by user for tool use]"
        rows = self.tool_interruption_rows()
        native = self.read(rows, pending_text=marker)
        control = native.messages[-1]
        self.assertEqual(control["nativeToolInterruption"], {
            "responseId": "tool-response", "toolUseId": "denied-tool", "toolResultId": "denial"})
        self.assertNotIn("interruptedMessageId", control, "不能伪造原生未提供的字段")
        self.assertFalse(namespace["real_user"](control))
        self.assertFalse(control["nativeEditable"])
        self.assertTrue(namespace["native_turn_finished"](native.messages))
        self.assertEqual(native.clients, {})
        self.assertEqual(len(native.pending), 1)
        self.assertEqual(native.log_path.read_text(encoding="utf-8"),
                         "".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows))
        quoted = {"type": "user", "uuid": "literal", "parentUuid": "tool-control", "promptId": "human-input",
                  "entrypoint": "cli", "message": {"content": [{"type": "text", "text": marker}]}}
        with_quote = self.read(rows + [quoted], pending_text=marker)
        self.assertTrue(namespace["real_user"](with_quote.messages[-1]))
        self.assertEqual(with_quote.clients, {"literal": "web-client"})

    def test_native_tool_interruption_preserves_identical_text_without_the_complete_native_denial_chain(self):
        changes = [
            (3, "promptId", "different-input"),
            (3, "entrypoint", "other"),
            (3, "parentUuid", "tool-call"),
            (2, "toolDenialKind", "hook-denied"),
            (2, "sourceToolAssistantUUID", "missing-call"),
            (2, "promptId", None),
        ]
        for index, field, value in changes:
            with self.subTest(field=field, value=value):
                rows = self.tool_interruption_rows()
                rows[index][field] = value
                native = self.read(rows)
                self.assertTrue(namespace["real_user"](native.messages[-1]))
                self.assertNotIn("nativeToolInterruption", native.messages[-1])
        for field, value in (("is_error", False), ("tool_use_id", "different-tool")):
            rows = self.tool_interruption_rows()
            rows[2]["message"]["content"][0][field] = value
            native = self.read(rows)
            self.assertTrue(namespace["real_user"](native.messages[-1]))
            self.assertNotIn("nativeToolInterruption", native.messages[-1])

    def queued(self, **changes):
        return {"type": "attachment", "uuid": "absorbed-user",
                "timestamp": "2026-11-11T07:03:46Z", "renderedRole": "system",
                "attachment": {"type": "queued_command", "commandMode": "prompt",
                               "prompt": "待接收消息", "humanTurn": True,
                               "origin": {"kind": "human"}, **changes}}

    def test_absorbed_prompt_clears_queue_and_keeps_identity_without_matching_older_identical_text(self):
        native = self.read([
            {"type": "user", "uuid": "older-user", "message": {"content": "待接收消息"}},
            self.queued(),
        ], after=1)
        self.assertEqual(native.pending, [])
        self.assertEqual(native.clients, {"absorbed-user": "web-client"})
        self.assertEqual(native.messages[1]["type"], "user")
        self.assertEqual(native.messages[1]["message"]["content"], "待接收消息")
        native.read_messages()
        self.assertEqual(len(native.messages), 2)
        self.assertEqual(native.clients, {"absorbed-user": "web-client"})

    def test_internal_or_shell_attachments_do_not_confirm_a_user_message(self):
        for change in [{"origin": {"kind": "agent"}}, {"humanTurn": False},
                       {"commandMode": "bash"}]:
            with self.subTest(change=change):
                native = self.read([self.queued(**change)])
                self.assertEqual(len(native.pending), 1)
                self.assertEqual(native.messages, [])


    def test_setting_commands_and_paired_outputs_are_control_records_but_typed_quotes_survive(self):
        command = ("<command-name>/effort</command-name>\n"
                   "<command-message>effort</command-message>\n"
                   "<command-args>xhigh</command-args>")
        output = "<local-command-stdout>Set effort level to xhigh</local-command-stdout>"
        native = self.read([
            {"type": "user", "uuid": "command", "entrypoint": "cli",
             "message": {"role": "user", "content": command}},
            {"type": "user", "uuid": "output", "parentUuid": "command", "entrypoint": "cli",
             "message": {"role": "user", "content": output}},
            {"type": "user", "uuid": "orphan-output", "parentUuid": "unrelated", "entrypoint": "cli",
             "message": {"role": "user", "content": output}},
            {"type": "user", "uuid": "quote", "entrypoint": "cli", "origin": {"kind": "human"},
             "promptSource": "typed", "turnOrigin": "human",
             "message": {"role": "user", "content": command}},
        ], pending_text=command)
        self.assertEqual(native.messages[0]["nativeSettingCommand"], {"name": "effort", "value": "xhigh"})
        self.assertEqual(native.messages[1]["nativeSettingOutput"]["commandId"], "command")
        self.assertFalse(namespace["real_user"](native.messages[0]))
        self.assertFalse(namespace["real_user"](native.messages[1]))
        self.assertNotIn("nativeSettingOutput", native.messages[2], "孤立回显不按标签文本猜测来源")
        self.assertNotIn("nativeSettingCommand", native.messages[3], "保留真实用户的同文引用")
        self.assertEqual(native.clients, {"quote": "web-client"}, "控制命令不能确认用户的待发送消息")
        self.assertEqual(native.pending, [])
        literal = self.read([{"type": "user", "uuid": "quoted-code", "entrypoint": "cli",
                             "message": {"content": "```xml\n" + command + "\n```"}}],
                            pending_text="```xml\n" + command + "\n```")
        self.assertEqual(literal.clients, {"quoted-code": "web-client"})

    def test_image_attachments_still_use_existing_image_projection(self):
        native = self.read([{"type": "attachment", "uuid": "image",
                            "attachment": {"type": "file", "filename": "/test/0.png",
                                           "content": {"type": "image", "file": {
                                               "type": "image/png", "base64": "AQID"}}}}])
        self.assertEqual(native.messages[0]["type"], "attachment")
        self.assertEqual(native.messages[0]["message"]["content"],
                         [{"type": "image", "url": "data:image/png;base64,AQID"}])
        self.assertEqual(len(native.pending), 1)

    def test_eight_accepted_web_images_survive_partial_native_attachments_and_reconnect(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        root = Path(folder.name)
        uploads = root / "remote-attachments" / "web-client"
        uploads.mkdir(parents=True)
        images = []
        for index in range(8):
            data = bytes([index + 1, 2, 3])
            (uploads / f"{index}.png").write_bytes(data)
            images.append({"type": "image", "source": {"type": "base64", "media_type": "image/png",
                "data": namespace["base64"].b64encode(data).decode()}})
        prompt = "八张图片" + "".join(f"\n@{uploads / f'{index}.png'}" for index in range(8))
        rows = [{"type": "user", "uuid": "native-user", "message": {"content": prompt}},
                {"type": "attachment", "uuid": "only-last-image", "attachment": {
                    "type": "file", "filename": str(uploads / "7.png"), "content": {
                        "type": "image", "file": {"type": "image/png", "base64": images[-1]["source"]["data"]}}}}]
        log = root / "native.jsonl"
        log.write_text("".join(json.dumps(row) + "\n" for row in rows))
        native = SdkReader("unused", "test-session")
        native.root, native.log_path = root, log
        native.pending = [{"clientUserMessageId": "web-client", "_nativeText": prompt, "_after": 0,
                           "content": [{"type": "text", "text": "八张图片"}, *images]}]
        native.read_messages()
        self.assertEqual(len(native.messages[0]["web_images"]), 8)
        self.assertEqual(len(native.messages[0]["remote_upload_paths"]), 8)
        self.assertEqual(native.pending, [])
        restored = SdkReader("unused", "test-session")
        restored.root, restored.log_path = root, log
        restored.control({"subtype": "initialize", "catalog": {}, "clients": native.clients})
        restored.read_messages()
        self.assertEqual(restored.messages[0]["web_images"], native.messages[0]["web_images"])
        restored.read_messages()
        self.assertEqual(len(restored.messages), 2)

    def test_native_usage_and_cache_iterations_are_not_dropped_by_the_bridge(self):
        usage = {"input_tokens": 100, "cache_read_input_tokens": 1000,
                 "cache_creation_input_tokens": 200, "output_tokens": 50,
                 "iterations": [{"type": "message", "input_tokens": 30}]}
        native = self.read([{"type": "assistant", "uuid": "reply-uuid", "isUnmetered": False,
                            "requestedModel": "native-model", "effort": "xhigh",
                            "message": {"role": "assistant", "id": "reply", "model": "native-model",
                                        "content": [{"type": "text", "text": "回复"}], "usage": usage}}])
        self.assertEqual(native.messages[0]["message"]["usage"], usage)
        self.assertFalse(native.messages[0]["isUnmetered"])
        self.assertEqual(native.messages[0]["requestedModel"], "native-model")
        self.assertEqual(native.messages[0]["effort"], "xhigh")

    def test_context_configuration_keeps_compaction_separate_and_bound_to_the_usage_model(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        native = SdkReader("unused", "test-session")
        native.root = Path(folder.name)
        settings = native.root / "settings.json"
        key = "CLAUDE_CODE_MAX_CONTEXT_TOKENS"
        data = {"env": {key: "600000", "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "700000",
                        "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE": "95", "ANTHROPIC_AUTH_TOKEN": "not-for-export"},
                "autoCompactWindow": 800000, "modelSettings": {"gpt-6.1-sol": {"autoCompactWindow": 900000}}}
        native.model = "gpt-6.1-sol"
        native.read_messages = lambda: None
        native.lookup = lambda: None
        with patch.dict(namespace["os"].environ, {key: "400000"}, clear=True):
            settings.write_text(json.dumps(data))
            catalog = native.control({"subtype": "initialize", "model": native.model, "catalog": {}})
            context = catalog["contextConfiguration"]
            self.assertEqual(context["maxContextTokens"], 600000)
            self.assertEqual(context["compaction"]["windowTokens"], 700000)
            self.assertEqual(context["compaction"]["triggerPercent"], 95)
            self.assertEqual(context["source"], {"kind": "configuration", "key": key,
                                               "scope": "user-settings.env", "model": "gpt-6.1-sol"})
            self.assertNotIn("not-for-export", json.dumps(context))
            with contextlib.redirect_stdout(io.StringIO()) as output:
                native.poll(force=True)
            self.assertEqual(json.loads(output.getvalue())["context_configuration"], context)
            native.messages.append({"type": "assistant", "message": {"model": "other-model", "usage": {"input_tokens": 1}}})
            self.assertEqual(native.context_configuration()["model"], "other-model")
            native.messages.append({"type": "assistant", "isApiErrorMessage": True,
                                    "message": {"model": "error-model", "usage": {"input_tokens": 0}}})
            self.assertEqual(native.context_configuration()["model"], "other-model")
            for invalid in ("0", "-1", "600k", "bad", "9007199254740992", True, 600000):
                data["env"][key] = invalid
                settings.write_text(json.dumps(data))
                self.assertNotIn("maxContextTokens", native.context_configuration(),
                                 f"非法容量不能用于显示：{invalid}")
            del data["env"][key]
            settings.write_text(json.dumps(data))
            self.assertEqual(native.context_configuration()["maxContextTokens"], 400000)
            namespace["os"].environ.pop(key)
            context = native.context_configuration()
            self.assertNotIn("maxContextTokens", context)
            self.assertNotIn("native", context, "压缩配置不能冒充原生容量")
            self.assertEqual(context["compaction"]["windowTokens"], 700000)

    def test_native_statusline_context_is_sanitized_model_bound_and_cleared_on_log_replacement(self):
        usage = {"input_tokens": 100, "cache_read_input_tokens": 1000,
                 "cache_creation_input_tokens": 200, "output_tokens": 50}
        native = self.read([
            {"type": "assistant", "uuid": "reply", "message": {"model": "native-model", "usage": usage}},
            {"session_id": "test-session", "model": {"id": "native-model"},
             "context_window": {"context_window_size": 1000000, "used_percentage": 0.13,
                                "current_usage": usage, "private": "must-not-export"}}
        ])
        context = native.context_configuration()["native"]
        self.assertEqual(context["context_window_size"], 1000000)
        self.assertEqual(context["used_percentage"], 0.13)
        self.assertNotIn("private", json.dumps(context))
        self.assertNotIn("output_tokens", context["current_usage"])
        native.observe_native_context({"session_id": "another-session", "model": "native-model",
                                      "context_window": {"context_window_size": 600000}})
        self.assertEqual(native.native_context, context)
        native.messages.append({"type": "assistant", "message": {"model": "new-model", "usage": usage}})
        self.assertNotIn("native", native.context_configuration())
        native.log_path.write_text("", encoding="utf-8")
        native.read_messages()
        self.assertEqual(native.native_context, {})


class NativeSubagentHistoryTests(unittest.TestCase):
    """原生子代理协议的回归：使用真实格式的合成记录，不启动模型或读取用户日志。"""
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.root = Path(folder.name)
        self.native = "00000000-0000-4000-8000-000000000001"
        self.agent = "agent_fixture"
        self.parent = self.root / "projects" / "project" / f"{self.native}.jsonl"
        self.children = self.parent.with_suffix("") / "subagents"
        self.children.mkdir(parents=True)
        self.child = self.children / f"agent-{self.agent}.jsonl"
        self.meta = self.child.with_suffix(".meta.json")
        self.index = namespace["NativeSubagents"](self.root, self.native)
        self.user = {"type": "user", "uuid": "human", "sessionId": self.native, "timestamp": 1000,
                     "cwd": "/fixture", "message": {"role": "user", "content": "检查文件"}}
        self.spawn = {"type": "assistant", "uuid": "spawn", "sessionId": self.native, "timestamp": 1100,
                      "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "spawn-call",
                      "name": "Agent", "input": {"description": "读取文件", "subagent_type": "fork", "prompt": "读取测试文件"}}]}}
        self.accepted = {"type": "user", "uuid": "accepted", "sessionId": self.native, "timestamp": 1200,
                         "toolUseResult": {"agentId": self.agent, "status": "async_launched", "resolvedModel": "test-gpt"},
                         "message": {"content": [{"type": "tool_result", "tool_use_id": "spawn-call", "content": "已启动"}]}}
        self.write(self.parent, [self.user, self.spawn, self.accepted])
        self.meta.write_text(json.dumps({"isFork": True, "toolUseId": "spawn-call", "finished": False}), encoding="utf-8")
        self.write(self.child, [{**self.spawn, "isSidechain": True, "agentId": self.agent},
                   {**self.accepted, "isSidechain": True, "agentId": self.agent},
                   {"type": "assistant", "uuid": "read", "timestamp": 1300, "sessionId": self.native,
                    "agentId": self.agent, "isSidechain": True, "message": {"role": "assistant",
                    "id": "read-message", "model": "test-gpt", "content": [{"type": "tool_use",
                    "id": "read-call", "name": "Read", "input": {"file_path": "/fixture/input.txt"}}]}}])

    @staticmethod
    def write(path, rows, append=False):
        with path.open("a" if append else "w", encoding="utf-8") as stream:
            for row in rows:
                stream.write(json.dumps(row) + "\n")

    def notification(self, status="completed"):
        return {"type": "user", "uuid": "notice", "sessionId": self.native, "timestamp": 1800,
                "origin": {"kind": "task-notification", "producer": "session-task"},
                "message": {"content": f"<task-notification><task-id>{self.agent}</task-id>"
                    f"<tool-use-id>spawn-call</tool-use-id><status>{status}</status>"
                    "<usage><subagent_tokens>42</subagent_tokens><tool_uses>1</tool_uses>"
                    "<duration_ms>500</duration_ms></usage></task-notification>"}}

    def test_active_then_completed_fork_keeps_its_id_task_and_own_tools_only(self):
        active = self.index.collect()[0]
        self.assertEqual(active["status"], "active", "async_launched 不代表已完成")
        self.write(self.child, [{"type": "assistant", "uuid": "answer", "timestamp": 1600,
            "sessionId": self.native, "agentId": self.agent, "isSidechain": True,
            "message": {"role": "assistant", "id": "final-message", "model": "test-gpt",
                        "content": [{"type": "text", "text": "FILE_OK"}], "stop_reason": "end_turn"}}], append=True)
        self.meta.write_text(json.dumps({"isFork": True, "toolUseId": "spawn-call", "finished": True}), encoding="utf-8")
        self.write(self.parent, [self.notification()], append=True)
        completed = self.index.collect(self.agent, True)[0]
        self.assertEqual(completed["nativeId"], active["nativeId"])
        self.assertEqual(completed["status"], "completed")
        self.assertEqual(completed["totalToolUseCount"], 1)
        self.assertEqual(completed["totalTokens"], 42)
        self.assertEqual(completed["messages"][0]["message"]["content"], "读取测试文件")
        tools = [part["name"] for row in completed["messages"]
                 for part in row.get("message", {}).get("content", []) if isinstance(part, dict) and part.get("type") == "tool_use"]
        self.assertEqual(tools, ["Read"], "继承的启动 Agent 调用不能重复显示成子代理执行")
        self.assertEqual(self.index.collect(self.agent, True), [completed], "重读不得重复正文或切换原生身份")

    def test_partial_append_is_not_consumed_and_same_id_recovers_after_reader_restart(self):
        first = self.index.collect(self.agent, True)[0]
        row = {"type": "assistant", "uuid": "partial", "timestamp": 1700, "agentId": self.agent,
               "sessionId": self.native, "message": {"content": [{"type": "text", "text": "完成"}]}}
        encoded = json.dumps(row)
        with self.child.open("a", encoding="utf-8") as stream:
            stream.write(encoded[:20])
        self.assertEqual(self.index.collect(self.agent, True)[0]["messages"], first["messages"])
        with self.child.open("a", encoding="utf-8") as stream:
            stream.write(encoded[20:] + "\n")
        updated = self.index.collect(self.agent, True)[0]
        self.assertEqual(updated["messages"][-1]["uuid"], "partial")
        reopened = namespace["NativeSubagents"](self.root, self.native)
        self.assertEqual(reopened.collect(self.agent, True), [updated])

    def test_notification_is_control_but_human_quote_and_other_parent_id_cannot_be_misclassified(self):
        row = self.notification("failed")
        self.assertFalse(namespace["real_user"](row))
        self.assertTrue(namespace["real_user"]({**row, "origin": {"kind": "human"}}))
        self.assertIsNone(namespace["native_agent_notification"]({**row, "origin": {"kind": "human"}}))
        for identity in ("../agent_fixture", "", "x" * 65):
            with self.assertRaisesRegex(RuntimeError, "ID 无效"):
                self.index.collect(identity)
        with self.assertRaises(namespace["NativeError"]):
            self.index.collect("another-agent")
        another = namespace["NativeSubagents"](self.root, "00000000-0000-4000-8000-000000000002")
        with self.assertRaisesRegex(RuntimeError, "主会话记录"):
            another.collect(self.agent)

    def test_resume_acknowledgement_invalidates_old_completion_before_new_child_records_arrive(self):
        self.meta.write_text(json.dumps({"isFork": True, "toolUseId": "spawn-call", "finished": True}), encoding="utf-8")
        self.write(self.parent, [self.notification()], append=True)
        self.assertEqual(self.index.collect()[0]["status"], "completed")
        self.write(self.parent, [{"type": "user", "uuid": "resumed", "sessionId": self.native,
            "timestamp": 2000, "toolUseResult": {"success": True, "resumedAgentId": self.agent},
            "message": {"content": [{"type": "tool_result", "tool_use_id": "send-call", "content": "Resuming agent"}]}}], append=True)
        active = self.index.collect()[0]
        self.assertEqual(active["status"], "active")
        self.assertIsNone(active["outcome"])
        self.assertNotIn("totalDurationMs", active, "不能把上次已完成的耗时当成本次执行时间")
        self.assertEqual(active["startedAtMs"], 2000)
        self.meta.write_text(json.dumps({"isFork": True, "toolUseId": "spawn-call"}), encoding="utf-8")
        self.assertEqual(self.index.collect()[0]["status"], "active", "原生删除 finished 字段仍表示此次续跑中")
        self.write(self.parent, [{**self.notification(), "uuid": "again", "timestamp": 2400}], append=True)
        self.assertEqual(self.index.collect()[0]["status"], "completed")
        self.assertEqual(len(self.index.collect()), 1, "续聊继续使用同一原生 ID")

    def test_nested_fork_recovers_its_task_from_native_bootstrap_after_parent_call_is_compacted(self):
        self.write(self.parent, [self.user])
        detail = self.index.collect(self.agent, True)[0]
        self.assertEqual(detail["messages"][0]["message"]["content"], "读取测试文件")
        self.assertEqual(detail["name"], "读取文件")
        self.assertEqual(detail["messages"][1]["message"]["content"][0]["name"], "Read")

    def test_coordinator_input_is_visible_only_in_the_read_only_child_projection(self):
        row = {"type": "user", "uuid": "coordinator", "sessionId": self.native, "agentId": self.agent,
               "timestamp": 2000, "isMeta": True, "origin": {"kind": "coordinator"},
               "message": {"content": "继续读取"}}
        self.write(self.child, [row], append=True)
        projected = self.index.collect(self.agent, True)[0]["messages"][-1]
        self.assertFalse(projected["isMeta"])
        self.assertTrue(projected["nativeSubagentInput"])
        self.assertEqual(projected["message"]["content"], "继续读取")
        self.assertTrue(json.loads(self.child.read_text().splitlines()[-1])["isMeta"], "原生日志不能被改写")
        self.assertFalse(namespace["real_user"](row), "该投影不能改变主会话的真实用户准入规则")

    def test_a_symlink_cannot_redirect_subagent_reads_into_another_parent_session(self):
        native = "00000000-0000-4000-8000-000000000002"
        other = self.parent.with_name(native + ".jsonl")
        self.write(other, [{**self.user, "sessionId": native}])
        other.with_suffix("").mkdir()
        (other.with_suffix("") / "subagents").symlink_to(self.children, target_is_directory=True)
        reader = namespace["NativeSubagents"](self.root, native)
        with self.assertRaisesRegex(RuntimeError, "不属于当前会话"):
            reader.collect(self.agent)

    def test_unavailable_child_transcript_cannot_be_reported_as_an_empty_success(self):
        self.assertTrue(self.index.collect(self.agent, True)[0]["messages"])
        self.child.unlink()
        with self.assertRaisesRegex(RuntimeError, "暂不可读取"):
            self.index.collect(self.agent, True)


class ProjectTrustTests(unittest.TestCase):
    def fixture(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        root = Path(folder.name)
        project = root / "project"
        project.mkdir()
        config = root / ".claude.json"
        config.write_text(json.dumps({"oauthAccount": {"accountUuid": "preserved-account"},
                                      "projects": {"other-project": {"hasTrustDialogAccepted": True}},
                                      "nativeSetting": {"keep": True}}), encoding="utf-8")
        return project, config

    def test_check_is_read_only_and_confirmation_preserves_other_native_configuration(self):
        project, config = self.fixture()
        original = config.read_bytes()
        with patch.object(trust_namespace["subprocess"], "run") as run:
            run.return_value.returncode = 128
            checked = trust_namespace["inspect"](str(project), file=config)
            self.assertFalse(checked["trusted"])
            self.assertEqual(config.read_bytes(), original, "检查不修改原生配置")
            result = trust_namespace["confirm"](str(project), str(project.resolve()), file=config)
            self.assertTrue(result["trusted"])
            saved = json.loads(config.read_text())
            old = json.loads(original)
            self.assertEqual(saved.pop("projects")[str(project.resolve())], {"hasTrustDialogAccepted": True})
            self.assertEqual(saved, {k: v for k, v in old.items() if k != "projects"})
            self.assertEqual(json.loads(config.read_text())["projects"]["other-project"],
                             old["projects"]["other-project"])
            before_repeat = config.read_bytes()
            self.assertTrue(trust_namespace["confirm"](str(project), str(project.resolve()), file=config)["trusted"])
            self.assertEqual(config.read_bytes(), before_repeat, "相同目录确认幂等，不重复改写配置")
            self.assertFalse(Path(str(config) + ".lock").exists())

    def test_changed_path_and_broad_directory_are_rejected_before_writing(self):
        project, config = self.fixture()
        original = config.read_bytes()
        with patch.object(trust_namespace["subprocess"], "run") as run:
            run.return_value.returncode = 128
            with self.assertRaisesRegex(ValueError, "目录已变化"):
                trust_namespace["confirm"](str(project), str(project / "different"), file=config)
            with self.assertRaisesRegex(ValueError, "具体项目目录"):
                trust_namespace["confirm"]("/", "/", file=config)
        self.assertEqual(config.read_bytes(), original)
        self.assertFalse(Path(str(config) + ".lock").exists())
        with patch.dict(trust_namespace["os"].environ, {"CLAUDE_CONFIG_DIR": "relative-config"}):
            with self.assertRaisesRegex(ValueError, "绝对路径"):
                trust_namespace["config_path"]()

    def test_trusted_symlink_configuration_can_be_read_but_is_not_replaced(self):
        project, config = self.fixture()
        value = json.loads(config.read_text())
        value["projects"][str(project.resolve())] = {"hasTrustDialogAccepted": True}
        config.write_text(json.dumps(value))
        link = config.parent / "linked-config"
        link.symlink_to(config)
        with patch.object(trust_namespace["subprocess"], "run") as run:
            run.return_value.returncode = 128
            self.assertTrue(trust_namespace["inspect"](str(project), file=link)["trusted"])
            with self.assertRaisesRegex(RuntimeError, "符号链接"):
                trust_namespace["confirm"](str(project), str(project.resolve()), file=link)
        self.assertTrue(link.is_symlink())
        self.assertEqual(json.loads(config.read_text()), value)

    def test_foreign_native_lock_and_invalid_json_are_not_overwritten(self):
        project, config = self.fixture()
        original = config.read_bytes()
        lock = Path(str(config) + ".lock")
        lock.mkdir()
        with patch.object(trust_namespace["subprocess"], "run") as run, \
                patch.object(trust_namespace["time"], "monotonic", side_effect=[0, 6]):
            run.return_value.returncode = 128
            with self.assertRaisesRegex(RuntimeError, "正在更新配置"):
                trust_namespace["confirm"](str(project), str(project.resolve()), file=config)
        self.assertTrue(lock.exists(), "不能删除原生 CLI 的锁")
        self.assertEqual(config.read_bytes(), original)
        lock.rmdir()
        config.write_text("{broken", encoding="utf-8")
        with patch.object(trust_namespace["subprocess"], "run") as run:
            run.return_value.returncode = 128
            with self.assertRaises(ValueError):
                trust_namespace["confirm"](str(project), str(project.resolve()), file=config)
        self.assertEqual(config.read_text(), "{broken")
        self.assertFalse(lock.exists())

    def test_git_boundary_and_managed_workspace_require_the_exact_confirmed_path(self):
        project, config = self.fixture()
        child = project / "child"
        child.mkdir()
        with patch.object(trust_namespace["subprocess"], "run") as run:
            run.return_value.returncode = 0
            run.return_value.stdout = (str(project.resolve()) + "\n").encode()
            checked = trust_namespace["inspect"](str(child), file=config)
            self.assertEqual(checked["trustPath"], str(project.resolve()))
            with self.assertRaises(ValueError):
                trust_namespace["confirm"](str(child), str(child.resolve()), file=config)
            self.assertTrue(trust_namespace["confirm"](str(child), checked["trustPath"], file=config)["trusted"])
            managed = project.parent / "managed"
            run.return_value.returncode = 128
            checked = trust_namespace["inspect"](str(managed), managed=True, file=config)
            self.assertFalse(managed.exists(), "未确认的无项目目录不提前创建")
            self.assertTrue(trust_namespace["confirm"](str(managed), checked["trustPath"],
                                                       managed=True, file=config)["trusted"])
            self.assertTrue(managed.is_dir())


usage_source = source.with_name("claude_usage_stats.py")
usage_namespace = {"__name__": "claude_usage_stats_test"}
exec(compile(usage_source.read_text(encoding="utf-8"), str(usage_source), "exec"), usage_namespace)


class LocalUsageLedgerTests(unittest.TestCase):
    """数值账本的长期回归；全部输入与价格为合成数据，不读取真实 Claude 日志。"""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name) / "projects"
        self.root.mkdir()
        self.log = self.root / "session.jsonl"
        self.cache = Path(self.directory.name) / "usage.sqlite"
        self.now = int(usage_namespace["time"].time() * 1000)
        self.request = {"nowMs": self.now, "windows": [
            {"key": "fiveHour", "durationMs": 18000000, "resetsAt": self.now + 9000000},
            {"key": "sevenDay", "durationMs": 604800000, "resetsAt": self.now + 302400000}],
            "prices": {"test-model": {"input_cost_per_token": 0.000001,
                "output_cost_per_token": 0.000005, "cache_read_input_token_cost": 0.0000001,
                "cache_creation_input_token_cost": 0.00000125,
                "cache_creation_input_token_cost_above_1hr": 0.000002,
                "provider_specific_entry": {"fast": 2, "us": 1.1}}}}

    def row(self, identity="msg_one", at=None, **usage):
        return {"type": "assistant", "uuid": identity + "-line", "timestamp": at or self.now - 1000,
                "message": {"id": identity, "model": "test-model", "usage": {
                    "input_tokens": 100, "output_tokens": 10, **usage}}}

    def write(self, *rows, append=False, path=None):
        with (path or self.log).open("a" if append else "w", encoding="utf-8") as stream:
            for row in rows:
                stream.write(json.dumps(row) + "\n")

    def collect(self):
        return usage_namespace["collect"](self.request, root=self.root, cache_path=self.cache)

    def test_deleted_session_and_reopened_ledger_keep_recorded_consumption(self):
        self.write(self.row())
        original = self.collect()
        self.log.unlink()
        self.assertEqual(self.collect(), original, "删除日志后消费不回落")
        fresh = {"__name__": "usage_reopened"}
        exec(compile(usage_source.read_text(encoding="utf-8"), str(usage_source), "exec"), fresh)
        self.assertEqual(fresh["collect"](self.request, root=self.root, cache_path=self.cache), original)
        database = usage_namespace["sqlite3"].connect(self.cache)
        try:
            rows = database.execute("SELECT data FROM events").fetchall()
            self.assertNotIn("message", rows[0][0], "账本不保存对话正文")
        finally:
            database.close()

    def test_streaming_fork_and_subagent_replays_are_counted_once(self):
        initial = self.row(output_tokens=1)
        final = self.row(output_tokens=20, speed="fast")
        self.write(initial, final, initial)
        subagent = self.root / "subagents"
        subagent.mkdir()
        self.write(final, self.row("msg_child", output_tokens=30), path=subagent / "agent.jsonl")
        result = self.collect()["fiveHour"]
        self.assertEqual(result["usedTokens"], 250)
        self.assertAlmostEqual(result["usedCostUsd"], (100e-6 + 20 * 5e-6) * 2 + 100e-6 + 30 * 5e-6)
        self.assertEqual(self.collect()["fiveHour"], result)

    def test_appends_truncation_and_replacement_preserve_previous_requests(self):
        self.write(self.row())
        self.assertEqual(self.collect()["fiveHour"]["usedTokens"], 110)
        self.write(self.row("msg_two"), append=True)
        self.assertEqual(self.collect()["fiveHour"]["usedTokens"], 220)
        self.write(self.row("msg_three"))
        self.assertEqual(self.collect()["fiveHour"]["usedTokens"], 330)
        replacement = self.log.with_suffix(".replacement")
        self.write(self.row("msg_four"), path=replacement)
        replacement.replace(self.log)
        self.assertEqual(self.collect()["fiveHour"]["usedTokens"], 440)

    def test_compaction_iterations_replace_outer_usage_and_include_cache_ttl(self):
        self.write(self.row(input_tokens=999999, iterations=[
            {"type": "compaction", "input_tokens": 200, "output_tokens": 20},
            {"type": "message", "input_tokens": 100, "output_tokens": 10,
             "cache_read_input_tokens": 300, "cache_creation_input_tokens": 80,
             "cache_creation": {"ephemeral_1h_input_tokens": 50}}]))
        result = self.collect()["fiveHour"]
        self.assertEqual(result["usedTokens"], 710)
        self.assertAlmostEqual(result["usedCostUsd"], 300e-6 + 30 * 5e-6 + 300e-7 + 30 * 1.25e-6 + 50 * 2e-6)
        self.assertNotIn("forecastTokens", result, "账本不再按经过时间推算额度")
        self.assertNotIn("forecastCostUsd", result)

    def test_unknown_prices_do_not_hide_tokens_or_invent_cost(self):
        self.write(self.row())
        self.request["prices"] = {}
        result = self.collect()["fiveHour"]
        self.assertEqual(result["usedTokens"], 110)
        self.assertIsNone(result["usedCostUsd"])

    def test_window_boundaries_and_replay_timestamp_do_not_move_spend(self):
        start = self.now - 9000000
        self.write(self.row("msg_old", at=start - 1), self.row("msg_edge", at=start),
                   self.row("msg_future", at=self.now + 1000))
        result = self.collect()
        self.assertEqual(result["fiveHour"]["usedTokens"], 110)
        self.assertEqual(result["sevenDay"]["usedTokens"], 220)
        self.write(self.row("msg_old"), append=True)
        self.assertEqual(self.collect()["fiveHour"]["usedTokens"], 110)
        self.request["nowMs"] += 8 * 86400000
        self.request["windows"] = [{"key": "fiveHour", "durationMs": 18000000,
                                   "resetsAt": self.request["nowMs"] + 9000000}]
        self.assertEqual(self.collect()["fiveHour"]["usedTokens"], 0)

    def test_partial_line_is_unknown_until_complete_without_double_counting(self):
        row = json.dumps(self.row())
        self.log.write_text(row[:80], encoding="utf-8")
        self.assertIsNone(self.collect()["fiveHour"]["usedTokens"])
        with self.log.open("a", encoding="utf-8") as stream:
            stream.write(row[80:] + "\n")
        self.assertEqual(self.collect()["fiveHour"]["usedTokens"], 110)

    def test_scan_budget_resumes_incrementally_and_unreadable_directory_is_not_zero(self):
        self.write(self.row(), self.row("msg_two"))
        with patch.object(usage_namespace["time"], "monotonic", side_effect=[0, 0, 0, 13]):
            self.assertIsNone(self.collect()["fiveHour"]["usedTokens"])
        self.assertEqual(self.collect()["fiveHour"]["usedTokens"], 220)
        def denied_walk(root, **options):
            options["onerror"](PermissionError("fixture"))
            return []
        with patch.object(usage_namespace["os"], "walk", side_effect=denied_walk):
            self.assertIsNone(self.collect()["fiveHour"]["usedTokens"])
        self.assertEqual(self.collect()["fiveHour"]["usedTokens"], 220)

    def test_invalid_usage_can_be_repaired_and_never_becomes_fake_zero(self):
        incomplete = self.row()
        incomplete["message"]["usage"] = {}
        self.write(incomplete)
        self.assertIsNone(self.collect()["fiveHour"]["usedTokens"])
        self.write(self.row(), append=True)
        self.assertEqual(self.collect()["fiveHour"]["usedTokens"], 110)
        self.write(self.row(output_tokens=-1), self.row("msg_bad", output_tokens=True))
        self.assertIsNone(self.collect()["fiveHour"]["usedTokens"])
        self.write(self.row("msg_bad"))
        self.assertEqual(self.collect()["fiveHour"]["usedTokens"], 220)

    def test_synthetic_errors_and_nonassistant_rows_are_not_billed(self):
        synthetic = self.row("msg_synthetic")
        synthetic["message"]["model"] = "<synthetic>"
        api_error = self.row("msg_error")
        api_error["isApiErrorMessage"] = True
        self.write(synthetic, api_error, {"type": "user", "message": "assistant"},
                   ["assistant"], self.row())
        self.assertEqual(self.collect()["fiveHour"]["usedTokens"], 110)

    def test_malformed_line_stays_unknown_until_rewrite_repairs_it(self):
        self.log.write_text('{"type":"assistant",broken\n', encoding="utf-8")
        self.assertIsNone(self.collect()["fiveHour"]["usedTokens"])
        self.write(self.row())
        self.assertEqual(self.collect()["fiveHour"]["usedTokens"], 110)

    def test_large_numbers_and_unrecognized_tiers_remain_safe(self):
        self.assertFalse(usage_namespace["number"](10 ** 1000))
        self.write(self.row(service_tier={"invalid": True}))
        result = self.collect()["fiveHour"]
        self.assertEqual(result["usedTokens"], 110)
        self.assertIsNone(result["usedCostUsd"])

    def test_prices_update_without_rescanning_or_changing_tokens(self):
        self.write(self.row())
        original = self.collect()["fiveHour"]
        self.request["prices"]["test-model"]["input_cost_per_token"] *= 2
        with patch.dict(usage_namespace, {"read_events": MagicMock(side_effect=AssertionError("无需重新读正文"))}):
            result = self.collect()["fiveHour"]
        self.assertEqual(result["usedTokens"], original["usedTokens"])
        self.assertAlmostEqual(result["usedCostUsd"], original["usedCostUsd"] + 100e-6)

    def test_table_driven_long_context_tier_fast_and_geo_prices(self):
        row = self.row(input_tokens=150000, service_tier="priority", inference_geo="us")
        self.write(row)
        self.request["prices"]["test-model"].update({
            "input_cost_per_token_above_100k_tokens_priority": 0.000004,
            "output_cost_per_token_above_100k_tokens_priority": 0.00002})
        self.assertAlmostEqual(self.collect()["fiveHour"]["usedCostUsd"], (150000 * 4e-6 + 10 * 2e-5) * 1.1)

    def test_only_valid_windows_are_exported_and_parallel_ledger_updates_are_serialized(self):
        self.write(self.row())
        self.collect()
        database = usage_namespace["sqlite3"].connect(self.cache)
        try:
            database.execute("BEGIN IMMEDIATE")
            with self.assertRaises(usage_namespace["sqlite3"].OperationalError):
                self.collect()
            database.rollback()
        finally:
            database.close()
        self.assertEqual(self.collect()["fiveHour"]["usedTokens"], 110)
        self.request["windows"] = [{"key": "expired", "durationMs": 18000000, "resetsAt": self.now - 1}]
        self.assertEqual(self.collect(), {})


class HistoryEditTests(unittest.TestCase):
    def read(self, rows):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        path = Path(folder.name) / "session.jsonl"
        path.write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")
        native = SdkReader("unused", "00000000-0000-4000-8000-000000000001")
        native.log_path = path
        native.read_messages()
        return native

    def rows(self):
        return [
            {"type": "user", "uuid": "a", "parentUuid": None, "message": {"content": "问题A"}},
            {"type": "attachment", "uuid": "hidden-context", "parentUuid": "a",
             "attachment": {"type": "context"}},
            {"type": "assistant", "uuid": "aa", "parentUuid": "hidden-context",
             "message": {"id": "request-a", "model": "test-model", "content": [{"type": "text", "text": "回复A"}],
                         "usage": {"input_tokens": 100, "output_tokens": 10}}},
            {"type": "user", "uuid": "b", "parentUuid": "aa", "message": {"content": "很长的问题B"}},
            {"type": "assistant", "uuid": "bb", "parentUuid": "b",
             "message": {"id": "request-b", "model": "test-model", "content": [{"type": "text", "text": "回复B"}],
                         "usage": {"input_tokens": 200, "output_tokens": 20}}},
            {"type": "system", "uuid": "done", "parentUuid": "bb", "subtype": "turn_duration"},
        ]

    def test_native_rewind_selects_current_parent_chain_and_retains_numeric_usage_only(self):
        rows = self.rows() + [
            {"type": "user", "uuid": "replacement", "parentUuid": "aa", "message": {"content": "修改的问题B"}},
            {"type": "assistant", "uuid": "new-reply", "parentUuid": "replacement",
             "message": {"content": [{"type": "text", "text": "新回复"}]}}
        ]
        native = self.read(rows)
        self.assertEqual([row["uuid"] for row in native.messages], ["a", "aa", "replacement", "new-reply"])
        self.assertEqual(native.retired_usage[0]["message"], {
            "id": "request-b", "model": "test-model", "usage": {"input_tokens": 200, "output_tokens": 20}})
        native.read_messages()
        self.assertEqual([row["uuid"] for row in native.messages], ["a", "aa", "replacement", "new-reply"])
        self.assertTrue(all("content" not in row["message"] for row in native.retired_usage))


    def test_new_input_after_rewind_is_confirmed_by_append_offset_instead_of_old_index(self):
        native = self.read(self.rows())
        native.pending = [{"clientUserMessageId": "edited-client", "_nativeText": "修改后的问题B",
                           "_after": len(native.messages), "_afterOffset": native.log_offset}]
        with native.log_path.open("a", encoding="utf-8") as stream:
            stream.write(json.dumps({"type": "user", "uuid": "new-input", "parentUuid": "aa",
                                     "message": {"content": "修改后的问题B"}}) + "\n")
        native.read_messages()
        self.assertEqual([row["uuid"] for row in native.messages], ["a", "aa", "new-input"])
        self.assertEqual(native.clients["new-input"], "edited-client")
        self.assertEqual(native.pending, [])

    def test_editing_first_message_and_compaction_keep_distinct_history_semantics(self):
        native = self.read(self.rows() + [
            {"type": "user", "uuid": "new-root", "parentUuid": None, "message": {"content": "新起点"}},
            {"type": "system", "uuid": "compact", "parentUuid": None, "subtype": "compact_boundary"},
            {"type": "user", "uuid": "summary", "parentUuid": "compact", "isCompactSummary": True,
             "message": {"content": "内部摘要"}},
            {"type": "user", "uuid": "next", "parentUuid": "summary", "message": {"content": "继续"}},
        ])
        self.assertEqual([row["uuid"] for row in native.messages], ["new-root", "compact", "summary", "next"])
        self.assertFalse(next(row for row in native.messages if row["uuid"] == "summary")["nativeEditable"])
        self.assertEqual(len(native.retired_usage), 2)


    def test_structured_rewind_marker_cuts_branch_until_the_edited_message_arrives(self):
        marker = {"type": "last-prompt", "leafUuid": "aa", "explicit": True, "rewound": True, "cutUuid": "b"}
        native = self.read(self.rows() + [marker])
        self.assertEqual([row["uuid"] for row in native.messages], ["a", "aa"])
        self.assertEqual(native.history.current_head(), "aa")
        with native.log_path.open("a", encoding="utf-8") as stream:
            stream.write(json.dumps({"type": "user", "uuid": "edited", "parentUuid": "aa",
                                     "message": {"content": "修改后的问题B"}}) + "\n")
        native.read_messages()
        self.assertEqual([row["uuid"] for row in native.messages], ["a", "aa", "edited"])
        plain = self.read(self.rows() + [{"type": "last-prompt", "leafUuid": "done"},
                                         {"type": "last-prompt", "leafUuid": "aa", "explicit": True}])
        self.assertEqual([row["uuid"] for row in plain.messages], ["a", "aa", "b", "bb", "done"])

    def test_structured_rewind_uses_print_control_protocol_without_user_messages(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        root = Path(folder.name)
        cli = root / "claude"
        cli.write_text("\n".join([
            "#!/usr/bin/env python3",
            "import json, os, sys",
            "with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), 'calls.jsonl'), 'a') as log:",
            "    log.write(json.dumps({'argv': sys.argv[1:], 'cwd': os.getcwd()}) + '\\n')",
            "print(json.dumps({'type': 'system', 'subtype': 'noise'}), flush=True)",
            "for line in sys.stdin:",
            "    frame = json.loads(line)",
            "    request, identity = frame['request'], frame['request_id']",
            "    with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), 'calls.jsonl'), 'a') as log:",
            "        log.write(json.dumps({'request': request}) + '\\n')",
            "    if request['subtype'] == 'initialize':",
            "        value = {'subtype': 'success', 'request_id': identity, 'response': {}}",
            "    elif request['target_message_uuid'] == 'unsupported':",
            "        value = {'subtype': 'error', 'request_id': identity, 'error': 'unknown subtype'}",
            "    elif request['last_seen_user_message_uuid'] != request['target_message_uuid']:",
            "        value = {'subtype': 'success', 'request_id': identity,",
            "                 'response': {'rewound': False, 'reason': 'unseen_later_turn'}}",
            "    else:",
            "        value = {'subtype': 'success', 'request_id': identity,",
            "                 'response': {'rewound': True, 'targetMessageUuid': request['target_message_uuid']}}",
            "    print(json.dumps({'type': 'control_response', 'response': value}), flush=True)",
        ]) + "\n", encoding="utf-8")
        cli.chmod(0o755)
        native = SdkReader(str(cli), "00000000-0000-4000-8000-000000000001")
        self.assertTrue(native.run_rewind("b", "b", str(root))["rewound"])
        self.assertTrue(native.delivery_started)
        with self.assertRaises(namespace["NativeError"]) as raised:
            native.run_rewind("b", "a", str(root))
        self.assertEqual(raised.exception.code, "ESTALE")
        self.assertFalse(native.delivery_started)
        with self.assertRaises(namespace["NativeError"]) as raised:
            native.run_rewind("unsupported", "unsupported", str(root))
        self.assertEqual(raised.exception.code, "EUNSUPPORTED")
        records = [json.loads(line) for line in (root / "calls.jsonl").read_text().splitlines()]
        launch = records[0]
        self.assertEqual(os.path.realpath(launch["cwd"]), os.path.realpath(root))
        self.assertEqual(launch["argv"][:6], ["-p", "--input-format", "stream-json", "--output-format",
                                              "stream-json", "--verbose"])
        self.assertEqual(launch["argv"][-2:], ["--resume", native.native_id])
        subtypes = [record["request"]["subtype"] for record in records if "request" in record]
        self.assertEqual(subtypes, ["initialize", "rewind_conversation"] * 3)

    def test_edit_preserves_and_deduplicates_all_original_images(self):
        native = self.read(self.rows())
        target = native.messages[0]
        target["web_images"] = [{"url": f"data:image/png;base64,{namespace['base64'].b64encode(bytes([i])).decode()}"}
                                for i in range(8)]
        target["message"]["content"] = [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}}
        ]
        content = native.edit_content(target, "只修改文字")
        self.assertEqual(len(content), 9)
        self.assertEqual(content[0], {"type": "text", "text": "只修改文字"})
        target["web_images"] = [target["web_images"][0], target["web_images"][0]]
        self.assertEqual(len(native.edit_content(target, "重复添加的原图仍保留")), 3)


class SdkInterruptionTests(unittest.TestCase):
    """Claude Agent SDK 传输下 interrupt 产生的中断标记行（字段取自 CLI 2.1.329 的真实记录）。"""
    MARKER = "[Request interrupted by user]"

    def rows(self):
        return [
            {"type": "user", "uuid": "u1", "promptId": "p1", "message": {"content": [{"type": "text", "text": "SLOW"}]}},
            {"type": "assistant", "uuid": "a1", "parentUuid": "u1", "message": {"id": "msg_1", "content": [
                {"type": "text", "text": "slow-0"}]}},
        ]

    def marker(self, **overrides):
        row = {"type": "user", "uuid": "m1", "parentUuid": "a1", "promptId": "p1", "entrypoint": "sdk-cli",
               "userType": "external", "message": {"role": "user", "content": [{"type": "text", "text": self.MARKER}]}}
        row.update(overrides)
        return row

    def test_marker_written_by_an_sdk_interrupt_is_recognised_and_is_not_a_user_message(self):
        evidence = namespace["native_sdk_interruption"](self.marker(), self.rows())
        self.assertEqual(evidence["responseId"], "msg_1")
        row = {**self.marker(), "nativeSdkInterruption": evidence}
        self.assertTrue(namespace["native_interruption"](row))
        self.assertFalse(namespace["real_user"](row), "中断标记不能成为可编辑的用户消息")

    def test_marker_right_after_the_prompt_points_at_the_prompt_row(self):
        evidence = namespace["native_sdk_interruption"](self.marker(parentUuid="u1"), self.rows()[:1])
        self.assertEqual(evidence["responseId"], "u1")

    def test_the_same_words_typed_by_the_user_stay_an_ordinary_message(self):
        # 用户要求只认原生元数据：真人输入的新提示带 promptSource / turnOrigin / permissionMode，且有自己的 promptId。
        typed = self.marker(promptId="p2", promptSource="typed", turnOrigin="human", permissionMode="default")
        self.assertIsNone(namespace["native_sdk_interruption"](typed, self.rows()))
        self.assertIsNone(namespace["native_sdk_interruption"](self.marker(promptSource="typed"), self.rows()))
        self.assertTrue(namespace["real_user"](typed))

    def test_markers_without_the_native_evidence_are_not_recognised(self):
        recognise = namespace["native_sdk_interruption"]
        self.assertIsNone(recognise(self.marker(entrypoint="cli"), self.rows()), "终端传输沿用原有规则")
        self.assertIsNone(recognise(self.marker(promptId="unknown"), self.rows()))
        self.assertIsNone(recognise(self.marker(parentUuid="missing"), self.rows()))
        quoted = {"content": [{"type": "text", "text": "请解释 " + self.MARKER}]}
        self.assertIsNone(recognise(self.marker(message=quoted), self.rows()))


class SessionHoldersTests(unittest.TestCase):
    """别的进程持有原生会话时网页只读：只枚举 CLI 的进程登记，不抢占、不停止对方。"""

    NATIVE = "11111111-2222-4333-8444-555555555555"

    def register(self, root, pid, *, session=None, start=True, entrypoint="cli", kind="interactive"):
        directory = Path(root) / "sessions"
        directory.mkdir(parents=True, exist_ok=True)
        record = {"pid": pid, "sessionId": session or self.NATIVE, "entrypoint": entrypoint, "kind": kind}
        if start:
            record["procStart"] = namespace["native_process_start"](pid)
        (directory / f"{pid}.json").write_text(json.dumps(record), encoding="utf-8")

    def test_live_registered_process_holds_the_session_and_others_do_not(self):
        with tempfile.TemporaryDirectory() as root:
            self.register(root, os.getpid(), entrypoint="cli")
            holders = namespace["native_session_holders"](root, self.NATIVE)
            self.assertEqual(holders, [{"pid": os.getpid(), "entrypoint": "cli", "kind": "interactive"}])
            self.assertEqual(namespace["native_session_holders"](root, "99999999-2222-4333-8444-555555555555"), [])

    def test_stale_registration_never_counts_even_when_the_pid_is_reused(self):
        with tempfile.TemporaryDirectory() as root:
            # PID 仍存在但启动标识不同：旧登记属于已退出的进程，不能造成永久“已在另一个应用中打开”。
            self.register(root, os.getpid())
            record = Path(root) / "sessions" / f"{os.getpid()}.json"
            data = json.loads(record.read_text())
            data["procStart"] = "1"
            record.write_text(json.dumps(data), encoding="utf-8")
            self.assertEqual(namespace["native_session_holders"](root, self.NATIVE), [])
            # 进程已退出：登记残留也不算。
            dead = 4_194_000
            while Path(f"/proc/{dead}").exists():
                dead -= 1
            self.register(root, dead, start=False)
            self.assertEqual(namespace["native_session_holders"](root, self.NATIVE), [])
            # 损坏的登记文件按不存在处理，不抛错。
            (Path(root) / "sessions" / "broken.json").write_text("{", encoding="utf-8")
            self.assertEqual(namespace["native_session_holders"](root, self.NATIVE), [])

    def test_processes_started_by_this_program_are_not_external_holders(self):
        with tempfile.TemporaryDirectory() as root:
            host = Path(root) / "claude-sdk-host.py"
            # 宿主脚本启动一个子进程（相当于 CLI），子进程登记为该会话；它的父进程命令行里有宿主脚本路径。
            host.write_text(
                "import subprocess, sys, time\n"
                "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])\n"
                "print(child.pid, flush=True)\n"
                "time.sleep(30)\n", encoding="utf-8")
            process = subprocess.Popen([sys.executable, str(host), "--serve"], stdout=subprocess.PIPE, text=True)
            try:
                child = int(process.stdout.readline())
                self.register(root, child, entrypoint="sdk-cli")
                self.assertEqual(len(namespace["native_session_holders"](root, self.NATIVE)), 1)
                self.assertEqual(namespace["native_session_holders"](root, self.NATIVE, [str(host)]), [])
                # 路径不完全相同的标记不能误伤：必须是命令行里的完整参数。
                self.assertEqual(len(namespace["native_session_holders"](root, self.NATIVE, [str(host) + "x", "claude"])), 1)
                self.assertEqual(len(namespace["native_session_holders"](root, self.NATIVE, [])), 1)
            finally:
                process.kill()
                process.wait()
                with contextlib.suppress(ProcessLookupError):
                    os.kill(child, 9)

    def test_discovery_reports_holders_without_starting_anything(self):
        with tempfile.TemporaryDirectory() as root:
            self.register(root, os.getpid())
            discovery = NativeDiscovery(root)
            reply = discovery.control({"subtype": "holders", "native_id": self.NATIVE, "own": []})
            self.assertEqual([holder["pid"] for holder in reply["holders"]], [os.getpid()])
            with self.assertRaises(ValueError):
                discovery.control({"subtype": "holders", "native_id": "not-a-uuid"})
            with self.assertRaises(RuntimeError):
                discovery.control({"subtype": "holders", "native_id": self.NATIVE, "own": ["x"] * 9})



class SdkHostConnectionTests(unittest.TestCase):
    """宿主对 Harness 连接的超时只能限制发送，读取必须保持阻塞。

    真实联调里，计划确认等待超过一分钟时宿主丢掉了 Harness 连接：网页显示会话空闲、审批请求消失，CLI 仍在等回答。"""

    def load_host(self):
        # 与其他样例一样直接执行源码，不经导入机制，避免在源码目录留下字节码缓存。
        path = Path(__file__).resolve().parents[1] / "crates/elves-harness/src/claude_sdk/host.py"
        scope = {"__name__": "claude_sdk_host_test"}
        exec(compile(path.read_text(encoding="utf-8"), str(path), "exec"), scope)
        return scope

    def test_a_silent_harness_is_not_dropped_by_a_read_timeout(self):
        scope = self.load_host()
        scope["CLIENT_SEND_TIMEOUT"] = 0.3
        host = scope["Host"]("00000000-0000-4000-8000-000000000000", {})
        # cat 充当 CLI：写入的内容原样回显，足以证明连接在沉默之后仍然双向可用。
        host.cli = subprocess.Popen(["cat"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        server, client = socket.socketpair(socket.AF_UNIX)
        self.addCleanup(client.close)
        self.addCleanup(host.cli.wait)
        self.addCleanup(host.cli.kill)
        host.attach(server)
        reader = client.makefile("rb")
        self.assertEqual(json.loads(reader.readline())["type"], "host_hello")
        # 读取方向不能有超时；旧实现 settimeout(60) 会在 Harness 沉默 60 秒后丢掉连接。
        self.assertIsNone(server.gettimeout())
        sending = server.getsockopt(socket.SOL_SOCKET, socket.SO_SNDTIMEO, 16)
        seconds, micros = struct.unpack("ll", sending)
        self.assertAlmostEqual(seconds + micros / 1_000_000, 0.3, places=1)

        time.sleep(1.0)  # 超过发送超时数倍的沉默
        self.assertIs(host.client, server)
        client.sendall(b'{"type":"user","message":"still attached"}\n')
        self.assertIn(b"still attached", host.cli.stdout.readline())


if __name__ == "__main__":
    unittest.main()
