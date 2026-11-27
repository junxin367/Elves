//! SDK 对端的纯状态：把 CLI 的 stream-json 输出折算成快照字段，不做任何 I/O。
//!
//! 用户要求 Claude 实例改为基于 Claude Agent SDK 的结构化协议对接，不再解析终端画面：
//! 执行状态、审批、问答、计划确认和消息回执都只来自 CLI 的结构化帧，这里不读取画面文字。
//! 帧的形状以 `@anthropic-ai/claude-agent-sdk` 的类型定义为准（`system/session_state_changed`、
//! `command_lifecycle`、`can_use_tool` 等），并已用 Claude Code 2.1.330 实测核对。

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::background::BackgroundTasks;

/// 会话执行状态；与旧共享对端快照里的 `native_status.tempo` 取值相同，上层队列与视图不需改动。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Tempo {
    Idle,
    Active,
    Blocked,
}

impl Tempo {
    fn as_str(self) -> &'static str {
        match self {
            Tempo::Idle => "idle",
            Tempo::Active => "active",
            Tempo::Blocked => "blocked",
        }
    }
}

/// 网页消息写入 CLI 后的回执阶段，对应 `command_lifecycle` 的 queued → started → completed。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum Phase {
    Written,
    Queued,
    Started,
    Completed,
}

/// 已写入 CLI 的网页消息只需要回执阶段；消息正文、网页身份与“待确认”展示由读取器按原生记录维护。
#[derive(Clone, Debug)]
pub(super) struct Sent {
    pub phase: Phase,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum RequestKind {
    Tool,
    Question,
    Plan,
}

#[derive(Clone, Debug)]
pub(super) struct Request {
    pub kind: RequestKind,
    pub input: Value,
    pub tool_use_id: String,
    /// 给上层 `permission_request` 的原始请求（工具审批、问答，或计划确认的 native_dialog）。
    pub raw: Value,
    /// 计划确认的选项（序号 → 批准后要切换到的权限模式）。
    pub plan_modes: BTreeMap<u64, Option<String>>,
}

/// 状态折算后需要对端额外处理的事项；纯状态机本身不执行它们。
#[derive(Default, Debug, PartialEq)]
pub(super) struct Observed {
    /// 快照需要重新拼装发出。
    pub dirty: bool,
    /// 需要告知 Python 读取器的实时信号（压缩进行中、命令失败等）。
    pub signals: Vec<Signal>,
    /// 正文预览有新内容。
    pub preview: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Signal {
    /// `system/status: compacting` 开始或结束。
    Compacting(bool),
    /// 压缩命令被 CLI 明确报告失败（`compact_result: failed`）。
    CompactFailed(String),
}

#[derive(Default, Clone, Debug)]
pub(super) struct Actual {
    pub model: String,
    pub permission_mode: String,
    pub effort: String,
}

pub(super) struct PeerState {
    /// 当前 CLI 声明支持的权限模式（来自实例检测），用于生成计划确认的批准选项。
    modes: Vec<String>,
    alive: bool,
    pub version: String,
    pub capabilities: BTreeSet<String>,
    session_state_seen: bool,
    tempo: Tempo,
    compacting: bool,
    since_ms: u64,
    pub actual: Actual,
    sent: BTreeMap<String, Sent>,
    requests: BTreeMap<String, Request>,
    tasks: BackgroundTasks,
    native_task_work: BTreeSet<String>,
    turn_users: Vec<String>,
    /// 当前流式消息的各内容块文字；只用于临时预览，正式正文以 JSONL 为准。
    streaming: BTreeMap<u64, String>,
    message_id: String,
    preview_text: String,
    preview_user: String,
}

impl PeerState {
    pub(super) fn new(modes: Vec<String>) -> Self {
        Self {
            modes,
            alive: false,
            version: String::new(),
            capabilities: BTreeSet::new(),
            session_state_seen: false,
            tempo: Tempo::Idle,
            compacting: false,
            since_ms: 0,
            actual: Actual::default(),
            sent: BTreeMap::new(),
            requests: BTreeMap::new(),
            tasks: BackgroundTasks::default(),
            native_task_work: BTreeSet::new(),
            turn_users: Vec::new(),
            streaming: BTreeMap::new(),
            message_id: String::new(),
            preview_text: String::new(),
            preview_user: String::new(),
        }
    }

    pub(super) fn alive(&self) -> bool {
        self.alive
    }

    #[cfg(test)]
    pub(super) fn has_requests(&self) -> bool {
        !self.requests.is_empty()
    }

    pub(super) fn compacting(&self) -> bool {
        self.compacting
    }

    pub(super) fn busy(&self) -> bool {
        self.tempo != Tempo::Idle || self.compacting || !self.requests.is_empty()
    }

    pub(super) fn has_background_tasks(&self) -> bool {
        // 本次只调整展示；前台任务不进入菜单，但原有原生任务保活保护不能随之取消。
        !self.native_task_work.is_empty() || self.tasks.has_tasks()
    }

    pub(super) fn set_alive(&mut self, alive: bool, now: u64) {
        if self.alive == alive {
            return;
        }
        self.alive = alive;
        if !alive {
            // 进程已退出：它持有的审批、后台任务和正在执行的轮次都随之结束。
            self.tempo = Tempo::Idle;
            self.compacting = false;
            self.requests.clear();
            self.tasks.clear();
            self.native_task_work.clear();
            self.turn_users.clear();
            self.streaming.clear();
            self.preview_text.clear();
            self.session_state_seen = false;
        }
        self.since_ms = now;
    }

    // ---------------------------------------------------------------- 消息回执

    pub(super) fn record_sent(&mut self, uuid: &str) {
        self.sent.insert(
            uuid.to_owned(),
            Sent {
                phase: Phase::Written,
            },
        );
    }

    pub(super) fn forget_sent(&mut self, uuid: &str) {
        self.sent.remove(uuid);
    }

    pub(super) fn sent_phase(&self, uuid: &str) -> Option<Phase> {
        self.sent.get(uuid).map(|sent| sent.phase)
    }

    // ---------------------------------------------------------------- CLI 帧

    pub(super) fn observe_cli(&mut self, frame: &Value, now: u64) -> Observed {
        let mut seen = Observed::default();
        match frame["type"].as_str() {
            Some("system") => self.observe_system(frame, now, &mut seen),
            Some("command_lifecycle") => self.observe_lifecycle(frame, now, &mut seen),
            Some("control_request") => self.observe_request(frame, now, &mut seen),
            Some("control_cancel_request") => {
                if let Some(id) = frame["request_id"].as_str()
                    && self.requests.remove(id).is_some()
                {
                    self.settle_after_request(&mut seen);
                }
            }
            Some("stream_event") => self.observe_stream(frame, &mut seen),
            Some("assistant") => {
                // 完整的助手消息会落进 JSONL，由读取器的快照接替；这里只让流式缓冲不再重复展示。
                if let Some(id) = frame["message"]["id"].as_str()
                    && id == self.message_id
                {
                    self.streaming.clear();
                }
            }
            Some("result") => {
                self.streaming.clear();
                if !self.session_state_seen && !self.has_active_lifecycle() {
                    self.set_tempo(Tempo::Idle, now, &mut seen);
                }
                seen.dirty = true;
            }
            _ => {}
        }
        seen
    }

    fn observe_system(&mut self, frame: &Value, now: u64, seen: &mut Observed) {
        match frame["subtype"].as_str() {
            Some("init") => {
                if let Some(version) = frame["claude_code_version"].as_str() {
                    self.version = version.to_owned();
                }
                self.capabilities = frame["capabilities"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                if let Some(model) = frame["model"].as_str() {
                    self.actual.model = model.to_owned();
                }
                if let Some(mode) = frame["permissionMode"].as_str() {
                    self.actual.permission_mode = mode.to_owned();
                }
                seen.dirty = true;
            }
            Some("session_state_changed") => {
                self.session_state_seen = true;
                let tempo = match frame["state"].as_str() {
                    Some("running") => Tempo::Active,
                    Some("requires_action") => Tempo::Blocked,
                    _ => Tempo::Idle,
                };
                if tempo == Tempo::Idle {
                    self.turn_users.clear();
                    self.streaming.clear();
                }
                self.set_tempo(tempo, now, seen);
            }
            Some("status") => {
                if let Some(mode) = frame["permissionMode"].as_str() {
                    self.actual.permission_mode = mode.to_owned();
                    seen.dirty = true;
                }
                let compacting = frame["status"] == "compacting";
                if compacting != self.compacting {
                    self.compacting = compacting;
                    seen.signals.push(Signal::Compacting(compacting));
                    seen.dirty = true;
                }
                if frame["compact_result"] == "failed" {
                    let message = frame["compact_error"].as_str().unwrap_or("原生压缩未完成");
                    seen.signals.push(Signal::CompactFailed(message.to_owned()));
                    seen.dirty = true;
                }
                if frame["status"] == "requesting"
                    && let Some(users) = frame["user_message_uuids"].as_array()
                {
                    self.turn_users = users
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                    self.preview_user = self.turn_users.first().cloned().unwrap_or_default();
                    self.preview_text.clear();
                    if !self.session_state_seen {
                        self.set_tempo(Tempo::Active, now, seen);
                    }
                }
            }
            Some("compact_boundary") => {
                // 压缩完成以 JSONL 里的 compact_boundary 记录为证据，由读取器按原有规则投影；这里只结束进行中状态。
                if self.compacting {
                    self.compacting = false;
                    seen.signals.push(Signal::Compacting(false));
                }
                seen.dirty = true;
            }
            Some("background_tasks_changed" | "task_started" | "task_progress" | "task_notification") => {
                let work_changed = match frame["subtype"].as_str() {
                    Some("background_tasks_changed") => {
                        let next = frame["tasks"].as_array().into_iter().flatten()
                            .filter(|task| task["ambient"] != true)
                            .filter_map(|task| task["task_id"].as_str().map(str::to_owned)).collect();
                        let changed = self.native_task_work != next;
                        self.native_task_work = next;
                        changed
                    }
                    Some("task_started") => frame["task_id"].as_str()
                        .is_some_and(|id| self.native_task_work.insert(id.to_owned())),
                    Some("task_notification") => frame["task_id"].as_str()
                        .is_some_and(|id| self.native_task_work.remove(id)),
                    _ => false,
                };
                seen.dirty |= self.tasks.observe(frame, now) || work_changed;
            }
            _ => {}
        }
    }

    fn observe_lifecycle(&mut self, frame: &Value, now: u64, seen: &mut Observed) {
        let Some(uuid) = frame["command_uuid"].as_str() else {
            return;
        };
        let phase = match frame["state"].as_str() {
            Some("queued") => Phase::Queued,
            Some("started") => Phase::Started,
            // queued、started 之外的状态都是终态：被打断的命令以 cancelled 结束（CLI 2.1.329 实测），
            // 只认 completed 会让它永远停在“进行中”。
            Some(_) => Phase::Completed,
            None => return,
        };
        if let Some(sent) = self.sent.get_mut(uuid)
            && phase > sent.phase
        {
            sent.phase = phase;
            seen.dirty = true;
        }
        if !self.session_state_seen {
            // 旧版 CLI 不发 session_state_changed 时，用消息开始与全部结束推断执行状态。
            if phase == Phase::Started {
                self.set_tempo(Tempo::Active, now, seen);
            } else if phase == Phase::Completed && !self.has_active_lifecycle() {
                self.set_tempo(Tempo::Idle, now, seen);
            }
        }
    }

    fn has_active_lifecycle(&self) -> bool {
        self.sent
            .values()
            .any(|sent| matches!(sent.phase, Phase::Queued | Phase::Started))
    }

    fn observe_request(&mut self, frame: &Value, now: u64, seen: &mut Observed) {
        let (Some(id), request) = (frame["request_id"].as_str(), &frame["request"]) else {
            return;
        };
        if request["subtype"] != "can_use_tool" {
            return;
        }
        let tool = request["tool_name"].as_str().unwrap_or("").to_owned();
        let input = request["input"].clone();
        let tool_use_id = request["tool_use_id"].as_str().unwrap_or("").to_owned();
        let (kind, raw, plan_modes) = match tool.as_str() {
            "AskUserQuestion" => (
                RequestKind::Question,
                json!({"subtype":"can_use_tool","tool_name":tool,"input":input,"tool_use_id":tool_use_id}),
                BTreeMap::new(),
            ),
            "ExitPlanMode" => {
                let (dialog, modes) = self.plan_dialog(id, &input, now);
                (
                    RequestKind::Plan,
                    json!({"subtype":"native_dialog","dialog":dialog}),
                    modes,
                )
            }
            _ => {
                let reason = ["decision_reason", "title", "description"]
                    .iter()
                    .find_map(|key| {
                        request[*key]
                            .as_str()
                            .filter(|text| !text.trim().is_empty())
                    })
                    .map_or(Value::Null, |text| json!(text));
                (
                    RequestKind::Tool,
                    json!({"subtype":"can_use_tool","tool_name":tool,"input":input,
                        "tool_use_id":tool_use_id,"decision_reason":reason,
                        "display_name":request["display_name"],"blocked_path":request["blocked_path"]}),
                    BTreeMap::new(),
                )
            }
        };
        self.requests.insert(
            id.to_owned(),
            Request {
                kind,
                input,
                tool_use_id,
                raw,
                plan_modes,
            },
        );
        if !self.session_state_seen {
            self.set_tempo(Tempo::Blocked, now, seen);
        }
        seen.dirty = true;
    }

    fn settle_after_request(&mut self, seen: &mut Observed) {
        seen.dirty = true;
        if !self.session_state_seen && self.requests.is_empty() && self.tempo == Tempo::Blocked {
            self.tempo = Tempo::Active;
        }
    }

    fn observe_stream(&mut self, frame: &Value, seen: &mut Observed) {
        let event = &frame["event"];
        match event["type"].as_str() {
            Some("message_start") => {
                self.message_id = event["message"]["id"].as_str().unwrap_or("").to_owned();
                self.streaming.clear();
            }
            Some("content_block_delta") if event["delta"]["type"] == "text_delta" => {
                let index = event["index"].as_u64().unwrap_or(0);
                if let Some(text) = event["delta"]["text"].as_str() {
                    self.streaming.entry(index).or_default().push_str(text);
                    self.preview_text = self
                        .streaming
                        .values()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("");
                    seen.preview =
                        !self.preview_text.trim().is_empty() && !self.preview_user.is_empty();
                }
            }
            _ => {}
        }
    }

    fn set_tempo(&mut self, tempo: Tempo, now: u64, seen: &mut Observed) {
        if self.tempo != tempo {
            self.tempo = tempo;
            self.since_ms = now;
            seen.dirty = true;
        }
    }

    // ---------------------------------------------------------------- 审批与问答

    fn plan_dialog(
        &self,
        id: &str,
        input: &Value,
        now: u64,
    ) -> (Value, BTreeMap<u64, Option<String>>) {
        // 批准项与原版计划确认框一致：先自动模式，再自动接受编辑，再逐项确认；只列出 CLI 实际支持的模式。
        let supports = |mode: &str| self.modes.iter().any(|value| value == mode);
        let mut approvals: Vec<(&str, &str)> = Vec::new();
        if supports("auto") {
            approvals.push(("Yes, and use auto mode", "auto"));
        }
        if supports("acceptEdits") {
            approvals.push(("Yes, auto-accept edits", "acceptEdits"));
        }
        if supports("default") || supports("manual") || approvals.is_empty() {
            approvals.push(("Yes, manually approve edits", "default"));
        }
        let mut options = Vec::new();
        let mut modes = BTreeMap::new();
        for (position, (label, mode)) in approvals.into_iter().enumerate() {
            let index = position as u64 + 1;
            options.push(json!({"index":index,"label":label,"action":"approve"}));
            modes.insert(index, Some(mode.to_owned()));
        }
        let index = options.len() as u64 + 1;
        options
            .push(json!({"index":index,"label":"Tell Claude what to change","action":"feedback"}));
        let plan = input["plan"].as_str().unwrap_or("").trim();
        (
            json!({"kind":"plan","id":id,"target":"","description":plan,
                "planPath":input["planFilePath"],"options":options,
                "statusAtMs":now,"remoteSupported":true,"state":"waiting"}),
            modes,
        )
    }

    /// 把上层对某个请求的回答折算成 CLI 能接受的 `can_use_tool` 回复。
    pub(super) fn answer(&self, request_id: &str, answer: &Value) -> Result<Value, AnswerError> {
        let request = self
            .requests
            .get(request_id)
            .ok_or(AnswerError::Gone)?
            .clone();
        let behavior = answer["behavior"].as_str().unwrap_or("");
        if !matches!(behavior, "allow" | "deny") {
            return Err(AnswerError::Invalid("请选择允许或拒绝"));
        }
        let mut response = match request.kind {
            RequestKind::Question | RequestKind::Tool => {
                if behavior == "allow" {
                    let input = answer.get("updatedInput").filter(|value| value.is_object());
                    json!({"behavior":"allow","updatedInput":input.cloned().unwrap_or_else(|| request.input.clone())})
                } else {
                    json!({"behavior":"deny",
                        "message":answer["message"].as_str().unwrap_or("用户拒绝本次操作"),
                        "interrupt":answer["interrupt"] == true})
                }
            }
            RequestKind::Plan => {
                if behavior == "allow" {
                    let index = answer["option"]
                        .as_u64()
                        .or_else(|| request.plan_modes.keys().next().copied());
                    let mode = index
                        .and_then(|index| request.plan_modes.get(&index))
                        .ok_or(AnswerError::Invalid("计划确认选项无效"))?;
                    let mut value = json!({"behavior":"allow","updatedInput":request.input});
                    if let Some(mode) = mode {
                        // 批准计划并同时决定后续权限模式；只在本会话内生效，不写入用户设置。
                        value["updatedPermissions"] =
                            json!([{"type":"setMode","mode":mode,"destination":"session"}]);
                    }
                    value
                } else {
                    let feedback = answer["feedback"].as_str().unwrap_or("").trim();
                    let message = if feedback.is_empty() {
                        "The user did not approve this plan yet. Stay in plan mode and wait for their feedback.".to_owned()
                    } else {
                        format!(
                            "The user did not approve this plan and asks for these changes. Revise the plan accordingly:\n{feedback}"
                        )
                    };
                    json!({"behavior":"deny","message":message})
                }
            }
        };
        if !request.tool_use_id.is_empty() {
            response["toolUseID"] = json!(request.tool_use_id);
        }
        Ok(response)
    }

    pub(super) fn answered(&mut self, request_id: &str, now: u64) -> Observed {
        let mut seen = Observed::default();
        if self.requests.remove(request_id).is_some() {
            self.settle_after_request(&mut seen);
            if !self.session_state_seen {
                self.set_tempo(Tempo::Active, now, &mut seen);
            }
        }
        seen
    }

    // ---------------------------------------------------------------- 快照字段

    pub(super) fn status(&self) -> Value {
        let waiting = !self.requests.is_empty();
        let questions = self
            .requests
            .values()
            .any(|request| request.kind == RequestKind::Question);
        let (tempo, state) = if waiting {
            ("blocked", "blocked")
        } else if self.compacting {
            ("active", "compacting")
        } else {
            (
                self.tempo.as_str(),
                match self.tempo {
                    Tempo::Idle => "idle",
                    Tempo::Active => "working",
                    Tempo::Blocked => "blocked",
                },
            )
        };
        let mut status = json!({"tempo":tempo,"state":state,"detail":null,"outcome":null,
            "endedAtMs":null,"sessionAlive":self.alive,
            "needs":if waiting {json!(if questions {"input needed"} else {"permission prompt"})} else {Value::Null}});
        if self.alive {
            status["sessionStatus"] = json!(if waiting {
                "waiting"
            } else if tempo == "active" {
                "busy"
            } else {
                "idle"
            });
            status["sessionStatusAtMs"] = json!(self.since_ms);
        }
        // 用户要求后台任务进入会话菜单；明细只来自当前在线 CLI，不缓存成历史运行状态。
        status["backgroundTaskItems"] = if self.alive { self.tasks.items() } else { json!([]) };
        if self.alive && self.tasks.has_tasks() {
            status["background"] = json!(self.tasks.summary());
        }
        status
    }

    pub(super) fn requests_value(&self) -> Value {
        let mut map = serde_json::Map::new();
        for (id, request) in &self.requests {
            map.insert(id.clone(), request.raw.clone());
        }
        Value::Object(map)
    }

    /// 以读取器的 JSONL 快照为底，叠加对端从结构化帧得到的实时字段。
    ///
    /// 消息的网页身份（`clients`）与“待确认”列表（`pending`）由读取器按原生记录维护，这里原样保留；
    /// `changes` 是对端自己登记的设置变更（目前只有需要重启才生效的完全访问权限）。
    pub(super) fn compose(&self, reader: &Value, changes: &Value) -> Value {
        let mut frame = reader.clone();
        frame["job_id"] = json!("");
        frame["native_status"] = self.status();
        if !self.tasks.receipts().is_empty() {
            frame["background_task_results"] = json!(self.tasks.receipts());
        }
        frame["requests"] = self.requests_value();
        if frame["native_control"].is_object() {
            // 权限模式、模型与等级以 CLI 刚报告的实际值为准；JSONL 要等下一条用户记录才带这些字段。
            let settings = &mut frame["native_control"]["settings"];
            if !settings.is_object() {
                *settings = json!({});
            }
            if !self.actual.permission_mode.is_empty() {
                settings["permissionMode"] = json!(self.actual.permission_mode);
            }
            if !self.actual.model.is_empty() {
                settings["model"] = json!(self.actual.model);
            }
            if !self.actual.effort.is_empty() {
                settings["effort"] = json!(self.actual.effort);
            }
            settings["changes"] = if changes.is_object() {
                changes.clone()
            } else {
                json!({})
            };
            frame["native_control"]["dialog"] = Value::Null;
            frame["native_control"]["versionRecovery"] = Value::Null;
        }
        frame
    }

    /// 正文预览：绑定当前轮次的用户消息和读取器给出的正式正文基线。
    pub(super) fn preview_frame(&self, basis: &str) -> Option<Value> {
        let text = self.preview_text.trim();
        if text.is_empty() || self.preview_user.is_empty() || basis.len() != 64 {
            return None;
        }
        Some(
            json!({"type":"shared_preview","native_user_id":self.preview_user,
            "preview_basis":basis,"text":self.preview_text}),
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum AnswerError {
    /// 请求已被 CLI 取消或已回答。
    Gone,
    Invalid(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> PeerState {
        PeerState::new(
            ["default", "acceptEdits", "plan", "auto"]
                .map(str::to_owned)
                .to_vec(),
        )
    }

    fn lifecycle(uuid: &str, name: &str) -> Value {
        json!({"type":"command_lifecycle","command_uuid":uuid,"state":name})
    }

    fn session_state(state: &str) -> Value {
        json!({"type":"system","subtype":"session_state_changed","state":state})
    }

    #[test]
    fn session_state_frames_drive_the_tempo_and_waiting_status() {
        let mut state = state();
        state.set_alive(true, 1);
        assert_eq!(state.status()["tempo"], "idle");
        assert_eq!(state.status()["sessionStatus"], "idle");

        assert!(state.observe_cli(&session_state("running"), 2).dirty);
        assert_eq!(state.status()["tempo"], "active");
        assert_eq!(state.status()["sessionStatus"], "busy");

        let request = json!({"type":"control_request","request_id":"r1","request":{
            "subtype":"can_use_tool","tool_name":"Bash","input":{"command":"touch a"},
            "tool_use_id":"toolu_1","description":"create a file"}});
        state.observe_cli(&request, 3);
        state.observe_cli(&session_state("requires_action"), 3);
        let status = state.status();
        assert_eq!(status["tempo"], "blocked");
        assert_eq!(status["sessionStatus"], "waiting");
        assert_eq!(status["needs"], "permission prompt");
        let raw = &state.requests_value()["r1"];
        assert_eq!(raw["subtype"], "can_use_tool");
        assert_eq!(raw["decision_reason"], "create a file");

        assert!(state.answered("r1", 4).dirty);
        state.observe_cli(&session_state("running"), 4);
        state.observe_cli(&session_state("idle"), 5);
        assert_eq!(state.status()["tempo"], "idle");
        assert!(state.requests_value().as_object().unwrap().is_empty());
    }

    #[test]
    fn older_cli_without_session_state_events_falls_back_to_message_lifecycle() {
        let mut state = state();
        state.set_alive(true, 1);
        state.record_sent("u1");
        state.observe_cli(&lifecycle("u1", "queued"), 3);
        assert_eq!(state.status()["tempo"], "idle", "排队本身不是执行");
        state.observe_cli(&lifecycle("u1", "started"), 4);
        assert_eq!(state.status()["tempo"], "active");
        state.observe_cli(&lifecycle("u1", "completed"), 5);
        assert_eq!(state.status()["tempo"], "idle");
    }

    /// 真实 CLI 联调发现：被打断的命令以 cancelled 结束，不是 completed；它也必须结束“进行中”。
    #[test]
    fn a_cancelled_message_is_finished_not_left_in_flight() {
        let mut state = state();
        state.set_alive(true, 1);
        state.record_sent("u1");
        state.observe_cli(&lifecycle("u1", "queued"), 2);
        state.observe_cli(&lifecycle("u1", "started"), 3);
        assert!(state.has_active_lifecycle());
        state.observe_cli(&lifecycle("u1", "cancelled"), 4);
        assert_eq!(state.sent_phase("u1"), Some(Phase::Completed));
        assert!(!state.has_active_lifecycle());
    }

    #[test]
    fn written_messages_only_count_as_work_after_the_cli_acknowledges_them() {
        let mut state = state();
        state.set_alive(true, 1);
        state.record_sent("u1");
        assert_eq!(state.sent_phase("u1"), Some(Phase::Written));
        assert!(!state.has_active_lifecycle());
        state.observe_cli(&lifecycle("u1", "queued"), 3);
        assert!(state.has_active_lifecycle());
        state.forget_sent("u1");
        assert!(!state.has_active_lifecycle());
    }

    #[test]
    fn question_requests_keep_the_questions_and_need_input() {
        let mut state = state();
        state.set_alive(true, 1);
        let request = json!({"type":"control_request","request_id":"q1","request":{
            "subtype":"can_use_tool","tool_name":"AskUserQuestion","tool_use_id":"toolu_q",
            "input":{"questions":[{"question":"Pick?","header":"H","multiSelect":true,
                "options":[{"label":"A","description":"a"},{"label":"B","description":"b"}]}]}}});
        state.observe_cli(&request, 2);
        assert_eq!(state.status()["needs"], "input needed");
        let raw = &state.requests_value()["q1"];
        assert_eq!(raw["tool_name"], "AskUserQuestion");
        assert_eq!(raw["input"]["questions"][0]["multiSelect"], true);

        let answered =
            json!({"behavior":"allow","updatedInput":{"questions":[],"answers":{"Pick?":"A, B"}}});
        let reply = state.answer("q1", &answered).unwrap();
        assert_eq!(reply["behavior"], "allow");
        assert_eq!(reply["updatedInput"]["answers"]["Pick?"], "A, B");
        assert_eq!(reply["toolUseID"], "toolu_q");
    }

    #[test]
    fn denied_tool_requests_carry_the_message_and_cancel_flag() {
        let mut state = state();
        state.observe_cli(
            &json!({"type":"control_request","request_id":"r","request":{
                "subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm x"},"tool_use_id":"t"}}),
            1,
        );
        let reply = state
            .answer(
                "r",
                &json!({"behavior":"deny","message":"用户拒绝本次操作","interrupt":true}),
            )
            .unwrap();
        assert_eq!(reply["behavior"], "deny");
        assert_eq!(reply["interrupt"], true);
        assert_eq!(reply["message"], "用户拒绝本次操作");
        assert_eq!(
            state.answer("r", &json!({"behavior":"allow"})).unwrap()["behavior"],
            "allow"
        );
        assert!(state.answered("r", 2).dirty);
        assert_eq!(
            state.answer("r", &json!({"behavior":"allow"})),
            Err(AnswerError::Gone)
        );
    }

    #[test]
    fn plan_approval_offers_native_modes_and_applies_the_chosen_one_to_the_session() {
        let mut state = state();
        state.observe_cli(
            &json!({"type":"control_request","request_id":"p1","request":{
                "subtype":"can_use_tool","tool_name":"ExitPlanMode","tool_use_id":"toolu_p",
                "input":{"plan":"# Plan\n1. step"}}}),
            1,
        );
        let raw = &state.requests_value()["p1"];
        assert_eq!(raw["subtype"], "native_dialog");
        let dialog = &raw["dialog"];
        assert_eq!(dialog["kind"], "plan");
        assert_eq!(dialog["description"], "# Plan\n1. step");
        let labels: Vec<_> = dialog["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| option["label"].as_str().unwrap())
            .collect();
        assert_eq!(
            labels,
            [
                "Yes, and use auto mode",
                "Yes, auto-accept edits",
                "Yes, manually approve edits",
                "Tell Claude what to change"
            ]
        );
        let approved = state
            .answer("p1", &json!({"behavior":"allow","option":2}))
            .unwrap();
        assert_eq!(approved["behavior"], "allow");
        assert_eq!(approved["updatedPermissions"][0]["mode"], "acceptEdits");
        assert_eq!(approved["updatedPermissions"][0]["destination"], "session");

        let revised = state
            .answer("p1", &json!({"behavior":"deny","feedback":"请拆成两步"}))
            .unwrap();
        assert_eq!(revised["behavior"], "deny");
        assert!(revised["message"].as_str().unwrap().contains("请拆成两步"));
        assert_eq!(
            state.answer("p1", &json!({"behavior":"allow","option":9})),
            Err(AnswerError::Invalid("计划确认选项无效"))
        );
    }

    #[test]
    fn cancelled_requests_disappear_without_an_answer() {
        let mut state = state();
        state.observe_cli(
            &json!({"type":"control_request","request_id":"r","request":{
                "subtype":"can_use_tool","tool_name":"Bash","input":{},"tool_use_id":"t"}}),
            1,
        );
        assert!(state.has_requests());
        let seen = state.observe_cli(
            &json!({"type":"control_cancel_request","request_id":"r"}),
            2,
        );
        assert!(seen.dirty);
        assert!(!state.has_requests());
    }

    #[test]
    fn streaming_text_becomes_a_preview_bound_to_the_turn_user_message() {
        let mut state = state();
        state.set_alive(true, 1);
        state.observe_cli(
            &json!({"type":"system","subtype":"status","status":"requesting","user_message_uuids":["u1"]}),
            2,
        );
        state.observe_cli(
            &json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"m1"}}}),
            3,
        );
        let seen = state.observe_cli(
            &json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,
                "delta":{"type":"text_delta","text":"你好"}}}),
            4,
        );
        assert!(seen.preview);
        state.observe_cli(
            &json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,
                "delta":{"type":"text_delta","text":"，世界"}}}),
            5,
        );
        let basis = "a".repeat(64);
        let frame = state.preview_frame(&basis).unwrap();
        assert_eq!(frame["native_user_id"], "u1");
        assert_eq!(frame["text"], "你好，世界");
        assert!(state.preview_frame("short").is_none());

        // 完整消息到达（已写入 JSONL）后不再重复展示流式缓冲。
        state.observe_cli(
            &json!({"type":"assistant","message":{"id":"m1","content":[]}}),
            6,
        );
        assert!(
            state.preview_frame(&basis).is_some(),
            "预览保留到正式正文接替，避免中间闪烁"
        );
    }

    #[test]
    fn compaction_status_is_reported_as_active_and_forwarded_as_a_signal() {
        let mut state = state();
        state.set_alive(true, 1);
        let seen = state.observe_cli(
            &json!({"type":"system","subtype":"status","status":"compacting"}),
            2,
        );
        assert_eq!(seen.signals, vec![Signal::Compacting(true)]);
        assert_eq!(state.status()["tempo"], "active");
        assert_eq!(state.status()["state"], "compacting");

        let seen = state.observe_cli(
            &json!({"type":"system","subtype":"status","status":null,"compact_result":"failed","compact_error":"boom"}),
            3,
        );
        assert_eq!(
            seen.signals,
            vec![
                Signal::Compacting(false),
                Signal::CompactFailed("boom".into())
            ]
        );
        assert_eq!(state.status()["tempo"], "idle");
    }

    #[test]
    fn background_tasks_are_summarized_like_the_native_footer() {
        let mut state = state();
        state.set_alive(true, 1);
        state.observe_cli(
            &json!({"type":"system","subtype":"background_tasks_changed","tasks":[
                {"task_id":"a","task_type":"local_bash","description":"x"},
                {"task_id":"b","task_type":"local_bash","description":"y"},
                {"task_id":"c","task_type":"monitor","description":"z"},
                {"task_id":"d","task_type":"local_workflow","description":"hidden","ambient":true}]}),
            2,
        );
        assert_eq!(state.status()["background"], "2 shells, 1 monitor");
        state.observe_cli(
            &json!({"type":"system","subtype":"background_tasks_changed","tasks":[]}),
            3,
        );
        assert!(state.status().get("background").is_none());
    }

    #[test]
    fn foreground_task_is_hidden_without_removing_native_lifetime_protection() {
        let mut state = state();
        state.set_alive(true, 1);
        state.observe_cli(&json!({"type":"system","subtype":"task_started",
            "task_id":"foreground","task_type":"local_bash","is_backgrounded":false}), 2);
        assert!(state.has_background_tasks(), "只调整显示，仍保留原生任务保活保护");
        assert!(state.status()["backgroundTaskItems"].as_array().unwrap().is_empty());
        state.observe_cli(&json!({"type":"system","subtype":"task_notification",
            "task_id":"foreground","status":"completed"}), 3);
        assert!(!state.has_background_tasks());
    }

    #[test]
    fn background_task_terminal_frame_survives_exit_without_claiming_a_wake() {
        let mut state = state();
        state.set_alive(true, 1);
        let ended = json!({"type":"system","subtype":"task_notification","status":"failed",
            "task_id":"bg","tool_use_id":"tool","summary":"  原生错误\r\n "});
        assert!(state.observe_cli(&ended, 2).dirty);
        assert!(!state.observe_cli(&ended, 3).dirty);
        state.set_alive(false, 4);
        let frame = state.compose(&json!({"messages":[]}), &Value::Null);
        assert_eq!(frame["background_task_results"].as_array().unwrap().len(), 1);
        assert_eq!(frame["background_task_results"][0]["summary"], "  原生错误\r\n ");
        assert_eq!(frame["messages"], json!([]));
        assert_eq!(frame["native_status"]["backgroundTaskItems"], json!([]));
        assert!(!state.has_background_tasks());
    }

    #[test]
    fn process_exit_clears_everything_that_died_with_it() {
        let mut state = state();
        state.set_alive(true, 1);
        state.observe_cli(&session_state("running"), 2);
        state.observe_cli(
            &json!({"type":"control_request","request_id":"r","request":{
                "subtype":"can_use_tool","tool_name":"Bash","input":{},"tool_use_id":"t"}}),
            3,
        );
        state.set_alive(false, 4);
        let status = state.status();
        assert_eq!(status["tempo"], "idle");
        assert_eq!(status["sessionAlive"], false);
        assert!(status["needs"].is_null());
        assert!(!state.has_requests());
    }

    #[test]
    fn composed_snapshot_keeps_the_reader_history_and_overrides_live_fields() {
        let mut state = state();
        state.set_alive(true, 1);
        state.observe_cli(&session_state("running"), 2);
        state.observe_cli(
            &json!({"type":"system","subtype":"status","status":null,"permissionMode":"plan"}),
            2,
        );
        state.record_sent("u2");
        state.observe_cli(&lifecycle("u2", "queued"), 3);
        let reader = json!({"type":"shared_snapshot","native_id":"n","job_id":"stale",
            "native_status":{"tempo":"idle"},"messages":[{"uuid":"u1","type":"user"}],
            "history_head":"h","clients":{"old":"x"},"pending":[{"id":"old"}],
            "requests":{"native-x":{}},"preview_basis":"b",
            "native_control":{"settings":{"model":"m","permissionMode":"default","changes":{"model":{"state":"pending"}}},
                "dialog":{"kind":"unknown"},"versionRecovery":{"state":"pending"}}});
        let frame = state.compose(&reader, &json!({}));
        assert_eq!(frame["job_id"], "");
        assert_eq!(frame["history_head"], "h");
        assert_eq!(frame["messages"][0]["uuid"], "u1");
        assert_eq!(frame["native_status"]["tempo"], "active");
        // 网页消息身份与待确认列表属于读取器，对端不改写。
        assert_eq!(frame["pending"][0]["id"], "old");
        assert_eq!(frame["clients"]["old"], "x");
        assert!(frame["requests"].as_object().unwrap().is_empty());
        assert_eq!(
            frame["native_control"]["settings"]["permissionMode"],
            "plan"
        );
        assert_eq!(frame["native_control"]["settings"]["model"], "m");
        assert!(
            frame["native_control"]["settings"]["changes"]
                .as_object()
                .unwrap()
                .is_empty()
        );
        let pending =
            json!({"permissionMode":{"id":"c1","requested":"bypassPermissions","state":"pending"}});
        let frame = state.compose(&reader, &pending);
        assert_eq!(
            frame["native_control"]["settings"]["changes"]["permissionMode"]["state"],
            "pending"
        );
        assert!(frame["native_control"]["dialog"].is_null());
        assert!(frame["native_control"]["versionRecovery"].is_null());
    }
}
