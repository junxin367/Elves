use serde_json::{Value, json};

use super::CodexHarness;

impl CodexHarness {
    pub async fn naming_context(&self, id: &str, initial: bool) -> anyhow::Result<Value> {
        super::execution::Scope::service(self.audit.clone(), "naming-context", &json!({"threadId":id}), None)
            .run(async {
                // 首条消息接收后即后台命名；复用本连接新会话的元数据只读重试，不等待首轮完成，
                // 也不重放创建、发送或标题生成请求。重试范围与时限统一由 runtime::Connection::read 控制。
                let raw = self.runtime.read("thread/read", json!({"threadId":id,"includeTurns":false})).await?;
                let known_side = self.known_side(id).await;
                let side = known_side
                    || crate::codex_service::attached_parent_id(&raw["thread"]).is_some();
                let thread = if side {
                    crate::codex_service::normalize_thread_metadata(&raw["thread"])?
                } else {
                    crate::codex_service::normalize_thread_response(&raw)?
                };
                let page = self.runtime.read("thread/turns/list", json!({
                    "threadId":id,
                    "limit":if initial { if side {8} else {1} } else {8},
                    "itemsView":"full",
                    "sortDirection":if initial {"asc"} else {"desc"}
                })).await?;
                let mut turns = page["data"].as_array().cloned().unwrap_or_default();
                if !initial { turns.reverse(); }
                let name = if known_side {
                    self.side_detail(id)
                        .await
                        .ok()
                        .and_then(|value| value["thread"]["title"].as_str().map(str::to_owned))
                        .unwrap_or_default()
                } else {
                    raw["thread"]["name"].as_str().unwrap_or("").to_owned()
                };
                let first = if side {
                    super::super::conversation_naming::first_side_user_message(&turns)
                } else {
                    super::super::conversation_naming::first_user_message(&turns)
                };
                let default_side_title = first
                    .as_deref()
                    .map(super::super::conversation_naming::side_title_from_message);
                let eligible = if side {
                    name.trim().is_empty()
                        || name == "侧边聊天"
                        || default_side_title.as_deref() == Some(name.as_str())
                } else {
                    name.trim().is_empty()
                };
                Ok(json!({"title":thread["title"],"expectedName":name,"eligible":eligible,
                    "side":side,"firstUserMessage":if initial { first } else { None },
                    "messages":if initial { Vec::new() } else { super::super::conversation_naming::messages(&turns) }}))
            }).await
    }
}
