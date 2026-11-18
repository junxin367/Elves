//! 持久侧边复用原生 attached thread；Harness 不复制正文，只保留首次发送也必须遵守的创建快照。
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{CodexHarness, error};
use crate::HarnessAuthorization;
use crate::codex_service::{attached_parent_id, normalize_session_response};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PersistentSide {
    pub parent: String,
    pub permissions: Value,
    pub collaboration_mode: Value,
    pub permission_mode: Value,
    pub initialized: bool,
    #[serde(default)]
    pub named: bool,
}

impl CodexHarness {
    async fn persistent_sides_supported(&self) -> anyhow::Result<bool> {
        // 用户要求真正的持久侧边；以当前执行引擎版本判定，不能使用父历史里的旧 cliVersion 或静默降级。
        let connection = self.runtime.connection().await?;
        let current = connection
            .user_agent
            .get()
            .and_then(|agent| crate::cli_wrapper::parse_codex_cli_version(&agent.replace('/', " ")))
            .ok_or_else(|| error(503, "无法确认当前独立 Codex CLI 版本，持久侧边能力未确认"))?;
        // 已实测 0.167.0-alpha.4.1 不返回 attachedToThreadId，不能沿用 App 的旧版本门槛。
        // 以完整验证过关联字段、重启历史和删除的原生协议版本为最低兼容边界。
        let minimum = crate::cli_wrapper::parse_codex_cli_version("0.169.0-alpha.11.1").unwrap();
        Ok(!crate::cli_wrapper::compare_codex_versions(&current, &minimum).is_lt())
    }

    pub async fn require_persistent_sides(&self) -> anyhow::Result<()> {
        if !self.persistent_sides_supported().await? {
            return Err(error(
                409,
                "当前独立 Codex CLI 尚未确认支持持久侧边的原生关联字段，请升级至 0.169.0-alpha.11.1 或更新版本",
            ));
        }
        Ok(())
    }

    pub(super) async fn persistent_sides_enabled(&self) -> anyhow::Result<bool> {
        let directory = self.store.lock().await.path.parent().unwrap().to_owned();
        Ok(crate::instance_settings::persistent_sides_enabled(
            &directory,
            "codex:local",
        )?)
    }

    pub async fn any_side(&self, id: &str) -> bool {
        if self.known_side(id).await
            || self
                .store
                .lock()
                .await
                .data
                .persistent_sides
                .contains_key(id)
        {
            return true;
        }
        self.runtime
            .read("thread/read", json!({"threadId":id,"includeTurns":false}))
            .await
            .is_ok_and(|response| attached_parent_id(&response["thread"]).is_some())
    }

    pub(super) async fn persistent_snapshot(&self, raw: &Value) -> anyhow::Result<PersistentSide> {
        let parent = attached_parent_id(raw).ok_or_else(|| error(409, "此会话不是原生持久侧边"))?;
        let id = raw["id"].as_str().unwrap_or("");
        self.store
            .lock()
            .await
            .data
            .persistent_sides
            .get(id)
            .filter(|side| {
                // 恢复数据虽为合法 JSON，也必须保有可执行的权限与模式快照；缺失或损坏时只读，不能回落到全局权限。
                let permissions = &side.permissions;
                let mode = &side.collaboration_mode;
                side.parent == parent
                    && side.initialized
                    && (permissions["approvalPolicy"].is_string()
                        || permissions["approvalPolicy"].is_object())
                    && permissions["approvalsReviewer"].is_string()
                    && (permissions["permissions"]
                        .as_str()
                        .is_some_and(|profile| !profile.is_empty())
                        || matches!(
                            permissions["sandboxPolicy"]["type"].as_str(),
                            Some("readOnly" | "workspaceWrite" | "dangerFullAccess")
                        ))
                    && matches!(mode["mode"].as_str(), Some("default" | "plan"))
                    && mode["settings"].is_object()
                    && (mode["settings"]["developer_instructions"].is_null()
                        || mode["settings"]["developer_instructions"].is_string())
            })
            .cloned()
            .ok_or_else(|| {
                error(
                    409,
                    "此持久侧边缺少已确认的创建权限或初始化记录，当前仅支持查看",
                )
            })
    }

    pub(super) async fn decorate_persistent_side(&self, raw: &Value, view: &mut Value) {
        if let Some(parent) = attached_parent_id(raw) {
            let snapshot = self.persistent_snapshot(raw).await.ok();
            view["viewKind"] = json!("side");
            view["serviceSide"] = json!(true);
            view["ephemeral"] = json!(false);
            view["parentThreadId"] = json!(parent);
            view["attachedToThreadId"] = json!(parent);
            view["capabilities"]["sessionPermissions"] = json!(false);
            view["capabilities"]["messageEdit"] = json!(false);
            view["capabilities"]["messageEditHistory"] = json!(false);
            view["capabilities"]["forkThread"] = json!(false);
            view["editableMessage"] = Value::Null;
            view["messageEditReady"] = json!(false);
            if let Some(snapshot) = snapshot {
                view["permissionMode"] = snapshot.permission_mode;
            } else {
                view["readOnly"] = json!(true);
                view["readOnlyReason"] =
                    json!("此持久侧边缺少已确认的创建权限或初始化记录，当前仅支持查看");
            }
            let mut projection = self.projection.lock().await;
            projection.persistent_sides.insert(
                raw["id"].as_str().unwrap_or("").to_owned(),
                parent.to_owned(),
            );
        }
    }

    pub(super) async fn attached_sides(
        &self,
        parent: &str,
        archived: bool,
        fresh: bool,
    ) -> anyhow::Result<Vec<Value>> {
        if !self.persistent_sides_supported().await? {
            // 旧引擎没有 attached 列表接口；已有持久侧边时必须明确失败，不能漏删已有历史。
            if self
                .store
                .lock()
                .await
                .data
                .persistent_sides
                .values()
                .any(|side| side.parent == parent)
            {
                self.require_persistent_sides().await?;
            }
            return Ok(Vec::new());
        }
        let mut rows = Vec::new();
        // 每页 100 条，单父会话最多累计 4096 条/游标，防止异常分页无限占用服务；
        // 总后代上限仍由 deletion::persistent_descendants 的 4096 个会话（含根）再次约束。
        let mut ids = std::collections::HashSet::new();
        let mut cursors = std::collections::HashSet::new();
        let mut cursor = Value::Null;
        loop {
            let params = json!({"attachedToThreadId":parent,"archived":archived,
                "limit":100,"useStateDbOnly":true,"cursor":cursor});
            let page = if fresh {
                self.runtime.request("thread/list", params, None).await?
            } else {
                self.runtime.read("thread/list", params).await?
            };
            // 删除必须取得完整原生关联证据；字段缺失、非法游标和重复行不能被解释成完整空列表。
            let entries = page["data"]
                .as_array()
                .ok_or_else(|| error(503, "原生持久侧边分页缺少有效 data"))?;
            // 游标最多 4096 字节，与普通历史读取边界相同；缺失游标不能当作已完整读取。
            let next = match page.get("nextCursor") {
                Some(Value::Null) => None,
                Some(Value::String(next)) if !next.is_empty() && next.len() <= 4096 => Some(next),
                _ => return Err(error(503, "原生持久侧边分页游标无效或缺失")),
            };
            for row in entries {
                // 旧版可能忽略过滤字段；逐行核对精确关联，绝不把普通主会话当成侧边。
                if attached_parent_id(row) != Some(parent) {
                    return Err(error(503, "原生持久侧边列表返回了不匹配的父会话关系"));
                }
                normalize_session_response(&json!({"thread":row}))?;
                if !ids.insert(row["id"].as_str().unwrap().to_owned()) || ids.len() > 4096 {
                    return Err(error(503, "原生持久侧边分页重复或超出单次读取范围"));
                }
                rows.push(row.clone());
            }
            let Some(next) = next else { break };
            if !cursors.insert(next.to_owned()) || cursors.len() > 4096 {
                return Err(error(503, "原生持久侧边分页游标未前进"));
            }
            cursor = json!(next);
        }
        Ok(rows)
    }

    pub(super) async fn persistent_views(&self, parent: &str) -> (Vec<Value>, Vec<String>) {
        let rows = match self.attached_sides(parent, false, false).await {
            Ok(rows) => rows,
            Err(failure) => {
                let projection = self.projection.lock().await;
                let retained = projection
                    .persistent_sides
                    .iter()
                    .filter(|(_, source)| source.as_str() == parent)
                    .filter_map(|(id, _)| {
                        projection
                            .threads
                            .get(id)
                            .filter(|view| view["id"] == id.as_str())
                            .map(|view| projection.thread(id, view.clone()))
                    })
                    .collect();
                return (retained, vec![failure.to_string()]);
            }
        };
        let mut views = Vec::new();
        let mut failures = Vec::new();
        for raw in rows {
            match self.persistent_summary(&raw).await {
                Ok(view) => views.push(view),
                Err(failure) => {
                    failures.push(format!("{}：{failure}", raw["id"].as_str().unwrap_or("")));
                    // 一个侧边暂时读不到时保留已核对的列表记录；不能阻断其他会话，或伪造为已关闭。
                    if let Ok(mut view) = normalize_session_response(&json!({"thread":raw})) {
                        view["sideThreadReadError"] = json!(failure.to_string());
                        let id = raw["id"].as_str().unwrap();
                        let mut projection = self.projection.lock().await;
                        projection
                            .persistent_sides
                            .insert(id.to_owned(), parent.to_owned());
                        views.push(projection.thread(id, view));
                    }
                }
            }
        }
        (views, failures)
    }

    pub(super) async fn persistent_sidebar_rows(&self) -> (Vec<Value>, Vec<String>) {
        let parents: std::collections::BTreeSet<_> = self
            .projection
            .lock()
            .await
            .persistent_sides
            .values()
            .cloned()
            .collect();
        let mut views = Vec::new();
        let mut failures = Vec::new();
        for parent in parents {
            let (rows, errors) = self.persistent_views(&parent).await;
            views.extend(rows);
            failures.extend(errors);
        }
        (views, failures)
    }

    pub(super) async fn persistent_summary(&self, raw: &Value) -> anyhow::Result<Value> {
        let id = raw["id"].as_str().unwrap_or("");
        let response = self
            .runtime
            .read("thread/read", json!({"threadId":id,"includeTurns":false}))
            .await?;
        if attached_parent_id(&response["thread"]) != attached_parent_id(raw)
            || response["thread"]["id"] != id
        {
            return Err(error(503, "持久侧边读取的原生关联不匹配"));
        }
        let view = normalize_session_response(&response)?;
        let mut projection = self.projection.lock().await;
        projection.persistent_sides.insert(
            id.to_owned(),
            raw["attachedToThreadId"].as_str().unwrap().to_owned(),
        );
        Ok(projection.thread(id, view))
    }

    pub(super) async fn finish_persistent_side(
        &self,
        parent: &str,
        fork: &Value,
        mode: Value,
        permission_mode: Value,
        generation: &str,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        let raw = &fork["thread"];
        let id = raw["id"].as_str().unwrap();
        // 原生 started 会先于创建回执暴露 ID；初始化必须与该侧边的发送、改名和删除使用同一写锁。
        let lock = self.thread_lock(id).await;
        let _serial = lock.lock().await;
        self.check_deletion_barrier(id).await?;
        let permissions = super::sides::side_permissions(fork)?;
        // 先保存未初始化记录；崩溃或边界结果未知时不能恢复成可发送会话，也不能自动重放边界。
        self.store.lock().await.commit(|data| {
            data.persistent_sides.insert(
                id.to_owned(),
                PersistentSide {
                    parent: parent.to_owned(),
                    permissions,
                    collaboration_mode: mode,
                    permission_mode,
                    initialized: false,
                    named: false,
                },
            );
            Ok(())
        })?;
        self.projection
            .lock()
            .await
            .persistent_sides
            .insert(id.to_owned(), parent.to_owned());
        self.runtime
            .request_in_generation(
                generation,
                "thread/inject_items",
                json!({"threadId":id,"items":super::sides::side_boundary_items(parent)}),
                authorization,
            )
            .await?;
        let named = self
            .runtime
            .request_in_generation(
                generation,
                "thread/name/set",
                json!({"threadId":id,"name":"侧边聊天"}),
                authorization,
            )
            .await;
        self.remember_settings(id, fork).await?;
        self.store.lock().await.commit(|data| {
            data.persistent_sides
                .get_mut(id)
                .ok_or_else(|| error(409, "持久侧边创建记录已变化，初始化未确认"))?
                .initialized = true;
            Ok(())
        })?;
        self.loaded
            .lock()
            .await
            .insert(id.to_owned(), generation.to_owned());
        self.side_idle.lock().await.loaded(id, generation, false);
        let mut view = normalize_session_response(fork)?;
        self.decorate_persistent_side(raw, &mut view).await;
        let mut projection = self.projection.lock().await;
        if projection.is_deleted(id) || projection.generation != generation {
            return Err(error(
                503,
                "持久侧边初始化期间会话或引擎连接已变化，请核对原回执",
            ));
        }
        view["title"] = json!("侧边聊天");
        projection.threads.insert(id.to_owned(), view.clone());
        projection
            .push(json!({"method":"remote/relatedThreadsChanged","params":{"threadId":parent}}));
        let mut result = json!({"ok":true,"threadId":id,"parentThreadId":parent,"thread":view});
        if named.is_err() {
            result["warning"] = json!("初始侧边标题未保存，请核对原生名称；不要重复创建。");
        }
        Ok(result)
    }

    pub(super) async fn apply_persistent_turn(
        &self,
        id: &str,
        params: &mut Value,
    ) -> anyhow::Result<()> {
        let saved = self
            .store
            .lock()
            .await
            .data
            .persistent_sides
            .get(id)
            .cloned();
        let Some(side) = saved else { return Ok(()) };
        if !side.initialized {
            return Err(error(409, "持久侧边初始化尚未确认，消息未发送"));
        }
        for (key, value) in side.permissions.as_object().into_iter().flatten() {
            params[key] = value.clone();
        }
        let settings = self
            .store
            .lock()
            .await
            .data
            .settings
            .get(id)
            .cloned()
            .unwrap_or(Value::Null);
        let model = params["model"]
            .as_str()
            .or_else(|| settings["model"].as_str())
            .filter(|model| !model.is_empty())
            .ok_or_else(|| error(503, "无法确认持久侧边协作模式使用的模型"))?
            .to_owned();
        let effort = params
            .get("effort")
            .cloned()
            .unwrap_or_else(|| settings["reasoningEffort"].clone());
        let mut mode = side.collaboration_mode;
        mode["settings"]["model"] = json!(model);
        mode["settings"]["reasoning_effort"] = effort;
        params["collaborationMode"] = mode;
        Ok(())
    }

    pub(super) async fn name_persistent_side(
        &self,
        id: &str,
        payload: &Value,
        authorization: &HarnessAuthorization,
    ) {
        let Some(side) = self
            .store
            .lock()
            .await
            .data
            .persistent_sides
            .get(id)
            .cloned()
        else {
            return;
        };
        if side.named {
            return;
        }
        let title = super::super::conversation_naming::side_title_from_message(
            payload["prompt"].as_str().unwrap_or(""),
        );
        if title.is_empty() {
            return;
        }
        // 用户要求首条已接收文字命名且重启保留；命名失败不能把已接收的消息改报失败或重新发送。
        if self
            .runtime
            .request(
                "thread/name/set",
                json!({"threadId":id,"name":title}),
                Some(authorization),
            )
            .await
            .is_ok()
        {
            let parent = side.parent.clone();
            let saved = self.store.lock().await.commit(|data| {
                if let Some(side) = data.persistent_sides.get_mut(id) {
                    side.named = true;
                }
                Ok(())
            });
            if saved.is_ok() {
                self.projection.lock().await.push(json!({
                    "method":"remote/relatedThreadsChanged",
                    "params":{"threadId":parent,"title":title}
                }));
            }
        }
    }
}
