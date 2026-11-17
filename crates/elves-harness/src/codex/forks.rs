//! 持久主会话分支只调用独立原生引擎；不创建 worktree，也不修改源会话。
use std::collections::HashSet;

use serde_json::{Value, json};

use super::{CodexHarness, HarnessAuthorization, error, required};

impl CodexHarness {
    pub(super) async fn fork_thread(
        &self,
        id: &str,
        source: &Value,
        payload: &Value,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        // 用户只允许当前工作区；目录由原生源会话决定，拒绝客户端指定目录或其他分支目的地。
        if [
            "cwd",
            "workspaceRoots",
            "worktree",
            "destination",
            "ephemeral",
            "beforeTurnId",
        ]
        .iter()
        .any(|key| payload.get(key).is_some())
        {
            return Err(error(400, "分支只支持源会话的当前工作区"));
        }
        let turn_id = match payload.get("turnId") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_str()
                    .filter(|value| !value.is_empty() && value.len() <= 120)
                    .ok_or_else(|| error(400, "分支轮次无效"))?,
            ),
        };
        if let Some(turn_id) = turn_id {
            self.validate_fork_turn(id, turn_id).await?;
        }
        let cwd = required(source, "cwd")?;
        let source_title = crate::codex_service::normalize_thread(source)?["title"]
            .as_str()
            .unwrap_or("新会话")
            .to_owned();
        let base = self
            .store
            .lock()
            .await
            .data
            .fork_sources
            .get(id)
            .and_then(|origin| origin["baseTitle"].as_str())
            .unwrap_or(&source_title)
            .to_owned();
        let rows = self.fork_title_rows().await?;
        let title = fork_title(&base, &rows);
        let mut params = json!({
            "threadId":id,"cwd":cwd,"ephemeral":false,"excludeTurns":true,"threadSource":"user"
        });
        if let Some(turn_id) = turn_id {
            // 回复入口包含目标完整轮次；最新入口省略截止轮次，由原生处理正在执行的历史快照。
            params["lastTurnId"] = json!(turn_id);
        }
        super::super::permissions::apply_codex(payload, &mut params, cwd, true)?;
        let generation = self.runtime.connection().await?.generation.clone();
        let response = self
            .runtime
            .request("thread/fork", params, Some(authorization))
            .await?;
        let Some(new_id) = response["thread"]["id"]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
        else {
            return Ok(json!({"ok":false,"outcomeUnknown":true,
                "message":"原生分支未返回新会话身份，请核对会话列表；不要重复创建"}));
        };
        if new_id == id || response["thread"]["ephemeral"] == true {
            return Ok(json!({"ok":false,"outcomeUnknown":true,
                "message":"原生分支返回身份无效，请核对会话列表；不要重复创建"}));
        }
        // 创建成功之后的命名/状态保存失败只作附加提示，不能把已创建分支报告为未创建并诱发重试。
        let mut warnings = Vec::new();
        let origin = json!({"threadId":id,"title":source_title,"baseTitle":base,"turnId":turn_id});
        if let Err(failure) = self.store.lock().await.commit(|data| {
            data.fork_sources.insert(new_id.clone(), origin.clone());
            let mut settings = data.settings.get(id).cloned().unwrap_or_else(|| json!({}));
            for key in ["model", "reasoningEffort", "serviceTier"] {
                if !response[key].is_null() {
                    settings[key] = response[key].clone();
                }
            }
            data.settings.insert(new_id.clone(), settings);
            Ok(())
        }) {
            warnings.push(format!("分支已创建，来源和设置保存失败：{failure}"));
        }
        let mut native_thread = response["thread"].clone();
        match self
            .runtime
            .request(
                "thread/name/set",
                json!({"threadId":new_id,"name":title}),
                Some(authorization),
            )
            .await
        {
            Ok(_) => native_thread["name"] = json!(title),
            Err(failure) => warnings.push(format!("分支已创建，标题设置未确认：{failure}")),
        }
        self.loaded.lock().await.insert(new_id.clone(), generation);
        let event = json!({"method":"thread/started","params":{"thread":native_thread}});
        self.catalog.observe(&event).await;
        self.projection.lock().await.ingest(event);
        Ok(
            json!({"ok":true,"status":"created","threadId":new_id,"conversationId":new_id,
            "sourceThreadId":id,"forkedFrom":origin,"warning":warnings.join("；")}),
        )
    }

    async fn validate_fork_turn(&self, id: &str, target: &str) -> anyhow::Result<()> {
        let mut cursor = Value::Null;
        let mut seen = HashSet::new();
        loop {
            let page = self.runtime.request("thread/turns/list", json!({
                "threadId":id,"cursor":cursor,"limit":100,"itemsView":"notLoaded","sortDirection":"desc"
            }), None).await?;
            let turns = page["data"]
                .as_array()
                .ok_or_else(|| error(503, "历史轮次返回无效"))?;
            if let Some(turn) = turns.iter().find(|turn| turn["id"] == target) {
                if !matches!(
                    turn["status"].as_str(),
                    Some("completed" | "failed" | "interrupted")
                ) {
                    return Err(error(409, "该回复所属轮次尚未结束，暂不能从此处分支"));
                }
                return Ok(());
            }
            let Some(next) = page["nextCursor"]
                .as_str()
                .filter(|value| !value.is_empty())
            else {
                return Err(error(409, "分支回复已不在源会话中，请重新读取后操作"));
            };
            if !seen.insert(next.to_owned()) {
                return Err(error(503, "历史轮次分页游标未前进"));
            }
            cursor = json!(next);
        }
    }

    async fn fork_title_rows(&self) -> anyhow::Result<Vec<Value>> {
        // 连续分支时编号必须读取原生现值；目录缓存可能仍保留上次分支命名前的标题。
        let mut rows = Vec::new();
        let mut cursor = Value::Null;
        let mut seen = HashSet::new();
        loop {
            let page = self
                .runtime
                .request(
                    "thread/list",
                    json!({"limit":100,"archived":false,"cursor":cursor}),
                    None,
                )
                .await?;
            rows.extend(
                page["data"]
                    .as_array()
                    .ok_or_else(|| error(503, "会话列表返回无效"))?
                    .iter()
                    .cloned(),
            );
            let Some(next) = page["nextCursor"]
                .as_str()
                .filter(|value| !value.is_empty())
            else {
                return Ok(rows);
            };
            if !seen.insert(next.to_owned()) {
                return Err(error(503, "会话列表分页游标未前进"));
            }
            cursor = json!(next);
        }
    }
}

fn fork_title(base: &str, rows: &[Value]) -> String {
    // 与 Codex 的编号标题一致：同一标题继续递增，标题最多 60 字符（含省略号和编号）。
    for number in 2.. {
        let suffix = format!(" ({number})");
        let remaining = 60_usize.saturating_sub(suffix.chars().count());
        let prefix = if base.chars().count() > remaining {
            format!(
                "{}…",
                base.chars()
                    .take(remaining.saturating_sub(1))
                    .collect::<String>()
            )
        } else {
            base.to_owned()
        };
        let title = format!("{prefix}{suffix}");
        if !rows
            .iter()
            .any(|row| row["name"] == title || row["title"] == title)
        {
            return title;
        }
    }
    unreachable!()
}
