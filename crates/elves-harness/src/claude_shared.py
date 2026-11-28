"""Claude 会话的 WSL 读取端：单个会话的历史读取器与只读原生发现；不运行模型、不连接 CLI、不修改 CLI、设置或权限。

用户要求改用 Claude Agent SDK 对接后，执行、审批、问答和消息回执由 Rust 对端经 stream-json 控制协议完成，
这里不再有 daemon 后台任务和终端画面识别。唯一会结束进程的入口是用户在会话里确认“关闭其他占用”后的
release_holders，它只结束登记为该会话且不属于本程序的 CLI 进程。"""
import argparse
import base64
import contextlib
from datetime import datetime
import fcntl
import hashlib
import html
import json
import os
from pathlib import Path
import queue
import re
import signal
import socket
import subprocess
import sys
import threading
import time
import unicodedata
import uuid


def emit(value):
    print(json.dumps(value, ensure_ascii=False, separators=(",", ":")), flush=True)


class NativeError(RuntimeError):
    def __init__(self, code, message=None):
        self.code = code
        super().__init__(message or f"原版 Claude 未接受操作（{code}）")


# 原生 rewind_conversation 明确拒绝时返回的原因；这些答复都表示记录未被修改。
# 未列出的原因统一按“不支持”报告，不猜测是否已回退。
REWIND_REFUSALS = {
    "target_not_found": ("ESTALE", "原生会话里找不到要编辑的消息（可能已被压缩或回退），请刷新后重试"),
    "stale_target": ("ESTALE", "原生会话有网页尚未显示的新内容，请刷新后重新编辑"),
    "unseen_later_turn": ("ESTALE", "原生会话有网页尚未显示的新内容，请刷新后重新编辑"),
    "state_changed": ("ESTALE", "原生会话状态已变化，请刷新后重新编辑"),
    "turn_running": ("EBUSY", "Claude 仍在执行，请等待结束后再编辑"),
    "commands_queued": ("EBUSY", "Claude 仍有排队中的输入，请等待处理后再编辑"),
    "prompt_pending": ("EBUSY", "Claude 仍有待处理的输入，请等待处理后再编辑"),
    "usage_limited": ("EBUSY", "当前用量受限，原版暂不允许回退"),
    "poll_tool_result_target": ("EUNSUPPORTED", "该消息位于工具调用过程中，原版不支持从这里编辑"),
    "target_splits_tool_call": ("EUNSUPPORTED", "该消息位于工具调用过程中，原版不支持从这里编辑"),
    "delivered_poll_events_in_range": ("EUNSUPPORTED", "该消息之后有已送达的后台事件，原版不支持从这里编辑"),
    "persist_failed": ("ESESSION", "原版未能保存回退记录，未修改历史"),
}


def native_time(value):
    try:
        if isinstance(value, str):
            return max(0, int(datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp() * 1000))
        return max(0, int(value or 0))
    except (ValueError, TypeError, OverflowError):
        return 0


def native_interruption(row):
    # 用户要求核实中断提示：只认原生关联字段与完整控制块，真实用户引用/同名文字不能按前缀过滤。
    if row.get("type") != "user":
        return False
    message = row.get("message")
    if not isinstance(message, dict):
        return False
    content = message.get("content")
    reference = row.get("interruptedMessageId")
    if not isinstance(reference, str) or not reference.strip():
        interruption = row.get("nativeToolInterruption")
        reference = interruption.get("responseId") if isinstance(interruption, dict) else None
    if not isinstance(reference, str) or not reference.strip():
        interruption = row.get("nativeSdkInterruption")
        reference = interruption.get("responseId") if isinstance(interruption, dict) else None
    return (isinstance(reference, str) and bool(reference.strip()) and isinstance(content, list)
            and len(content) == 1 and isinstance(content[0], dict) and content[0].get("type") == "text"
            and isinstance(content[0].get("text"), str) and content[0]["text"].strip() in
            ("[Request interrupted by user]", "[Request interrupted by user for tool use]"))


SDK_INTERRUPTION_TEXTS = ("[Request interrupted by user]", "[Request interrupted by user for tool use]")

# 不进入历史的记录（附件等）的父子关系最多保留这么多条；它们只夹在相邻几条消息之间，保留最近的就够用。
ROW_LINK_LIMIT = 4096

# 除 sdk-* 之外，本机真实会话里确实出现过中断标记的非终端入口：Claude 桌面应用（2.1.321 的记录，与 SDK 同样没有
# interruptedMessageId）。入口不明的记录不认，没有实测依据的入口（如编辑器扩展）不在此列。
INTERRUPTION_HOSTS = ("claude-desktop",)


def native_message_ancestor(row, previous, links):
    """沿 parentUuid 向上找到最近一条进入历史的消息。

    CLI 2.1.330 起，工具结果与后面的记录之间会写入 attachment（prompt_snapshot、deferred_tools_delta 等）；
    它们不进入历史，所以中断标记的父节点可能是这样一条附件（用户本机真实会话里就是三条附件）。
    `links` 记着被跳过记录的父节点（uuid → parentUuid），顺着它们继续向上找。"""
    parent_id = row.get("parentUuid")
    for _ in range(64):
        if not isinstance(parent_id, str) or not parent_id:
            return None
        found = next((value for value in reversed(previous) if value.get("uuid") == parent_id), None)
        if found is not None:
            return found
        parent_id = (links or {}).get(parent_id)
    return None


def native_sdk_interruption(row, previous, links=None):
    """非终端入口（Claude Agent SDK 的 stream-json、桌面应用）用 interrupt 控制请求中止回复时 CLI 写入的中断标记。

    这行没有 interruptedMessageId，entrypoint 是 sdk-*（2.1.329、2.1.330 实测）或 INTERRUPTION_HOSTS 里的入口，
    不带真人输入才有的 promptSource / turnOrigin / turnAttributionKey / permissionMode，并且与被中断的那条提示共用
    同一个 promptId。用户要求只修正经原生元数据确认的中断，所以这几条同时满足才认；用户自己输入同样文字的消息是一条
    新提示，带有这些属性并有自己的 promptId，不会被吞掉。
    父节点沿父子链穿过附件去找（见 native_message_ancestor）；链断了也不影响判定，因为 promptId 已经证明它属于该提示，
    此时以同一提示里最近的一行作为被中断的位置。"""
    message = row.get("message")
    content = message.get("content") if isinstance(message, dict) else None
    entrypoint = row.get("entrypoint")
    if (row.get("type") != "user" or not isinstance(entrypoint, str)
            or not (entrypoint.startswith("sdk") or entrypoint in INTERRUPTION_HOSTS)
            or not isinstance(content, list) or len(content) != 1 or not isinstance(content[0], dict)
            or content[0].get("type") != "text" or content[0].get("text") not in SDK_INTERRUPTION_TEXTS):
        return None
    if any(key in row for key in ("promptSource", "turnOrigin", "turnAttributionKey", "permissionMode",
                                  "interruptedMessageId")):
        return None
    prompt = row.get("promptId")
    if not isinstance(prompt, str) or not prompt.strip():
        return None
    in_prompt = next((value for value in reversed(previous) if value.get("promptId") == prompt), None)
    if in_prompt is None:
        return None
    parent = native_message_ancestor(row, previous, links) or in_prompt
    response = parent.get("message", {}).get("id") if parent.get("type") == "assistant" else None
    return {"responseId": response if isinstance(response, str) and response.strip() else parent["uuid"],
            "source": "sdk-interrupt"}


def native_tool_interruption(row, previous, links=None):
    # 2.1.325 实际历史中，工具拒绝后的中断没有 interruptedMessageId。
    # 用户要求历史也显示“已停止”：必须核对同一 promptId、拒绝结果父节点及原始工具调用，不能只按英文隐藏。
    # 父节点穿过附件去找（见 native_message_ancestor）：新版本在拒绝结果和中断标记之间也会写附件。
    message = row.get("message")
    content = message.get("content") if isinstance(message, dict) else None
    if (row.get("type") != "user" or row.get("entrypoint") != "cli"
            or not isinstance(content, list) or len(content) != 1
            or not isinstance(content[0], dict) or content[0].get("type") != "text"
            or content[0].get("text") != "[Request interrupted by user for tool use]"):
        return None
    prompt = row.get("promptId")
    if not isinstance(prompt, str) or not prompt.strip():
        return None
    parent = native_message_ancestor(row, previous, links)
    if (not parent or parent.get("type") != "user" or parent.get("promptId") != prompt
            or parent.get("toolDenialKind") != "user-rejected"):
        return None
    result = parent.get("message", {}).get("content")
    if (not isinstance(result, list) or len(result) != 1 or not isinstance(result[0], dict)
            or result[0].get("type") != "tool_result" or result[0].get("is_error") is not True):
        return None
    tool = result[0].get("tool_use_id")
    assistant_id = parent.get("sourceToolAssistantUUID")
    if not isinstance(tool, str) or not tool.strip() or not isinstance(assistant_id, str) or not assistant_id:
        return None
    assistant = next((value for value in reversed(previous) if value.get("uuid") == assistant_id), None)
    if not assistant or assistant.get("type") != "assistant":
        return None
    response = assistant.get("message", {})
    blocks = response.get("content")
    reference = response.get("id")
    if (not isinstance(reference, str) or not reference.strip() or not isinstance(blocks, list)
            or not any(isinstance(part, dict) and part.get("type") == "tool_use"
                       and part.get("id") == tool for part in blocks)):
        return None
    # 只增加只读投影的关联证据，不补写原生 JSONL，也不伪造原本缺失的 interruptedMessageId。
    return {"responseId": reference, "toolUseId": tool, "toolResultId": parent["uuid"]}


def native_setting_source(row):
    """原生 CLI 自己写入的控制记录（本地斜杠命令及其输出）的来源核对；用户输入或引用的同样标签不属于此类。

    终端（entrypoint=cli）：没有真人输入属性（origin=human、promptSource=typed、turnOrigin=human）。
    SDK（stream-json，entrypoint=sdk-cli）：网页等 SDK 客户端提交的提示带 promptSource、permissionMode（2.1.330 起
    还有 turnOrigin；2.1.329 没有 turnOrigin），自定义命令和技能的命令行只带 turnOrigin；CLI 自己写的本地命令记录
    （caveat、命令行、输出行）一项都没有。所以三项中任一存在即视为用户输入（2.1.329、2.1.330 在本机真实会话中核对）。
    用户要求 Claude 实例不把 /model、/compact 这类命令记录显示成带标签的用户气泡；SDK 记录以前只认终端，
    所以直接露出了 <command-name> 等标签。用户手写或引用的同样标签带有上面的属性，仍按用户正文保留。"""
    if row.get("type") != "user":
        return False
    origin, entrypoint = row.get("origin"), row.get("entrypoint")
    if entrypoint == "cli":
        return ((origin is None or isinstance(origin, dict) and origin.get("kind") != "human")
                and row.get("promptSource") != "typed" and row.get("turnOrigin") != "human")
    return (isinstance(entrypoint, str) and entrypoint.startswith("sdk") and origin is None
            and not any(key in row for key in ("promptSource", "turnOrigin", "permissionMode")))


def native_setting_command(row):
    # 用户要求模型/等级命令使用变更提示行；仅识别 CLI 控制记录，保留用户输入或引用的同样标签。
    if not native_setting_source(row) or not isinstance(row.get("message"), dict):
        return None
    content = row.get("message", {}).get("content")
    if not isinstance(content, str):
        return None
    match = re.fullmatch(
        r"\s*<command-name>/(model|effort)</command-name>\s*"
        r"<command-message>\1</command-message>\s*"
        r"<command-args>([^<>\r\n]*)</command-args>\s*", content)
    return {"name": match[1], "value": match[2].strip()} if match else None


def native_skill_command(row):
    # 原生 Skill 调用用 command-message/name/args 包装；这里只识别候选，
    # 必须再关联网页原输入或同 promptId 的原生技能展开记录，不能按 XML 文本改写用户引用。
    if row.get("type") != "user" or row.get("isMeta") is True \
            or row.get("entrypoint") not in ("cli", "sdk-cli") or not row.get("promptId"):
        return None
    content = row.get("message", {}).get("content")
    if not isinstance(content, str):
        return None
    match = re.fullmatch(
        r"\s*<command-message>([\w.:-]+)</command-message>\s*"
        r"<command-name>/([\w.:-]+)</command-name>"
        r"(?:\s*<command-args>([\s\S]*)</command-args>)?\s*", content)
    if not match or match[2] in ("model", "effort", "compact", "goal"):
        return None
    arguments = (match[3] or "").strip()
    return {"name": match[2], "text": "/" + match[2] + (" " + arguments if arguments else "")}


def skill_input_key(text):
    # 原生会规整命令与参数之间的空白；只用于 Skill 回执关联，参数内部的空白和其他正文保持原样。
    parts = text.strip().split(maxsplit=1) if isinstance(text, str) else []
    return parts[0] + (" " + parts[1].strip() if len(parts) > 1 else "") if parts else ""


def after_delivery_intent(row, index, intent):
    # 新意图用追加偏移跨越原生回退分支；旧存档继续兼容原有顺序索引。
    return row.get("nativeOffset", -1) >= intent["_afterOffset"] if "_afterOffset" in intent \
        else index >= intent.get("_after", 0)


def normalize_skill_records(messages, pending):
    candidates = [(index, row) for index, row in enumerate(messages)
                  if row.get("nativeSkillCommand") and not row["nativeSkillCommand"].get("accepted")]
    if not candidates:
        return False
    companions = {(row.get("parentUuid"), row.get("promptId")) for row in messages
                  if row.get("type") == "user" and row.get("isMeta") is True
                  and row.get("turnCompanion") is True and row.get("promptId")}
    changed = False
    for index, row in candidates:
        command = row.get("nativeSkillCommand")
        raw = row.get("message", {}).get("content")
        intents = [intent for intent in pending if after_delivery_intent(row, index, intent)]
        if any(intent.get("_nativeText") == raw for intent in intents):
            # 网页明确发送的是 XML 引用时保持原文，即使另有内部伴随记录也不能误当命令。
            row.pop("nativeSkillCommand", None)
            continue
        intent = next((intent for intent in intents if skill_input_key(intent.get("_nativeText")) == command["text"]), None)
        if intent is None and (row.get("uuid"), row.get("promptId")) not in companions:
            continue
        row["message"] = {**row["message"], "content": command["text"]}
        row["nativeSkillCommand"] = {"name": command["name"], "accepted": True}
        changed = True
    return changed


def native_control_command(row):
    # 只识别原生 CLI 控制记录；用户引用相同 XML 不能被解释为目标或压缩操作。
    if not native_setting_source(row):
        return None
    content = row.get("message", {}).get("content")
    if not isinstance(content, str):
        return None
    match = re.fullmatch(r"\s*<command-name>/(goal|compact)</command-name>\s*"
                        r"<command-message>\1</command-message>\s*"
                        r"<command-args>([\s\S]*)</command-args>\s*", content)
    return {"name": match[1], "value": match[2].strip()} if match else None


def native_agent_notification(row):
    # 子代理完成通知是原生控制消息；只认原生来源和完整结构，用户引用相同 XML 仍按普通正文显示。
    origin = row.get("origin") or {}
    if (row.get("type") != "user" or not isinstance(origin, dict) or origin.get("kind") != "task-notification"
            or origin.get("producer") != "session-task"):
        return None
    content = row.get("message", {}).get("content")
    if (isinstance(content, list) and len(content) == 1 and isinstance(content[0], dict)
            and content[0].get("type") == "text"):
        content = content[0].get("text")
    if not isinstance(content, str) or not re.fullmatch(r"\s*<task-notification>[\s\S]*</task-notification>\s*", content):
        return None
    def field(name):
        match = re.search(rf"<{name}>([^<]*)</{name}>", content)
        return html.unescape(match[1]).strip() if match else ""
    identity, status = field("task-id"), field("status")
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", identity) or status not in ("completed", "failed", "stopped"):
        return None
    value = {"agentId": identity, "status": status, "toolUseId": field("tool-use-id"),
             "completedAtMs": native_time(row.get("timestamp"))}
    for tag, key in (("subagent_tokens", "totalTokens"), ("tool_uses", "totalToolUseCount"),
                     ("duration_ms", "totalDurationMs")):
        text = field(tag)
        if text.isdigit():
            value[key] = int(text)
    return value


def native_task_notification(row):
    # 此处只识别任务回执，不推断续跑；Rust 用原生父消息身份核验是否确实产生了后续模型输出。
    # 只认原生来源（origin.kind=task-notification）且正文完整为 <task-notification> 的记录；用户引用相同 XML 仍是正文。
    # 监控事件没有 status，不能套用子代理通知的状态校验（2.1.323 实测）。
    origin = row.get("origin") or {}
    message = row.get("message")
    if row.get("type") != "user" or not isinstance(origin, dict) or origin.get("kind") != "task-notification" \
            or not isinstance(message, dict):
        return None
    content = message.get("content")
    if (isinstance(content, list) and len(content) == 1 and isinstance(content[0], dict)
            and content[0].get("type") == "text"):
        content = content[0].get("text")
    if not isinstance(content, str) or not re.fullmatch(r"\s*<task-notification>[\s\S]*</task-notification>\s*", content):
        return None

    def field(name, limit=400):
        match = re.search(rf"<{name}>([\s\S]*?)</{name}>", content)
        if not match:
            return ""
        text = html.unescape(match[1])
        # 原生摘要中的错误按原文回填工具记录，不裁剪空白、换行或长度。
        return text if limit is None else text.strip()[:limit]

    identity = field("task-id")
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", identity):
        return None
    value = {"taskId": identity, "summary": field("summary", None)}
    if row.get("shouldQuery") is False or row.get("skip_transcript") is True or row.get("ambient") is True:
        value["statusOnly"] = True
    status, tool, event = field("status"), field("tool-use-id"), field("event", 2000)
    if re.fullmatch(r"[a-z_]{1,32}", status):
        value["status"] = status
    if re.fullmatch(r"[A-Za-z0-9_-]{1,128}", tool):
        value["toolUseId"] = tool
    if event:
        value["event"] = event
    return value


_LOCAL_COMMAND = re.compile(r"\s*<command-name>/([\w.:-]+)</command-name>\s*<command-message>[^<]*</command-message>\s*"
                            r"(?:<command-args>([\s\S]*)</command-args>)?\s*")
_LOCAL_OUTPUT = re.compile(r"\s*<local-command-(stdout|stderr)>([\s\S]*)</local-command-\1>\s*")
_ANSI = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")


def native_local_command(row, parent=None):
    # 用户要求终端执行的本地斜杠命令（如 /resume、/skills）显示为“终端执行”事件，不显示成可编辑的用户消息。
    # 旧格式：CLI 写入的 user 行，父节点是同一 promptId 的 <local-command-caveat> 元数据（2.1.323 实测）；
    # 新格式：system/local_command。模型/等级、目标、压缩命令另有专门展示，这里排除。
    if row.get("type") == "system":
        content = row.get("content") if row.get("subtype") == "local_command" else None
    elif native_setting_source(row) and isinstance(row.get("message"), dict):
        # SDK 记录的来源已由 native_setting_source 按“没有任何提示属性”核对（用户提示带 promptSource、turnOrigin 或 permissionMode），
        # 不再依赖 caveat 父节点；终端记录的属性不足以区分用户输入，仍要求同一 promptId 的 caveat 父节点。
        if row.get("entrypoint") == "cli":
            caveat = parent.get("message", {}).get("content") if isinstance(parent, dict) else None
            if not (isinstance(caveat, str) and caveat.lstrip().startswith("<local-command-caveat>")
                    and parent.get("isMeta") is True and parent.get("promptId") == row.get("promptId") is not None):
                return None
        content = row["message"].get("content")
    else:
        return None
    match = _LOCAL_COMMAND.fullmatch(content) if isinstance(content, str) else None
    if not match or match[1] in ("model", "effort", "goal", "compact"):
        return None
    command = {"name": "/" + match[1], "args": (match[2] or "").strip()[:400]}
    if str(row.get("entrypoint") or "").startswith("sdk"):
        # 网页等 SDK 客户端发起的命令不是在终端执行的；网页据此显示“执行了”而不是“终端执行了”。
        command["via"] = "sdk"
    return command


def native_local_output(row, parent=None):
    # 本地命令输出只归属到已识别的命令记录；去掉终端控制序列，保留原文供网页信息提示查看。
    if not isinstance(parent, dict) or not parent.get("nativeLocalCommand"):
        return None
    content = row.get("content") if row.get("type") == "system" else row.get("message", {}).get("content") \
        if isinstance(row.get("message"), dict) else None
    match = _LOCAL_OUTPUT.fullmatch(content) if isinstance(content, str) else None
    if not match:
        return None
    return {"commandId": parent["uuid"], "stream": match[1], "text": _ANSI.sub("", html.unescape(match[2])).strip()[:4000]}


def native_system_notice(row):
    # 用户要求不再静默丢弃原生系统记录：离开期间回顾、原生提示、阻止继续或出错的 Stop Hook 以事件行展示；
    # 无输出、未阻止的 Stop Hook 汇总只是例行记录，不展示。
    if row.get("type") != "system":
        return None
    subtype, content = row.get("subtype"), row.get("content")
    if subtype == "away_summary" and isinstance(content, str) and content.strip():
        return {"kind": "awaySummary", "text": re.sub(r"\s*\(disable recaps in /config\)\s*$", "", content.strip())[:2000]}
    if subtype == "informational" and isinstance(content, str) and content.strip():
        return {"kind": "informational", "text": content.strip()[:2000]}
    if subtype == "stop_hook_summary":
        errors = [error if isinstance(error, str) else json.dumps(error, ensure_ascii=False)
                  for error in row.get("hookErrors") or [] if error][:5] if isinstance(row.get("hookErrors"), list) else []
        reason = row.get("stopReason") if isinstance(row.get("stopReason"), str) else ""
        if errors or row.get("preventedContinuation") is True:
            return {"kind": "stopHook", "prevented": row.get("preventedContinuation") is True,
                    "reason": reason.strip()[:1000], "errors": [error[:400] for error in errors]}
    return None


def native_turn_start(row):
    # 原生自动续跑以任务通知开启新的一轮；判断轮次是否结束时与真实用户输入同等对待。
    return real_user(row) or bool(row.get("nativeTaskNotification") or native_task_notification(row))


# 与网页 app.js 的 editedFileDiffLimit（1 MiB）保持一致；超过时只转发“覆盖修改”，网页显示未提供差异。
NATIVE_PATCH_LIMIT = 1024 * 1024
# 网页图片沿用远程输入限制：单张 5 MiB、合计 12 MiB；文件名 0-7 对应最多 8 张，与 remote_access.rs 同步核对。
REMOTE_IMAGE_LIMIT = 5 * 1024 * 1024
REMOTE_IMAGES_TOTAL_LIMIT = 12 * 1024 * 1024
REMOTE_IMAGE_TYPES = {".png": "image/png", ".jpg": "image/jpeg", ".gif": "image/gif", ".webp": "image/webp"}


def remote_upload_path(line, uploads):
    # 用户要求空网页登记也恢复原图：仅认可原生 prompt 内本用户上传目录的直接文件，不能跨目录/符号链接读取或伪造 client ID。
    # 网页消息与 Claude 提问回答中的“@路径”引用共用此校验。
    path = Path(line[1:]) if line.startswith("@") else None
    if (path is None or path.parent.parent != uploads or not re.fullmatch(r"[A-Za-z0-9_-]{1,120}", path.parent.name)
            or not re.fullmatch(r"[0-7]\.(png|jpg|gif|webp)", path.name)
            or path.parent.resolve().parent != uploads.resolve()
            or path.resolve().parent != path.parent.resolve()):
        return None
    return path


def remote_upload_url(path):
    try:
        if path.stat().st_size > REMOTE_IMAGE_LIMIT:
            return None
        data = path.read_bytes()
    except OSError:
        return None
    return f"data:{REMOTE_IMAGE_TYPES[path.suffix]};base64,{base64.b64encode(data).decode()}"


def native_question_answers(result, root):
    # 用户要求经远程会话发出的回复在正文中可见（2026-11-15：提问等待时从输入框发出的文字和截图被原生记为
    # AskUserQuestion 回答，网页只在折叠的工具分组里能看到）。按题目顺序转发原生回答原文，未作答为 None；
    # 回答里本用户上传目录的“@路径”行改为图片，不显示内部路径。2.1.325 实测 toolUseResult 只有 questions 与 answers。
    questions, answers = result.get("questions"), result.get("answers")
    if (not isinstance(questions, list) or not isinstance(answers, dict)
            or not all(isinstance(question, dict) and isinstance(question.get("question"), str) for question in questions)):
        return None
    uploads = Path(root) / "remote-attachments" if root else None
    replies, total = [], 0
    for question in questions:
        answer = answers.get(question["question"])
        if not isinstance(answer, str):
            replies.append({"question": question["question"], "answer": None, "images": []})
            continue
        lines, images, folder = [], [], None
        for line in answer.splitlines():
            path = remote_upload_path(line, uploads) if uploads else None
            # 同一回答只认可同一上传目录（与网页消息一致）；其余以“@”开头的文字按原文保留。
            if path is None or folder not in (None, path.parent):
                lines.append(line)
                continue
            folder = path.parent
            url = remote_upload_url(path)
            size = len(base64.b64decode(url.split(",", 1)[1])) if url else 0
            if size and total + size <= REMOTE_IMAGES_TOTAL_LIMIT and all(image["url"] != url for image in images):
                total += size
                images.append({"url": url})
        replies.append({"question": question["question"], "answer": "\n".join(lines).strip(), "images": images})
    return replies


def native_tool_result(row, root=None):
    # 用户要求按 Claude 原生语义渲染工具：只转发展示需要的结构化补丁、后台任务 ID、任务清单 ID 与提问回答，
    # 不复制 originalFile、stdout 等大字段。由 Rust claude_tools.rs 消费，字段改名需同步核对。
    # root 为原生配置目录，用于读取提问回答引用的网页上传图片；子代理只读视图不传（子代理不能向用户提问）。
    result = row.get("toolUseResult")
    message = row.get("message")
    content = message.get("content") if isinstance(message, dict) else None
    if row.get("type") != "user" or not isinstance(result, dict) or not isinstance(content, list):
        return None
    calls = [part.get("tool_use_id") for part in content
             if isinstance(part, dict) and part.get("type") == "tool_result"]
    # 原生每个工具结果单独成行；一行多个结果时元数据归属不明，不转发。
    if len(calls) != 1 or not isinstance(calls[0], str):
        return None
    value = {}
    if isinstance(result.get("filePath"), str) and isinstance(result.get("structuredPatch"), list):
        value["filePath"] = result["filePath"]
        value["fileChange"] = "create" if result.get("type") == "create" else "update"
        hunks, size = [], 0
        for hunk in result["structuredPatch"]:
            numbers = [hunk.get(key) if isinstance(hunk, dict) else None
                       for key in ("oldStart", "oldLines", "newStart", "newLines")]
            lines = hunk.get("lines") if isinstance(hunk, dict) else None
            if (not all(type(number) is int and number >= 0 for number in numbers)
                    or not isinstance(lines, list) or not all(isinstance(line, str) for line in lines)):
                hunks = None
                break
            size += sum(len(line) + 1 for line in lines)
            hunks.append({**dict(zip(("oldStart", "oldLines", "newStart", "newLines"), numbers)), "lines": lines})
        if hunks is not None and size <= NATIVE_PATCH_LIMIT:
            value["structuredPatch"] = hunks
    task = result.get("backgroundTaskId")
    if isinstance(task, str) and re.fullmatch(r"[A-Za-z0-9_-]{1,64}", task):
        value["backgroundTaskId"] = task
    if result.get("interrupted") is True:
        value["interrupted"] = True
    created = result.get("task")
    if isinstance(created, dict) and type(created.get("id")) in (str, int):
        value["createdTaskId"] = str(created["id"])
    if result.get("success") is False:
        value["success"] = False
    replies = native_question_answers(result, root)
    if replies is not None:
        value["questionAnswers"] = replies
    return {"toolUseId": calls[0], **value} if value else None


def native_sdk_local_record(row):
    """SDK 传输下 CLI 写入的本地命令行或输出行（只凭行自身核对，与按父链核对的 native_local_command 互补）。

    会话列表标题、最近时间等读取原始行的位置没有父链可查，也要避免把 /context、/model 这类命令行当成用户的第一条消息。"""
    if not str(row.get("entrypoint") or "").startswith("sdk") or not native_setting_source(row):
        return False
    message = row.get("message")
    content = message.get("content") if isinstance(message, dict) else None
    return isinstance(content, str) and bool(_LOCAL_COMMAND.fullmatch(content) or _LOCAL_OUTPUT.fullmatch(content))


def queued_human_prompt(attachment):
    """执行中被 CLI 吸收的用户消息（网页“引导”，或排队后提前送入）记为 queued_command 附件。

    返回可直接作为用户消息 content 的正文（字符串或内容块数组）；不是用户消息或没有可显示内容时返回 None。
    形态随 CLI 版本变化，两种都必须认：
    - 2.1.330 及以前：prompt 是字符串，以 humanTurn=true 且 origin.kind=human 标明。
    - 2.1.332 起：prompt 是内容块数组，不再有 humanTurn / origin，改以 hostsOwnTurn=true 标明
      （没有 origin；若带 origin 则必须是 human）。按 2026-11 实测的原生 JSONL 记录确认。
    任务通知（commandMode=task-notification）、其他来源、shell 模式，以及缺少人工标记的未知形态都不是用户消息，
    保持隐藏而不是凭猜测显示；以前这里只认旧形态，新形态整行被丢弃，引导消息从原生历史消失。
    """
    if not isinstance(attachment, dict) or attachment.get("type") != "queued_command" \
            or attachment.get("commandMode") != "prompt":
        return None
    origin = attachment.get("origin")
    human_origin = isinstance(origin, dict) and origin.get("kind") == "human"
    if attachment.get("humanTurn") is True:
        if not human_origin:
            return None
    elif attachment.get("hostsOwnTurn") is True and attachment.get("humanTurn") is not False:
        if origin is not None and not human_origin:
            return None
    else:
        return None
    prompt = attachment.get("prompt")
    if isinstance(prompt, str):
        return prompt if prompt.strip() else None
    if isinstance(prompt, list) and prompt and all(isinstance(part, dict) for part in prompt) \
            and any(part.get("type") == "image" or part.get("type") == "text"
                    and isinstance(part.get("text"), str) and bool(part["text"].strip()) for part in prompt):
        return prompt
    return None


def real_user(row):
    if any(row.get(key) is True for key in ("isMeta", "isSidechain", "isCompactSummary", "isVisibleInTranscriptOnly")):
        return False
    if native_sdk_local_record(row):
        return False
    command = row.get("nativeControlCommand") or native_control_command(row)
    if command:
        return command["name"] == "goal" and command["value"].lower() not in ("", "clear", "stop", "off", "reset", "none", "cancel")
    if native_interruption(row) or native_agent_notification(row) or row.get("nativeAgentNotification") \
            or row.get("nativeTaskNotification") or native_task_notification(row) \
            or row.get("nativeLocalCommand") or row.get("nativeLocalOutput") \
            or native_setting_command(row) or row.get("nativeControlOutput") \
            or row.get("nativeSettingCommand") or row.get("nativeSettingOutput"):
        return False
    if row.get("type") == "attachment":
        return queued_human_prompt(row.get("attachment")) is not None
    if row.get("type") != "user":
        return False
    message = row.get("message")
    if not isinstance(message, dict):
        return False
    content = message.get("content", [])
    if isinstance(content, str):
        return bool(content.strip())
    if not isinstance(content, list):
        return False
    return any(
        isinstance(part, dict) and (part.get("type") == "image"
        or part.get("type") == "text" and isinstance(part.get("text"), str) and bool(part["text"].strip()))
        for part in content)


class NativeHistory:
    """保留原生父子链；回退只改变展示分支，不改写 JSONL 或删除旧分支。"""
    def __init__(self):
        self.parents, self.head = {}, None
        self.cut_before = None

    def observe(self, row):
        if row.get("type") == "last-prompt":
            # 用户要求网页编辑采用结构化回退（B 方案）：原生 rewind_conversation 成功后只追加
            # {"type":"last-prompt","explicit":true,"rewound":true,"cutUuid":目标}，不写新的消息节点。
            # 按该标记把展示分支切到目标之前，与重启后的原生 CLI 一致；终端 /rewind 不写此标记（2.1.323 实测）。
            cut = row.get("cutUuid")
            if row.get("explicit") is True and row.get("rewound") is True and cut in self.parents:
                self.cut_before = cut
            return
        identity = row.get("uuid")
        if not identity or row.get("isSidechain") or identity in self.parents:
            return
        parent = row.get("parentUuid", self.head)
        if parent and parent not in self.parents:
            # 旧存档/不完整控制记录的父节点可能缺失；没有完整回退证据时保留原有顺序，不能误删已确认消息。
            parent = self.head
        if row.get("subtype") == "compact_boundary":
            # 压缩边界是上下文变换，历史仍连续；不能把压缩误判为用户回退。
            logical = row.get("logicalParentUuid")
            parent = logical if logical in self.parents else self.head
        self.parents[identity] = parent
        if row.get("type") in ("user", "assistant", "system", "attachment"):
            self.head = identity
            if self.cut_before and real_user(row) and identity != self.cut_before:
                self.cut_before = None

    def select(self, rows):
        head = self.parents.get(self.cut_before) if self.cut_before else self.head
        if not self.parents:
            return rows
        active = set()
        while head and head not in active:
            active.add(head)
            head = self.parents.get(head)
        # 并行工具调用（2.1.323 实测）：每条结果的父节点是各自的 tool_use 行，只有最后一条在当前链上。
        # 用户要求工具状态如实显示：答复的调用都在当前分支、且父节点也在当前分支的结果行一并保留，
        # 否则其余工具会一直显示进行中或状态未知、任务清单也会丢失编号；被回退分支上的调用不在当前链上，不受影响。
        uses = {part.get("id") for row in rows if row.get("uuid") in active and row.get("type") == "assistant"
                for part in (row.get("message", {}).get("content") or []) if isinstance(part, dict)
                and part.get("type") == "tool_use"}

        def parallel_result(row):
            content = row.get("message", {}).get("content") if row.get("type") == "user" else None
            if not isinstance(content, list) or not content or self.parents.get(row.get("uuid")) not in active:
                return False
            return all(isinstance(part, dict) and part.get("type") == "tool_result"
                       and part.get("tool_use_id") in uses for part in content)

        return [row for row in rows if row.get("uuid") in active or parallel_result(row)]

    def current_head(self):
        return self.parents.get(self.cut_before) if self.cut_before else self.head


def native_turn_finished(messages):
    # 用户要求只有执行中才排队；原生完成、中断后仍可能用 blocked/needs 保存等待用户答复的说明。
    # 只认当前真实用户轮次的结束记录，不按回复文本猜测；后续新输入或工具调用使旧结束记录失效。
    for row in reversed(messages):
        if native_interruption(row) or row.get("isApiErrorMessage") is True:
            return True
        if native_turn_start(row):
            return False
        message = row.get("message")
        content = message.get("content", []) if isinstance(message, dict) else []
        if row.get("type") == "assistant" and isinstance(content, list) \
                and any(part.get("type") == "tool_use" for part in content if isinstance(part, dict)):
            return False
        if row.get("type") == "system" and row.get("subtype") == "turn_duration":
            return True
    return False


def preview_text_key(value):
    # 只用于排除终端预览重复，正式 Markdown 原文不改写；终端已去掉代码、强调、链接等显示标记。
    value = html.unescape(value)
    value = re.sub(r"!\[([^\]]*)\]\([^\n)]*\)|\[([^\]]*)\]\([^\n)]*\)",
                   lambda match: match.group(1) if match.group(1) is not None else match.group(2), value)
    value = re.sub(r"(?m)^\s{0,3}(?:#{1,6}\s+|>+\s*)", "", value)
    value = re.sub(r"[`*_~<>]", "", value)
    return "".join(value.split())


def preview_prompt_key(content):
    # 用户输入只折叠终端换行与缩进，不去掉 Markdown/标点，也不能用截取前缀或空文字认领回复。
    if isinstance(content, str):
        text = content
    elif isinstance(content, list):
        text = "\n".join(part["text"] for part in content if isinstance(part, dict)
                         and part.get("type") == "text" and isinstance(part.get("text"), str))
    else:
        return ""
    return "".join(text.split())


def native_pending_tool_uses(rows):
    # 用户要求实时预览不能把工具进度当回复正文：记录里已有 tool_use 而结果未返回时模型在等工具，不可能在流式输出文字，
    # 此时终端上的 “●” 一定是工具分组行。工具进度改由正式 tool_use 条目（inProgress）在活动区显示。
    pending = set()
    for row in rows:
        message = row.get("message")
        content = message.get("content") if isinstance(message, dict) else None
        for part in content if isinstance(content, list) else []:
            if not isinstance(part, dict):
                continue
            if part.get("type") == "tool_use" and part.get("id"):
                pending.add(part["id"])
            elif part.get("type") == "tool_result":
                pending.discard(part.get("tool_use_id"))
    return pending


class NativeSubagents:
    """只在列表/详情读取时观察原生子代理文件，不进入主会话的发送、250ms 轮询或权限处理。"""
    def __init__(self, root, native_id):
        self.projects = (root / "projects").resolve()
        self.native_id = str(uuid.UUID(native_id))
        self.logs = {}

    @staticmethod
    def valid_id(value):
        return isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9_-]{1,64}", value) is not None

    def path(self):
        path = next(self.projects.glob(f"*/{self.native_id}.jsonl"), None)
        if path is None or not path.resolve().is_relative_to(self.projects):
            raise RuntimeError("原生主会话记录暂不可读取")
        return path

    def rows(self, path, agent_id=None):
        if not path.exists():
            return []
        if not path.resolve().is_relative_to(self.projects):
            raise RuntimeError("原生子代理记录不在所属会话目录内")
        stat = path.stat()
        cached = self.logs.setdefault(str(path), {"offset": 0, "rows": [], "seen": set(),
                                                 "signature": None, "history": NativeHistory()})
        identity = (stat.st_dev, stat.st_ino)
        signature = (identity, stat.st_size, stat.st_mtime_ns)
        previous = cached["signature"]
        if previous and (previous[0] != identity or stat.st_size < cached["offset"]
                         or previous[1] == stat.st_size and previous[2] != stat.st_mtime_ns):
            cached.update(offset=0, rows=[], seen=set(), history=NativeHistory())
        if signature == previous:
            return cached["rows"] if agent_id else cached["history"].select(cached["rows"])
        with path.open("rb") as stream:
            stream.seek(cached["offset"])
            while True:
                position = stream.tell()
                line = stream.readline()
                if not line.endswith(b"\n"):
                    cached["offset"] = position
                    break
                cached["offset"] = stream.tell()
                try:
                    row = json.loads(line)
                except ValueError:
                    continue
                if isinstance(row, dict) and not agent_id:
                    cached["history"].observe(row)
                if (not isinstance(row, dict) or not row.get("uuid")
                        or row.get("type") not in ("user", "assistant", "system")
                        or row.get("sessionId", self.native_id) != self.native_id
                        or agent_id and row.get("agentId", agent_id) != agent_id
                        or not agent_id and row.get("isSidechain")
                        or row["uuid"] in cached["seen"]):
                    continue
                if row["type"] in ("user", "assistant") and not isinstance(row.get("message"), dict):
                    continue
                interruption = native_tool_interruption(row, cached["rows"])
                value = {key: row[key] for key in ("type", "uuid", "parentUuid", "timestamp", "subtype",
                         "agentId", "parentAgentId", "isMeta", "isApiErrorMessage", "isCompactSummary",
                         "isVisibleInTranscriptOnly", "apiBlockIndex", "requestedModel", "effort",
                         "interruptedMessageId", "promptId", "toolDenialKind", "sourceToolAssistantUUID") if key in row}
                if interruption:
                    value["nativeToolInterruption"] = interruption
                value["timestampMs"] = native_time(row.get("timestamp"))
                if (agent_id and row["type"] == "user"
                        and isinstance(row.get("origin"), dict) and row["origin"].get("kind") == "coordinator"):
                    # 已写入子代理原生记录的主代理消息是该子视图的真实输入；只调整只读投影，不参与主会话准入。
                    value["isMeta"] = False
                    value["nativeSubagentInput"] = True
                if "message" in row:
                    message = row["message"]
                    value["message"] = {key: message[key] for key in
                                        ("role", "id", "model", "content", "usage", "stop_reason") if key in message}
                    content = value["message"].get("content")
                    if isinstance(content, list):
                        value["message"]["content"] = [{key: item for key, item in part.items()
                            if key not in ("signature", "data")} if part.get("type") == "thinking" else part
                            for part in content if isinstance(part, dict) and part.get("type") != "redacted_thinking"]
                result = row.get("toolUseResult")
                if isinstance(result, dict) and self.valid_id(result.get("agentId") or result.get("resumedAgentId")):
                    value["toolUseResult"] = {key: result[key] for key in
                        ("agentId", "resumedAgentId", "status", "description", "resolvedModel", "totalDurationMs",
                         "totalTokens", "totalToolUseCount") if key in result}
                tool_result = native_tool_result(row)
                if tool_result:
                    # 子代理只读视图与主会话共用工具渲染，同样只带展示所需的精简元数据。
                    value["nativeToolResult"] = tool_result
                notification = native_agent_notification(row)
                if notification:
                    value["nativeAgentNotification"] = notification
                cached["seen"].add(row["uuid"])
                cached["rows"].append(value)
        cached["signature"] = signature
        return cached["rows"] if agent_id else cached["history"].select(cached["rows"])

    def collect(self, requested=None, include_messages=False):
        if requested is not None and not self.valid_id(requested):
            raise RuntimeError("原生子代理 ID 无效")
        parent = self.path()
        directory = parent.with_suffix("") / "subagents"
        if directory.exists() and directory.resolve() != parent.parent.resolve() / self.native_id / "subagents":
            raise RuntimeError("原生子代理目录不属于当前会话")
        calls, agents, notifications, resumes = {}, {}, {}, {}
        for row in self.rows(parent):
            content = row.get("message", {}).get("content", [])
            if isinstance(content, list):
                for part in content:
                    if part.get("type") == "tool_use" and part.get("name") in ("Agent", "Task"):
                        calls[part["id"]] = {"input": part.get("input", {}), "at": row["timestampMs"]}
                    if part.get("type") == "tool_result" and part.get("tool_use_id") in calls:
                        result = row.get("toolUseResult", {})
                        if self.valid_id(result.get("agentId")):
                            agents[result["agentId"]] = {**result, "toolUseId": part["tool_use_id"]}
            notification = row.get("nativeAgentNotification")
            if notification:
                notifications[notification["agentId"]] = notification
            resumed_id = row.get("toolUseResult", {}).get("resumedAgentId")
            if self.valid_id(resumed_id):
                resumes[resumed_id] = row["timestampMs"]
        # 原生日志保留被回退的分支及其文件；仅排除明确属于失效调用的子代理，不删除磁盘记录。
        all_rows = self.logs[str(parent)]["rows"]
        discarded_calls = {
            part["id"] for row in all_rows if isinstance(row.get("message", {}).get("content"), list)
            for part in row["message"]["content"]
            if isinstance(part, dict) and part.get("type") == "tool_use"
            and part.get("name") in ("Agent", "Task") and part.get("id") not in calls
        }
        discarded_agents = {
            row.get("toolUseResult", {}).get("agentId")
            for row in all_rows if isinstance(row.get("message", {}).get("content"), list)
            and any(part.get("type") == "tool_result" and part.get("tool_use_id") in discarded_calls
                    for part in row["message"]["content"] if isinstance(part, dict))
        }
        identities = set(agents)
        for path in directory.glob("agent-*"):
            match = re.fullmatch(r"agent-([A-Za-z0-9_-]{1,64})\.(?:jsonl|meta\.json)", path.name)
            if match:
                identities.add(match[1])
        if requested is not None and requested not in identities:
            raise NativeError("ENOENT", "当前主会话没有该原生子代理记录")
        result = []
        for identity in sorted(identities):
            if requested is not None and requested != identity:
                continue
            metadata_path = directory / f"agent-{identity}.meta.json"
            metadata = {}
            if metadata_path.exists():
                if not metadata_path.resolve().is_relative_to(directory.resolve()):
                    raise RuntimeError("原生子代理元数据不属于当前会话")
                try:
                    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
                except ValueError:
                    pass
                if not isinstance(metadata, dict):
                    metadata = {}
            native = agents.get(identity, {})
            tool_id = metadata.get("toolUseId") or native.get("toolUseId", "")
            if tool_id in discarded_calls or identity in discarded_agents:
                discarded_agents.add(identity)
                continue
            call = calls.get(tool_id, {})
            task = call.get("input", {})
            path = directory / f"agent-{identity}.jsonl"
            if include_messages and not path.is_file():
                # 记录暂不可读时沿用现有读取失败处理，不能用空历史撤掉网页已经确认并显示的消息。
                raise RuntimeError("原生子代理记录暂不可读取")
            if path.exists() and not path.resolve().is_relative_to(directory.resolve()):
                raise RuntimeError("原生子代理记录不属于当前会话")
            rows = self.rows(path, identity)
            if not call and metadata.get("isFork"):
                # 嵌套 fork 或父会话压缩后，启动调用仍保存在该 fork 文件中；只按原生 toolUseId 取回任务。
                for row in rows:
                    content = row.get("message", {}).get("content", [])
                    if not isinstance(content, list):
                        continue
                    block = next((part for part in content if part.get("type") == "tool_use"
                                  and part.get("id") == tool_id and part.get("name") in ("Agent", "Task")), None)
                    if block:
                        call = {"input": block.get("input", {}), "at": row["timestampMs"]}
                        task = call["input"]
                        break
            # fork 文件开头是启动调用及继承说明，不属于子代理再次执行 Agent；用原始任务替换该原生启动说明。
            if metadata.get("isFork"):
                boundary = next((index for index, row in enumerate(rows)
                    if row.get("type") == "user" and isinstance(row.get("message", {}).get("content"), list)
                    and any(part.get("type") == "tool_result" and part.get("tool_use_id") == tool_id
                            for part in row["message"]["content"])), None)
                rows = rows[boundary + 1:] if boundary is not None else []
                if task.get("prompt"):
                    rows = [{"type": "user", "uuid": f"task-{identity}", "timestampMs": call.get("at", 0),
                             "message": {"role": "user", "content": task["prompt"]}}, *rows]
            notification = notifications.get(identity, {})
            model, effort, updated, started = native.get("resolvedModel", ""), "", 0, call.get("at", 0)
            latest_prompt, latest_final = 0, 0
            for row in rows:
                updated = max(updated, row["timestampMs"])
                started = started or row["timestampMs"]
                if real_user(row):
                    latest_prompt = max(latest_prompt, row["timestampMs"])
                message = row.get("message", {})
                if row.get("type") == "assistant" and message.get("model") not in (None, "", "<synthetic>"):
                    model = message["model"]
                    if message.get("stop_reason") in ("end_turn", "stop_sequence"):
                        latest_final = max(latest_final, row["timestampMs"])
                if row.get("effort"):
                    effort = row["effort"]
            # 同 ID 续跑会移除 finished 而非写 false；新的原生输入使上次完成通知失效，不能继续归入已完成列表。
            current_input = max(latest_prompt, resumes.get(identity, 0))
            resumed = bool(notification.get("completedAtMs") and current_input > notification["completedAtMs"])
            status = notification.get("status") or native.get("status")
            if metadata.get("finished") is False or resumed and latest_final < current_input:
                status = "active"
            elif metadata.get("finished") is True:
                status = "completed" if resumed or status not in ("failed", "stopped") else status
            elif resumed:
                status = "active"
            elif status not in ("completed", "failed", "stopped"):
                status = "active" if status in ("active", "async_launched", "running") else "unknown"
            if resumed and status == "active":
                started = current_input
            outcome = status if status in ("completed", "failed", "stopped") else None
            entry = {"nativeId": identity, "toolUseId": tool_id,
                     "name": task.get("description") or metadata.get("description") or identity,
                     "agentType": metadata.get("agentType") or task.get("subagent_type", ""),
                     "model": model, "reasoningEffort": effort, "status": status, "outcome": outcome,
                     "startedAtMs": started or None, "updatedAtMs": updated or None,
                     "parentAgentId": next((row["parentAgentId"] for row in rows if row.get("parentAgentId")), None)}
            for key in ("totalDurationMs", "totalTokens", "totalToolUseCount"):
                if not resumed and isinstance(notification.get(key, native.get(key)), (int, float)):
                    entry[key] = notification.get(key, native.get(key))
            if outcome:
                entry["completedAtMs"] = (None if resumed else notification.get("completedAtMs")) or updated or None
            if include_messages:
                entry["messages"] = rows
            result.append(entry)
        while True:
            removed = {row["nativeId"] for row in result if row.get("parentAgentId") in discarded_agents}
            if removed.issubset(discarded_agents):
                break
            discarded_agents.update(removed)
        return [row for row in result if row["nativeId"] not in discarded_agents]


def native_json(path):
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
        return value if isinstance(value, dict) else {}
    except (OSError, ValueError):
        return {}


def native_process_start(pid):
    """/proc 的第 22 列是进程启动标识；先越过可能含空格的 comm，用来识别被复用的 PID。"""
    try:
        return (Path("/proc") / str(pid) / "stat").read_text().rsplit(") ", 1)[1].split()[19]
    except (OSError, IndexError):
        return None


def native_process_zombie(pid):
    """已退出但尚未被父进程回收的进程（Z）或已死亡的进程（X）：它不再持有会话，只剩一个等待回收的进程表项。"""
    try:
        return (Path("/proc") / str(pid) / "stat").read_text().rsplit(") ", 1)[1].split()[0] in ("Z", "X")
    except (OSError, IndexError):
        return False


def native_owned_by(pid, markers):
    """进程自身或最多六层父进程的命令行参数恰为某个标记（本程序的宿主/读取器脚本路径）时视为本程序所有。"""
    markers = {marker for marker in markers if isinstance(marker, str) and marker}
    if not markers:
        return False
    for _ in range(6):
        if pid <= 1:
            return False
        try:
            arguments = (Path("/proc") / str(pid) / "cmdline").read_bytes().split(b"\0")
            if any(argument.decode("utf-8", "replace") in markers for argument in arguments):
                return True
            pid = int((Path("/proc") / str(pid) / "stat").read_text().rsplit(") ", 1)[1].split()[1])
        except (OSError, IndexError, ValueError):
            return False
    return False


def native_session_holders(root, native_id, own=()):
    """仍在运行并登记为该原生会话的 CLI 进程：用户终端里的 claude、旧版 Elves 的后台任务、其他应用的 SDK 宿主等。

    用户要求别的应用持有会话时网页只读，所以这里只枚举，不抢占、不停止对方；用户在会话里明确点“关闭其他占用”时，
    才由 release_native_holders 结束这里列出的进程（返回的 started 是核对身份用的进程启动标识，job 是旧版后台任务 ID）。
    `own` 是本程序宿主和读取器脚本的 WSL 路径；属于它们（含子进程）的 CLI 是本程序自己持有的，不算在内。
    登记文件在进程退出后会被清理，残留文件靠 PID 存活和启动标识核对，防止复用 PID 造成永久误报。
    """
    holders = []
    for path in (Path(root) / "sessions").glob("*.json"):
        record = native_json(path)
        if record.get("sessionId") != native_id:
            continue
        try:
            pid = int(record.get("pid", path.stem))
        except (TypeError, ValueError):
            continue
        if pid <= 1:
            continue
        started = native_process_start(pid)
        if started is None:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                continue
            except PermissionError:
                pass
        elif record.get("procStart") is not None and str(record["procStart"]) != started:
            continue
        elif native_process_zombie(pid):
            continue
        if native_owned_by(pid, own):
            continue
        job = str(record.get("jobId") or record.get("parkedJobId") or "")
        holders.append({"pid": pid, "entrypoint": str(record.get("entrypoint") or "")[:40],
                        "kind": str(record.get("kind") or "")[:40], "started": started,
                        # 只接受原生后台任务的 8 位十六进制短 ID，登记里的其他内容不会变成 claude stop 的参数。
                        "job": job if re.fullmatch(r"[0-9a-f]{8}", job) else ""})
    return sorted(holders, key=lambda holder: holder["pid"])


# 回退前等持有者退出的最长时间，见 NativeSession.wait_for_no_holders。
HOLDER_RELEASE_WAIT = 5.0

# 用户要求会话提示“已在另一个应用中打开”旁提供“关闭其他占用”：SIGTERM 让 CLI 自己清理登记并恢复终端，
# 超时仍不退出才 SIGKILL；claude stop 通常一秒内返回，上限 20 秒，保证两个任务加上等待仍在对端 60 秒回执期限内。
RELEASE_TERM_WAIT = 5.0
RELEASE_KILL_WAIT = 3.0
RELEASE_STOP_TIMEOUT = 20


def native_holder_alive(holder):
    """持有者仍是枚举时的那个进程：启动标识一致才算，PID 被别的进程复用或进程已退出都不算。"""
    started = native_process_start(holder["pid"])
    return started is not None and started == holder["started"] and not native_process_zombie(holder["pid"])


def release_native_holders(root, native_id, own=(), cli=None):
    """结束仍持有该原生会话、且不属于本程序的 CLI 进程：用户终端里的 claude、旧版 Elves 的后台任务、其他应用的 SDK 宿主等。

    只在用户于会话里确认“关闭其他占用”后调用；本程序自己的宿主和读取器（`own`）不会被结束。
    旧版 Elves 的后台任务由 claude 守护进程监管，只结束进程会在十几秒内被重新拉起（真实 CLI 2.1.329 验证），
    所以先用原生 `claude stop <任务ID>` 让守护进程把任务标为已停止；其余持有者发信号。
    每次发信号前都核对进程启动标识，登记过期或 PID 已被复用时不会误伤别的进程。
    返回 {"released": 已结束的 PID, "remaining": 结束后仍持有会话的进程（含新出现的）, "stopFailed": 停止失败的后台任务 ID}。
    """
    holders = [holder for holder in native_session_holders(root, native_id, own) if holder["started"] is not None]

    def waiting(targets, seconds):
        end = time.monotonic() + seconds
        targets = [holder for holder in targets if native_holder_alive(holder)]
        while targets and time.monotonic() < end:
            time.sleep(0.1)
            targets = [holder for holder in targets if native_holder_alive(holder)]
        return targets

    def signaling(targets, number):
        for holder in targets:
            if not native_holder_alive(holder):
                continue
            with contextlib.suppress(ProcessLookupError, PermissionError):
                os.kill(holder["pid"], number)

    stop_failed = []
    jobs = sorted({holder["job"] for holder in holders if holder["job"]})
    for job in jobs if cli else []:
        try:
            stopped = subprocess.run([cli, "stop", job], stdin=subprocess.DEVNULL, capture_output=True,
                                     timeout=RELEASE_STOP_TIMEOUT).returncode == 0
        except (OSError, subprocess.SubprocessError):
            stopped = False
        if not stopped:
            stop_failed.append(job)
    if jobs and cli:
        # 只等后台任务的持有者退出；终端里的 claude 等其他持有者不必陪着等。
        waiting([holder for holder in holders if holder["job"]], RELEASE_TERM_WAIT)
    alive = [holder for holder in holders if native_holder_alive(holder)]
    signaling(alive, signal.SIGTERM)
    alive = waiting(alive, RELEASE_TERM_WAIT)
    signaling(alive, signal.SIGKILL)
    waiting(alive, RELEASE_KILL_WAIT)
    remaining = native_session_holders(root, native_id, own)
    present = {holder["pid"] for holder in remaining}
    return {"released": [holder["pid"] for holder in holders if holder["pid"] not in present],
            "remaining": remaining, "stopFailed": stop_failed}


class NativeDiscovery:
    """只读原生目录；发现与读取不调用 CLI、守护进程或模型，也不使用网页登记文件作为发现来源。
    只有用户确认“关闭其他占用”的 release_holders 会结束别的进程（旧版后台任务经原生 claude stop）。"""
    def __init__(self, root=None, cli=None):
        self.root = Path(root or os.environ.get("CLAUDE_CONFIG_DIR", Path.home() / ".claude")).resolve()
        # 仅“关闭其他占用”停止旧版后台任务时用到 claude stop；其余发现与读取仍不调用 CLI。
        self.cli = cli
        self.logs, self.readers, self.rows = {}, {}, {}
        self.scanned_at = None
        self.bytes_read = 0

    def scan(self, force=False):
        # 用户要求正常刷新自动发现且不高频重扫正文：目录元数据最多每两秒核对，JSONL 仅从已读取偏移继续。
        if not force and self.scanned_at is not None and time.monotonic() - self.scanned_at < 2:
            return list(self.rows.values())
        rows, alive = {}, set()
        projects = (self.root / "projects").resolve()
        for path in projects.glob("*/*.jsonl"):
            try:
                native = str(uuid.UUID(path.stem))
                if not path.resolve().is_relative_to(projects):
                    continue
                stat = path.stat()
                alive.add(path)
                cached = self.logs.get(path)
                identity = (stat.st_dev, stat.st_ino)
                if (cached is None or cached["identity"] != identity or stat.st_size < cached["offset"]
                        or stat.st_size == cached["offset"] and stat.st_mtime_ns != cached["mtime"]):
                    cached = {"identity": identity, "offset": 0, "mtime": 0, "real": False,
                              "nativeId": native, "title": "", "customTitle": "", "cwd": "",
                              "createdAt": 0, "updatedAt": 0, "model": "", "effort": ""}
                    self.logs[path] = cached
                if stat.st_size > cached["offset"]:
                    with path.open("rb") as stream:
                        stream.seek(cached["offset"])
                        while True:
                            position = stream.tell()
                            line = stream.readline()
                            if not line.endswith(b"\n"):
                                cached["offset"] = position
                                break
                            cached["offset"] = stream.tell()
                            self.bytes_read += len(line)
                            try:
                                row = json.loads(line)
                            except ValueError:
                                continue
                            if not isinstance(row, dict):
                                continue
                            if row.get("type") in ("user", "assistant") and not isinstance(row.get("message"), dict):
                                continue
                            if row.get("isSidechain") or row.get("sessionId", native) != native:
                                continue
                            if row.get("type") == "custom-title" and isinstance(row.get("customTitle"), str):
                                cached["customTitle"] = " ".join(row["customTitle"].split())[:120]
                            if real_user(row):
                                if "parentUuid" in row and row["parentUuid"] is None:
                                    cached["title"] = ""
                                cached["real"] = True
                                if not cached["title"]:
                                    content = row.get("message", {}).get("content", row.get("attachment", {}).get("prompt", ""))
                                    control = native_control_command(row)
                                    text = control["value"] if control else content if isinstance(content, str) else " ".join(part.get("text", "") for part in content if part.get("type") == "text")
                                    cached["title"] = " ".join(text.split())[:60] or "图片会话"
                                if not cached["cwd"] and isinstance(row.get("cwd"), str) and row["cwd"].startswith("/"):
                                    cached["cwd"] = row["cwd"]
                            # 内部摘要和旁支不贡献标题/时间；只有真实对话及其原生完成记录更新最近时间。
                            meaningful = real_user(row) or row.get("type") == "assistant" and row.get("isApiErrorMessage") is not True or row.get("subtype") == "turn_duration"
                            if meaningful:
                                at = native_time(row.get("timestamp"))
                                if at:
                                    cached["createdAt"] = min(cached["createdAt"] or at, at)
                                    cached["updatedAt"] = max(cached["updatedAt"], at)
                                model = row.get("message", {}).get("model") if row.get("type") == "assistant" else row.get("requestedModel")
                                if isinstance(model, str) and model and model != "<synthetic>":
                                    cached["model"] = model
                                if isinstance(row.get("effort"), str):
                                    cached["effort"] = row["effort"]
                cached["mtime"] = stat.st_mtime_ns
                if cached["real"]:
                    rows[native] = {key: cached[key] for key in ("nativeId", "cwd", "createdAt", "updatedAt", "model", "effort")}
                    # 用户要求忽略本程序 --name 生成的内部连接标签，真实原生自定义名称仍优先于首条问题。
                    custom = cached["customTitle"]
                    rows[native]["title"] = (custom if not re.fullmatch(r"elves-[0-9a-f]{10}", custom) else "") or cached["title"]
            except (OSError, ValueError, TypeError):
                continue
        self.logs = {path: data for path, data in self.logs.items() if path in alive}
        for native, row in rows.items():
            # 用户要求改用 Claude Agent SDK 对接：不再读取 daemon 后台任务；会话是否被别的进程持有由 holders 单独核对，
            # 发现列表里的会话没有 Elves 自己的执行状态，一律按空闲展示。
            row["nativeStatus"] = self.status({})
        rows = {native: row for native, row in rows.items() if row["cwd"].startswith("/")}
        self.rows, self.scanned_at = rows, time.monotonic()
        return list(rows.values())

    @staticmethod
    def status(job, messages=None):
        status = {key: job.get(key) for key in ("tempo", "state", "needs", "detail", "outcome",
                                              "endedAtMs", "sessionStatus", "sessionStatusAtMs")}
        if job.get("sessionStatus") == "busy":
            status.update(tempo="active", state="working", endedAtMs=None)
        elif job.get("sessionStatus") == "waiting":
            # 新的原生确认框优先于旧轮次 done；等待确认不能变成空闲，也不能伪装执行中触发网页排队。
            status.update(tempo="blocked", state="blocked", endedAtMs=None)
        elif not job or job.get("state") in ("done", "stopped", "failed", "ended") or job.get("outcome"):
            status["tempo"] = "idle"
        elif status.get("tempo") in ("active", "blocked") and messages is not None and native_turn_finished(messages):
            # 原生完成证据也须纠正残留 active；新的 busy 会话状态仍优先，保留目标续跑等真实执行。
            status["tempo"] = "idle"
        if status.get("tempo") == "idle":
            status["needs"] = None
        return status

    def control(self, request):
        subtype = request.get("subtype")
        if subtype == "initialize":
            return {}
        if subtype == "discover":
            return {"sessions": self.scan(force=True)}
        if subtype == "holders":
            # 只读核对别的进程是否持有某个原生会话；不 initialize、不恢复会话、不启动 CLI。
            native = str(uuid.UUID(request["native_id"]))
            own = request.get("own", [])
            if not isinstance(own, list) or len(own) > 8:
                raise RuntimeError("本程序脚本标记无效")
            return {"holders": native_session_holders(self.root, native, own)}
        if subtype == "release_holders":
            # 用户在会话里确认“关闭其他占用”后才会收到；持有者由这里重新枚举，不信任调用方给出的 PID。
            native = str(uuid.UUID(request["native_id"]))
            own = request.get("own", [])
            if not isinstance(own, list) or len(own) > 8:
                raise RuntimeError("本程序脚本标记无效")
            return release_native_holders(self.root, native, own, self.cli)
        if subtype not in ("read_history", "read_subagents", "read_subagent"):
            raise RuntimeError("只读发现连接不接受执行操作")
        native = str(uuid.UUID(request["native_id"]))
        self.scan()
        if native not in self.rows:
            raise RuntimeError("指定 WSL 内没有该原生会话的有效历史")
        reader = self.readers.get(native)
        if reader is None:
            reader = SdkReader("unused", native)
            reader.root = self.root
            self.readers[native] = reader
            # 只缓存最近四条打开的完整历史，发现索引本身只保存非敏感元数据和偏移。
            if len(self.readers) > 4:
                self.readers.pop(next(iter(self.readers)))
        if subtype in ("read_subagents", "read_subagent"):
            if reader.subagents is None:
                reader.subagents = NativeSubagents(self.root, native)
            if subtype == "read_subagents":
                return {"agents": reader.subagents.collect()}
            identity = request.get("agent_id")
            if not NativeSubagents.valid_id(identity):
                raise RuntimeError("原生子代理 ID 无效")
            return {"agent": reader.subagents.collect(identity, request.get("include_messages") is True)[0]}
        reader.clients.update(request.get("clients", {}))
        reader.pending = request.get("pending", [])
        reader.native_commands = (request.get("native_control") or {}).get("commands", [])
        previous_compaction = (request.get("native_control") or {}).get("compaction") or {}
        if previous_compaction.get("state") in ("pending", "running", "unknown"):
            reader.compaction = {**previous_compaction, "state": "unknown"}
        reader.restore_compaction_events(request.get("native_control") or {})
        reader.model = request.get("model", "")
        reader.restore_settings(request.get("native_control") or {})
        reader.read_messages()
        row = self.rows[native]
        # 发现的会话没有 Elves 自己的 CLI，也就没有待回答的审批或问答；执行状态按空闲展示。
        return {"type": "shared_snapshot", "native_id": native, "job_id": "",
                "native_status": self.status(row["nativeStatus"], reader.messages), "messages": reader.messages,
                "compaction_anchor": reader.compaction_anchor(),
                "history_head": reader.history.current_head(),
                "retired_usage": reader.retired_usage,
                "clients": reader.clients, "pending": reader.pending,
                "requests": {},
                "context_configuration": reader.context_configuration(), "updated_at": row["updatedAt"],
                "native_control": reader.control_projection()}

    def poll(self, force=False):
        pass


def native_history_updated_at(rows):
    # 用户要求服务重启只恢复原生历史，不改变最近会话顺序；更新时间必须来自原生记录，
    # 缺失时由 Rust 保留已有值，不能用服务当前时间伪造一次活动。
    latest = 0
    for row in rows:
        meaningful = (real_user(row)
                      or row.get("type") == "assistant" and row.get("isApiErrorMessage") is not True
                      or row.get("subtype") == "turn_duration")
        if not meaningful:
            continue
        at = row.get("timestampMs")
        if isinstance(at, (int, float)) and at > latest:
            latest = int(at)
    return latest


class NativeSession:
    def __init__(self, cli, native_id):
        self.cli = cli
        self.root = Path(os.environ.get("CLAUDE_CONFIG_DIR", Path.home() / ".claude")).resolve()
        digest = hashlib.sha256(str(self.root).encode()).hexdigest()[:8]
        self.native_id = native_id
        self.model = ""
        self.effort = ""
        self.messages = []
        self.all_messages = []
        # 不进入历史的记录（附件等）的 uuid → parentUuid，给中断标记穿过它们找父节点用；上限见 ROW_LINK_LIMIT。
        self.row_links = {}
        self.history = NativeHistory()
        self.retired_usage = []
        self.image_inputs = {}
        self.clients = {}
        self.pending = []
        self.signature = None
        self.log_path = None
        self.log_offset = 0
        self.log_identity = None
        self.log_mtime = None
        self.requests = {}
        self.initialized = False
        self.seen = set()
        self.incoming = None
        # 定时任务触发记录 UUID → 原生说明，见 scheduled_prompt。
        self.scheduled_fires = {}
        self.preview_user = ""
        self.preview_basis = ""
        self.preview_context_cache = None
        self.preview_prompt_cache = None
        self.delivery_started = False
        self.preview = ""
        self.preview_emitted = ""
        self.context_files = {}
        self.native_context = {}
        self.goal = None
        self.goal_revision = ""
        self.goal_notice = ""
        self.compaction = {"state": "idle"}
        self.native_commands = []
        self.native_capabilities = {}
        self.compaction_screen_active = False
        self.compaction_events = {}
        self.compaction_event_id = ""
        self.subagents = None
        self.setting_changes = {}
        self.actual_settings = {}
        self.setting_evidence_at = {}
        self.setting_catalog = []
        self.permission_mode = None
        self.permission_catalog = []
        self.session_cwd = ""
        # Claude CLI 升级后，旧会话进程可能仍在运行；记录自动迁移状态，让网页能解释延迟，
        # 并确保同一条消息只在确认新进程接管同一会话后重试一次。
        self.version_recovery = None
        self.lock_folder = Path(f"/tmp/codex-elves-claude-{os.getuid()}")
        self.edit_lock_owner = None

    def control_projection(self):
        current = self.compaction_events.get(self.compaction_event_id)
        if current and current.get("afterUuid") is None and not current.get("_anchorResolved") and self.messages:
            current["afterUuid"] = self.compaction_anchor(current.get("startedAtMs", current.get("completedAtMs")))
            current["_anchorResolved"] = True
        return {"goal": self.goal, "goalRevision": self.goal_revision, "goalNotice": self.goal_notice,
                "compaction": self.compaction, "commands": self.native_commands,
                "capabilities": {**self.native_capabilities, "permissionModes": self.permission_modes()},
                "compactionEvents": list(self.compaction_events.values()),
                "compactionEventId": self.compaction_event_id,
                "settings": self.settings_projection(),
                "versionRecovery": self.version_recovery}

    def settings_projection(self):
        return {**self.actual_settings, "changes": self.setting_changes}

    def restore_settings(self, control):
        settings = control.get("settings") or {}
        for name in ("model", "effort"):
            if isinstance(settings.get(name), str):
                self.actual_settings[name] = settings[name]
            change = (settings.get("changes") or {}).get(name)
            if isinstance(change, dict) and change.get("id"):
                self.setting_changes[name] = dict(change)
        if settings.get("permissionMode") in ("default", "manual", "acceptEdits", "plan", "auto", "dontAsk", "bypassPermissions"):
            self.actual_settings["permissionMode"] = settings["permissionMode"]
        permission = (settings.get("changes") or {}).get("permissionMode")
        if (isinstance(permission, dict) and permission.get("id")
                and permission.get("requested") == self.permission_mode):
            self.setting_changes["permissionMode"] = dict(permission)

    def permission_modes(self):
        # 可选模式来自 Rust 对端交来的实例目录（initialize 的 catalog）；bypassPermissions 必须在 CLI 启动时声明，
        # 由对端在空闲时通过重启 CLI 切换，所以这里不再按旧后台任务的启动参数收敛。
        return list(self.permission_catalog or ["default", "manual", "acceptEdits", "plan",
                                                "auto", "bypassPermissions", "dontAsk"])

    def configured_permission_mode(self):
        path = self.root / "settings.json"
        try:
            if path.exists() and path.stat().st_size > 2 * 1024 * 1024:
                raise ValueError("config too large")
            settings = json.loads(path.read_text(encoding="utf-8-sig")) if path.exists() else {}
            mode = settings.get("permissions", {}).get("defaultMode", "default")
        except (OSError, ValueError, AttributeError):
            raise NativeError("ECONFIG", "无法读取 Claude 实例的全局权限，已暂缓切换权限") from None
        if mode not in ("default", "manual", "acceptEdits", "plan", "auto", "dontAsk", "bypassPermissions"):
            raise NativeError("ECONFIG", "Claude 实例的默认权限模式无效，已暂缓切换权限")
        return mode


    def resolve_setting_label(self, name, label):
        if name == "effort":
            value = label.lower().removesuffix(" effort")
            return "" if value == "auto" else value if value in ("low", "medium", "high", "xhigh", "max") else None
        for model in self.setting_catalog:
            if label in (model.get("value"), model.get("resolvedModel"), model.get("displayName")):
                return model["value"]
        return None

    def observe_used_setting(self, name, actual, row):
        at = native_time(row.get("timestamp"))
        if at < self.setting_evidence_at.get(name, 0):
            return
        self.actual_settings[name] = actual
        self.setting_evidence_at[name] = at
        change = self.setting_changes.get(name) or {}
        if change.get("state") not in ("pending", "confirmation", "confirming") or at < change.get("issuedAt", 0):
            return
        def normalize(value):
            resolved = self.resolve_setting_label(name, value)
            return value if resolved is None else resolved
        if normalize(actual) == normalize(change["requested"]):
            state = "applied"
        elif (change.get("dialogId") and at >= change.get("dialogAtMs", 0)
              and normalize(actual) == normalize(change.get("previous", ""))):
            # CLI 的拒绝结果可能不写 JSONL；确认框之后的新请求实际使用原模型，也是保留原设置的证据。
            state = "rejected"
        else:
            return
        change.update(state=state, actual=actual, resultId=row["uuid"])
        setattr(self, name, actual)

    def observe_setting_record(self, row):
        output = row.get("nativeSettingOutput")
        at = native_time(row.get("timestamp"))
        if output:
            name, requested = output["name"], output["value"]
            if name == "effort" and requested == "auto":
                requested = ""
            text = re.sub(r"\x1b\[[0-9;]*m", "", row["message"]["content"])
            pattern = (r"<local-command-stdout>(Set model to|Kept model as|Current model:)\s+(?:`([^`]+)`|([^<\n]+))"
                       if name == "model" else
                       r"<local-command-stdout>(Set effort level to|Kept effort level as|Current effort level:)\s+(?:`([^`]+)`|([^<\n]+))")
            match = re.search(pattern, text)
            if not match:
                return
            state = "rejected" if match[1].startswith("Kept ") else "applied"
            change = self.setting_changes.get(name) or {}
            if at < max(self.setting_evidence_at.get(name, 0), change.get("issuedAt", 0)):
                return
            label = (match[2] or match[3]).strip()
            actual = requested if state == "applied" and requested else self.resolve_setting_label(name, label)
            if actual is None and state == "rejected":
                actual = change.get("previous")
            if actual is None:
                return
            matching = change.get("requested") == requested or change.get("source") == "native"
            if not matching and change.get("state") in ("pending", "confirmation", "confirming"):
                # 先前的命令回显不能覆盖后来明确选择的设置；仍更新实际值，后一个命令继续由 CLI 排队。
                change["previous"] = actual
            else:
                self.setting_changes[name] = {
                    "id": change["id"] if matching else output["commandId"], "requested": requested,
                    "previous": change.get("previous", self.actual_settings.get(name, "")),
                    "state": state, "actual": actual, "resultId": row["uuid"], "issuedAt": at,
                    "dialogId": change.get("dialogId") if matching else None}
        elif row.get("type") == "assistant" and row.get("isApiErrorMessage") is not True:
            model = row.get("requestedModel") or row.get("message", {}).get("model")
            if model and model != "<synthetic>":
                self.observe_used_setting("model", model, row)
            effort = row.get("effort")
            if isinstance(effort, str):
                self.observe_used_setting("effort", effort, row)
            return
        else:
            return
        self.actual_settings[name] = actual
        self.setting_evidence_at[name] = at
        setattr(self, name, actual)


    def restore_compaction_events(self, control):
        # 正文状态行属于已观察到的原生活动；重连和下一条命令不能删除已确认的中断、失败记录。
        for event in control.get("compactionEvents", []):
            if isinstance(event, dict) and isinstance(event.get("id"), str):
                self.compaction_events.setdefault(event["id"], dict(event))
        self.compaction_event_id = control.get("compactionEventId") or self.compaction_event_id
        event = self.compaction_events.get(self.compaction_event_id)
        if event and event.get("status") == "inProgress" and self.compaction.get("state") == "unknown":
            event["status"] = "unknown"

    def compaction_anchor(self, before=None):
        return next((row["uuid"] for row in reversed(self.messages)
                     if row.get("uuid") and (before is None or native_time(row.get("timestamp")) <= before)
                     and (real_user(row) or row.get("type") == "assistant"
                                            or row.get("subtype") == "turn_duration")), None)

    def record_compaction_event(self, identity, status, at=None, measure_start=True, **fields):
        event = self.compaction_events.setdefault(identity, {
            "id": identity, "afterUuid": self.compaction_anchor(), "source": "manual"})
        # 模型是这次操作的快照；重读旧记录或修改当前会话设置，都不能降级已确认的型号。
        command = next((c for c in self.native_commands if c.get("id") == identity and c.get("name") == "compact"), {})
        rank = {"inferred": 1, "specified": 2, "native": 3}
        for value, source in ((command.get("compactionModel"), command.get("compactionModelSource")),
                              (fields.pop("compactionModel", None), fields.pop("compactionModelSource", "native"))):
            if (isinstance(value, str) and value.strip() and len(value) <= 200 and value.strip() not in ("unknown", "<synthetic>")
                    and not any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in value)
                    and rank.get(source, 0) > (rank.get(event.get("compactionModelSource", "native"), 0)
                                                if event.get("compactionModel") else 0)):
                event.update(compactionModel=value.strip(), compactionModelSource=source)
        event.update(status=status, **fields)
        observed_at = int(time.time() * 1000) if at is None else at
        if observed_at > 0:
            event["recordedAtMs"] = observed_at
        if status == "inProgress":
            if measure_start:
                event.setdefault("startedAtMs", observed_at)
                if at is None and not getattr(self, "_compact_clock", None):
                    self._compact_clock = (identity, time.monotonic(), None)
        elif status in ("completed", "failed", "interrupted"):
            event["completedAtMs"] = observed_at
            clock = getattr(self, "_compact_clock", None)
            if clock and clock[0] == identity:
                end = clock[2] if clock[2] is not None else time.monotonic()
                elapsed = round((end-clock[1])*1000)
                if elapsed > 0 and "durationMs" not in event and event.get("outcome") != "noop":
                    event.update(durationMs=elapsed, compactionDurationSource="observed")
                self._compact_clock = None
            event.pop("phase", None)
        return event

    def observe_control_record(self, row):
        # 顺序扫描同一原生转录时保存当时的调用证据，不拿恢复时的 self.model 给老边界补值。
        if row.get("type") == "assistant" and not row.get("isSidechain"):
            msg = row.get("message") or {}
            model = msg.get("model") if isinstance(msg, dict) and not row.get("isApiErrorMessage") and not row.get("isUnmetered") else None
            self._compact_history_model = (native_time(row.get("timestamp")), model, row.get("uuid"))
        elif real_user(row) or row.get("type") == "user" and "<command-name>/model</command-name>" in str((row.get("message") or {}).get("content", ""))[:512]:
            self._compact_history_model = None
        version = row.get("version", "")
        if not self.native_capabilities and isinstance(version, str) and re.fullmatch(r"\d+\.\d+\.\d+", version):
            # 只读历史不启动 CLI；2.1.322 已实测，执行前仍用 initialize.commands 核对当前实例。
            supported = tuple(map(int, version.split("."))) >= (2, 1, 322)
            self.native_capabilities = {"goal": supported, "compact": supported}
        attachment = row.get("attachment")
        if not isinstance(attachment, dict):
            attachment = {}
        at = native_time(row.get("timestamp"))
        if row.get("type") == "attachment" and attachment.get("type") == "goal_status":
            condition, met = attachment.get("condition"), attachment.get("met")
            if not isinstance(condition, str) or not condition.strip() or type(met) is not bool:
                return
            self.goal_revision = row["uuid"]
            if attachment.get("sentinel") is True:
                if not met:
                    self.goal_notice = ""
                    self.goal = {"objective": condition, "status": "active", "createdAt": at // 1000,
                                 "updatedAt": at // 1000, "nativeId": row["uuid"], "source": "claude"}
                elif not self.goal or self.goal.get("status") != "completed":
                    # 原生 clear 也写 met=true 的 sentinel；它仅表示结束，不能冒充目标已达成。
                    self.goal = None
                    self.goal_notice = ""
            elif self.goal and self.goal["objective"] == condition:
                self.goal["status"] = "completed" if met else "active"
                if met:
                    self.goal_notice = ""
                self.goal["updatedAt"] = at // 1000
                if isinstance(attachment.get("reason"), str):
                    self.goal["reason"] = attachment["reason"][:4000]
                # 只映射原生显式统计；不估算 Token、轮次或等待中的耗时，新目标从空统计开始。
                for source, target in (("tokens", "tokensUsed"), ("iterations", "iterations"), ("durationMs", "timeUsedSeconds")):
                    value = attachment.get(source)
                    if type(value) is int and 0 <= value <= 9007199254740991:
                        self.goal[target] = value / 1000 if source == "durationMs" else value
            for command in self.native_commands:
                if (command["name"] == "goal" and command["state"] in ("pending", "unknown")
                        and at >= command["issuedAt"] and row["uuid"] != command.get("baseline")
                        and (command["argument"] == condition and not met
                             or command["argument"] == "clear" and met and attachment.get("sentinel") is True)):
                    command["state"], command["nativeId"] = "completed", row["uuid"]
        elif row.get("type") == "system" and row.get("subtype") == "informational":
            text = row.get("content")
            if isinstance(text, str) and text.startswith(("Goal paused", "Goal still active", "Goal cleared after an unrecoverable error")):
                # 只投影原生目标通知，不返回 hook stderr、认证信息或请求诊断。
                self.goal_notice = text[:2000]
                if self.goal and text.startswith("Goal paused"):
                    self.goal["status"] = "blocked"
                elif self.goal and text.startswith("Goal still active"):
                    self.goal["status"] = "active"
        elif row.get("type") == "system" and row.get("subtype") == "compact_boundary":
            metadata = row.get("compactMetadata") or {}
            if not isinstance(metadata, dict):
                metadata = {}
            automatic = metadata.get("trigger") in ("auto", "automatic")
            active = next((command for command in self.native_commands
                           if command["name"] == "compact" and command["state"] in ("pending", "running", "stopping", "unknown")), None)
            current = self.compaction_events.get(self.compaction_event_id)
            # 自动边界即使在手动请求等待期间到达，也不是该命令的完成回执。
            stale = active and (automatic or at < active["issuedAt"] or row["uuid"] == active.get("baseline"))
            if not active and current and current.get("status") in ("inProgress", "unknown"):
                stale = at < current.get("startedAtMs", 0)
            known = next((event for event in self.compaction_events.values() if event.get("nativeId") == row["uuid"]), None)
            identity = known["id"] if known else active["id"] if active and not stale \
                else current["id"] if current and not stale and current.get("status") in ("inProgress", "unknown") else row["uuid"]
            fields = {key: metadata[key] for key in ("preTokens", "postTokens", "durationMs")
                      if type(metadata.get(key)) is int and 0 <= metadata[key] <= 9007199254740991}
            # 原生优先。无型号的历史，仅用同一父链已发生的助手请求作推定，不看恢复时的会话模型。
            model = next((metadata[key] for key in ("compactionModel", "model")
                          if isinstance(metadata.get(key), str) and metadata[key].strip()
                          and len(metadata[key]) <= 200
                          and not any(ord(char) < 32 or 127 <= ord(char) <= 159 for char in metadata[key])), None)
            if model is not None:
                fields["compactionModel"] = model.strip()
                fields["compactionModelSource"] = "native"
            elif ((evidence := getattr(self, "_compact_history_model", None)) and 0 < evidence[0] <= at
                  and evidence[2] and evidence[2] in (row.get("parentUuid"), row.get("logicalParentUuid"))):
                fields.update(compactionModel=evidence[1], compactionModelSource="inferred")
            self._compact_history_model = None
            if "durationMs" in fields:
                fields["compactionDurationSource"] = "native"
            if "durationMs" in fields and at >= fields["durationMs"]:
                fields["startedAtMs"] = at - fields["durationMs"]
            self.record_compaction_event(identity, "completed", at, nativeId=row["uuid"],
                                         source="automatic" if metadata.get("trigger") in ("auto", "automatic") else "manual",
                                         **fields)
            if stale:
                return
            self.compaction_event_id = identity
            self.compaction = {"state": "idle", "itemId": row["uuid"]}
            for command in self.native_commands:
                if (command["name"] == "compact" and command["state"] in ("pending", "running", "stopping", "unknown")
                        and at >= command["issuedAt"] and row["uuid"] != command.get("baseline")):
                    command["state"], command["nativeId"] = "completed", row["uuid"]


    def context_configuration(self):
        # 用户要求容量与压缩设置分开：这里只传配置来源，不能把任何配置值当作原生容量。
        key = "CLAUDE_CODE_MAX_CONTEXT_TOKENS"
        keys = (key, "CLAUDE_CODE_AUTO_COMPACT_WINDOW", "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE")
        values = {name: (os.environ.get(name), "environment") for name in keys}
        path = self.root / "settings.json"
        try:
            stat = path.stat()
            signature = (stat.st_mtime_ns, stat.st_size)
            cached = self.context_files.get(path)
            if cached is None or cached[0] != signature:
                data = json.loads(path.read_text(encoding="utf-8"))
                env = data.get("env", {}) if isinstance(data, dict) else {}
                self.context_files[path] = (signature, {name: env[name] for name in keys if name in env} if isinstance(env, dict) else {})
            configured = self.context_files[path][1]
            values.update({name: (raw, "user-settings.env") for name, raw in configured.items()})
        except (OSError, ValueError):
            pass
        # 配置、原生窗口和最后实际请求绑定同一模型，不能把其他模型或会话的窗口带过来。
        model = next((row["message"].get("model", "") for row in reversed(self.messages)
                      if row.get("type") == "assistant" and row.get("isUnmetered") is not True
                      and row.get("isApiErrorMessage") is not True and row.get("message", {}).get("model") != "<synthetic>"
                      and isinstance(row.get("message", {}).get("usage", {}).get("input_tokens"), int)), self.model)
        result = {"model": model}
        for name, field in ((key, "maxContextTokens"),
                            ("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "windowTokens"),
                            ("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", "triggerPercent")):
            raw, scope = values[name]
            if not isinstance(raw, str) or not re.fullmatch(r"\s*\+?\d+\s*", raw):
                continue
            count = int(raw)
            if not 0 < count <= (100 if field == "triggerPercent" else 9007199254740991):
                continue
            source = {"kind": "configuration", "key": name, "scope": scope, "model": model}
            if name == key:
                result.update(maxContextTokens=count, source=source)
            else:
                result.setdefault("compaction", {})[field] = count
                result["compaction"][field + "Source"] = source
        if self.native_context.get("model") == model:
            result["native"] = self.native_context
        return result if len(result) > 1 else {}

    def observe_native_context(self, row):
        # 只接收同一原生日志/会话明确报告的 statusline 字段；不调用模型或复刻 CLI 的窗口计算。
        if row.get("session_id", row.get("sessionId", self.native_id)) != self.native_id:
            return
        context = row.get("context_window")
        message = row.get("message")
        if not isinstance(context, dict) and isinstance(message, dict):
            context = message.get("context_window")
        if not isinstance(context, dict):
            return
        model = row.get("model")
        if isinstance(model, dict):
            model = model.get("id")
        if not isinstance(model, str) and isinstance(message, dict):
            model = message.get("model")
        size = context.get("context_window_size")
        if (not isinstance(model, str) or not model or type(size) is not int
                or not 0 < size <= 9007199254740991):
            return
        report = {"model": model, "context_window_size": size}
        percentage = context.get("used_percentage")
        if type(percentage) in (int, float) and 0 <= percentage <= 100:
            report["used_percentage"] = percentage
        usage = context.get("current_usage")
        if isinstance(usage, dict):
            report["current_usage"] = {key: usage[key] for key in (
                "input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens")
                if type(usage.get(key)) is int and 0 <= usage[key] <= 9007199254740991}
        self.native_context = report


    def prepare_text(self, content, client_id):
        text = "\n".join(part["text"] for part in content if part.get("type") == "text")
        images = [part for part in content if part.get("type") == "image"]
        if images:
            folder = self.root / "remote-attachments" / client_id
            folder.mkdir(parents=True, exist_ok=True, mode=0o700)
            for index, image in enumerate(images):
                source = image["source"]
                extension = {"image/png": ".png", "image/jpeg": ".jpg",
                             "image/gif": ".gif", "image/webp": ".webp"}[source["media_type"]]
                path = folder / f"{index}{extension}"
                path.write_bytes(base64.b64decode(source["data"], validate=True))
                path.chmod(0o600)
                text += f"\n@{path}"
        return text


    @contextlib.contextmanager
    def edit_lock(self):
        # 用户要求只禁止同时编辑历史：同一 WSL 用户下连接同一原生会话的多个连接端（如正式服务与验收服务）
        # 串行执行编辑，第二个编辑直接拒绝；别的应用已打开该会话时由 session_holders 在回退前拒绝，不改写历史。
        owner = threading.get_ident()
        if self.edit_lock_owner == owner:
            yield
            return
        folder = self.lock_folder
        folder.mkdir(mode=0o700, exist_ok=True)
        if folder.is_symlink() or folder.stat().st_uid != os.getuid():
            raise NativeError("ESESSION", "编辑锁目录归属异常，未修改历史")
        handle = os.open(folder / f"edit-{uuid.UUID(self.native_id)}.lock",
                         os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        try:
            try:
                fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise NativeError("EBUSY", "该会话正在由另一个网页编辑历史或恢复环境，请稍后重试") from None
            # 历史编辑内的恢复共用同一把锁，允许同一线程嵌套，禁止其他连接端同时迁移。
            self.edit_lock_owner = owner
            yield
        finally:
            if self.edit_lock_owner == owner:
                self.edit_lock_owner = None
            os.close(handle)

    def session_holders(self):
        # 两个进程不能同时写同一份原生记录：交接前后确认没有任何 CLI 进程（后台或终端）打开该会话。
        return [holder["pid"] for holder in native_session_holders(self.root, self.native_id)]

    def wait_for_no_holders(self, seconds=HOLDER_RELEASE_WAIT):
        """等仍持有该会话的 CLI 退出，最多 seconds 秒；返回此时还在持有的进程（空表示可以继续）。

        对端已确认自己的 CLI 退出后才会回退，这里只兜底 CLI 刚退出、登记文件和进程表项还没清理完的短暂间隙；
        别的应用（终端里的 claude 等）持有时不会在几秒内消失，等满时限后照常拒绝，不改写历史。"""
        end = time.monotonic() + seconds
        holders = self.session_holders()
        while holders and time.monotonic() < end:
            time.sleep(0.1)
            holders = self.session_holders()
        return holders


    def scheduled_prompt(self, row):
        fire = self.scheduled_fires.get(row.get("parentUuid")) if row.get("type") == "user" else None
        content = row.get("message", {}).get("content") if fire is not None else None
        text = content if isinstance(content, str) else "\n".join(
            part.get("text", "") for part in content if isinstance(part, dict) and part.get("type") == "text") \
            if isinstance(content, list) else ""
        if fire is None or not text.strip():
            return None
        return {"taskId": "scheduled", "kind": "schedule", "summary": text.strip()[:400], "event": fire}


    def run_rewind(self, target, last_seen, cwd):
        # 用户要求（B 方案）用结构化接口回退，不再模拟按键操作 /rewind 菜单。原生 print 进程按 stream-json
        # 控制协议先 initialize，再发送 rewind_conversation；last_seen 为网页当前分支的最后一条 user 记录，
        # 原生发现网页尚未显示的新轮次会拒绝。该进程不发送用户消息、不调用模型；--strict-mcp-config 与
        # disableAllHooks 只作用于这个短进程，避免为一次回退启动 MCP 或触发用户 Hook。
        # 成功返回原生答复；明确拒绝抛 NativeError（记录未修改）；发出回退请求后失去答复抛 RuntimeError（结果未知）。
        process = subprocess.Popen(
            [self.cli, "-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
             "--strict-mcp-config", "--settings", '{"disableAllHooks":true}', "--resume", self.native_id],
            cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        lines = queue.Queue()

        def drain():
            for line in process.stdout:
                lines.put(line)
            lines.put(None)

        drain_thread = threading.Thread(target=drain, daemon=True)
        drain_thread.start()

        def request(subtype, timeout, **fields):
            identity = uuid.uuid4().hex
            process.stdin.write(json.dumps({"type": "control_request", "request_id": identity,
                                            "request": {"subtype": subtype, **fields}}).encode() + b"\n")
            process.stdin.flush()
            deadline = time.monotonic() + timeout
            while True:
                line = lines.get(timeout=max(0.01, deadline - time.monotonic()))
                if line is None:
                    raise EOFError("原生回退进程已退出")
                try:
                    frame = json.loads(line)
                except ValueError:
                    continue
                response = frame.get("response") if isinstance(frame, dict) and frame.get("type") == "control_response" else None
                if isinstance(response, dict) and response.get("request_id") == identity:
                    return response
                if time.monotonic() >= deadline:
                    raise TimeoutError("原生回退进程未答复")

        lost = (OSError, ValueError, EOFError, TimeoutError, queue.Empty)
        try:
            try:
                ready = request("initialize", 60)
            except lost:
                raise NativeError("ESESSION", "无法启动原生回退进程，未修改历史") from None
            if ready.get("subtype") != "success":
                raise NativeError("ESESSION", "原生回退进程初始化失败，未修改历史")
            # 从发出回退请求起记录可能已被修改；此后失去答复只能按未知处理，不能当作未修改。
            self.delivery_started = True
            try:
                response = request("rewind_conversation", 30, target_message_uuid=target,
                                   last_seen_user_message_uuid=last_seen)
            except lost:
                raise RuntimeError("原生回退结果未知，修改后的消息未发送，请刷新核对历史") from None
        finally:
            with contextlib.suppress(OSError):
                process.stdin.close()
            try:
                process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            drain_thread.join(timeout=5)
            with contextlib.suppress(OSError):
                process.stdout.close()
        value = response.get("response") if response.get("subtype") == "success" else None
        if isinstance(value, dict) and value.get("rewound") is True:
            return value
        self.delivery_started = False
        reason = value.get("reason") if isinstance(value, dict) else None
        if not isinstance(reason, str) or not reason:
            raise NativeError("EUNSUPPORTED", "当前原版 Claude 未接受结构化回退，未修改历史")
        code, message = REWIND_REFUSALS.get(reason, ("EUNSUPPORTED", f"原版 Claude 拒绝回退（{reason}），未修改历史"))
        raise NativeError(code, message)


    def edit_content(self, target, prompt):
        # 编辑文字保留原图；图片从原生消息/已读取附件还原，继续经现有上传入口进入同一 CLI。
        content, total = [{"type": "text", "text": prompt}], 0
        originals = target.get("web_images", [])
        if originals:
            # 网页原图清单是完整来源；不再叠加 CLI 转录的同一批图片，用户主动重复添加的原图仍保留。
            blocks = [{"type": "image", "url": image["url"]} for image in originals]
        else:
            blocks = list(target.get("message", {}).get("content", [])) \
                if isinstance(target.get("message", {}).get("content"), list) else []
            start = self.messages.index(target) + 1
            for row in self.messages[start:]:
                if row.get("type") != "attachment":
                    break
                blocks.extend(row.get("message", {}).get("content", []))
        for block in blocks:
            if not isinstance(block, dict) or block.get("type") != "image":
                continue
            image = block.get("source", {})
            url = block.get("url", "")
            match = re.fullmatch(r"data:(image/(?:png|jpeg|gif|webp));base64,([A-Za-z0-9+/=\r\n]+)", url)
            if match:
                image = {"type": "base64", "media_type": match[1], "data": match[2]}
            if (not isinstance(image, dict) or image.get("type") != "base64"
                    or image.get("media_type") not in ("image/png", "image/jpeg", "image/gif", "image/webp")):
                raise NativeError("EIMAGE", "原消息图片暂不可恢复，未回退历史")
            try:
                size = len(base64.b64decode(image.get("data"), validate=True))
            except (ValueError, TypeError):
                raise NativeError("EIMAGE", "原消息图片内容暂不可恢复，未回退历史") from None
            total += size
            # 与 remote_access.rs 的 8 张、单图 5 MiB、合计 12 MiB 同步；附件无法完整保留时不回退。
            if len(content) > 8 or size > 5 * 1024 * 1024 or total > 12 * 1024 * 1024:
                raise NativeError("EIMAGE", "原消息图片超过远程输入限制，请在原版 CLI 编辑；未回退历史")
            content.append({"type": "image", "source": image})
        return content


    def retain_web_images(self, row, intent=None):
        # 用户要求已受理网页图片在历史/重连仍完整：优先保留原输入，其次读取该 client 专属原图，不以原生附件转录数量为准。
        client = self.clients.get(row["uuid"], "")
        content = row.get("message", {}).get("content", [])
        text = content if isinstance(content, str) else "\n".join(
            part.get("text", "") for part in content if part.get("type") == "text")
        uploads = self.root / "remote-attachments"
        folder = uploads / client if re.fullmatch(r"[A-Za-z0-9_-]{1,120}", client) else None
        paths = []
        for line in text.splitlines():
            # 用户要求空网页登记也恢复原图：校验规则见 remote_upload_path，仅认可本用户上传目录内的直接文件。
            path = remote_upload_path(line, uploads)
            if path is None:
                continue
            if folder is None:
                folder = path.parent
            if path.parent == folder and path not in paths:
                paths.append(path)
        row["remote_upload_paths"] = [str(path) for path in paths]
        saved = {image["filename"]: image["url"] for image in self.image_inputs.get(row["uuid"], [])}
        originals = [part for part in (intent or {}).get("content", []) if part.get("type") == "image"]
        images, total = [], 0
        for index, path in enumerate(paths):
            url = saved.get(str(path))
            if index < len(originals):
                source = originals[index]["source"]
                url = f"data:{source['media_type']};base64,{source['data']}"
            if url is None:
                url = remote_upload_url(path)
                if url is None:
                    continue
            size = len(base64.b64decode(url.split(",", 1)[1], validate=True))
            # 仅张数改为 8；沿用单张 5 MiB、合计 12 MiB，历史恢复不能另设更宽松的大小限制。
            if not size or size > 5 * 1024 * 1024 or total + size > 12 * 1024 * 1024:
                continue
            total += size
            if not any(image["filename"] == str(path) for image in images):
                images.append({"filename": str(path), "url": url})
        self.image_inputs[row["uuid"]] = row["web_images"] = images

    def read_messages(self):
        if not self.log_path and self.native_id:
            paths = list((self.root / "projects").glob(f"*/{self.native_id}.jsonl"))
            if paths:
                self.log_path = paths[0]
        if not self.log_path or not self.log_path.exists():
            return
        previous_offset = self.log_offset
        stat = self.log_path.stat()
        identity = (stat.st_dev, stat.st_ino)
        # 用户要求重连补齐真实历史；原生日志替换或重写后不能沿用旧偏移，同一大小的新文件也须重新投影。
        if (self.log_identity is not None and identity != self.log_identity
                or stat.st_size < self.log_offset
                or self.log_mtime is not None and stat.st_size == self.log_offset
                and stat.st_mtime_ns != self.log_mtime):
            self.messages, self.all_messages, self.log_offset, self.seen = [], [], 0, set()
            self.row_links = {}
            self._compact_history_model = None
            self.history = NativeHistory()
            self.scheduled_fires = {}
            self.retired_usage = []
            previous_offset = -1
            self.preview_prompt_cache = None
            self.native_context = {}
            self.goal, self.goal_revision = None, ""
            self.goal_notice = ""
            active_compact = next((command for command in self.native_commands
                                   if command["name"] == "compact" and command["state"] in ("pending", "running", "stopping", "unknown")), None)
            self.compaction = {"state": active_compact["state"] if active_compact["state"] in ("pending", "unknown")
                               else "running" if self.compaction_screen_active else "unknown",
                               "itemId": active_compact["id"]} if active_compact else {"state": "idle"}
            self.signature = None
        self.log_identity, self.log_mtime = identity, stat.st_mtime_ns
        with self.log_path.open("rb") as stream:
            stream.seek(self.log_offset)
            while True:
                position = stream.tell()
                line = stream.readline()
                if not line.endswith(b"\n"):
                    self.log_offset = position
                    break
                self.log_offset = stream.tell()
                try:
                    row = json.loads(line)
                except ValueError:
                    self._compact_history_model = None
                    continue
                if not isinstance(row, dict):
                    self._compact_history_model = None
                    continue
                self.history.observe(row)
                if (not self.session_cwd and not row.get("isSidechain") and isinstance(row.get("cwd"), str)
                        and row["cwd"].startswith("/")):
                    # 原生按会话启动目录保存记录；交接回退的 print 进程须在同一目录 --resume 才能找到原记录。
                    self.session_cwd = row["cwd"]
                if not row.get("isSidechain"):
                    self.observe_native_context(row)
                if not row.get("uuid") or row.get("isSidechain"):
                    continue
                if row.get("type") in ("user", "assistant") and not isinstance(row.get("message"), dict):
                    continue
                if row["uuid"] in self.seen:
                    continue
                self.row_links[row["uuid"]] = row.get("parentUuid")
                if len(self.row_links) > ROW_LINK_LIMIT:
                    self.row_links.pop(next(iter(self.row_links)))
                self.observe_control_record(row)
                if row.get("type") == "user" and row.get("permissionMode") in (
                        "default", "manual", "acceptEdits", "plan", "auto", "dontAsk", "bypassPermissions"):
                    self.actual_settings["permissionMode"] = row["permissionMode"]
                content = row.get("content") if row.get("type") == "system" else row["message"].get("content") \
                    if isinstance(row.get("message"), dict) else None
                if isinstance(content, str) and ("<command-name>" in content or "<local-command-" in content):
                    # 本地命令与输出须先标注再判断可编辑；父节点只在最近几条中查找，避免长历史逐行全量扫描。
                    parent = next((message for message in reversed(self.all_messages[-8:])
                                   if message.get("uuid") == row.get("parentUuid")), None)
                    local_command = native_local_command(row, parent)
                    if local_command:
                        row["nativeLocalCommand"] = local_command
                    local_output = native_local_output(row, parent)
                    if local_output:
                        row["nativeLocalOutput"] = local_output
                system_notice = native_system_notice(row)
                if system_notice:
                    row["nativeSystemNotice"] = system_notice
                if row.get("type") == "system" and row.get("subtype") == "scheduled_task_fire":
                    # 定时任务触发（2.1.323 实测）：system/scheduled_task_fire 后紧跟一条以它为父节点、
                    # 没有 origin 标记的 user 提示。用户要求按“定时任务触发”的自动续跑展示，不当作用户消息、不可编辑。
                    self.scheduled_fires[row["uuid"]] = str(row.get("content") or "").strip()[:200]
                notification = native_agent_notification(row)
                if notification:
                    row["nativeAgentNotification"] = notification
                # 须在判断可编辑之前标注，real_user 才能排除通知与定时任务提示。
                task_notification = native_task_notification(row) or self.scheduled_prompt(row)
                if task_notification:
                    row["nativeTaskNotification"] = task_notification
                interruption = native_tool_interruption(row, self.all_messages, self.row_links)
                if interruption:
                    row["nativeToolInterruption"] = interruption
                sdk_interruption = native_sdk_interruption(row, self.all_messages, self.row_links)
                if sdk_interruption:
                    row["nativeSdkInterruption"] = sdk_interruption
                native_editable = row.get("type") == "user" and real_user(row)
                if row.get("type") == "attachment":
                    attachment = row.get("attachment", {})
                    absorbed = queued_human_prompt(attachment)
                    if attachment.get("type") == "goal_status":
                        row = {**row, "attachment": {key: attachment[key] for key in
                               ("type", "condition", "met", "sentinel", "reason") if key in attachment}}
                    elif absorbed is not None:
                        # 原版执行中吸收用户排队/引导消息时记为附件；它已实际接收，必须关联原消息并清掉排队残留。
                        # 行自己的 uuid 是 CLI 新生成的，网页发送帧的 UUID 只在 source_uuid 里，随行保留供读取器对账；
                        # 位置就是文件里的吸收点（工具结果之后、下一段回复之前），不能挪到轮次末尾。
                        row = {**row, "type": "user", "message": {"role": "user", "content": absorbed},
                               "attachment": {key: attachment[key] for key in
                                              ("type", "source_uuid", "delivery_id") if key in attachment}}
                    else:
                        image = attachment.get("content", {})
                        if attachment.get("type") != "file" or image.get("type") != "image":
                            continue
                        source = image.get("file", {})
                        # 保留网页原有图片气泡；只投影原版已读取的图片，不传环境、工具目录等内部附件。
                        row = {"type": "attachment", "uuid": row["uuid"],
                               "timestamp": row.get("timestamp"),
                               "filename": attachment.get("filename", ""),
                               "remote_upload": attachment.get("filename", "").startswith(
                                   str(self.root / "remote-attachments") + "/"),
                               "message": {"content": [{"type": "image", "url":
                                   f"data:{source['type']};base64,{source['base64']}"}]}}
                if row.get("type") not in ("user", "assistant", "system", "attachment"):
                    continue
                if row.get("type") == "system" and row.get("subtype") not in ("turn_duration", "compact_boundary") \
                        and not (row.get("nativeLocalCommand") or row.get("nativeLocalOutput")
                                 or row.get("nativeSystemNotice")):
                    continue
                control = native_control_command(row)
                if control:
                    row["nativeControlCommand"] = control
                elif row.get("type") == "user" and isinstance(row.get("message", {}).get("content"), str):
                    output = re.fullmatch(r"\s*<local-command-stdout>([\s\S]*)</local-command-stdout>\s*",
                                          row["message"]["content"])
                    parent = next((message for message in reversed(self.messages)
                                   if message.get("uuid") == row.get("parentUuid")
                                   and message.get("nativeControlCommand")), None)
                    if output and parent:
                        row["nativeControlOutput"] = parent["nativeControlCommand"]
                setting = native_setting_command(row)
                if setting:
                    row["nativeSettingCommand"] = setting
                elif (native_setting_source(row)
                      and isinstance(row.get("message", {}).get("content"), str)
                      and re.fullmatch(r"\s*<local-command-stdout>[\s\S]*</local-command-stdout>\s*",
                                       row["message"]["content"])):
                    command = next((message for message in reversed(self.messages)
                                    if message.get("uuid") == row.get("parentUuid")
                                    and message.get("nativeSettingCommand")), None)
                    if command:
                        # 必须能关联到同一原生命令；孤立回显或用户引用不能按文本前缀误删。
                        row["nativeSettingOutput"] = {"commandId": command["uuid"],
                                                      **command["nativeSettingCommand"]}
                self.observe_setting_record(row)
                skill = native_skill_command(row)
                if skill:
                    row["nativeSkillCommand"] = skill
                self.seen.add(row["uuid"])
                # 原生回退菜单定位真实 user 节点；吸收在工具轮次中的 queued_command 仍保留正文，不伪造独立回退点。
                row["nativeEditable"] = native_editable and not row.get("nativeControlCommand")
                row["nativeOffset"] = position
                self.all_messages.append({key: row[key] for key in
                                      ("type", "uuid", "parentUuid", "interruptedMessageId", "nativeToolInterruption",
                                       "nativeSdkInterruption", "toolDenialKind", "sourceToolAssistantUUID", "timestamp", "message", "subtype", "isMeta",
                                       "isCompactSummary", "isVisibleInTranscriptOnly",
                                       "isApiErrorMessage", "isUnmetered", "apiBlockIndex", "durationMs",
                                       "requestedModel", "effort", "filename", "remote_upload",
                                       "nativeSettingCommand", "nativeSettingOutput", "nativeControlCommand",
                                       "nativeControlOutput", "attachment", "compactMetadata", "nativeEditable", "nativeOffset",
                                       "nativeSkillCommand", "promptId", "turnCompanion",
                                       "nativeLocalCommand", "nativeLocalOutput", "nativeSystemNotice")
                                      if key in row})
                self.messages = self.all_messages
                if notification:
                    self.messages[-1]["nativeAgentNotification"] = notification
                if task_notification:
                    self.messages[-1]["nativeTaskNotification"] = task_notification
                result = row.get("toolUseResult")
                if isinstance(result, dict) and NativeSubagents.valid_id(result.get("agentId") or result.get("resumedAgentId")):
                    # 原生 Agent 的受理回执足以关联卡片；不为更新主会话卡片扫描子代理文件或复制工具输出。
                    self.messages[-1]["nativeAgentResult"] = {key: result[key] for key in
                        ("agentId", "status", "resolvedModel", "totalDurationMs", "totalTokens",
                         "totalToolUseCount") if key in result}
                    if result.get("resumedAgentId"):
                        self.messages[-1]["nativeAgentResult"].update(
                            agentId=result["resumedAgentId"], status="async_launched", resumed=True)
                tool_result = native_tool_result(row, self.root)
                if tool_result:
                    self.messages[-1]["nativeToolResult"] = tool_result
                if isinstance(row.get("message"), dict):
                    message = {key: row["message"][key] for key in
                               # 用户要求显示最后实际请求用量；保留原生 usage（含缓存和 iterations），不读取 job 累计 tokens。
                               ("role", "id", "model", "content", "usage") if key in row["message"]}
                    if isinstance(message.get("content"), list):
                        # 展示不需要加密签名和请求诊断；原生文件保留原文，避免把不可见数据反复传到网页。
                        message["content"] = [{key: value for key, value in part.items()
                                               if key not in ("signature", "data")}
                                              if part.get("type") == "thinking" else part
                                              for part in message["content"]
                                              if part.get("type") != "redacted_thinking"]
                    self.messages[-1]["message"] = message
                if row.get("timestamp"):
                    self.messages[-1]["timestampMs"] = native_time(row["timestamp"])
        self.messages = self.history.select(self.all_messages)
        # 只在原生记录变化时核对 Skill 展开，不给每次 250ms 状态轮询增加历史扫描。
        if self.log_offset != previous_offset and normalize_skill_records(self.messages, self.pending):
            self.preview_context_cache = self.preview_prompt_cache = None
        if self.log_offset != previous_offset:
            # 只在原生日志新增/重写时更新失效分支的数字账本；正文、图片不重复传输，不增加发送前磁盘工作。
            active = {row["uuid"] for row in self.messages}
            self.retired_usage = [
                {"type": "assistant", "uuid": row["uuid"], "timestampMs": row.get("timestampMs"),
                 "isApiErrorMessage": row.get("isApiErrorMessage"),
                 "message": {key: row["message"][key] for key in ("id", "model", "usage") if key in row["message"]}}
                for row in self.all_messages if row["uuid"] not in active
                and row.get("type") == "assistant" and row.get("message", {}).get("usage")
            ]
        # 原生 prompt 的 UUID 是原程序生成的；按已受理顺序关联网页 client ID，重连仍保留关联。
        mapped = set(self.clients)
        for index, row in enumerate(self.messages):
            # 用户要求内部摘要与中断控制记录不充当用户消息，也不能确认待发送消息或成为预览锚点。
            if not real_user(row):
                continue
            if row["uuid"] in mapped:
                if "web_images" not in row:
                    self.retain_web_images(row)
                continue
            content = row.get("message", {}).get("content", [])
            if isinstance(content, str):
                text = content
            else:
                text = "\n".join(part.get("text", "") for part in content
                                 if part.get("type") == "text")
            # 回退后当前分支的序号会变小；新增意图用原生追加偏移核对，不能因旧序号漏认新消息或认领旧同文消息。
            match = next((p for p in self.pending
                          if after_delivery_intent(row, index, p) and (text == p["_nativeText"]
                          or row.get("nativeSkillCommand", {}).get("accepted")
                          and text == skill_input_key(p["_nativeText"]))), None)
            if match:
                self.clients[row["uuid"]] = match["clientUserMessageId"]
                self.pending.remove(match)
                mapped.add(row["uuid"])
                self.retain_web_images(row, match)
            elif "web_images" not in row:
                self.retain_web_images(row)

    def preview_context(self):
        # 绑定当前原生用户和已落盘正文；工具状态、用量及 pending 变化不更换预览身份。
        # 按日志身份、修改时间及偏移缓存，避免每个终端片段重复扫描长历史。
        key = (self.native_id, self.log_identity, self.log_mtime, self.log_offset, len(self.messages))
        if self.preview_context_cache and self.preview_context_cache[0] == key:
            return self.preview_context_cache[1]
        start = next((index for index in range(len(self.messages) - 1, -1, -1)
                      if native_turn_start(self.messages[index])), None)
        result = None
        # 自动续跑轮次在终端不显示原始通知文本，无法定位回复区域；不预览，避免把新回复挂到上一轮。
        if start is not None and not real_user(self.messages[start]):
            start = None
        if start is not None:
            user, records, known = self.messages[start], [], []
            prompt = preview_prompt_key(user["message"]["content"])
            prompt_identity = (self.native_id, self.log_identity, user["uuid"], prompt)
            if self.preview_prompt_cache is None or self.preview_prompt_cache[0] != prompt_identity:
                # 历史序次只在切换用户输入或重读日志后计算一次，不在每个流式片段重复扫描长历史。
                occurrences = sum(real_user(row) and preview_prompt_key(row.get("message", {}).get("content")) == prompt
                                  for row in self.messages[:start + 1]) if prompt else 0
                self.preview_prompt_cache = (prompt_identity, occurrences)
            for row in self.messages[start + 1:]:
                message = row.get("message")
                if row.get("type") != "assistant" or not isinstance(message, dict):
                    continue
                content = message.get("content", [])
                if not isinstance(content, list):
                    continue
                texts = [part["text"] for part in content
                         if isinstance(part, dict) and part.get("type") == "text"
                         and isinstance(part.get("text"), str) and part["text"]]
                # 与正式投影一致：明确的原生合成占位不是正文，不能让它退役仍有效的预览。
                if message.get("model") == "<synthetic>" and row.get("isApiErrorMessage") is False \
                        and len(content) == 1 and len(texts) == 1 and texts[0].strip() == "No response requested.":
                    continue
                if texts:
                    records.append([row.get("uuid"), message.get("id"), row.get("apiBlockIndex"), texts])
                    known.extend(preview_text_key(text) for text in texts)
            basis = hashlib.sha256(json.dumps([user["uuid"], records], ensure_ascii=False,
                                               separators=(",", ":")).encode("utf-8")).hexdigest()
            result = (user, basis, known, prompt, self.preview_prompt_cache[1],
                      bool(native_pending_tool_uses(self.messages[start + 1:])))
        self.preview_context_cache = (key, result)
        return result


class SdkReader(NativeSession):
    """Claude Agent SDK（stream-json）传输的历史读取器。

    只做两件事：把原生 JSONL 规范化成网页历史，以及维护 /goal、/compact 这类需要原生记录佐证的状态。
    不连接原生后台、不读取终端画面、不控制 CLI；执行状态、审批、问答和消息回执由 Rust 对端的
    stream-json 连接提供，对端会用实时状态覆盖本读取器快照里的 native_status / requests。
    """

    # 用户要求缩短操作等待：每份快照都带全部历史（实测 19 MB 的会话序列化约 0.1 秒，传输、解析和投影合计约 1 秒），
    # 活跃回合里每 0.25 秒一份会把对端和服务占满。按上次发出一份所花时间的 8 倍放宽最短间隔（上限 5 秒）；
    # 耗时不到 0.02 秒（约 1 MB 以内）的会话不限制，行为不变。间隔内的变化不丢：签名不更新，
    # 间隔结束后的下一次轮询发出最新状态。force（回执、命令结果等需要立即可见的场合）不受限。
    SNAPSHOT_FREE_COST = 0.02
    SNAPSHOT_COST_FACTOR = 8
    SNAPSHOT_MAX_INTERVAL = 5.0

    def __init__(self, cli, native_id):
        super().__init__(cli, native_id)
        # 对端报告“压缩结束”后等待原生 compact_boundary 记录的起点；超时仍无终态证据则标记待核对。
        self.compaction_ended_at = None
        self.next_snapshot_at = 0.0


    def read_messages(self):
        super().read_messages()
        waiting = [intent for intent in self.pending if intent.get("_nativeUuid")]
        if not waiting:
            return
        # 对端以原生用户消息 UUID 发送；记录出现同一 UUID 就是精确确认，不依赖正文逐字相同
        # （Skill 展开、命令改写后正文可能不同）。关联方式与按正文关联一致，只是身份更强。
        rows = {row.get("uuid"): row for row in self.all_messages if row.get("uuid")}
        # 执行中被吸收的引导消息没有以发送帧 UUID 为身份的原生行：CLI 另生成行 uuid，原帧 UUID 只在 source_uuid 里。
        # 不补这一步，正文被 Skill 展开或命令改写后无法按文本认出，待确认记录会一直残留。
        for row in self.all_messages:
            attachment = row.get("attachment")
            source = attachment.get("source_uuid") if isinstance(attachment, dict) else None
            if source and row.get("type") == "user" and row.get("uuid"):
                rows.setdefault(source, row)
        for intent in waiting:
            row = rows.get(intent["_nativeUuid"])
            if row is None:
                continue
            self.clients[row["uuid"]] = intent["clientUserMessageId"]
            self.pending.remove(intent)
            self.retain_web_images(row, intent)

    def expire_compaction(self):
        if self.compaction_ended_at is None or self.compaction_screen_active:
            return
        event = self.compaction_events.get(self.compaction_event_id)
        if not event or event.get("status") != "inProgress":
            self.compaction_ended_at = None
            return
        if time.monotonic() - self.compaction_ended_at < 5:
            return
        # 压缩已结束但原生记录里没有终态证据：保留同一行待核对，不无限显示“压缩中”或猜测成功。
        self.compaction_ended_at = None
        event["status"] = "unknown"
        self.compaction["state"] = "unknown"
        for command in self.native_commands:
            if command["name"] == "compact" and command["state"] == "running":
                command["state"] = "unknown"

    def poll(self, force=False, skip_unchanged=False):
        if not self.initialized or not self.native_id:
            return
        self.read_messages()
        self.expire_compaction()
        status = NativeDiscovery.status({}, self.messages)
        # /compact 是本地原生命令；对端报告压缩进行中时补充执行状态。
        if self.compaction.get("state") == "running":
            status.update(tempo="active", state="compacting")
        status["sessionAlive"] = False
        self.requests = {}
        context = self.context_configuration()
        preview = self.preview_context()
        updated_at = native_history_updated_at(self.all_messages or self.messages)
        signature = (self.log_identity, self.log_mtime, self.log_offset, json.dumps(status, sort_keys=True),
                     json.dumps(self.pending, sort_keys=True), json.dumps(context, sort_keys=True),
                     updated_at, json.dumps(self.control_projection(), sort_keys=True))
        if skip_unchanged and signature == self.signature:
            # 用户要求缩短发送前的等待：发送前同步只要求读到最新原生记录，没有变化时上一份快照就是最新的。
            # 重发整份历史对大会话是一次完整的序列化、传输和解析（实测 8290 条消息、21.6 MiB 的会话仅读取器侧
            # 强制快照就要 0.17–0.18 秒，其中序列化约 0.13 秒，上层还要解析和投影），白白拖慢每次发送；有变化时仍走下面的强制发出。
            return
        if force or signature != self.signature:
            if not force and time.monotonic() < self.next_snapshot_at:
                return
            self.signature = signature
            started = time.monotonic()
            emit({"type": "shared_snapshot", "native_id": self.native_id, "job_id": "",
                  "native_status": status, "messages": self.messages,
                  "compaction_anchor": self.compaction_anchor(),
                  "history_head": self.history.current_head(),
                  "retired_usage": self.retired_usage,
                  "clients": self.clients, "pending": self.pending, "requests": {},
                  "preview_basis": preview[1] if preview else None,
                  "updated_at": updated_at or None,
                  "context_configuration": context, "native_control": self.control_projection()})
            cost = time.monotonic() - started
            self.next_snapshot_at = 0.0 if cost < self.SNAPSHOT_FREE_COST else \
                time.monotonic() + min(cost * self.SNAPSHOT_COST_FACTOR, self.SNAPSHOT_MAX_INTERVAL)

    def sdk_prepare(self, request):
        """登记一条即将写入 CLI 的网页消息；图片落盘为 @路径，与原有上传目录和权限保持一致。"""
        client_id, content, native_uuid = request["client_id"], request["content"], request["uuid"]
        if not isinstance(content, list) or str(uuid.UUID(native_uuid)) != native_uuid:
            raise NativeError("EINVAL", "Claude 消息格式无效")
        if any(part.get("type") == "image" for part in content) and not re.fullmatch(r"[A-Za-z0-9_-]{1,120}", client_id):
            raise NativeError("EINVAL", "Claude 消息标识无效，图片未保存")
        self.read_messages()
        text = self.prepare_text(content, client_id)
        intent = {"id": client_id, "clientUserMessageId": client_id,
                  "text": "\n".join(p.get("text", "") for p in content if p.get("type") == "text"),
                  "content": content, "_nativeText": text, "_nativeUuid": native_uuid,
                  "_after": len(self.messages), "_afterOffset": self.log_offset,
                  "deliveryConfirmed": False}
        self.pending.append(intent)
        # 先报告派发意图，断线后可按原生记录核对，不能盲目重发。
        emit({"type": "shared_delivery_intent", "intent": intent})
        return {"text": text}

    def sdk_delivered(self, request):
        for intent in self.pending:
            if intent.get("_nativeUuid") == request.get("uuid"):
                intent["deliveryConfirmed"] = True
        self.poll(force=True)
        return {}

    def sdk_abandon(self, request):
        self.pending = [intent for intent in self.pending if intent.get("_nativeUuid") != request.get("uuid")]
        self.poll(force=True)
        return {}

    def sdk_command(self, request):
        """登记 /goal、/compact 命令；对端随后把同一段斜杠文本写入 CLI。"""
        name, argument = request.get("name"), (request.get("argument") or "").strip()
        if name not in ("goal", "compact") or not self.native_capabilities.get(name):
            raise NativeError("EUNSUPPORTED", "当前 Claude CLI 未提供此原生命令")
        if name == "goal" and (not argument or len(argument.encode("utf-16-le")) // 2 > 4000):
            raise NativeError("EINVAL", "Claude 目标需要 1–4000 个字符")
        if name == "compact" and not self.native_id:
            raise NativeError("EINVAL", "请先创建会话并产生对话，再压缩上下文")
        self.read_messages()
        expected = request.get("expectedGoal")
        if expected is not None and (not self.goal or any(
                self.goal.get(key) != expected.get(key) for key in ("nativeId", "objective", "status"))):
            raise NativeError("EBUSY", "原生目标已变化，请重新读取后操作")
        if any(command["name"] == name and command["state"] in ("pending", "running", "stopping", "unknown")
               for command in self.native_commands):
            raise NativeError("EBUSY", "上一条同类原生命令尚未确认，请先核对状态")
        command = {"id": request["id"], "name": name, "argument": argument,
                   "state": "pending", "issuedAt": int(time.time() * 1000),
                   "baseline": self.goal_revision if name == "goal" else self.compaction.get("itemId"),
                   "beforeOutput": ""}
        # 只保留每类最近一次操作；已接收操作重连后仅核对，绝不重新发送。
        self.native_commands = [value for value in self.native_commands if value["name"] != name] + [command]
        if name == "compact":
            command.update(compactionModel=request.get("compactionModel"), compactionModelSource=request.get("compactionModelSource"))
            self.compaction_event_id = ""
            self.compaction = {"state": "pending", "itemId": command["id"]}
        emit({"type": "shared_native_command_intent", "native_control": self.control_projection()})
        return {"nativeCommand": {key: command[key] for key in ("id", "name", "state")},
                "nativeControl": self.control_projection()}

    def sdk_command_result(self, request):
        command = next((value for value in self.native_commands if value["id"] == request.get("id")), None)
        if command is None:
            return {}
        if request.get("state") == "failed":
            command["state"] = "failed"
            if command["name"] == "compact":
                self.compaction = {"state": "idle", "itemId": command["id"]}
        elif request.get("state") == "unknown":
            command["state"] = "unknown"
            if command["name"] == "compact":
                self.compaction["state"] = "unknown"
                event = self.compaction_events.get(self.compaction_event_id)
                if event and event.get("status") == "inProgress":
                    event["status"] = "unknown"
        self.poll(force=True)
        return {}

    def sdk_compacting(self, active):
        if active and not self.compaction_screen_active:
            self.compaction = {**self.compaction, "state": "running"}
            command = next((value for value in self.native_commands if value["name"] == "compact"
                            and value["state"] in ("pending", "running", "unknown")), None)
            previous = self.compaction_events.get(self.compaction_event_id)
            identity = command["id"] if command else previous["id"] if previous and previous.get("status") == "unknown" else str(uuid.uuid4())
            self.compaction_event_id = identity
            resumed = identity in self.compaction_events or command is not None and command["state"] in ("unknown", "running")
            if not previous or previous.get("id") != identity:
                self._compact_clock = None
            event = self.record_compaction_event(identity, "inProgress", source="manual" if command else "automatic",
                                         measure_start=not resumed,
                                         compactionModel=None if resumed else self.actual_settings.get("model") or self.model,
                                         compactionModelSource="inferred")
            for value in self.native_commands:
                if value["name"] == "compact" and value["state"] in ("pending", "unknown"):
                    value["state"] = "running"
            # 手机收到原生开始就能显示；只发同次条目，不等 poll 读盘或序列化整份历史。
            self.compaction["itemId"] = identity
            emit({"type": "shared_compaction_started", "native_id": self.native_id, "event": event})
        if self.compaction_screen_active and not active:
            event = self.compaction_events.get(self.compaction_event_id)
            ended = time.monotonic()
            clock = getattr(self, "_compact_clock", None)
            if clock and event and clock[0] == event["id"] and clock[2] is None:
                # 原生 SDK 已停止压缩即停表，稍后转录确认时不把轮询等待记入同次耗时。
                self._compact_clock = (clock[0], clock[1], ended)
            if event and event.get("phase") == "stopping":
                self.record_compaction_event(event["id"], "interrupted")
                self.compaction = {"state": "idle", "itemId": event["id"]}
            for value in self.native_commands:
                if value["name"] == "compact" and value["state"] == "stopping":
                    value.update(state="completed", message="压缩已停止")
                    self.record_compaction_event(value["id"], "interrupted")
                    self.compaction = {"state": "idle", "itemId": value["id"]}
            self.compaction_ended_at = ended
        self.compaction_screen_active = active

    def sdk_compact_failed(self, message):
        text = str(message or "")
        for command in self.native_commands:
            if command["name"] != "compact" or command["state"] not in ("pending", "running"):
                continue
            if re.search(r"Nothing to compact|Not enough messages|Conversation too short", text, re.I):
                command.update(state="completed", message="当前历史无需压缩")
                self.record_compaction_event(command["id"], "completed", outcome="noop")
            else:
                command.update(state="failed", message="原生压缩未完成，请在 CLI 中核对错误")
                self.record_compaction_event(command["id"], "failed")
            self.compaction_event_id = command["id"]
            self.compaction = {"state": "idle", "itemId": command["id"]}
            self.compaction_ended_at = None

    def sdk_signal(self, request):
        if "compacting" in request:
            self.sdk_compacting(bool(request["compacting"]))
        if request.get("compact_failed") is not None:
            self.sdk_compact_failed(request["compact_failed"])
        if request.get("stopping") and self.compaction.get("state") == "running":
            event = self.compaction_events.get(self.compaction_event_id)
            if event and event.get("status") == "inProgress":
                event["phase"] = "stopping"
            for command in self.native_commands:
                if command["name"] == "compact" and command["state"] == "running":
                    command["state"] = "stopping"
        self.poll(force=True)
        return {}

    def sdk_edit_prepare(self, request):
        """编辑历史消息的预检：只读取记录并校验目标、分支、文字和原图，不修改任何内容。"""
        self.delivery_started = False
        self.read_messages()
        target = next((row for row in self.messages if row.get("uuid") == request.get("native_user_id")
                       and row.get("nativeEditable") is True), None)
        if target is None or request.get("history_head") != self.history.current_head():
            raise NativeError("ESTALE", "原生历史已变化，请重新打开要编辑的消息")
        prompt = request.get("prompt")
        if not isinstance(prompt, str) or not prompt.strip() or len(prompt.encode()) > 128 * 1024:
            raise NativeError("EINVAL", "修改后的消息不能为空或超过 128 KiB")
        content = self.edit_content(target, prompt)
        if self.compaction.get("state") == "running":
            raise NativeError("EBUSY", "请等待压缩结束后再编辑历史消息")
        last_seen = next((row["uuid"] for row in reversed(self.messages)
                          if row.get("type") == "user" and not row.get("isMeta")), None)
        cwd = self.session_cwd or ""
        if not isinstance(cwd, str) or not cwd.startswith("/") or not Path(cwd).is_dir():
            raise NativeError("ESESSION", "找不到原生会话的项目目录，未修改历史")
        return {"content": content, "target": target["uuid"], "last_seen": last_seen, "cwd": cwd}

    def sdk_rewind(self, request):
        """回退到目标消息之前。调用前对端必须已让自己的 CLI 退出，使回退进程独占这份记录。

        明确拒绝抛 NativeError（记录未修改）；已发出回退请求却失去答复抛 RuntimeError（结果未知）。
        """
        target, last_seen, cwd = request["target"], request["last_seen"], request["cwd"]
        with self.edit_lock():
            self.read_messages()
            if request.get("history_head") != self.history.current_head():
                raise NativeError("ESTALE", "原生历史已变化，未提交编辑")
            if self.wait_for_no_holders():
                raise NativeError("ESESSIONOPEN", "原生会话仍在其他 CLI 中打开，未修改历史")
            self.run_rewind(target, last_seen, cwd)
            # 回退已被原生确认；修改后的消息尚未发送，后续失败按“未发送”报告。
            self.delivery_started = False
            self.read_messages()
            if any(row.get("uuid") == target for row in self.messages):
                self.history.cut_before = target
                self.messages = self.history.select(self.all_messages)
            self.preview = self.preview_emitted = self.preview_user = self.preview_basis = ""
            self.preview_context_cache = self.preview_prompt_cache = None
        self.poll(force=True)
        return {"rewound": True}

    def sdk_configured_mode(self, request):
        # “沿用实例配置”的权限：按实例 settings.json 的 defaultMode 解析成具体模式名，由对端交给 CLI。
        return {"mode": self.configured_permission_mode()}

    def initialize(self, request):
        self.model = request.get("model", "")
        self.effort = request.get("effort", "")
        self.permission_mode = request.get("permission_mode")
        self.permission_catalog = request["catalog"].get("permissionModes", [])
        self.setting_catalog = request["catalog"].get("models", [])
        self.restore_settings(request.get("native_control") or {})
        self.clients = request.get("clients", {})
        self.pending = request.get("pending", [])
        self.native_commands = (request.get("native_control") or {}).get("commands", [])
        previous_compaction = (request.get("native_control") or {}).get("compaction") or {}
        if previous_compaction.get("state") in ("pending", "running", "unknown"):
            self.compaction = {**previous_compaction, "state": "unknown"}
        self.restore_compaction_events(request.get("native_control") or {})
        commands = {command.get("name") for command in request["catalog"].get("commands", [])}
        self.native_capabilities = {name: name in commands for name in ("goal", "compact")}
        self.initialized = True
        return {**request["catalog"], "contextConfiguration": self.context_configuration()}

    def control(self, request):
        subtype = request.get("subtype")
        if subtype == "initialize":
            return self.initialize(request)
        if subtype == "shared_sync":
            self.poll(force=True, skip_unchanged=True)
            return {}
        handler = {"sdk_prepare": self.sdk_prepare, "sdk_configured_mode": self.sdk_configured_mode,
                   "sdk_edit_prepare": self.sdk_edit_prepare, "sdk_rewind": self.sdk_rewind, "sdk_delivered": self.sdk_delivered,
                   "sdk_abandon": self.sdk_abandon, "sdk_command": self.sdk_command,
                   "sdk_command_result": self.sdk_command_result, "sdk_signal": self.sdk_signal}.get(subtype)
        if handler is None:
            raise NativeError("EUNSUPPORTED", "此原生 Claude 控制操作尚未适配")
        return handler(request)


def main():
    arguments = argparse.ArgumentParser()
    arguments.add_argument("--cli", required=True)
    arguments.add_argument("--native-id", default="")
    arguments.add_argument("--discover", action="store_true")
    options = arguments.parse_args()
    # 用户要求改用 Claude Agent SDK 对接：这里只有两种只读角色——发现（列出原生会话、读历史、核对持有者）
    # 和单个会话的历史读取器；不连接原生后台、不读取终端画面、不控制 CLI（只有用户确认“关闭其他占用”才会结束别的进程）。
    # 执行、审批、问答由 Rust 对端的 stream-json 连接负责。
    native = NativeDiscovery(cli=options.cli) if options.discover else SdkReader(options.cli, options.native_id)
    incoming = queue.Queue()
    native.incoming = incoming

    def read_input():
        for line in sys.stdin:
            incoming.put(line)
        incoming.put(None)

    threading.Thread(target=read_input, daemon=True).start()
    next_poll = 0
    while True:
        if time.monotonic() >= next_poll:
            try:
                native.poll()
            except (OSError, ValueError, RuntimeError):
                pass
            next_poll = time.monotonic() + 0.25
        try:
            line = incoming.get(timeout=0.25)
        except queue.Empty:
            continue
        if line is None:
            # 对端关闭连接：读取器随之退出，没有需要保留的后台会话。
            return
        frame = json.loads(line)
        request_id = frame["request_id"]
        try:
            value = native.control(frame["request"])
            response = {"subtype": "success", "request_id": request_id, "response": value}
        except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as failure:
            # 用户要求接收确认不能倒退为未发送；只有发送函数明确保留“尚未准入”标记才可判为未投递，
            # 已开始或已接收后的错误只能待核对。
            not_sent = not native.delivery_started if frame["request"].get("subtype") in ("shared_send", "shared_native_command", "shared_edit_message") \
                else isinstance(failure, NativeError)
            response = {"subtype": "error", "request_id": request_id, "error": str(failure),
                        "not_sent": not_sent}
            if isinstance(failure, NativeError):
                response["code"] = failure.code
        emit({"type": "control_response", "response": response})


if __name__ == "__main__":
    main()
