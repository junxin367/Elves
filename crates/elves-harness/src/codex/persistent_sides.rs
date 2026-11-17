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
            .filter(|side| side.parent == parent && side.initialized)
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
        let rows = self
            .list_all(
                "thread/list",
                json!({"attachedToThreadId":parent,
            "archived":archived,"limit":100,"useStateDbOnly":true}),
            )
            .await?;
        for row in &rows {
            // 旧版可能忽略过滤字段；每行必须保有精确原生关联，绝不把普通主会话当成侧边。
            if attached_parent_id(row) != Some(parent) {
                return Err(error(503, "原生持久侧边列表返回了不匹配的父会话关系"));
            }
            normalize_session_response(&json!({"thread":row}))?;
        }
        Ok(rows)
    }

    pub(super) async fn persistent_sidebar_rows(&self) -> anyhow::Result<Vec<Value>> {
        let parents: std::collections::BTreeSet<_> = self
            .projection
            .lock()
            .await
            .persistent_sides
            .values()
            .cloned()
            .collect();
        let mut views = Vec::new();
        for parent in parents {
            for raw in self.attached_sides(&parent, false).await? {
                views.push(self.persistent_summary(&raw).await?);
            }
        }
        Ok(views)
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
        self.store.lock().await.commit(|data| {
            data.persistent_sides.get_mut(id).unwrap().initialized = true;
            Ok(())
        })?;
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
        self.loaded
            .lock()
            .await
            .insert(id.to_owned(), generation.to_owned());
        let mut view = normalize_session_response(fork)?;
        self.decorate_persistent_side(raw, &mut view).await;
        let mut projection = self.projection.lock().await;
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
        let title: String = payload["prompt"]
            .as_str()
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(15)
            .collect();
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
            let _ = self.store.lock().await.commit(|data| {
                if let Some(side) = data.persistent_sides.get_mut(id) {
                    side.named = true;
                }
                Ok(())
            });
        }
    }
}
