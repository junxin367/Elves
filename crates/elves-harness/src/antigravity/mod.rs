//! Antigravity 会话执行后端：调用官方运行时与上游在线能力严格经官方 CLI 的 ACP（Agent Client Protocol）协议处理；
//! 本地用户配置文件与本地已落盘对话转录按本机或所属 WSL 实例直接只读/安全写入（见 AGENTS.md）。

pub(crate) mod acp;
pub(crate) mod local_store;
pub(crate) mod projection;
pub(crate) mod queue;
pub(crate) mod usage;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};

use self::acp::{AcpChannels, AcpClient, AcpIncomingRequest, EnvConfig};
use self::projection::MappedPermission;
use self::queue::AntigravityQueue;
use super::{ApiError, HarnessAuthorization, HarnessController, now_ms};

const SESSIONS_FILE: &str = "remote-antigravity-sessions.json";
const TURNS_DIR: &str = "antigravity-session-turns";
const CATALOG_TTL: Duration = Duration::from_secs(300);
const DISCOVERY_TTL: Duration = Duration::from_secs(60);
const USAGE_TTL: Duration = Duration::from_secs(60);
const GOAL_MAX_ITERATIONS: u64 = 20;
const GOAL_COMPLETE_MARKER: &str = "[GOAL_COMPLETE]";

fn error(code: u16, message: impl Into<String>) -> anyhow::Error {
    ApiError::new(code, message.into()).into()
}

pub(crate) fn api_error(failure: anyhow::Error) -> anyhow::Error {
    if failure.downcast_ref::<ApiError>().is_some() {
        failure
    } else {
        ApiError::new(503, format!("Antigravity ACP 操作失败：{failure}")).into()
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SavedStore {
    conversations: BTreeMap<String, Conversation>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Conversation {
    #[serde(default = "antigravity_backend")]
    backend: String,
    id: String,
    native_id: String,
    project_id: Value,
    cwd: String,
    linux_cwd: String,
    config: EnvConfig,
    title: String,
    model: String,
    effort: String,
    #[serde(default)]
    permission_mode: Option<String>,
    #[serde(default)]
    permission_revision: u64,
    #[serde(default)]
    available_modes: Vec<String>,
    created_at: u64,
    updated_at: u64,
    pinned: bool,
    unread: bool,
    deleted: bool,
    #[serde(default)]
    discovered: bool,
    #[serde(default)]
    history_loaded: bool,
    #[serde(default)]
    local_transcript_mtime_ms: u64,
    #[serde(default)]
    local_transcript_len: u64,
    #[serde(default)]
    edit_history_head: String,
    #[serde(default)]
    token_usage: Value,
    #[serde(default)]
    goal: Value,
    #[serde(default)]
    remote_queue: AntigravityQueue,
    #[serde(default)]
    active_turn_id: Option<String>,
    #[serde(default)]
    turns: Vec<Value>,
}

fn antigravity_backend() -> String {
    "antigravity".into()
}

impl Conversation {
    fn active(&self) -> bool {
        self.active_turn_id.is_some()
            || self
                .turns
                .last()
                .is_some_and(|t| t["status"] == "inProgress")
    }

    fn effective_cwd(&self) -> &str {
        if self.config.is_local() {
            &self.cwd
        } else if !self.linux_cwd.is_empty() {
            &self.linux_cwd
        } else {
            &self.cwd
        }
    }

    fn displayed_turn(&self, turn: &Value) -> Value {
        let mut value = turn.clone();
        if let Some(obj) = value.as_object_mut() {
            obj.remove("_acpUsage");
        }
        if let Some(items) = value["items"].as_array_mut() {
            for item in items.iter_mut() {
                if let Some(obj) = item.as_object_mut() {
                    obj.remove("_acpKind");
                    obj.remove("_acpTitle");
                    obj.remove("_acpRawInput");
                    obj.remove("_acpRawOutput");
                }
                if item["type"] == "userMessage" {
                    item["messageMetadata"] = json!({
                        "turnId": turn["id"],
                        "messageId": item["id"],
                        "editable": !self.deleted,
                    });
                }
            }
        }
        value
    }

    fn session_plan(&self) -> Vec<Value> {
        self.turns
            .iter()
            .rev()
            .find_map(|turn| {
                turn["plan"]
                    .as_array()
                    .filter(|plan| !plan.is_empty())
                    .cloned()
            })
            .unwrap_or_default()
    }

    fn view(&self, has_waiting_request: bool, queue_epoch: &str) -> Value {
        let instance_id = self.config.instance_id();
        let instance_label = super::instances::label(
            "antigravity",
            &self.config.distribution,
            &self.config.user,
        );
        let capabilities = json!({
            "sideThreads": false,
            "subagents": false,
            "goalComposer": true,
            "goalPause": true,
            "goalResume": true,
            "messageEdit": !self.edit_history_head.is_empty(),
            "messageEditHistory": true,
            "messageEditModelSettings": true,
            "queue": true,
            "queueSendNow": true,
            "remoteQueue": true,
            "queueEditRecovery": true,
            "queuedImageEditing": true,
            "queuedImageRemoval": true,
            "compact": false,
            "sessionPermissions": true,
            "forkThread": false,
        });
        let status = if has_waiting_request {
            json!({ "type": "waiting", "activeFlags": ["waitingOnApproval"] })
        } else if self.active() {
            json!({ "type": "active" })
        } else {
            json!({ "type": "idle" })
        };
        let queue = self.remote_queue.snapshot(&self.id, queue_epoch);
        let mut view = json!({
            "id": self.id,
            "backend": self.backend,
            "instanceId": instance_id,
            "instanceLabel": instance_label,
            "wslDistribution": if self.config.is_local() { Value::Null } else { json!(self.config.distribution) },
            "wslUser": if self.config.is_local() { Value::Null } else { json!(self.config.user) },
            "title": self.title,
            "cwd": self.cwd,
            "projectId": self.project_id,
            "model": self.model,
            "reasoningEffort": self.effort,
            "permissionMode": self.permission_mode,
            "permissionRevision": self.permission_revision,
            "permissionModes": json!(exposed_permission_modes(&self.available_modes)),
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
            "recencyAt": self.updated_at,
            "pinned": self.pinned,
            "hasUnreadTurn": self.unread,
            "hostId": "local",
            "ephemeral": false,
            "status": status,
            "editableMessage": null,
            "editHistoryHead": self.edit_history_head,
            "messageEditReady": !self.active() && !has_waiting_request,
            "goal": self.goal,
            "tokenUsage": self.token_usage,
            "capabilities": capabilities,
            "queuedMessages": queue["queuedMessages"],
            "queuedEdits": queue["queuedEdits"],
            "queueEpoch": queue["queueEpoch"],
            "queueRevision": queue["queueRevision"],
            "queuePaused": queue["queuePaused"],
        });
        if has_waiting_request {
            view["priority"] = json!({ "attentionState": "waiting" });
        }
        view
    }
}

struct ActiveRuntime {
    client: Arc<AcpClient>,
    native_session_id: String,
    #[allow(dead_code)]
    initialize_result: Value,
    supports_image: bool,
    supports_steering: bool,
    dangerously_skip_permissions: bool,
}

struct PendingPermission {
    rpc_id: Value,
    mapped: MappedPermission,
}

#[derive(Clone)]
struct CachedCatalog {
    at: Instant,
    models: Value,
    commands: Vec<Value>,
    tier: Option<Value>,
    agent_info: Value,
}

#[derive(Clone)]
struct CachedUsage {
    at: Instant,
    response: Value,
}

pub(crate) struct Antigravity {
    #[allow(dead_code)]
    weak: Weak<Self>,
    directory: PathBuf,
    saved: Mutex<SavedStore>,
    runtimes: Mutex<HashMap<String, Arc<ActiveRuntime>>>,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    requests: Mutex<HashMap<String, BTreeMap<String, PendingPermission>>>,
    catalogs: Mutex<HashMap<String, CachedCatalog>>,
    discoveries: Mutex<HashMap<String, Instant>>,
    usage_cache: Mutex<HashMap<String, CachedUsage>>,
    quota_usage_cache: Mutex<HashMap<String, usage::CachedQuotaUsage>>,
    local_quota_cache: Mutex<HashMap<String, usage::CachedLocalQuotaMetrics>>,
    quota_prices: Mutex<usage::QuotaPriceCache>,
    syncing_discovered: Mutex<HashSet<String>>,
    controller: OnceLock<Weak<HarnessController>>,
    accepting: AtomicBool,
    lifecycle_gate: RwLock<()>,
    epoch: String,
    request_revision: AtomicU64,
}

impl Antigravity {
    pub(crate) fn owns_id(id: &str) -> bool {
        id.starts_with("antigravity-") && id.len() > "antigravity-".len()
    }

    pub(crate) fn new(directory: &Path) -> anyhow::Result<Arc<Self>> {
        let path = directory.join(SESSIONS_FILE);
        let mut saved: SavedStore = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("Antigravity 会话存档损坏，未覆盖原文件")?
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => SavedStore::default(),
            Err(err) => return Err(err.into()),
        };
        let turns_dir = directory.join(TURNS_DIR);
        for conversation in saved.conversations.values_mut() {
            if conversation.turns.is_empty() {
                let turn_file = turns_dir.join(format!("{}.json", conversation.id));
                if let Ok(bytes) = std::fs::read(&turn_file)
                    && let Ok(turns) = serde_json::from_slice::<Vec<Value>>(&bytes)
                {
                    conversation.turns = turns;
                }
            }
            conversation.remote_queue.recover();
            conversation.active_turn_id = None;
            if let Some(last) = conversation.turns.last_mut()
                && last["status"] == "inProgress"
            {
                last["status"] = json!("interrupted");
                last["error"] = json!({ "message": "服务曾重启，请核对上下文后继续；未自动重发消息" });
                projection::finalize_turn_items(last, false, true, now_ms());
            }
            if conversation.edit_history_head.is_empty() {
                conversation.edit_history_head = format!("agy-head-{}", conversation.turns.len());
            }
            if conversation.goal["status"] == "active" {
                conversation.goal["status"] = json!("paused");
                conversation.goal["updatedAt"] = json!(now_ms());
            }
        }
        Ok(Arc::new_cyclic(|weak| Self {
            weak: weak.clone(),
            directory: directory.to_owned(),
            saved: Mutex::new(saved),
            runtimes: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            requests: Mutex::new(HashMap::new()),
            catalogs: Mutex::new(HashMap::new()),
            discoveries: Mutex::new(HashMap::new()),
            usage_cache: Mutex::new(HashMap::new()),
            quota_usage_cache: Mutex::new(HashMap::new()),
            local_quota_cache: Mutex::new(HashMap::new()),
            quota_prices: Mutex::new(usage::QuotaPriceCache::default()),
            syncing_discovered: Mutex::new(HashSet::new()),
            controller: OnceLock::new(),
            accepting: AtomicBool::new(true),
            lifecycle_gate: RwLock::new(()),
            epoch: format!("antigravity-{}", uuid::Uuid::new_v4()),
            request_revision: AtomicU64::new(0),
        }))
    }

    pub(crate) fn attach(self: &Arc<Self>, controller: Weak<HarnessController>) {
        let _ = self.controller.set(controller);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak = Arc::downgrade(self);
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(750));
            loop {
                interval.tick().await;
                let Some(service) = weak.upgrade() else {
                    break;
                };
                if !service.is_accepting() {
                    break;
                }
                service.drain_queues_and_goals().await;
            }
        });
    }

    fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::Acquire)
            && !self
                .controller
                .get()
                .and_then(Weak::upgrade)
                .is_some_and(|c| c.closing.load(Ordering::Acquire))
    }

    pub(crate) async fn release(self: &Arc<Self>) {
        self.accepting.store(false, Ordering::Release);
        let _gate = self.lifecycle_gate.write().await;
        let active: Vec<Arc<ActiveRuntime>> =
            self.runtimes.lock().await.drain().map(|(_, v)| v).collect();
        for runtime in active {
            let _ = runtime.client.session_close(&runtime.native_session_id).await;
            runtime.client.close().await;
        }
        self.requests.lock().await.clear();
    }

    fn publish(&self, event: Value) {
        if let Some(controller) = self.controller.get().and_then(Weak::upgrade) {
            controller.publish_backend_event("antigravity", event);
        }
    }

    async fn lock(&self, key: &str) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .await
            .entry(key.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn persist_locked(&self, saved: &SavedStore, touched_turn_id: Option<&str>) -> anyhow::Result<()> {
        if let Some(id) = touched_turn_id
            && let Some(c) = saved.conversations.get(id)
        {
            let turns_dir = self.directory.join(TURNS_DIR);
            std::fs::create_dir_all(&turns_dir)?;
            let turn_path = turns_dir.join(format!("{id}.json"));
            let bytes = serde_json::to_vec(&c.turns)?;
            crate::settings::atomic_write(&turn_path, &bytes)?;
        }
        // 主存档不内联完整 turns，仅保留元数据、队列、目标和统计，避免大体积写盘阻塞。
        let mut meta = saved.clone();
        for conv in meta.conversations.values_mut() {
            conv.turns.clear();
        }
        let bytes = serde_json::to_vec(&meta)?;
        crate::settings::atomic_write(&self.directory.join(SESSIONS_FILE), &bytes)?;
        Ok(())
    }

    async fn conversation(&self, id: &str) -> anyhow::Result<Conversation> {
        self.saved
            .lock()
            .await
            .conversations
            .get(id)
            .filter(|c| !c.deleted)
            .cloned()
            .ok_or_else(|| error(404, "Antigravity 会话不存在或已删除"))
    }

    pub(crate) async fn instance_id(&self, thread_id: &str) -> anyhow::Result<String> {
        Ok(self.conversation(thread_id).await?.config.instance_id())
    }

    pub(crate) async fn add_known_instances(&self, instances: &mut Vec<super::instances::Instance>) {
        let saved = self.saved.lock().await;
        for c in saved.conversations.values().filter(|c| !c.deleted) {
            let inst_id = c.config.instance_id();
            if c.config.validate().is_err() || instances.iter().any(|item| item.id == inst_id) {
                continue;
            }
            instances.push(super::instances::Instance {
                id: inst_id,
                backend: "antigravity".into(),
                label: super::instances::label(
                    "antigravity",
                    &c.config.distribution,
                    &c.config.user,
                ),
                environment_id: if c.config.is_local() {
                    "local".into()
                } else {
                    format!("wsl:{}", c.config.distribution)
                },
                distribution: c.config.distribution.clone(),
                user: c.config.user.clone(),
                executable: c.config.executable.clone(),
                ready: false,
                status: "已有会话，环境暂未检测到".into(),
                legacy_default: false,
                connection_modes: vec![],
            });
        }
    }

    async fn linux_cwd(config: &EnvConfig, cwd: &str) -> anyhow::Result<String> {
        if config.is_local() {
            return Ok(String::new());
        }
        if cwd.is_empty() {
            return Ok("~".into());
        }
        if cwd.starts_with('/') {
            return Ok(cwd.to_owned());
        }
        let path = Path::new(cwd);
        if let Some(dist) = super::preferences::wsl_distribution(path) {
            if dist.eq_ignore_ascii_case(&config.distribution) {
                let text = cwd.replace('\\', "/");
                let text = text
                    .strip_prefix("//?/UNC/")
                    .map(|v| format!("//{v}"))
                    .unwrap_or(text);
                if let Some(after_host) = text.strip_prefix("//") {
                    let mut parts = after_host.splitn(3, '/');
                    let _host = parts.next();
                    let _dist = parts.next();
                    if let Some(rest) = parts.next() {
                        return Ok(format!("/{rest}"));
                    }
                }
            }
        }
        let bytes = cwd.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            let drive = (bytes[0] as char).to_ascii_lowercase();
            let rest = cwd[2..].replace('\\', "/");
            let rest = rest.trim_start_matches('/');
            return Ok(if rest.is_empty() {
                format!("/mnt/{drive}")
            } else {
                format!("/mnt/{drive}/{rest}")
            });
        }
        Ok("~".into())
    }

    pub(crate) async fn local_input_paths(
        &self,
        thread_id: Option<&str>,
        config: Option<EnvConfig>,
        paths: Vec<PathBuf>,
    ) -> anyhow::Result<Vec<String>> {
        let env = match (thread_id, config) {
            (Some(id), _) => self.conversation(id).await?.config,
            (None, Some(cfg)) => cfg,
            (None, None) => return Err(error(400, "缺少 Antigravity 实例配置")),
        };
        let mut out = Vec::with_capacity(paths.len());
        for path in paths {
            let text = path.to_string_lossy().into_owned();
            if env.is_local() {
                out.push(text);
            } else {
                out.push(Self::linux_cwd(&env, &text).await?);
            }
        }
        Ok(out)
    }

    fn resolve_project_id_for_cwd(sidebar: &Value, cwd: &str) -> Value {
        if cwd.is_empty() {
            return Value::Null;
        }
        let target = normalize_path_key(cwd);
        for project in sidebar["projects"].as_array().into_iter().flatten() {
            if let Some(p_cwd) = project["cwd"].as_str()
                && normalize_path_key(p_cwd) == target
            {
                return project["id"].clone();
            }
        }
        Value::Null
    }

    /// 通过 ACP `session/list` 发现指定本机或 WSL Antigravity 实例中的原生会话。
    pub(crate) async fn discover_in(
        self: &Arc<Self>,
        config: &EnvConfig,
        sidebar: &Value,
    ) -> anyhow::Result<()> {
        config.validate()?;
        let inst_id = config.instance_id();
        {
            let cache = self.discoveries.lock().await;
            if cache
                .get(&inst_id)
                .is_some_and(|at| at.elapsed() < DISCOVERY_TTL)
            {
                return Ok(());
            }
        }
        let gate = self.lock(&format!("discover:{inst_id}")).await;
        let _guard = gate.lock().await;
        {
            let cache = self.discoveries.lock().await;
            if cache
                .get(&inst_id)
                .is_some_and(|at| at.elapsed() < DISCOVERY_TTL)
            {
                return Ok(());
            }
        }
        let home_cwd = if config.is_local() {
            directories::BaseDirs::new()
                .map(|d| d.home_dir().to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let (client, _channels) = AcpClient::spawn(config, &home_cwd, "~", None, false).await?;
        let res = async {
            let _init = client.initialize().await?;
            // 先直接调用只读 session/list；未登录时会立即返回 -32000，避免触发 authenticate 阻塞等待浏览器 OAuth。
            client.session_list().await
        }
        .await;
        client.close().await;
        let sessions = res?;
        let mut saved = self.saved.lock().await;
        let mut changed = false;
        for entry in sessions {
            let Some(native_id) = entry["sessionId"].as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            let id = format!("antigravity-{native_id}");
            let raw_cwd = entry["cwd"].as_str().unwrap_or("");
            let normalized_cwd = projection::normalize_acp_cwd(raw_cwd, !config.is_local());
            let display_cwd = if !config.is_local() && normalized_cwd.starts_with('/') {
                super::preferences::wsl_path(&config.distribution, &normalized_cwd)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| normalized_cwd.clone())
            } else {
                normalized_cwd.clone()
            };
            let title = entry["title"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("Antigravity 会话")
                .to_owned();
            let updated_at = entry["updatedAt"]
                .as_str()
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                .and_then(|dt| u64::try_from(dt.timestamp_millis()).ok())
                .unwrap_or_else(now_ms);
            let project_id = Self::resolve_project_id_for_cwd(sidebar, &display_cwd);

            if let Some(existing) = saved.conversations.get_mut(&id) {
                if existing.deleted {
                    continue;
                }
                if existing.project_id.is_null() && !project_id.is_null() {
                    existing.project_id = project_id;
                    changed = true;
                }
                if existing.discovered && updated_at > existing.updated_at {
                    existing.updated_at = updated_at;
                    changed = true;
                }
                continue;
            }
            let linux_cwd = if config.is_local() {
                String::new()
            } else {
                normalized_cwd
            };
            saved.conversations.insert(
                id.clone(),
                Conversation {
                    backend: antigravity_backend(),
                    id,
                    native_id: native_id.to_owned(),
                    project_id,
                    cwd: display_cwd,
                    linux_cwd,
                    config: config.clone(),
                    title,
                    model: String::new(),
                    effort: String::new(),
                    permission_mode: Some("default".into()),
                    permission_revision: 0,
                    available_modes: vec![
                        "default".into(),
                        "accept-edits".into(),
                        "auto".into(),
                        "bypassPermissions".into(),
                    ],
                    created_at: updated_at,
                    updated_at,
                    pinned: false,
                    unread: false,
                    deleted: false,
                    discovered: true,
                    history_loaded: false,
                    local_transcript_mtime_ms: 0,
                    local_transcript_len: 0,
                    edit_history_head: "agy-head-0".into(),
                    token_usage: Value::Null,
                    goal: Value::Null,
                    remote_queue: AntigravityQueue::default(),
                    active_turn_id: None,
                    turns: Vec::new(),
                },
            );
            changed = true;
        }
        if changed {
            let _ = self.persist_locked(&saved, None).await;
        }
        drop(saved);
        self.discoveries
            .lock()
            .await
            .insert(inst_id, Instant::now());
        Ok(())
    }

    pub(crate) async fn merge_sidebar_instances(
        &self,
        sidebar: &mut Value,
        enabled: Option<&[String]>,
    ) {
        let waiting: std::collections::HashSet<String> = self
            .requests
            .lock()
            .await
            .iter()
            .filter(|(_, map)| !map.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        let saved = self.saved.lock().await;
        let mut rows: Vec<Value> = saved
            .conversations
            .values()
            .filter(|c| {
                !c.deleted && enabled.is_none_or(|ids| ids.contains(&c.config.instance_id()))
            })
            .map(|c| c.view(waiting.contains(&c.id), &self.epoch))
            .collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row["updatedAt"].as_u64().unwrap_or(0)));
        for mut row in rows {
            if row["projectId"].is_null()
                && let Some(cwd) = row["cwd"].as_str()
            {
                let matched = Self::resolve_project_id_for_cwd(sidebar, cwd);
                if !matched.is_null() {
                    row["projectId"] = matched;
                }
            }
            if let Some(recent) = sidebar["recent"].as_array_mut() {
                recent.push(row.clone());
            }
            if row["pinned"] == true
                && let Some(pins) = sidebar["pinnedThreads"].as_array_mut()
            {
                pins.push(row.clone());
            }
            if let Some(projects) = sidebar["projects"].as_array_mut()
                && let Some(project) = projects.iter_mut().find(|p| p["id"] == row["projectId"])
                && let Some(threads) = project["threads"].as_array_mut()
            {
                threads.push(row);
            }
        }
    }

    /// 通过 ACP 探测实例模型目录、思考深度、权限模式、原生斜杠命令及官方订阅等级（`_meta.antigravity.tier`）。
    async fn probe_catalog(
        self: &Arc<Self>,
        config: &EnvConfig,
        cwd: &str,
        force: bool,
    ) -> anyhow::Result<CachedCatalog> {
        config.validate()?;
        let inst_id = config.instance_id();
        if !force
            && let Some(cached) = self.catalogs.lock().await.get(&inst_id).cloned()
            && cached.at.elapsed() < CATALOG_TTL
        {
            return Ok(cached);
        }
        let gate = self.lock(&format!("catalog:{inst_id}")).await;
        let _guard = gate.lock().await;
        if !force
            && let Some(cached) = self.catalogs.lock().await.get(&inst_id).cloned()
            && cached.at.elapsed() < CATALOG_TTL
        {
            return Ok(cached);
        }

        let probe_cwd = if !cwd.is_empty() {
            cwd.to_owned()
        } else if config.is_local() {
            directories::BaseDirs::new()
                .map(|d| d.home_dir().to_string_lossy().into_owned())
                .unwrap_or_else(|| ".".into())
        } else {
            "~".into()
        };
        let linux_cwd = Self::linux_cwd(config, &probe_cwd).await?;
        let effective_cwd = if config.is_local() {
            probe_cwd.clone()
        } else {
            linux_cwd.clone()
        };

        let (client, mut channels) = AcpClient::spawn(config, &probe_cwd, &linux_cwd, None, false).await?;
        let mut temp_session_id: Option<String> = None;
        let outcome = async {
            let init = client.initialize().await?;
            // 先调用只读 session/list 确认已登录；未登录时会立即返回 -32000，不触发 authenticate 的交互式 OAuth 阻塞。
            let _sessions = client.session_list().await?;
            let auth = client
                .authenticate("oauth-personal")
                .await
                .unwrap_or(Value::Null);
            let mut tier = projection::extract_tier(&auth);
            // 直接创建轻量临时 ACP 会话获取完整模型/思考深度/模式目录及 available_commands_update，
            // 绝不对 session/list 中的任意桌面 App 历史会话调用 session/load（跨 app-data-dir 或外部占用会报 404 trajectory not found）。
            let setup = client.session_new(&effective_cwd, None).await?;
            temp_session_id = setup["sessionId"]
                .as_str()
                .filter(|s| local_store::valid_native_id(s))
                .map(str::to_owned);
            if tier.is_none() {
                tier = projection::extract_tier(&setup);
            }
            // 在关闭临时会话前等待并接收 available_commands_update（最多 1200ms，收到即提前结束）。
            let mut commands = Vec::new();
            let deadline = tokio::time::Instant::now() + Duration::from_millis(1200);
            while let Ok(Some(frame)) =
                tokio::time::timeout_at(deadline, channels.updates.recv()).await
            {
                if frame["method"] == "session/update" {
                    let update = &frame["params"]["update"];
                    if update["sessionUpdate"] == "available_commands_update"
                        && let Some(list) = update["availableCommands"].as_array()
                    {
                        commands = projection::normalize_available_commands(list);
                        break;
                    }
                }
            }
            if let Some(sid) = temp_session_id.as_deref() {
                let _ = client.session_close(sid).await;
            }
            let mut models = projection::normalize_models_from_setup(&setup);
            if models["data"].as_array().is_none_or(Vec::is_empty)
                && let Some(fallback) = usage::probe_models_cli(config).await
            {
                models = fallback;
            }
            Ok::<_, anyhow::Error>(CachedCatalog {
                at: Instant::now(),
                models,
                commands,
                tier,
                agent_info: init["agentInfo"].clone(),
            })
        }
        .await;
        client.close().await;
        // 清理本次目录探测产生的空临时会话轨迹，不在用户历史中留下空白记录。
        if let Some(sid) = temp_session_id
            && let Some(gemini_root) = self.resolve_gemini_root(config).await
        {
            tokio::task::spawn_blocking(move || {
                let cli_root = gemini_root.join("antigravity-cli");
                for ext in ["db", "pb"] {
                    let p = cli_root.join("conversations").join(format!("{sid}.{ext}"));
                    if std::fs::symlink_metadata(&p).is_ok_and(|m| !m.file_type().is_symlink() && m.is_file()) {
                        let _ = std::fs::remove_file(&p);
                    }
                }
                let brain = cli_root.join("brain").join(&sid);
                if std::fs::symlink_metadata(&brain).is_ok_and(|m| !m.file_type().is_symlink() && m.is_dir()) {
                    let _ = std::fs::remove_dir_all(&brain);
                }
            })
            .await
            .ok();
        }
        let catalog = outcome?;
        self.catalogs
            .lock()
            .await
            .insert(inst_id, catalog.clone());
        Ok(catalog)
    }

    pub(crate) async fn models_in(
        self: &Arc<Self>,
        config: EnvConfig,
        cwd: &str,
        force: bool,
    ) -> anyhow::Result<Value> {
        let catalog = self.probe_catalog(&config, cwd, force).await?;
        let inst_id = config.instance_id();
        Ok(super::instance_settings::decorate_antigravity_model_visibility(
            &self.directory,
            &inst_id,
            catalog.models,
        )?)
    }

    pub(crate) async fn session_models(
        self: &Arc<Self>,
        id: &str,
        _include_available: bool,
        force: bool,
    ) -> anyhow::Result<Value> {
        let c = self.conversation(id).await?;
        self.models_in(c.config, &c.cwd, force).await
    }

    pub(crate) async fn skills_in(
        self: &Arc<Self>,
        config: EnvConfig,
        cwd: &str,
        force: bool,
    ) -> anyhow::Result<Value> {
        let catalog = self.probe_catalog(&config, cwd, force).await?;
        let inst_id = config.instance_id();
        let gemini_root = self.resolve_gemini_root(&config).await;
        let workspace_path = (!cwd.is_empty()).then(|| PathBuf::from(cwd));
        let local_skills = if let Some(root) = gemini_root {
            let user_home = root.parent().map(Path::to_path_buf);
            let ws = workspace_path.clone();
            tokio::task::spawn_blocking(move || {
                local_store::scan_slash_skills(&root, user_home.as_deref(), ws.as_deref())
            })
            .await
            .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut seen = std::collections::BTreeSet::new();
        let mut merged = Vec::new();
        for item in catalog.commands.into_iter().chain(local_skills) {
            if let Some(name) = item["name"].as_str() {
                let key = name.to_ascii_lowercase();
                if seen.insert(key) {
                    merged.push(item);
                }
            }
        }
        Ok(json!({
            "ok": true,
            "backend": "antigravity",
            "instanceId": inst_id,
            "cwd": cwd,
            "globalPaths": [],
            "data": merged,
            "nativeCapabilities": { "goal": true, "compact": false },
        }))
    }

    pub(crate) async fn session_skills(
        self: &Arc<Self>,
        id: &str,
        force: bool,
    ) -> anyhow::Result<Value> {
        let c = self.conversation(id).await?;
        self.skills_in(c.config, &c.cwd, force).await
    }

    /// 严格通过 ACP 获取 Antigravity 实例的官方订阅等级、CLI 版本、认证状态与 ACP 会话 Token 统计。
    ///
    /// 重要约束：官方 `agy acp` 协议不提供 5h/7d 剩余额度百分比、重置时间及 Google 账号邮箱，
    /// 因此本接口如实返回 `quotaPercentSupported: false` 与 `accountEmailSupported: false`，绝不绕过 CLI 窃取凭据。
    pub(crate) async fn account_usage(
        self: &Arc<Self>,
        config: EnvConfig,
        allow_probe: bool,
    ) -> anyhow::Result<Value> {
        config.validate()?;
        let inst_id = config.instance_id();
        let inst_label =
            super::instances::label("antigravity", &config.distribution, &config.user);

        // 聚合该实例所有会话通过 ACP usage_update / PromptResponse 采集的真实 Token 统计。
        let token_stats = {
            let saved = self.saved.lock().await;
            let mut per_session = BTreeMap::new();
            for c in saved
                .conversations
                .values()
                .filter(|c| !c.deleted && c.config.instance_id() == inst_id)
            {
                let metrics =
                    projection::compute_session_metrics(&c.turns, &c.token_usage, &c.model, now_ms());
                per_session.insert(c.id.clone(), metrics);
            }
            projection::aggregate_instance_acp_tokens(&per_session)
        };

        if let Some(cached) = self.usage_cache.lock().await.get(&inst_id).cloned()
            && cached.at.elapsed() < USAGE_TTL
        {
            let mut response = cached.response;
            response["usage"]["acpSessionTokens"] = token_stats;
            return Ok(response);
        }

        if !allow_probe {
            return Ok(json!({
                "ok": true,
                "backend": "antigravity",
                "instanceId": inst_id,
                "usage": Value::Null,
            }));
        }

        let probe_result = self.probe_catalog(&config, "", false).await;
        let now = now_ms();
        let response = match probe_result {
            Ok(catalog) => {
                let tier = catalog.tier.clone();
                let tier_name = tier
                    .as_ref()
                    .and_then(|t| t["name"].as_str())
                    .unwrap_or("");
                let tier_id = tier.as_ref().and_then(|t| t["id"].as_str()).unwrap_or("");
                let agent_version = catalog.agent_info["version"].as_str().unwrap_or("");
                json!({
                    "ok": true,
                    "backend": "antigravity",
                    "instanceId": inst_id,
                    "instanceLabel": inst_label,
                    "usage": {
                        "source": "antigravity-acp",
                        "observedAtMs": now,
                        "authenticated": true,
                        "authRequired": false,
                        "instanceLabel": inst_label,
                        "tierName": if !tier_name.is_empty() { json!(tier_name) } else { Value::Null },
                        "tierId": if !tier_id.is_empty() { json!(tier_id) } else { Value::Null },
                        "subscriptionTier": tier,
                        "subscriptionType": if !tier_name.is_empty() { json!(tier_name) } else { Value::Null },
                        "subscriptionTierId": if !tier_id.is_empty() { json!(tier_id) } else { Value::Null },
                        "agentVersion": if !agent_version.is_empty() { json!(agent_version) } else { Value::Null },
                        "agentInfo": catalog.agent_info,
                        "sessionTokens": {
                            "sessionCount": token_stats["sessions"],
                            "totalTokens": token_stats["totalTokens"],
                            "inputTokens": token_stats["inputTokens"],
                            "outputTokens": token_stats["outputTokens"],
                            "cachedInputTokens": token_stats["cachedInputTokens"],
                            "reasoningTokens": token_stats["reasoningTokens"],
                        },
                        "acpSessionTokens": token_stats,
                        "quotaPercentSupported": false,
                        "accountEmailSupported": false,
                        "acpLimitationNote": "官方 Antigravity ACP 协议暂未开放 5 小时 / 7 天剩余额度百分比、重置时间与账号邮箱接口，此处仅展示 ACP 官方返回的订阅等级与会话 Token 用量。",
                    }
                })
            }
            Err(failure) => {
                let message = failure.to_string();
                let auth_required = failure
                    .downcast_ref::<ApiError>()
                    .is_some_and(|e| e.code == 401);
                json!({
                    "ok": true,
                    "backend": "antigravity",
                    "instanceId": inst_id,
                    "instanceLabel": inst_label,
                    "usage": {
                        "source": "antigravity-acp",
                        "observedAtMs": now,
                        "authenticated": false,
                        "authRequired": auth_required,
                        "authMessage": message,
                        "instanceLabel": inst_label,
                        "error": message,
                        "sessionTokens": {
                            "sessionCount": token_stats["sessions"],
                            "totalTokens": token_stats["totalTokens"],
                            "inputTokens": token_stats["inputTokens"],
                            "outputTokens": token_stats["outputTokens"],
                            "cachedInputTokens": token_stats["cachedInputTokens"],
                            "reasoningTokens": token_stats["reasoningTokens"],
                        },
                        "acpSessionTokens": token_stats,
                        "quotaPercentSupported": false,
                        "accountEmailSupported": false,
                        "acpLimitationNote": "官方 Antigravity ACP 协议暂未开放 5 小时 / 7 天剩余额度百分比、重置时间与账号邮箱接口。",
                    }
                })
            }
        };
        self.usage_cache.lock().await.insert(
            inst_id,
            CachedUsage {
                at: Instant::now(),
                response: response.clone(),
            },
        );
        Ok(response)
    }

    pub(crate) async fn session_account_usage(
        self: &Arc<Self>,
        id: &str,
        allow_probe: bool,
    ) -> anyhow::Result<Value> {
        let c = self.conversation(id).await?;
        self.account_usage(c.config, allow_probe).await
    }

    pub(crate) async fn session_metrics(&self, id: &str) -> anyhow::Result<Value> {
        self.sync_local_transcript_if_available(id).await;
        let c = self.conversation(id).await?;
        Ok(projection::compute_session_metrics(
            &c.turns,
            &c.token_usage,
            &c.model,
            now_ms(),
        ))
    }

    pub(crate) async fn workspace(&self, id: &str) -> anyhow::Result<Value> {
        let c = self.conversation(id).await?;
        Ok(json!({
            "ok": true,
            "threadId": c.id,
            "hostId": "local",
            "backend": "antigravity",
            "instanceId": c.config.instance_id(),
            "cwd": c.cwd,
            "linuxCwd": c.linux_cwd,
            "wslDistribution": if c.config.is_local() { Value::Null } else { json!(c.config.distribution) },
            "wslUser": if c.config.is_local() { Value::Null } else { json!(c.config.user) },
            "projectId": c.project_id,
        }))
    }

    pub(crate) async fn related_threads(&self, id: &str) -> anyhow::Result<Value> {
        self.conversation(id).await?;
        Ok(json!({
            "ok": true,
            "threadId": id,
            "sideThreads": [],
            "subagents": [],
        }))
    }

    pub(crate) async fn queue(&self, id: &str) -> anyhow::Result<Value> {
        let c = self.conversation(id).await?;
        Ok(c.remote_queue.snapshot(id, &self.epoch))
    }

    pub(crate) async fn queue_edit_state(&self, payload: &Value) -> anyhow::Result<Value> {
        let id = payload["threadId"].as_str().context("缺少 Antigravity 会话 ID")?;
        let edit_id = payload["editId"].as_str().context("缺少编辑 ID")?;
        let c = self.conversation(id).await?;
        if let Some(edit) = c.remote_queue.edits.get(edit_id) {
            return Ok(json!({
                "ok": true,
                "threadId": id,
                "editId": edit_id,
                "state": edit.state,
                "message": edit.original.view(c.remote_queue.paused),
                "result": edit.result,
            }));
        }
        Ok(json!({ "ok": true, "threadId": id, "editId": edit_id, "state": "missing" }))
    }

    pub(crate) async fn queue_send_state(&self, payload: &Value) -> anyhow::Result<Value> {
        let id = payload["threadId"].as_str().context("缺少 Antigravity 会话 ID")?;
        let client_id = payload["clientUserMessageId"].as_str().unwrap_or("");
        let c = self.conversation(id).await?;
        let found = c.turns.iter().any(|turn| {
            turn["id"] == client_id
                || turn["items"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|i| i["clientId"] == client_id))
        });
        let queued = c.remote_queue.messages.iter().any(|m| m.id == client_id);
        Ok(json!({
            "ok": true,
            "threadId": id,
            "clientUserMessageId": client_id,
            "state": if found { "delivered" } else if queued { "queued" } else { "unknown" },
        }))
    }

    /// 定位目标 Antigravity 实例的 `~/.gemini` 本地配置与转录根目录（本机或 WSL UNC）。
    async fn resolve_gemini_root(&self, config: &EnvConfig) -> Option<PathBuf> {
        if config.is_local() {
            return directories::BaseDirs::new().map(|d| d.home_dir().join(".gemini"));
        }
        let envs = super::preferences::detect(&self.directory, false).await;
        if let Some(env) = envs.iter().find(|e| {
            e.error.is_none()
                && e.distribution.eq_ignore_ascii_case(&config.distribution)
                && (config.user.is_empty() || e.user == config.user)
        }) {
            let home = env.antigravity_home();
            if !home.is_empty()
                && let Ok(path) = super::preferences::wsl_path(&config.distribution, &home)
            {
                return Some(path);
            }
        }
        let fallback_home = if config.user == "root" {
            "/root/.gemini".to_owned()
        } else if !config.user.is_empty() {
            format!("/home/{}/.gemini", config.user)
        } else {
            return None;
        };
        super::preferences::wsl_path(&config.distribution, &fallback_home).ok()
    }

    /// 优先直接读取本地 Antigravity JSONL 转录文件（毫秒级完成），避免打开历史会话时阻塞在 `agy acp session/load` 冷启动上。
    async fn sync_local_transcript_if_available(&self, id: &str) {
        let Ok(c) = self.conversation(id).await else {
            return;
        };
        if c.active() || !local_store::valid_native_id(&c.native_id) {
            return;
        }
        let Some(gemini_root) = self.resolve_gemini_root(&c.config).await else {
            return;
        };
        let native_id = c.native_id.clone();
        let probe_root = gemini_root.clone();
        let probe_id = native_id.clone();
        let found = tokio::task::spawn_blocking(move || {
            local_store::find_local_transcript_file(&probe_root, &probe_id)
        })
        .await
        .ok()
        .flatten();
        let Some((_path, mtime_ms, file_len)) = found else {
            return;
        };
        let needs_read = !c.history_loaded
            || c.turns.is_empty()
            || mtime_ms != c.local_transcript_mtime_ms
            || file_len != c.local_transcript_len;
        if !needs_read {
            return;
        }
        let parsed = tokio::task::spawn_blocking(move || {
            local_store::read_local_transcript(&gemini_root, &native_id)
        })
        .await
        .ok()
        .flatten();
        let Some(parsed) = parsed else {
            return;
        };
        let mut saved = self.saved.lock().await;
        if let Some(conv) = saved.conversations.get_mut(id)
            && !conv.deleted
            && !conv.active()
        {
            if conv.discovered || conv.turns.is_empty() {
                conv.turns = parsed.turns;
                conv.edit_history_head = format!("agy-head-{}", conv.turns.len());
            } else {
                // Elves 自身创建的会话在轮次完成后同步本地转录记录的多步规划步数与真实输入/缓存/输出明细，
                // 不改写已确认的用户消息、图片或正式轮次 ID。
                sync_turn_usage_from_transcript(&mut conv.turns, &parsed.turns);
            }
            conv.history_loaded = true;
            conv.local_transcript_mtime_ms = parsed.mtime_ms;
            conv.local_transcript_len = parsed.file_len;
            if conv.model.is_empty() && !parsed.model.is_empty() {
                conv.model = parsed.model;
            }
            if conv.effort.is_empty() && !parsed.effort.is_empty() {
                conv.effort = parsed.effort;
            }
            if !parsed.token_usage.is_null() {
                conv.token_usage = projection::merge_token_usage(
                    &conv.token_usage,
                    &parsed.token_usage,
                    &conv.model,
                );
            }
            let _ = self.persist_locked(&saved, Some(id)).await;
        }
    }

    /// 本地无可用转录时，在后台异步触发一次 ACP `session/load` 补齐历史与元数据，绝不阻塞 `detail`/`history` 响应。
    async fn spawn_background_acp_sync(self: &Arc<Self>, id: &str) {
        {
            let mut syncing = self.syncing_discovered.lock().await;
            if !syncing.insert(id.to_owned()) {
                return;
            }
        }
        let service = self.clone();
        let tid = id.to_owned();
        tokio::spawn(async move {
            let changed = service.sync_discovered_if_needed(&tid).await.unwrap_or(false);
            service.syncing_discovered.lock().await.remove(&tid);
            if changed {
                service.publish(json!({
                    "method": "remote/resync",
                    "params": { "threadId": tid, "reason": "antigravity_discovered_synced" }
                }));
            }
        });
    }

    /// 只读检查指定 Antigravity 会话是否正被其他应用（Antigravity App / IDE / 终端 `agy` CLI / 其他 ACP 进程）占用。
    /// 本服务自己持有的活跃 ACP 连接不算外部占用。
    async fn is_externally_occupied(&self, c: &Conversation) -> bool {
        if self
            .runtimes
            .lock()
            .await
            .get(&c.id)
            .is_some_and(|rt| rt.client.is_live())
        {
            return false;
        }
        if !local_store::valid_native_id(&c.native_id) {
            return false;
        }
        if c.config.is_local() {
            let Some(gemini_root) = self.resolve_gemini_root(&c.config).await else {
                return false;
            };
            let native_id = c.native_id.clone();
            tokio::task::spawn_blocking(move || {
                local_store::is_local_session_occupied(&gemini_root, &native_id)
            })
            .await
            .unwrap_or(false)
        } else {
            local_store::is_wsl_session_occupied(
                &c.config.distribution,
                &c.config.user,
                &c.native_id,
            )
            .await
        }
    }

    /// 写操作或启动 ACP 前的强制占用保护：只要检测到外部 Antigravity App / CLI 占用，立即拒绝写入。
    async fn ensure_not_externally_occupied(&self, c: &Conversation) -> anyhow::Result<()> {
        if self.is_externally_occupied(c).await {
            return Err(error(
                409,
                "此 Antigravity 会话已在另一个应用（Antigravity App 或 agy CLI）中打开，请先在那边关闭会话后再继续。",
            ));
        }
        Ok(())
    }

    /// 检测已有原生会话实际归属的 Antigravity 数据目录（`antigravity` / `antigravity-cli` / `antigravity-ide`），
    /// 供 `agy acp --app-data-dir` 定位轨迹库，避免桌面 App 会话续聊时触发 `trajectory not found`。
    async fn resolve_session_app_data_dir(&self, c: &Conversation) -> Option<&'static str> {
        if !local_store::valid_native_id(&c.native_id) {
            return None;
        }
        let gemini_root = self.resolve_gemini_root(&c.config).await?;
        let native_id = c.native_id.clone();
        tokio::task::spawn_blocking(move || {
            local_store::detect_session_app_data_dir(&gemini_root, &native_id)
        })
        .await
        .ok()
        .flatten()
    }

    /// 后台同步从 `session/list` 发现且本地尚无转录文件的外部会话：经 ACP `session/load` 补齐元数据和回放历史。
    async fn sync_discovered_if_needed(self: &Arc<Self>, id: &str) -> anyhow::Result<bool> {
        let c = self.conversation(id).await?;
        if !c.discovered || c.history_loaded || self.is_externally_occupied(&c).await {
            return Ok(false);
        }
        let lock = self.lock(id).await;
        let _guard = lock.lock().await;
        let c = self.conversation(id).await?;
        if !c.discovered || c.history_loaded || self.is_externally_occupied(&c).await {
            return Ok(false);
        }
        let effective_cwd = c.effective_cwd().to_owned();
        let app_data_dir = self.resolve_session_app_data_dir(&c).await;
        let (client, mut channels) =
            AcpClient::spawn(&c.config, &c.cwd, &c.linux_cwd, app_data_dir, false).await?;
        let mut updates = Vec::new();
        let outcome = async {
            let _init = client.initialize().await?;
            let _auth = client.authenticate("oauth-personal").await?;
            let loaded = client.session_load(&c.native_id, &effective_cwd).await?;
            let deadline = tokio::time::Instant::now() + Duration::from_millis(600);
            while let Ok(Some(frame)) =
                tokio::time::timeout_at(deadline, channels.updates.recv()).await
            {
                if frame["method"] == "session/update" {
                    updates.push(frame["params"]["update"].clone());
                }
            }
            Ok::<_, anyhow::Error>(loaded)
        }
        .await;
        client.close().await;
        let loaded = match outcome {
            Ok(loaded) => loaded,
            Err(_) => {
                let mut saved = self.saved.lock().await;
                if let Some(conv) = saved.conversations.get_mut(id) {
                    conv.history_loaded = true;
                }
                return Ok(false);
            }
        };
        let replay_turns = projection::collect_replay_turns(&updates, now_ms());
        let models_view = projection::normalize_models_from_setup(&loaded);
        let mut updated_any = false;
        let mut saved = self.saved.lock().await;
        if let Some(conv) = saved.conversations.get_mut(id) {
            conv.history_loaded = true;
            if conv.model.is_empty()
                && let Some(m) = models_view["defaultModel"].as_str().filter(|s| !s.is_empty())
            {
                conv.model = m.to_owned();
                updated_any = true;
            }
            if conv.effort.is_empty()
                && let Some(e) = models_view["defaultReasoningEffort"]
                    .as_str()
                    .filter(|s| !s.is_empty())
            {
                conv.effort = e.to_owned();
                updated_any = true;
            }
            if let Some(modes) = models_view["permissionModes"].as_array() {
                conv.available_modes = modes
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
            }
            if conv.turns.is_empty() && !replay_turns.is_empty() {
                conv.turns = replay_turns;
                conv.edit_history_head = format!("agy-head-{}", conv.turns.len());
                updated_any = true;
            }
            let _ = self.persist_locked(&saved, Some(id)).await;
        }
        Ok(updated_any)
    }

    pub(crate) async fn detail_authorized(
        self: &Arc<Self>,
        id: &str,
        _authorization: Option<&HarnessAuthorization>,
    ) -> anyhow::Result<Value> {
        self.sync_local_transcript_if_available(id).await;
        let c = self.conversation(id).await?;
        // 用户要求检测 agy CLI / Antigravity App 占用：若被外部占用，参考 Codex/Claude 原位提示“已在另一个应用中打开”并只读保护。
        let occupied = self.is_externally_occupied(&c).await;
        if c.discovered && !c.history_loaded && !occupied {
            self.spawn_background_acp_sync(id).await;
        }
        let requests = self.requests.lock().await;
        let req_list: Vec<Value> = requests
            .get(id)
            .map(|map| map.values().map(|p| p.mapped.elves_request.clone()).collect())
            .unwrap_or_default();
        let has_waiting = !req_list.is_empty();
        let mut thread = c.view(has_waiting, &self.epoch);
        thread["requests"] = json!(req_list);
        thread["requestsKnown"] = json!(true);
        thread["requestsEpoch"] = json!(self.epoch);
        thread["requestsRevision"] = json!(self.request_revision.load(Ordering::Acquire));
        thread["externalWriter"] = json!(occupied);
        if occupied {
            thread["readOnly"] = json!(true);
            thread["readOnlyReason"] = json!("请先在那边关闭会话，才能在这里继续。");
        } else {
            thread["readOnly"] = json!(false);
        }
        Ok(json!({
            "ok": true,
            "thread": thread,
            "nativeBridge": "antigravity-acp",
        }))
    }

    pub(crate) async fn history_authorized(
        self: &Arc<Self>,
        payload: &Value,
        _authorization: Option<&HarnessAuthorization>,
    ) -> anyhow::Result<Value> {
        let id = payload["threadId"]
            .as_str()
            .context("缺少 Antigravity 会话 ID")?;
        self.sync_local_transcript_if_available(id).await;
        let c = self.conversation(id).await?;
        if c.discovered && !c.history_loaded && !self.is_externally_occupied(&c).await {
            self.spawn_background_acp_sync(id).await;
        }
        let end = match &payload["cursor"] {
            Value::Null => c.turns.len(),
            Value::String(value) => value
                .parse::<usize>()
                .ok()
                .filter(|n| *n <= c.turns.len())
                .ok_or_else(|| error(400, "Antigravity 历史游标无效"))?,
            _ => return Err(error(400, "Antigravity 历史游标无效")),
        };
        let start = end.saturating_sub(10);
        let turns: Vec<Value> = c.turns[start..end]
            .iter()
            .rev()
            .map(|turn| c.displayed_turn(turn))
            .collect();
        Ok(json!({
            "ok": true,
            "threadId": id,
            "data": turns,
            "turnIds": c.turns.iter().map(|turn| &turn["id"]).collect::<Vec<_>>(),
            "nextCursor": if start == 0 { Value::Null } else { json!(start.to_string()) },
            "transcriptEvents": [],
            "backend": "antigravity",
            "sessionPlan": c.session_plan(),
        }))
    }

    /// 为自动命名服务提供 Antigravity 会话的当前标题与首条/近期对话文本，不启动 ACP 进程。
    pub(crate) async fn naming_context(&self, id: &str, initial: bool) -> anyhow::Result<Value> {
        let c = self.conversation(id).await?;
        let first = super::conversation_naming::first_user_message(&c.turns);
        let fallback = first
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(36)
            .collect::<String>();
        let eligible = c.title.is_empty()
            || c.title == "Antigravity 会话"
            || c.title == "新会话"
            || (!fallback.is_empty() && c.title == fallback);
        Ok(json!({
            "title": c.title,
            "expectedName": c.title,
            "eligible": eligible,
            "firstUserMessage": if initial { first } else { None },
            "messages": if initial {
                Vec::new()
            } else {
                super::conversation_naming::messages(&c.turns[c.turns.len().saturating_sub(8)..])
            }
        }))
    }

    /// 启动新的 `agy acp` 进程并完成初始化与建/恢会话，返回尚未挂载监听泵的通道，
    /// 供新建会话在写入正式会话 ID 后再绑定 `spawn_runtime_pumps`，避免事件误投到临时 ID。
    async fn start_runtime(
        self: &Arc<Self>,
        c: &Conversation,
        create_new: bool,
        system_prompt_append: Option<&str>,
    ) -> anyhow::Result<(Arc<ActiveRuntime>, Value, AcpChannels)> {
        let effective_cwd = c.effective_cwd().to_owned();
        let app_data_dir = if create_new || c.native_id.is_empty() {
            None
        } else {
            self.resolve_session_app_data_dir(c).await
        };
        let dangerously_skip_permissions = c.permission_mode.as_deref() == Some("bypassPermissions");
        let (client, channels) = AcpClient::spawn(
            &c.config,
            &c.cwd,
            &c.linux_cwd,
            app_data_dir,
            dangerously_skip_permissions,
        )
        .await?;
        let setup_res = async {
            let init = client.initialize().await?;
            let _auth = client.authenticate("oauth-personal").await?;
            let setup = if create_new || c.native_id.is_empty() {
                client
                    .session_new(&effective_cwd, system_prompt_append)
                    .await?
            } else {
                match client.session_resume(&c.native_id, &effective_cwd).await {
                    Ok(res) => res,
                    Err(_) => client.session_load(&c.native_id, &effective_cwd).await?,
                }
            };
            Ok::<_, anyhow::Error>((init, setup))
        }
        .await;
        let (init, setup) = match setup_res {
            Ok(pair) => pair,
            Err(err) => {
                client.close().await;
                return Err(err);
            }
        };

        let native_session_id = setup["sessionId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| c.native_id.clone());
        let supports_image = init["agentCapabilities"]["promptCapabilities"]["image"] == true;
        let supports_steering = projection::supports_prompt_steering(&init);
        let runtime = Arc::new(ActiveRuntime {
            client: client.clone(),
            native_session_id,
            initialize_result: init,
            supports_image,
            supports_steering,
            dangerously_skip_permissions,
        });
        Ok((runtime, setup, channels))
    }

    /// 确保目标会话已有活跃的 `agy acp` 进程，并绑定更新和审批监听任务。
    async fn ensure_runtime(
        self: &Arc<Self>,
        c: &Conversation,
        create_new: bool,
        system_prompt_append: Option<&str>,
    ) -> anyhow::Result<(Arc<ActiveRuntime>, Value)> {
        let want_skip = c.permission_mode.as_deref() == Some("bypassPermissions");
        if !create_new
            && let Some(existing) = self.runtimes.lock().await.get(&c.id).cloned()
            && existing.client.is_live()
        {
            if existing.dangerously_skip_permissions == want_skip || c.active() {
                return Ok((existing, Value::Null));
            }
            // 切入或切出“无限制模式”需调整官方 `--dangerously-skip-permissions` 启动参数：
            // 仅在会话空闲时优雅关闭旧连接并用 session/resume 接回，不中断正在执行的任务。
            if let Some(old_rt) = self.runtimes.lock().await.remove(&c.id) {
                let _ = old_rt.client.session_close(&old_rt.native_session_id).await;
                old_rt.client.close().await;
            }
        }
        if !create_new && !c.native_id.is_empty() {
            self.ensure_not_externally_occupied(c).await?;
        }
        let (runtime, setup, channels) = self
            .start_runtime(c, create_new, system_prompt_append)
            .await?;
        self.runtimes
            .lock()
            .await
            .insert(c.id.clone(), runtime.clone());
        self.spawn_runtime_pumps(c.id.clone(), runtime.clone(), channels);
        Ok((runtime, setup))
    }

    fn spawn_runtime_pumps(
        self: &Arc<Self>,
        thread_id: String,
        runtime: Arc<ActiveRuntime>,
        mut channels: AcpChannels,
    ) {
        let weak_updates = Arc::downgrade(self);
        let tid_updates = thread_id.clone();
        tokio::spawn(async move {
            while let Some(frame) = channels.updates.recv().await {
                let Some(service) = weak_updates.upgrade() else {
                    break;
                };
                if frame["method"] == "session/update" {
                    let update = &frame["params"]["update"];
                    service.handle_session_update(&tid_updates, update).await;
                }
            }
        });

        let weak_reqs = Arc::downgrade(self);
        let tid_reqs = thread_id;
        tokio::spawn(async move {
            while let Some(req) = channels.requests.recv().await {
                let Some(service) = weak_reqs.upgrade() else {
                    break;
                };
                service
                    .handle_incoming_request(&tid_reqs, &runtime, req)
                    .await;
            }
        });
    }

    async fn handle_session_update(&self, thread_id: &str, update: &Value) {
        let now = now_ms();
        let mut events = Vec::new();
        let mut commands_to_cache: Option<(String, Vec<Value>)> = None;
        {
            let mut saved = self.saved.lock().await;
            let Some(c) = saved.conversations.get_mut(thread_id).filter(|c| !c.deleted) else {
                return;
            };
            let turn_idx = c
                .active_turn_id
                .as_deref()
                .and_then(|aid| c.turns.iter().position(|t| t["id"] == aid))
                .or_else(|| c.turns.len().checked_sub(1));
            let Some(idx) = turn_idx else {
                return;
            };
            let turn_id = c.turns[idx]["id"].clone();
            let effect = projection::apply_session_update(&mut c.turns[idx], update, now);
            c.updated_at = now;
            if let Some(title) = effect.title_updated
                && (c.title.is_empty() || c.title == "Antigravity 会话" || c.title == "新会话")
            {
                c.title = title.clone();
                events.push(json!({
                    "method": "thread/name/updated",
                    "params": { "threadId": thread_id, "name": title, "title": title }
                }));
            }
            if let Some(mode) = effect.mode_updated {
                c.permission_mode = Some(mode);
            }
            if let Some(cmds) = effect.available_commands {
                commands_to_cache = Some((c.config.instance_id(), cmds));
            }
            if let Some(usage) = effect.token_usage {
                let merged = projection::merge_token_usage(&c.token_usage, &usage, &c.model);
                c.token_usage = merged.clone();
                // 仅当通知携带真实输入/输出 Token 明细时更新 `_acpUsage`；纯上下文窗口通知（`{used, size}`）不得将轮次 Token 置零。
                if let Some(last) = usage.get("last") {
                    let in_tok = last["inputTokens"].as_u64().unwrap_or(0);
                    let out_tok = last["outputTokens"].as_u64().unwrap_or(0);
                    if in_tok > 0 || out_tok > 0 {
                        let cached_tok = last["cachedInputTokens"].as_u64().unwrap_or(0);
                        let thought_tok = last["reasoningOutputTokens"].as_u64().unwrap_or(0);
                        c.turns[idx]["_acpUsage"] = json!({
                            "inputTokens": in_tok,
                            "outputTokens": out_tok,
                            "cachedReadTokens": cached_tok,
                            "thoughtTokens": thought_tok,
                            "lastObservedAtMs": now,
                            "model": c.model,
                        });
                    }
                }
                events.push(json!({
                    "method": "thread/tokenUsage/updated",
                    "params": { "threadId": thread_id, "tokenUsage": merged }
                }));
            }
            for item in effect.changed_items {
                let mut clean = item;
                if let Some(obj) = clean.as_object_mut() {
                    obj.remove("_acpKind");
                    obj.remove("_acpTitle");
                    obj.remove("_acpRawInput");
                    obj.remove("_acpRawOutput");
                }
                events.push(json!({
                    "method": "item/updated",
                    "params": { "threadId": thread_id, "turnId": turn_id, "item": clean }
                }));
            }
            if let Some(plan) = effect.plan_changed {
                events.push(json!({
                    "method": "turn/plan/updated",
                    "params": { "threadId": thread_id, "turnId": turn_id, "plan": plan }
                }));
            }
        }
        if let Some((inst_id, cmds)) = commands_to_cache
            && !cmds.is_empty()
            && let Some(cached) = self.catalogs.lock().await.get_mut(&inst_id)
        {
            cached.commands = cmds;
        }
        for event in events {
            self.publish(event);
        }
    }

    async fn handle_incoming_request(
        &self,
        thread_id: &str,
        runtime: &Arc<ActiveRuntime>,
        req: AcpIncomingRequest,
    ) {
        if req.method != "session/request_permission" {
            let _ = runtime
                .client
                .respond_error(
                    req.id,
                    -32601,
                    &format!("Elves ACP client does not support {}", req.method),
                )
                .await;
            return;
        }
        let request_id = format!("agy-req-{}", uuid::Uuid::new_v4());
        let mapped = projection::map_permission_request(thread_id, &request_id, &req.params);
        self.requests
            .lock()
            .await
            .entry(thread_id.to_owned())
            .or_default()
            .insert(
                request_id,
                PendingPermission {
                    rpc_id: req.id,
                    mapped,
                },
            );
        self.requests_changed(thread_id).await;
    }

    async fn requests_changed(&self, thread_id: &str) {
        let rev = self.request_revision.fetch_add(1, Ordering::AcqRel) + 1;
        let list: Vec<Value> = self
            .requests
            .lock()
            .await
            .get(thread_id)
            .map(|map| map.values().map(|p| p.mapped.elves_request.clone()).collect())
            .unwrap_or_default();
        self.publish(json!({
            "method": "remote/requestsChanged",
            "params": {
                "threadId": thread_id,
                "requests": list,
                "requestsEpoch": self.epoch,
                "requestsRevision": rev,
            }
        }));
    }

    async fn queue_changed(&self, thread_id: &str) {
        if let Ok(snapshot) = self.queue(thread_id).await {
            self.publish(json!({
                "method": "thread/queue/changed",
                "params": snapshot,
            }));
        }
    }

    async fn apply_acp_options(
        runtime: &Arc<ActiveRuntime>,
        c: &mut Conversation,
        payload: &Value,
    ) -> anyhow::Result<()> {
        if let Some(model) = payload["model"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            && model != c.model
        {
            let _ = runtime
                .client
                .session_set_config_option(&runtime.native_session_id, "model", model)
                .await?;
            c.model = model.to_owned();
        }
        if let Some(effort) = payload["reasoningEffort"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            && effort != c.effort
        {
            // 部分模型无 thought_level 选项时忽略错误，保留主流程可用。
            if runtime
                .client
                .session_set_config_option(&runtime.native_session_id, "thought_level", effort)
                .await
                .is_ok()
            {
                c.effort = effort.to_owned();
            }
        }
        if let Some(mode) = payload["permissionMode"]
            .as_str()
            .map(str::trim)
            .filter(|s| matches!(*s, "default" | "accept-edits" | "auto" | "bypassPermissions"))
        {
            // 无限制模式由启动参数 `--dangerously-skip-permissions` 控制，不调用 session/set_mode 发送非原生值。
            if mode != "bypassPermissions"
                && runtime
                    .client
                    .session_set_mode(&runtime.native_session_id, mode)
                    .await
                    .is_err()
            {
                let _ = runtime
                    .client
                    .session_set_config_option(&runtime.native_session_id, "mode", mode)
                    .await;
            }
            c.permission_mode = Some(mode.to_owned());
            c.permission_revision += 1;
        }
        Ok(())
    }

    /// 创建新 Antigravity 会话并发送首条消息（或首条目标）。
    pub(crate) async fn create_in(
        self: &Arc<Self>,
        config: EnvConfig,
        payload: &Value,
        cwd: &str,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        let _lifecycle = self.lifecycle_gate.read().await;
        anyhow::ensure!(self.is_accepting(), "远程服务正在关闭，Antigravity 操作尚未派发");
        config.validate()?;
        authorization.check().await?;

        let goal_mode = payload["goalMode"] == true;
        let resolved_cwd = if cwd.is_empty() {
            if config.is_local() {
                let dir = self.directory.join("antigravity-workspaces");
                std::fs::create_dir_all(&dir)?;
                dir.to_string_lossy().into_owned()
            } else {
                String::new()
            }
        } else {
            cwd.to_owned()
        };
        let linux_cwd = Self::linux_cwd(&config, &resolved_cwd).await?;
        let now = now_ms();
        let prompt_preview = payload["prompt"]
            .as_str()
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(36)
            .collect::<String>();
        let inst_id = config.instance_id();
        let mut effective_payload = payload.clone();
        if effective_payload["permissionMode"]
            .as_str()
            .is_none_or(|s| s.trim().is_empty())
            && let Ok(default_mode) =
                super::instance_settings::antigravity_default_permission_mode(&self.directory, &inst_id)
            && !default_mode.is_empty()
            && default_mode != "default"
        {
            effective_payload["permissionMode"] = json!(default_mode);
        }
        if let Ok((pref_model, pref_effort)) =
            super::instance_settings::antigravity_default_model_and_effort(&self.directory, &inst_id)
        {
            if effective_payload["model"]
                .as_str()
                .is_none_or(|s| s.trim().is_empty())
                && let Some(model) = pref_model
            {
                effective_payload["model"] = json!(model);
            }
            if effective_payload["reasoningEffort"]
                .as_str()
                .is_none_or(|s| s.trim().is_empty())
                && let Some(effort) = pref_effort
            {
                effective_payload["reasoningEffort"] = json!(effort);
            }
        }
        let temp_conv = Conversation {
            backend: antigravity_backend(),
            id: format!("antigravity-pending-{}", uuid::Uuid::new_v4()),
            native_id: String::new(),
            project_id: payload["projectId"].clone(),
            cwd: resolved_cwd.clone(),
            linux_cwd: linux_cwd.clone(),
            config: config.clone(),
            title: if prompt_preview.is_empty() {
                "Antigravity 会话".into()
            } else {
                prompt_preview
            },
            model: String::new(),
            effort: String::new(),
            permission_mode: effective_payload["permissionMode"].as_str().map(str::to_owned),
            permission_revision: u64::from(effective_payload["permissionMode"].is_string()),
            available_modes: vec![
                "default".into(),
                "accept-edits".into(),
                "auto".into(),
                "plan".into(),
            ],
            created_at: now,
            updated_at: now,
            pinned: false,
            unread: false,
            deleted: false,
            discovered: false,
            history_loaded: true,
            local_transcript_mtime_ms: 0,
            local_transcript_len: 0,
            edit_history_head: "agy-head-0".into(),
            token_usage: Value::Null,
            goal: Value::Null,
            remote_queue: AntigravityQueue::default(),
            active_turn_id: None,
            turns: Vec::new(),
        };

        let (runtime, setup, channels) = self.start_runtime(&temp_conv, true, None).await?;
        let native_id = runtime.native_session_id.clone();
        let id = format!("antigravity-{native_id}");

        let models_view = projection::normalize_models_from_setup(&setup);
        let mut c = temp_conv;
        c.id = id.clone();
        c.native_id = native_id;
        c.model = models_view["defaultModel"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        c.effort = models_view["defaultReasoningEffort"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        if let Some(modes) = models_view["permissionModes"].as_array() {
            c.available_modes = modes
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
        }
        if let Err(err) = Self::apply_acp_options(&runtime, &mut c, &effective_payload).await {
            runtime.client.close().await;
            return Err(err);
        }

        let mut dispatch_payload = payload.clone();
        if goal_mode {
            let Some(objective) = payload["prompt"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
            else {
                runtime.client.close().await;
                return Err(error(400, "目标内容不能为空"));
            };
            c.goal = json!({
                "objective": objective,
                "status": "active",
                "createdAt": now,
                "updatedAt": now,
                "iterations": 1,
                "source": "antigravity",
            });
            dispatch_payload["prompt"] = json!(format_goal_initial_prompt(&objective));
        }

        {
            let mut saved = self.saved.lock().await;
            saved.conversations.insert(id.clone(), c.clone());
            if let Err(err) = self.persist_locked(&saved, Some(&id)).await {
                saved.conversations.remove(&id);
                drop(saved);
                runtime.client.close().await;
                return Err(err);
            }
        }
        // 必须在正式会话写入 saved 后，再以正式会话 ID 注册 runtime 并启动监听泵，
        // 避免 session/update（思考、工具、回复）与 session/request_permission 误投到临时占位 ID 而丢失。
        self.runtimes
            .lock()
            .await
            .insert(id.clone(), runtime.clone());
        self.spawn_runtime_pumps(id.clone(), runtime.clone(), channels);

        let thread_view = c.view(false, &self.epoch);
        self.publish(json!({
            "method": "thread/started",
            "params": { "threadId": id, "thread": thread_view },
        }));

        let mut started = self.dispatch_turn(&id, &runtime, &dispatch_payload).await?;
        started["conversationId"] = json!(id);
        started["threadId"] = json!(id);
        if goal_mode {
            started["goal"] = c.goal.clone();
            started["goalSubmission"] = json!({
                "threadId": id,
                "phase": "complete",
                "firstTurnAccepted": true,
                "goalSet": "succeeded",
            });
        }
        Ok(started)
    }

    async fn dispatch_turn(
        self: &Arc<Self>,
        thread_id: &str,
        runtime: &Arc<ActiveRuntime>,
        payload: &Value,
    ) -> anyhow::Result<Value> {
        let blocks = projection::build_prompt_blocks(payload, runtime.supports_image)?;
        let now = now_ms();
        let client_msg_id = payload["clientUserMessageId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let turn_id = format!("agy-turn-{}", uuid::Uuid::new_v4());

        let mut user_content = Vec::new();
        if let Some(text) = payload["prompt"].as_str().filter(|s| !s.trim().is_empty()) {
            user_content.push(json!({ "type": "text", "text": text }));
        }
        for image in payload["images"].as_array().into_iter().flatten() {
            if let Some(url) = image["dataUrl"].as_str() {
                user_content.push(json!({ "type": "image", "url": url }));
            }
        }
        let user_item = json!({
            "id": format!("{turn_id}-user"),
            "clientId": client_msg_id,
            "type": "userMessage",
            "content": user_content,
            "sentAtMs": now,
            "messageMetadata": {
                "turnId": turn_id,
                "messageId": format!("{turn_id}-user"),
                "editable": true,
            }
        });
        let turn = json!({
            "id": turn_id,
            "status": "inProgress",
            "turnStartedAtMs": now,
            "startedAt": now,
            "items": [user_item.clone()],
        });

        {
            let mut saved = self.saved.lock().await;
            let c = saved
                .conversations
                .get_mut(thread_id)
                .ok_or_else(|| error(404, "Antigravity 会话不存在"))?;
            c.turns.push(turn.clone());
            c.active_turn_id = Some(turn_id.clone());
            c.updated_at = now;
            c.edit_history_head = format!("agy-head-{}", c.turns.len());
            self.persist_locked(&saved, Some(thread_id)).await?;
        }

        self.publish(json!({
            "method": "turn/started",
            "params": {
                "threadId": thread_id,
                "turnId": turn_id,
                "turn": turn,
                "startedAtMs": now,
            }
        }));

        // 异步执行 ACP `session/prompt`，流式更新由 pump 实时投影并推送 SSE；完成后更新终态。
        let service = self.clone();
        let tid = thread_id.to_owned();
        let active_turn_id = turn_id.clone();
        let rt = runtime.clone();
        tokio::spawn(async move {
            let prompt_res = rt.client.session_prompt(&rt.native_session_id, blocks).await;
            service
                .finish_turn(&tid, &active_turn_id, prompt_res)
                .await;
        });

        Ok(json!({
            "ok": true,
            "threadId": thread_id,
            "turnId": turn_id,
            "clientUserMessageId": client_msg_id,
            "item": user_item,
            "backend": "antigravity",
        }))
    }

    async fn finish_turn(&self, thread_id: &str, turn_id: &str, result: anyhow::Result<Value>) {
        let now = now_ms();
        // 清理此轮残留的未回答审批请求。
        if let Some(removed) = self.requests.lock().await.remove(thread_id)
            && !removed.is_empty()
        {
            self.requests_changed(thread_id).await;
        }
        let (final_turn, token_usage_event, goal_event) = {
            let mut saved = self.saved.lock().await;
            let Some(c) = saved.conversations.get_mut(thread_id).filter(|c| !c.deleted) else {
                return;
            };
            let Some(turn) = c.turns.iter_mut().find(|t| t["id"] == turn_id) else {
                if c.active_turn_id.as_deref() == Some(turn_id) {
                    c.active_turn_id = None;
                }
                return;
            };
            let (status, error_obj, interrupted, failed, prompt_usage) = match result {
                Ok(value) => {
                    let stop = value["stopReason"].as_str().unwrap_or("end_turn");
                    let was_cancelled = matches!(stop, "cancelled" | "canceled")
                        || turn["_stopRequested"] == true;
                    let usage = value.get("usage").filter(|u| u.is_object()).cloned();
                    if was_cancelled {
                        ("interrupted", Value::Null, true, false, usage)
                    } else {
                        ("completed", Value::Null, false, false, usage)
                    }
                }
                Err(err) => (
                    "failed",
                    json!({ "message": err.to_string() }),
                    false,
                    true,
                    None,
                ),
            };
            turn["status"] = json!(status);
            turn["turnCompletedAtMs"] = json!(now);
            turn["completedAt"] = json!(now);
            if !error_obj.is_null() {
                turn["error"] = error_obj;
            }
            projection::finalize_turn_items(turn, failed, interrupted, now);

            let mut usage_ev = None;
            if let Some(mut u) = prompt_usage
                && let Some(normalized) = projection::normalize_usage_update(&u)
            {
                u["lastObservedAtMs"] = json!(now);
                if !c.model.is_empty() {
                    u["model"] = json!(c.model);
                }
                turn["_acpUsage"] = u;
                let merged = projection::merge_token_usage(&c.token_usage, &normalized, &c.model);
                c.token_usage = merged.clone();
                usage_ev = Some(merged);
            }

            let assistant_text: String = turn["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "agentMessage")
                .filter_map(|item| item["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");

            if c.active_turn_id.as_deref() == Some(turn_id) {
                c.active_turn_id = None;
            }
            c.updated_at = now;
            c.unread = true;
            if failed || interrupted {
                c.remote_queue.pause("上一轮未正常完成，队列已暂停");
                if c.goal["status"] == "active" {
                    c.goal["status"] = json!("paused");
                    c.goal["updatedAt"] = json!(now);
                }
            } else if c.goal["status"] == "active" {
                let iterations = c.goal["iterations"].as_u64().unwrap_or(1);
                if assistant_text.contains(GOAL_COMPLETE_MARKER) || iterations >= GOAL_MAX_ITERATIONS
                {
                    c.goal["status"] = json!("complete");
                    c.goal["updatedAt"] = json!(now);
                }
            }
            let goal_ev = (!c.goal.is_null()).then(|| c.goal.clone());
            let turn_snapshot = turn.clone();
            let displayed = c.displayed_turn(&turn_snapshot);
            let _ = self.persist_locked(&saved, Some(thread_id)).await;
            (displayed, usage_ev, goal_ev)
        };

        if let Some(usage) = token_usage_event {
            self.publish(json!({
                "method": "thread/tokenUsage/updated",
                "params": { "threadId": thread_id, "tokenUsage": usage }
            }));
        }
        if let Some(goal) = goal_event {
            self.publish(json!({
                "method": "remote/sidebarChanged",
                "params": { "threadId": thread_id, "goal": goal }
            }));
        }
        self.publish(json!({
            "method": "turn/completed",
            "params": {
                "threadId": thread_id,
                "turnId": turn_id,
                "turn": final_turn,
                "completedAtMs": now,
            }
        }));
        self.queue_changed(thread_id).await;
    }

    async fn drain_queues_and_goals(self: &Arc<Self>) {
        let candidates: Vec<String> = {
            let saved = self.saved.lock().await;
            saved
                .conversations
                .values()
                .filter(|c| {
                    !c.deleted
                        && !c.active()
                        && ((!c.remote_queue.paused && !c.remote_queue.messages.is_empty())
                            || c.goal["status"] == "active")
                })
                .map(|c| c.id.clone())
                .collect()
        };
        for id in candidates {
            let lock = self.lock(&id).await;
            let Ok(_guard) = lock.try_lock() else {
                continue;
            };
            let has_reqs = self
                .requests
                .lock()
                .await
                .get(&id)
                .is_some_and(|m| !m.is_empty());
            if has_reqs {
                continue;
            }
            if let Ok(candidate_conv) = self.conversation(&id).await
                && self.is_externally_occupied(&candidate_conv).await
            {
                let paused = {
                    let mut saved = self.saved.lock().await;
                    if let Some(c) = saved.conversations.get_mut(&id) {
                        let mut changed = false;
                        if !c.remote_queue.paused && !c.remote_queue.messages.is_empty() {
                            c.remote_queue.pause("会话已在另一个应用中打开，队列已暂停");
                            changed = true;
                        }
                        if c.goal["status"] == "active" {
                            c.goal["status"] = json!("paused");
                            c.goal["updatedAt"] = json!(now_ms());
                            changed = true;
                        }
                        if changed {
                            let _ = self.persist_locked(&saved, None).await;
                        }
                        changed
                    } else {
                        false
                    }
                };
                if paused {
                    self.queue_changed(&id).await;
                }
                continue;
            }
            enum NextDispatch {
                Queue(queue::QueuedMessage),
                Goal(String),
                None,
            }
            let (conv, next) = {
                let mut saved = self.saved.lock().await;
                let Some(c) = saved
                    .conversations
                    .get_mut(&id)
                    .filter(|c| !c.deleted && !c.active())
                else {
                    continue;
                };
                let (next, dirty) = if let Some(msg) = c.remote_queue.pop_next_ready() {
                    (NextDispatch::Queue(msg), true)
                } else if c.goal["status"] == "active" {
                    let objective = c.goal["objective"].as_str().unwrap_or("").to_owned();
                    let next_iter = c.goal["iterations"].as_u64().unwrap_or(1) + 1;
                    if objective.is_empty() || next_iter > GOAL_MAX_ITERATIONS {
                        c.goal["status"] = json!("complete");
                        c.goal["updatedAt"] = json!(now_ms());
                        (NextDispatch::None, true)
                    } else {
                        c.goal["iterations"] = json!(next_iter);
                        c.goal["updatedAt"] = json!(now_ms());
                        (
                            NextDispatch::Goal(format_goal_continue_prompt(&objective, next_iter)),
                            true,
                        )
                    }
                } else {
                    (NextDispatch::None, false)
                };
                let conv = c.clone();
                if dirty {
                    let _ = self.persist_locked(&saved, None).await;
                }
                (conv, next)
            };
            match next {
                NextDispatch::Queue(msg) => {
                    self.queue_changed(&id).await;
                    if let Ok((runtime, _)) = self.ensure_runtime(&conv, false, None).await {
                        let mut c_opt = conv.clone();
                        let _ = Self::apply_acp_options(&runtime, &mut c_opt, &msg.payload).await;
                        if self.dispatch_turn(&id, &runtime, &msg.payload).await.is_err() {
                            let mut saved = self.saved.lock().await;
                            if let Some(c) = saved.conversations.get_mut(&id) {
                                c.remote_queue.messages.insert(0, msg);
                                c.remote_queue.pause("队列自动发送失败，已暂停等待确认");
                                let _ = self.persist_locked(&saved, None).await;
                            }
                            self.queue_changed(&id).await;
                        }
                    }
                }
                NextDispatch::Goal(prompt) => {
                    if let Ok((runtime, _)) = self.ensure_runtime(&conv, false, None).await {
                        let payload = json!({
                            "prompt": prompt,
                            "clientUserMessageId": uuid::Uuid::new_v4().to_string(),
                        });
                        if self.dispatch_turn(&id, &runtime, &payload).await.is_err() {
                            let mut saved = self.saved.lock().await;
                            if let Some(c) = saved.conversations.get_mut(&id) {
                                c.goal["status"] = json!("paused");
                                c.goal["updatedAt"] = json!(now_ms());
                                let _ = self.persist_locked(&saved, None).await;
                            }
                        }
                    }
                }
                NextDispatch::None => {}
            }
        }
    }

    pub(crate) async fn command(
        self: &Arc<Self>,
        action: &str,
        payload: &Value,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        let _lifecycle = self.lifecycle_gate.read().await;
        anyhow::ensure!(self.is_accepting(), "远程服务正在关闭，Antigravity 操作尚未派发");
        let id = payload["threadId"]
            .as_str()
            .context("缺少 Antigravity 会话 ID")?;
        authorization.check().await?;

        if matches!(action, "markRead" | "pinThread" | "rename") {
            return self.change_metadata(action, id, payload).await;
        }

        let lock = self.lock(id).await;
        let _guard = lock.lock().await;
        let mut c = self.conversation(id).await?;
        // 若该会话正被 Antigravity App 或外部 agy CLI 占用，禁止 Harness 发起任何发送/编辑/权限/删除写操作。
        if matches!(
            action,
            "send"
                | "steerQueued"
                | "setGoal"
                | "updateGoal"
                | "editMessage"
                | "setPermissions"
                | "deleteThread"
        ) {
            self.ensure_not_externally_occupied(&c).await?;
        }

        match action {
            "send" => {
                // 仅明确确认当前会话正在执行时允许入队；空闲时直接通过 ACP 发送。
                if c.active() || payload["queueOnly"] == true {
                    let msg = {
                        let mut saved = self.saved.lock().await;
                        let conv = saved
                            .conversations
                            .get_mut(id)
                            .ok_or_else(|| error(404, "Antigravity 会话不存在"))?;
                        conv.remote_queue.paused = false;
                        let msg = conv.remote_queue.enqueue(payload.clone(), now_ms());
                        self.persist_locked(&saved, None).await?;
                        msg
                    };
                    self.queue_changed(id).await;
                    let mut snap = self.queue(id).await?;
                    snap["queued"] = json!(true);
                    snap["queuedMessage"] = msg.view(false);
                    return Ok(snap);
                }
                if let Some(mode) = payload["permissionMode"]
                    .as_str()
                    .map(str::trim)
                    .filter(|m| matches!(*m, "default" | "accept-edits" | "auto" | "bypassPermissions"))
                {
                    c.permission_mode = Some(mode.to_owned());
                }
                let (runtime, _) = self.ensure_runtime(&c, false, None).await?;
                Self::apply_acp_options(&runtime, &mut c, payload).await?;
                {
                    let mut saved = self.saved.lock().await;
                    if let Some(conv) = saved.conversations.get_mut(id) {
                        conv.model = c.model.clone();
                        conv.effort = c.effort.clone();
                        conv.permission_mode = c.permission_mode.clone();
                        conv.permission_revision = c.permission_revision;
                        conv.remote_queue.paused = false;
                        self.persist_locked(&saved, None).await?;
                    }
                }
                self.dispatch_turn(id, &runtime, payload).await
            }
            "steerQueued" => {
                let message_id = payload["messageId"].as_str().context("缺少队列消息标识")?;
                let msg = {
                    let mut saved = self.saved.lock().await;
                    let conv = saved
                        .conversations
                        .get_mut(id)
                        .ok_or_else(|| error(404, "Antigravity 会话不存在"))?;
                    let msg = conv.remote_queue.take_by_id(message_id)?;
                    conv.remote_queue.paused = false;
                    self.persist_locked(&saved, None).await?;
                    msg
                };
                self.queue_changed(id).await;
                if let Some(mode) = payload["permissionMode"]
                    .as_str()
                    .map(str::trim)
                    .filter(|m| matches!(*m, "default" | "accept-edits" | "auto" | "bypassPermissions"))
                {
                    c.permission_mode = Some(mode.to_owned());
                }
                let (runtime, _) = self.ensure_runtime(&c, false, None).await?;
                if c.active() && runtime.supports_steering {
                    // agy acp 1.5.0+ 原生支持运行中再次发送 session/prompt 作为引导，无需强行中断子任务。
                    let blocks =
                        projection::build_prompt_blocks(&msg.payload, runtime.supports_image)?;
                    let rt = runtime.clone();
                    tokio::spawn(async move {
                        let _ = rt.client.session_prompt(&rt.native_session_id, blocks).await;
                    });
                    return Ok(json!({
                        "ok": true,
                        "threadId": id,
                        "steered": true,
                        "messageId": message_id,
                    }));
                }
                if c.active() {
                    let _ = runtime.client.session_cancel(&runtime.native_session_id).await;
                }
                Self::apply_acp_options(&runtime, &mut c, &msg.payload).await?;
                self.dispatch_turn(id, &runtime, &msg.payload).await
            }
            "removeQueued" => {
                let message_id = payload["messageId"].as_str().context("缺少消息 ID")?;
                {
                    let mut saved = self.saved.lock().await;
                    let conv = saved
                        .conversations
                        .get_mut(id)
                        .ok_or_else(|| error(404, "Antigravity 会话不存在"))?;
                    conv.remote_queue.remove(message_id)?;
                    self.persist_locked(&saved, None).await?;
                }
                self.queue_changed(id).await;
                self.queue(id).await
            }
            "reorderQueued" => {
                let ids: Vec<String> = payload["messageIds"]
                    .as_array()
                    .ok_or_else(|| error(400, "缺少排序列表"))?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                {
                    let mut saved = self.saved.lock().await;
                    let conv = saved
                        .conversations
                        .get_mut(id)
                        .ok_or_else(|| error(404, "Antigravity 会话不存在"))?;
                    conv.remote_queue.reorder(&ids)?;
                    self.persist_locked(&saved, None).await?;
                }
                self.queue_changed(id).await;
                self.queue(id).await
            }
            "beginQueuedEdit" => {
                let message_id = payload["messageId"].as_str().context("缺少消息 ID")?;
                let edit_id = payload["editId"].as_str().context("缺少编辑 ID")?;
                let result = {
                    let mut saved = self.saved.lock().await;
                    let conv = saved
                        .conversations
                        .get_mut(id)
                        .ok_or_else(|| error(404, "Antigravity 会话不存在"))?;
                    let res = conv.remote_queue.begin_edit(message_id, edit_id)?;
                    self.persist_locked(&saved, None).await?;
                    res
                };
                self.queue_changed(id).await;
                Ok(result)
            }
            "editQueued" => {
                let edit_id = payload["editId"].as_str().context("缺少编辑 ID")?;
                let result = {
                    let mut saved = self.saved.lock().await;
                    let conv = saved
                        .conversations
                        .get_mut(id)
                        .ok_or_else(|| error(404, "Antigravity 会话不存在"))?;
                    let res = conv.remote_queue.commit_edit(edit_id, payload)?;
                    self.persist_locked(&saved, None).await?;
                    res
                };
                self.queue_changed(id).await;
                Ok(result)
            }
            "cancelQueuedEdit" => {
                let edit_id = payload["editId"].as_str().context("缺少编辑 ID")?;
                let result = {
                    let mut saved = self.saved.lock().await;
                    let conv = saved
                        .conversations
                        .get_mut(id)
                        .ok_or_else(|| error(404, "Antigravity 会话不存在"))?;
                    let res = conv.remote_queue.cancel_edit(edit_id)?;
                    self.persist_locked(&saved, None).await?;
                    res
                };
                self.queue_changed(id).await;
                Ok(result)
            }
            "dismissQueuedEdit" => {
                let edit_id = payload["editId"].as_str().context("缺少编辑 ID")?;
                let result = {
                    let mut saved = self.saved.lock().await;
                    let conv = saved
                        .conversations
                        .get_mut(id)
                        .ok_or_else(|| error(404, "Antigravity 会话不存在"))?;
                    let res = conv.remote_queue.dismiss_edit(edit_id);
                    self.persist_locked(&saved, None).await?;
                    res
                };
                self.queue_changed(id).await;
                Ok(result)
            }
            "setGoal" => {
                if !c.goal.is_null() && c.goal["status"] != "complete" {
                    return Err(error(409, "此会话已有未结束的目标"));
                }
                let objective = payload["prompt"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| error(400, "目标内容不能为空"))?
                    .to_owned();
                let now = now_ms();
                let goal = json!({
                    "objective": objective,
                    "status": "active",
                    "createdAt": now,
                    "updatedAt": now,
                    "iterations": 1,
                    "source": "antigravity",
                });
                if let Some(mode) = payload["permissionMode"]
                    .as_str()
                    .map(str::trim)
                    .filter(|m| matches!(*m, "default" | "accept-edits" | "auto" | "bypassPermissions"))
                {
                    c.permission_mode = Some(mode.to_owned());
                }
                let (runtime, _) = self.ensure_runtime(&c, false, None).await?;
                Self::apply_acp_options(&runtime, &mut c, payload).await?;
                {
                    let mut saved = self.saved.lock().await;
                    let conv = saved
                        .conversations
                        .get_mut(id)
                        .ok_or_else(|| error(404, "Antigravity 会话不存在"))?;
                    conv.goal = goal.clone();
                    conv.model = c.model.clone();
                    conv.effort = c.effort.clone();
                    self.persist_locked(&saved, None).await?;
                }
                let mut goal_payload = payload.clone();
                goal_payload["prompt"] = json!(format_goal_initial_prompt(&objective));
                let mut sent = self.dispatch_turn(id, &runtime, &goal_payload).await?;
                sent["goal"] = goal;
                sent["goalSubmission"] = json!({
                    "threadId": id,
                    "phase": "complete",
                    "firstTurnAccepted": true,
                    "goalSet": "succeeded",
                });
                Ok(sent)
            }
            "updateGoal" => {
                if c.goal.is_null() {
                    return Err(error(404, "当前会话没有进行中的目标"));
                }
                if c.goal["createdAt"] != payload["expectedCreatedAt"]
                    || c.goal["objective"] != payload["expectedObjective"]
                    || c.goal["status"] != payload["expectedStatus"]
                {
                    return Err(error(409, "目标已变化，请重新读取"));
                }
                let op = payload["operation"].as_str().unwrap_or("edit");
                let updated = {
                    let mut saved = self.saved.lock().await;
                    let conv = saved
                        .conversations
                        .get_mut(id)
                        .ok_or_else(|| error(404, "Antigravity 会话不存在"))?;
                    if op == "delete" {
                        conv.goal = Value::Null;
                    } else {
                        if let Some(obj) = payload["objective"]
                            .as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            conv.goal["objective"] = json!(obj);
                        }
                        if op == "pause" {
                            conv.goal["status"] = json!("paused");
                        } else if op == "resume" {
                            conv.goal["status"] = json!("active");
                        }
                        conv.goal["updatedAt"] = json!(now_ms());
                    }
                    let next = conv.goal.clone();
                    self.persist_locked(&saved, None).await?;
                    next
                };
                self.publish(json!({
                    "method": "remote/sidebarChanged",
                    "params": { "threadId": id }
                }));
                Ok(json!({ "ok": true, "threadId": id, "goal": updated }))
            }
            "editMessage" => {
                if c.active() {
                    return Err(error(409, "请等待 Antigravity 结束当前执行后再编辑历史消息"));
                }
                if !c.edit_history_head.is_empty()
                    && !payload["historyHead"].is_null()
                    && payload["historyHead"] != c.edit_history_head
                {
                    return Err(error(409, "会话历史已变化，请重新打开编辑"));
                }
                let turn_idx = c
                    .turns
                    .iter()
                    .position(|turn| {
                        turn["id"] == payload["turnId"]
                            && turn["items"].as_array().is_some_and(|items| {
                                items.iter().any(|item| {
                                    item["type"] == "userMessage"
                                        && item["id"] == payload["messageId"]
                                })
                            })
                    })
                    .ok_or_else(|| error(409, "历史消息已变化，请重新打开编辑"))?;

                // 因官方 ACP 协议暂不支持原生历史截断与 session/fork，
                // 这里严格通过 ACP `session/new` 开启新会话，并将截断点之前保留的轮次经 ACP 官方宣告的
                // `_meta.antigravity.systemPrompt.append` 传入，不触碰内部数据库，也不撤销磁盘文件改动。
                let kept_turns = c.turns[..turn_idx].to_vec();
                let replay_prompt = projection::build_edit_replay_system_prompt(&kept_turns);

                if let Some(old_rt) = self.runtimes.lock().await.remove(id) {
                    let _ = old_rt.client.session_close(&old_rt.native_session_id).await;
                    old_rt.client.close().await;
                }
                let (new_runtime, _) = self
                    .ensure_runtime(&c, true, replay_prompt.as_deref())
                    .await?;
                Self::apply_acp_options(&new_runtime, &mut c, payload).await?;
                {
                    let mut saved = self.saved.lock().await;
                    let conv = saved
                        .conversations
                        .get_mut(id)
                        .ok_or_else(|| error(404, "Antigravity 会话不存在"))?;
                    conv.native_id = new_runtime.native_session_id.clone();
                    conv.turns = kept_turns;
                    conv.model = c.model.clone();
                    conv.effort = c.effort.clone();
                    conv.permission_mode = c.permission_mode.clone();
                    conv.edit_history_head = format!("agy-head-{}", uuid::Uuid::new_v4());
                    conv.updated_at = now_ms();
                    self.persist_locked(&saved, Some(id)).await?;
                }
                self.publish(json!({
                    "method": "remote/threadReverted",
                    "params": { "threadId": id }
                }));
                let mut sent = self.dispatch_turn(id, &new_runtime, payload).await?;
                sent["edited"] = json!(true);
                Ok(sent)
            }
            "setPermissions" => {
                // 用户要求 Antigravity 去除计划模式，增加无限制模式（bypassPermissions）；
                // 切入/切出无限制模式时，若当前会话空闲则关闭旧 ACP 连接，使下一轮按新启动参数接回。
                let mode = payload["permissionMode"]
                    .as_str()
                    .filter(|m| matches!(*m, "default" | "accept-edits" | "auto" | "bypassPermissions"))
                    .ok_or_else(|| error(400, "Antigravity 权限模式无效"))?;
                let want_skip = mode == "bypassPermissions";
                let mut applied_live = false;
                if let Some(rt) = self.runtimes.lock().await.get(id).cloned()
                    && rt.client.is_live()
                {
                    if rt.dangerously_skip_permissions == want_skip {
                        if want_skip
                            || rt
                                .client
                                .session_set_mode(&rt.native_session_id, mode)
                                .await
                                .is_ok()
                            || rt
                                .client
                                .session_set_config_option(&rt.native_session_id, "mode", mode)
                                .await
                                .is_ok()
                        {
                            applied_live = true;
                        }
                    } else if !c.active() {
                        if let Some(old_rt) = self.runtimes.lock().await.remove(id) {
                            let _ = old_rt.client.session_close(&old_rt.native_session_id).await;
                            old_rt.client.close().await;
                        }
                    }
                }
                let instance_id = c.config.instance_id();
                let rev = {
                    let mut saved = self.saved.lock().await;
                    let conv = saved
                        .conversations
                        .get_mut(id)
                        .ok_or_else(|| error(404, "Antigravity 会话不存在"))?;
                    conv.permission_mode = Some(mode.to_owned());
                    conv.permission_revision += 1;
                    let rev = conv.permission_revision;
                    self.persist_locked(&saved, None).await?;
                    rev
                };
                // 必须同时返回并广播 instanceId 与 appliesTo，以通过前端 changeComposerPermissions 回执核对。
                let applies_to = if applied_live { "immediate" } else { "nextTurn" };
                self.publish(json!({
                    "method": "thread/settings/updated",
                    "params": {
                        "threadId": id,
                        "instanceId": instance_id,
                        "permissionMode": mode,
                        "permissionRevision": rev,
                        "appliesTo": applies_to,
                    }
                }));
                Ok(json!({
                    "ok": true,
                    "threadId": id,
                    "instanceId": instance_id,
                    "permissionMode": mode,
                    "permissionRevision": rev,
                    "appliesTo": applies_to,
                }))
            }
            "stop" => {
                {
                    let mut saved = self.saved.lock().await;
                    if let Some(conv) = saved.conversations.get_mut(id) {
                        conv.remote_queue.pause("已停止当前任务，队列已暂停");
                        if conv.goal["status"] == "active" {
                            conv.goal["status"] = json!("paused");
                            conv.goal["updatedAt"] = json!(now_ms());
                        }
                        if let Some(last) = conv.turns.last_mut()
                            && last["status"] == "inProgress"
                        {
                            last["_stopRequested"] = json!(true);
                        }
                        let _ = self.persist_locked(&saved, Some(id)).await;
                    }
                }
                self.queue_changed(id).await;
                if let Some(rt) = self.runtimes.lock().await.get(id).cloned()
                    && rt.client.is_live()
                {
                    let _ = rt.client.session_cancel(&rt.native_session_id).await;
                }
                Ok(json!({ "ok": true, "threadId": id }))
            }
            "approve" | "answer" => {
                let req_id = payload["requestId"]
                    .as_str()
                    .or_else(|| payload["id"].as_str())
                    .context("缺少请求标识")?;
                let pending = self
                    .requests
                    .lock()
                    .await
                    .get_mut(id)
                    .and_then(|map| map.remove(req_id))
                    .ok_or_else(|| error(404, "该审批或提问请求已处理或已过期"))?;
                let rt = self
                    .runtimes
                    .lock()
                    .await
                    .get(id)
                    .cloned()
                    .ok_or_else(|| error(409, "Antigravity ACP 会话连接已断开"))?;
                let reply =
                    projection::build_permission_response(&pending.mapped, action, payload);
                rt.client.respond_result(pending.rpc_id, reply).await?;
                self.requests_changed(id).await;
                self.publish(json!({
                    "method": "serverRequest/resolved",
                    "params": { "threadId": id, "requestId": req_id }
                }));
                Ok(json!({ "ok": true, "threadId": id, "requestId": req_id }))
            }
            "deleteThread" => {
                if c.active() {
                    return Err(error(409, "请先停止 Antigravity 当前任务后再移除会话"));
                }
                // 参考 Claude CLI 处理：关闭活跃连接并从 Elves 列表及缓存中移除，原生本地会话历史安全保留。
                if let Some(rt) = self.runtimes.lock().await.remove(id) {
                    let _ = rt.client.session_close(&rt.native_session_id).await;
                    rt.client.close().await;
                }
                self.requests.lock().await.remove(id);
                self.change_metadata("deleteThread", id, payload).await
            }
            "warmSession" => Ok(json!({ "ok": true, "threadId": id, "warm": false })),
            "compact" => Err(error(
                501,
                "当前 Antigravity ACP 协议未提供手动上下文压缩接口",
            )),
            _ => Err(error(409, "此操作暂不支持 Antigravity 会话")),
        }
    }

    async fn change_metadata(
        &self,
        action: &str,
        id: &str,
        payload: &Value,
    ) -> anyhow::Result<Value> {
        let mut renamed_title: Option<String> = None;
        {
            let mut saved = self.saved.lock().await;
            let c = saved
                .conversations
                .get_mut(id)
                .filter(|c| !c.deleted)
                .ok_or_else(|| error(404, "Antigravity 会话不存在或已删除"))?;
            match action {
                "markRead" => c.unread = false,
                "pinThread" => c.pinned = payload["pinned"] == true,
                "rename" => {
                    if let Some(expected) = payload["expectedName"].as_str()
                        && c.title != expected
                    {
                        return Err(error(409, "会话名称已变化，已丢弃自动命名结果"));
                    }
                    let title = payload["title"].as_str().unwrap_or("").trim();
                    if title.is_empty() || title.chars().count() > 200 {
                        return Err(error(400, "标题需为 1–200 个字符"));
                    }
                    c.title = title.to_owned();
                    renamed_title = Some(c.title.clone());
                }
                "deleteThread" => {
                    c.deleted = true;
                    c.turns.clear();
                }
                _ => unreachable!(),
            }
            self.persist_locked(&saved, None).await?;
        }
        if action == "deleteThread" {
            let _ = std::fs::remove_file(self.directory.join(TURNS_DIR).join(format!("{id}.json")));
            self.publish(json!({ "method": "thread/deleted", "params": { "threadId": id } }));
        } else {
            if let Some(title) = renamed_title {
                self.publish(json!({
                    "method": "thread/name/updated",
                    "params": { "threadId": id, "name": title, "title": title }
                }));
            }
            self.publish(json!({
                "method": "remote/sidebarChanged",
                "params": { "threadId": id, "reason": if action == "markRead" { "read" } else { "metadata" } }
            }));
        }
        Ok(json!({
            "ok": true,
            "threadId": id,
            "deleted": action == "deleteThread",
            "status": if action == "deleteThread" { "local_deleted" } else { "ok" },
        }))
    }
}

fn format_goal_initial_prompt(objective: &str) -> String {
    format!(
        "请持续推进以下目标直至彻底完成并在验证无误后，在最终回复末尾单独附带标记 `{GOAL_COMPLETE_MARKER}`：\n\n{objective}"
    )
}

fn format_goal_continue_prompt(objective: &str, iteration: u64) -> String {
    format!(
        "目标自动推进（第 {iteration} 轮）：请检查尚未完成的部分并继续执行以下目标；确认全部完成且验证通过后，请在回复末尾附带 `{GOAL_COMPLETE_MARKER}`：\n\n{objective}"
    )
}

fn normalize_path_key(path: &str) -> String {
    let trimmed = path.trim().replace('/', "\\");
    let trimmed = trimmed.trim_end_matches('\\');
    if cfg!(windows) {
        trimmed.to_ascii_lowercase()
    } else {
        trimmed.to_owned()
    }
}

/// 统一投影 Antigravity 会话可用权限模式：去除计划模式，保留原生三档并固定附带无限制模式（bypassPermissions），
/// 确保历史已存档的旧会话也能立即显示新菜单项。
fn exposed_permission_modes(raw: &[String]) -> Vec<String> {
    let mut modes: Vec<String> = raw
        .iter()
        .filter(|m| matches!(m.as_str(), "default" | "accept-edits" | "auto"))
        .cloned()
        .collect();
    if modes.is_empty() {
        modes = vec!["default".into(), "accept-edits".into(), "auto".into()];
    }
    if !modes.iter().any(|m| m == "bypassPermissions") {
        modes.push("bypassPermissions".into());
    }
    modes
}

/// 将本地转录中记录的多步规划步数与真实输入/缓存/输出明细同步到已完成轮次，不修改用户消息或轮次 ID。
fn sync_turn_usage_from_transcript(saved_turns: &mut [Value], parsed_turns: &[Value]) {
    if saved_turns.len() == parsed_turns.len() {
        for (saved, parsed) in saved_turns.iter_mut().zip(parsed_turns.iter()) {
            if !parsed["_acpUsage"].is_null() {
                saved["_acpUsage"] = parsed["_acpUsage"].clone();
            }
        }
    } else if let (Some(saved_last), Some(parsed_last)) = (saved_turns.last_mut(), parsed_turns.last())
        && !parsed_last["_acpUsage"].is_null()
    {
        saved_last["_acpUsage"] = parsed_last["_acpUsage"].clone();
    }
}

/// 安装升级检查辅助：判断存档中是否存在正在执行的 Antigravity 会话。
pub(crate) fn saved_execution_active(directory: &Path) -> bool {
    let Ok(bytes) = std::fs::read(directory.join(SESSIONS_FILE)) else {
        return false;
    };
    let Ok(saved) = serde_json::from_slice::<Value>(&bytes) else {
        return true;
    };
    saved["conversations"]
        .as_object()
        .into_iter()
        .flat_map(|map| map.values())
        .filter(|conv| conv["deleted"] != true)
        .any(|conv| conv["activeTurnId"].as_str().is_some_and(|s| !s.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_windows_uri_and_wsl_acp_cwds() {
        assert_eq!(
            projection::normalize_acp_cwd("/e%3A/code/junes/github/CodexPlusPlus", false),
            r"E:\code\junes\github\CodexPlusPlus"
        );
        assert_eq!(
            projection::normalize_acp_cwd("/C:/Users/junes", false),
            r"C:\Users\junes"
        );
        assert_eq!(
            projection::normalize_acp_cwd("/home/junes/repo", true),
            "/home/junes/repo"
        );
    }

    #[test]
    fn extracts_models_efforts_modes_and_tier_from_real_acp_setup() {
        let setup = json!({
            "sessionId": "s1",
            "models": {
                "availableModels": [
                    { "modelId": "gemini-4-neon-high", "name": "Gemini 4 Neon (High)" },
                    { "modelId": "gemini-4-neon-xhigh", "name": "Gemini 4 Neon (XHigh)" },
                    { "modelId": "gemini-4-neon-low", "name": "Gemini 4 Neon (Low)" },
                    { "modelId": "claude-opus-5-5-medium", "name": "Claude Opus 5.5 (Medium)" },
                    { "modelId": "claude-opus-5-5-high", "name": "Claude Opus 5.5 (High)" },
                    { "modelId": "gpt-oss-120b-medium", "name": "GPT-OSS 120B (Medium)" }
                ],
                "currentModelId": "gemini-4-neon-high"
            },
            "modes": {
                "currentModeId": "default",
                "availableModes": [
                    { "id": "default", "name": "Default" },
                    { "id": "accept-edits", "name": "Accept edits" },
                    { "id": "auto", "name": "Auto" },
                    { "id": "plan", "name": "Plan" }
                ]
            },
            "configOptions": [
                {
                    "id": "model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "gemini-4-neon",
                    "options": [
                        { "value": "gemini-4-neon", "name": "Gemini 4 Neon" },
                        { "value": "claude-opus-5-5", "name": "Claude Opus 5.5" },
                        { "value": "gpt-oss-120b-medium", "name": "GPT-OSS 120B (Medium)" }
                    ]
                },
                {
                    "id": "thought_level",
                    "category": "thought_level",
                    "type": "select",
                    "currentValue": "high",
                    "options": [
                        { "value": "low", "name": "Low" },
                        { "value": "medium", "name": "Medium" },
                        { "value": "high", "name": "High" },
                        { "value": "xhigh", "name": "Extra" }
                    ]
                }
            ],
            "_meta": {
                "antigravity": {
                    "tier": { "id": "g1-pro-tier", "name": "Google AI Pro" }
                }
            }
        });
        let normalized = projection::normalize_models_from_setup(&setup);
        assert_eq!(normalized["backend"], "antigravity");
        assert_eq!(normalized["defaultModel"], "gemini-4-neon");
        assert_eq!(normalized["defaultReasoningEffort"], "high");
        assert_eq!(
            normalized["permissionModes"],
            json!(["default", "accept-edits", "auto", "bypassPermissions"])
        );
        let rows = normalized["data"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0]["supportedReasoningEfforts"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            rows[1]["supportedReasoningEfforts"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            rows[2]["supportedReasoningEfforts"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        let tier = projection::extract_tier(&setup).unwrap();
        assert_eq!(tier["name"], "Google AI Pro");
    }

    #[test]
    fn projects_acp_updates_and_permission_requests_cleanly() {
        let mut turn = json!({ "id": "t1", "status": "inProgress", "items": [] });
        let e1 = projection::apply_session_update(
            &mut turn,
            &json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": "thinking..." } }),
            1000,
        );
        assert_eq!(e1.changed_items.len(), 1);
        assert_eq!(e1.changed_items[0]["type"], "reasoning");

        let e2 = projection::apply_session_update(
            &mut turn,
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_1",
                "kind": "execute",
                "title": "Run tests",
                "status": "in_progress",
                "rawInput": { "CommandLine": "cargo test" }
            }),
            1010,
        );
        assert_eq!(e2.changed_items[0]["type"], "commandExecution");
        assert_eq!(e2.changed_items[0]["command"], "cargo test");

        let e3 = projection::apply_session_update(
            &mut turn,
            &json!({
                "sessionUpdate": "plan",
                "entries": [
                    { "content": "Inspect ACP", "status": "completed" },
                    { "content": "Wire backend", "status": "in_progress" }
                ]
            }),
            1020,
        );
        assert_eq!(e3.plan_changed.as_ref().unwrap().len(), 2);
        assert_eq!(e3.plan_changed.as_ref().unwrap()[1]["status"], "inProgress");

        let e4 = projection::apply_session_update(
            &mut turn,
            &json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "All done." } }),
            1030,
        );
        assert_eq!(e4.changed_items[0]["type"], "agentMessage");
        assert_eq!(e4.changed_items[0]["text"], "All done.");

        let mapped_q = projection::map_permission_request(
            "antigravity-s1",
            "req-1",
            &json!({
                "toolCall": { "toolCallId": "interaction_1", "title": "Choose branch?" },
                "options": [
                    { "optionId": "opt_main", "kind": "allow_once", "name": "main" },
                    { "optionId": "opt_dev", "kind": "allow_once", "name": "dev" }
                ]
            }),
        );
        assert!(mapped_q.is_user_input);
        assert_eq!(mapped_q.elves_request["method"], "item/tool/requestUserInput");
        let resp = projection::build_permission_response(
            &mapped_q,
            "answer",
            &json!({ "answers": { "interaction_1": "opt_dev" } }),
        );
        assert_eq!(resp["outcome"]["optionId"], "opt_dev");
    }

    #[test]
    fn queue_supports_enqueue_reorder_edit_and_pause() {
        let mut q = AntigravityQueue::default();
        let m1 = q.enqueue(json!({ "prompt": "first", "clientUserMessageId": "m1" }), 10);
        let m2 = q.enqueue(json!({ "prompt": "second", "clientUserMessageId": "m2" }), 20);
        assert_eq!(m1.id, "m1");
        assert_eq!(m2.id, "m2");
        q.reorder(&["m2".into(), "m1".into()]).unwrap();
        assert_eq!(q.messages[0].id, "m2");
        q.begin_edit("m2", "edit-1").unwrap();
        assert_eq!(q.messages.len(), 1);
        q.commit_edit("edit-1", &json!({ "prompt": "second edited" }))
            .unwrap();
        assert_eq!(q.messages[0].payload["prompt"], "second edited");
        let next = q.pop_next_ready().unwrap();
        assert_eq!(next.id, "m2");
        q.pause("stopped");
        assert!(q.pop_next_ready().is_none());
    }

    #[test]
    fn validates_local_and_wsl_configs_and_edit_replay_prompt() {
        let local = EnvConfig {
            distribution: String::new(),
            user: String::new(),
            executable: if cfg!(windows) {
                r"C:\Users\junes\AppData\Local\agy\bin\agy.exe".into()
            } else {
                "/usr/local/bin/agy".into()
            },
        };
        assert!(local.is_local());
        assert_eq!(local.instance_id(), "antigravity:local");
        assert!(local.validate().is_ok());

        let wsl = EnvConfig {
            distribution: "claude-us".into(),
            user: "junes".into(),
            executable: "/home/junes/.local/bin/agy".into(),
        };
        assert!(!wsl.is_local());
        assert!(wsl.instance_id().starts_with("antigravity:wsl:"));
        assert!(wsl.validate().is_ok());

        let turns = vec![
            json!({
                "id": "turn-1",
                "items": [
                    { "type": "userMessage", "content": [{ "type": "text", "text": "Hello" }] },
                    { "type": "agentMessage", "text": "Hi there!" }
                ]
            }),
            json!({
                "id": "turn-2",
                "items": [
                    { "type": "userMessage", "content": [{ "type": "text", "text": "Second" }] }
                ]
            }),
        ];
        let sys = projection::build_edit_replay_system_prompt(&turns[..1]).unwrap();
        assert!(sys.contains("Hello"));
        assert!(sys.contains("Hi there!"));
        assert!(!sys.contains("Second"));

        let replayed = projection::collect_replay_turns(
            &[
                json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "Ping" } }),
                json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Pong" } }),
            ],
            1234,
        );
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0]["items"][0]["content"][0]["text"], "Ping");
        assert_eq!(replayed[0]["items"][1]["text"], "Pong");
    }

    #[tokio::test]
    #[ignore = "需要本机或 WSL 已安装官方 agy CLI 的真实 ACP 联调验收"]
    async fn live_real_agy_acp_local_and_wsl_smoke() {
        let dir = tempfile::tempdir().unwrap();
        let ag = Antigravity::new(dir.path()).unwrap();
        let envs = crate::preferences::detect(dir.path(), true).await;
        let instances: Vec<_> = crate::instances::catalog(&envs)
            .into_iter()
            .filter(|inst| inst.backend == "antigravity")
            .collect();
        eprintln!(
            "Detected Antigravity instances: {}",
            serde_json::to_string_pretty(&instances).unwrap()
        );
        assert!(
            !instances.is_empty(),
            "至少应识别到一个本机或 WSL Antigravity 实例"
        );
        for inst in &instances {
            assert!(inst.ready, "Antigravity 实例应就绪: {}", inst.id);
            assert_eq!(inst.connection_modes, vec!["acp"]);
            let cfg = EnvConfig {
                distribution: inst.distribution.clone(),
                user: inst.user.clone(),
                executable: inst.executable.clone(),
            };
            let usage = ag.account_usage(cfg.clone(), true).await.unwrap();
            eprintln!(
                "Usage for {}: {}",
                inst.id,
                serde_json::to_string(&usage).unwrap()
            );
            assert_eq!(usage["ok"], true);
            assert_eq!(usage["backend"], "antigravity");
            assert_eq!(usage["usage"]["source"], "antigravity-acp");

            if usage["usage"]["authenticated"] == true {
                let models = ag.models_in(cfg.clone(), "", true).await.unwrap();
                let count = models["data"].as_array().map_or(0, Vec::len);
                eprintln!("Models for {} ({}): default={}", inst.id, count, models["defaultModel"]);
                assert!(count > 0, "已登录实例应通过 ACP 返回至少一个模型");

                let sidebar_stub = json!({"projects": [], "recent": [], "pinnedThreads": []});
                ag.discover_in(&cfg, &sidebar_stub).await.unwrap();
                let mut merged = sidebar_stub;
                ag.merge_sidebar_instances(&mut merged, Some(&[inst.id.clone()]))
                    .await;
                let recents = merged["recent"].as_array().cloned().unwrap_or_default();
                eprintln!("Discovered recent count for {}: {}", inst.id, recents.len());
                for row in &recents {
                    if let Some(tid) = row["id"].as_str() {
                        let started = Instant::now();
                        let detail = ag.detail_authorized(tid, None).await.unwrap();
                        let history = ag
                            .history_authorized(&json!({"threadId": tid}), None)
                            .await
                            .unwrap();
                        let elapsed_ms = started.elapsed().as_millis();
                        let turn_count = history["data"].as_array().map_or(0, Vec::len);
                        eprintln!(
                            "Session {tid}: externalWriter={}, readOnly={}, turns={}, elapsed={}ms",
                            detail["thread"]["externalWriter"],
                            detail["thread"]["readOnly"],
                            turn_count,
                            elapsed_ms
                        );
                        assert_eq!(detail["ok"], true);
                        assert_eq!(detail["thread"]["instanceId"], inst.id);
                        assert!(elapsed_ms < 2500, "打开已发现会话不得被 ACP 冷启动阻塞: {elapsed_ms}ms");
                        if turn_count > 0 {
                            let sm = ag.session_metrics(tid).await.unwrap();
                            eprintln!("Session metrics for {tid}: {sm}");
                            assert_eq!(sm["source"], "antigravity-acp");
                            assert!(sm["turnId"].as_str().is_some_and(|s| !s.is_empty()));
                        }
                    }
                }

                let skills = ag.skills_in(cfg.clone(), "", false).await.unwrap();
                let skill_rows = skills["data"].as_array().unwrap();
                eprintln!("Skills for {}: count={}", inst.id, skill_rows.len());
                assert_eq!(skills["instanceId"], inst.id);
                assert!(skills["globalPaths"].is_array());
                assert!(!skill_rows.is_empty(), "已登录实例应返回可用斜杠命令或 Skills");
                assert!(skill_rows.iter().all(|s| {
                    s["backend"] == "antigravity"
                        && s["command"] == s["name"]
                        && s["path"] == format!("antigravity-skill:{}", s["command"].as_str().unwrap_or(""))
                }));

                let quota = ag.subscription_usage(cfg.clone(), true, true).await.unwrap();
                eprintln!("Quota for {}: {}", inst.id, serde_json::to_string(&quota).unwrap());
                assert_eq!(quota["ok"], true);
                assert_eq!(quota["backend"], "antigravity");
                assert_eq!(quota["usage"]["source"], "antigravity-native-usage");
                assert!(
                    quota["usage"]["groups"].as_array().is_some_and(|g| !g.is_empty()),
                    "官方 /usage 应返回模型额度分组"
                );
            }
        }
    }

    #[tokio::test]
    async fn set_permissions_response_includes_instance_id_and_applies_to() {
        let dir = tempfile::tempdir().unwrap();
        let ag = Antigravity::new(dir.path()).unwrap();
        let id = "antigravity-perm-test-1";
        let cfg = EnvConfig {
            distribution: String::new(),
            user: String::new(),
            executable: r"C:\agy\bin\agy.exe".into(),
        };
        {
            let mut saved = ag.saved.lock().await;
            saved.conversations.insert(
                id.into(),
                Conversation {
                    backend: antigravity_backend(),
                    id: id.into(),
                    native_id: "perm-test-1".into(),
                    project_id: Value::Null,
                    cwd: r"C:\work".into(),
                    linux_cwd: String::new(),
                    config: cfg.clone(),
                    title: "Perm Test".into(),
                    model: String::new(),
                    effort: String::new(),
                    permission_mode: Some("default".into()),
                    permission_revision: 0,
                    available_modes: vec![
                        "default".into(),
                        "accept-edits".into(),
                        "auto".into(),
                        "bypassPermissions".into(),
                    ],
                    created_at: 1,
                    updated_at: 1,
                    pinned: false,
                    unread: false,
                    deleted: false,
                    discovered: false,
                    history_loaded: true,
                    local_transcript_mtime_ms: 0,
                    local_transcript_len: 0,
                    edit_history_head: "agy-head-0".into(),
                    token_usage: Value::Null,
                    goal: Value::Null,
                    remote_queue: AntigravityQueue::default(),
                    active_turn_id: None,
                    turns: Vec::new(),
                },
            );
        }
        let detail = ag.detail_authorized(id, None).await.unwrap();
        assert_eq!(detail["thread"]["instanceId"], "antigravity:local");
        assert_eq!(detail["thread"]["permissionMode"], "default");
        assert_eq!(
            detail["thread"]["permissionModes"],
            json!(["default", "accept-edits", "auto", "bypassPermissions"])
        );
        assert_eq!(cfg.instance_id(), "antigravity:local");
    }

    #[test]
    fn syncs_multi_step_turn_usage_and_computes_complete_session_metrics() {
        let mut saved_turns = vec![json!({
            "id": "agy-turn-1",
            "status": "completed",
            "turnStartedAtMs": 1_000,
            "firstTokenAtMs": 1_400,
            "turnCompletedAtMs": 3_400,
            "items": [{
                "id": "agy-turn-1-user",
                "type": "userMessage",
                "content": [{ "type": "text", "text": "排查问题" }]
            }, {
                "id": "agy-turn-1-msg",
                "type": "agentMessage",
                "text": "已完成"
            }]
        })];
        let parsed = local_store::parse_transcript_jsonl(
            &[
                r#"{"type":"USER_INPUT","step_index":0,"created_at":"2026-11-26T12:00:01Z","content":"<USER_REQUEST>排查问题</USER_REQUEST>"}"#,
                r#"{"type":"PLANNER_RESPONSE","step_index":1,"created_at":"2026-11-26T12:00:02Z","model":"gemini-4-neon-xhigh","input_tokens":1000,"cache_read_tokens":9000,"output_tokens":200,"thinking":"step1","tool_calls":[{"name":"read_file","args":{"Path":"a.rs"}}]}"#,
                r#"{"type":"PLANNER_RESPONSE","step_index":2,"created_at":"2026-11-26T12:00:04Z","model":"gemini-4-neon-xhigh","input_tokens":1200,"cache_read_tokens":9500,"output_tokens":300,"content":"已完成"}"#,
            ]
            .join("\n"),
        );
        assert_eq!(parsed.token_usage["modelContextWindow"], 1_048_576);
        sync_turn_usage_from_transcript(&mut saved_turns, &parsed.turns);
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-26T12:01:04Z")
            .unwrap()
            .timestamp_millis() as u64;
        let metrics = projection::compute_session_metrics(
            &saved_turns,
            &parsed.token_usage,
            "gemini-4-neon",
            now,
        );
        assert_eq!(metrics["source"], "antigravity-acp");
        assert_eq!(metrics["turnId"], "agy-turn-1");
        assert_eq!(metrics["steps"], 2);
        assert_eq!(metrics["turnTokens"], 21_200);
        assert_eq!(metrics["totalTokens"], 21_200);
        assert_eq!(metrics["firstTokenMs"], 400);
        assert_eq!(metrics["durationMs"], 2_400);
        assert!((metrics["tokensPerSecond"].as_f64().unwrap() - 250.0).abs() < 1e-6);
        assert!(metrics["cacheHitRate"].as_f64().unwrap() <= 1.0);
        assert_eq!(metrics["cacheExpirySource"], "model-default");
        assert!(metrics["cacheRemainingMs"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn naming_context_and_rename_expected_name_support_antigravity() {
        let dir = tempfile::tempdir().unwrap();
        let ag = Antigravity::new(dir.path()).unwrap();
        let id = "antigravity-naming-1";
        let cfg = EnvConfig {
            distribution: String::new(),
            user: String::new(),
            executable: r"C:\agy\bin\agy.exe".into(),
        };
        {
            let mut saved = ag.saved.lock().await;
            saved.conversations.insert(
                id.into(),
                Conversation {
                    backend: antigravity_backend(),
                    id: id.into(),
                    native_id: "naming-1".into(),
                    project_id: Value::Null,
                    cwd: r"C:\work".into(),
                    linux_cwd: String::new(),
                    config: cfg,
                    title: "检查自动命名为什么失败".into(),
                    model: String::new(),
                    effort: String::new(),
                    permission_mode: Some("default".into()),
                    permission_revision: 0,
                    available_modes: vec!["default".into()],
                    created_at: 1,
                    updated_at: 1,
                    pinned: false,
                    unread: false,
                    deleted: false,
                    discovered: false,
                    history_loaded: true,
                    local_transcript_mtime_ms: 0,
                    local_transcript_len: 0,
                    edit_history_head: "agy-head-1".into(),
                    token_usage: Value::Null,
                    goal: Value::Null,
                    remote_queue: AntigravityQueue::default(),
                    active_turn_id: None,
                    turns: vec![json!({
                        "id": "t1",
                        "status": "completed",
                        "items": [
                            { "id": "u1", "type": "userMessage", "content": [{ "type": "text", "text": "检查自动命名为什么失败" }] },
                            { "id": "a1", "type": "agentMessage", "text": "已定位" }
                        ]
                    })],
                },
            );
        }
        let ctx = ag.naming_context(id, true).await.unwrap();
        assert_eq!(ctx["eligible"], true);
        assert_eq!(ctx["expectedName"], "检查自动命名为什么失败");
        assert_eq!(ctx["firstUserMessage"], "检查自动命名为什么失败");

        // expectedName 匹配时允许自动命名改名。
        ag.change_metadata(
            "rename",
            id,
            &json!({ "title": "排查自动命名失败", "expectedName": "检查自动命名为什么失败" }),
        )
        .await
        .unwrap();
        assert_eq!(ag.conversation(id).await.unwrap().title, "排查自动命名失败");

        // 标题已变后，迟到的旧 expectedName 必须返回 409 拒绝覆盖。
        let stale = ag
            .change_metadata(
                "rename",
                id,
                &json!({ "title": "迟到标题", "expectedName": "检查自动命名为什么失败" }),
            )
            .await
            .unwrap_err();
        assert_eq!(
            stale.downcast_ref::<ApiError>().map(|e| e.code),
            Some(409)
        );
        // 已有自定义标题后，初始自动命名 eligible 为 false。
        let ctx_after = ag.naming_context(id, true).await.unwrap();
        assert_eq!(ctx_after["eligible"], false);
    }

    #[tokio::test]
    async fn handle_session_update_delivers_streaming_reply_to_final_thread_id() {
        let dir = tempfile::tempdir().unwrap();
        let ag = Antigravity::new(dir.path()).unwrap();
        let id = "antigravity-final-stream-1";
        let cfg = EnvConfig {
            distribution: String::new(),
            user: String::new(),
            executable: r"C:\agy\bin\agy.exe".into(),
        };
        {
            let mut saved = ag.saved.lock().await;
            saved.conversations.insert(
                id.into(),
                Conversation {
                    backend: antigravity_backend(),
                    id: id.into(),
                    native_id: "final-stream-1".into(),
                    project_id: Value::Null,
                    cwd: r"C:\work".into(),
                    linux_cwd: String::new(),
                    config: cfg,
                    title: "Antigravity 会话".into(),
                    model: String::new(),
                    effort: String::new(),
                    permission_mode: Some("default".into()),
                    permission_revision: 0,
                    available_modes: vec!["default".into()],
                    created_at: 1,
                    updated_at: 1,
                    pinned: false,
                    unread: false,
                    deleted: false,
                    discovered: false,
                    history_loaded: true,
                    local_transcript_mtime_ms: 0,
                    local_transcript_len: 0,
                    edit_history_head: "agy-head-1".into(),
                    token_usage: Value::Null,
                    goal: Value::Null,
                    remote_queue: AntigravityQueue::default(),
                    active_turn_id: Some("agy-turn-1".into()),
                    turns: vec![json!({
                        "id": "agy-turn-1",
                        "status": "inProgress",
                        "items": [
                            { "id": "agy-turn-1-user", "type": "userMessage", "content": [{ "type": "text", "text": "hello" }] }
                        ]
                    })],
                },
            );
        }
        ag.handle_session_update(
            id,
            &json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": "thinking..." } }),
        )
        .await;
        ag.handle_session_update(
            id,
            &json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "你好！" } }),
        )
        .await;
        let conv = ag.conversation(id).await.unwrap();
        assert_eq!(conv.turns[0]["items"].as_array().unwrap().len(), 3);
        assert_eq!(conv.turns[0]["items"][1]["type"], "reasoning");
        assert_eq!(conv.turns[0]["items"][2]["type"], "agentMessage");
        assert_eq!(conv.turns[0]["items"][2]["text"], "你好！");
    }
}
