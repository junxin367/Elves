use serde_json::{Value, json};

use super::CodexHarness;

impl CodexHarness {
    pub async fn naming_context(&self, id: &str, initial: bool) -> anyhow::Result<Value> {
        super::execution::Scope::service(self.audit.clone(), "naming-context", &json!({"threadId":id}), None)
            .run(async {
                // 首条消息接收后即后台命名；复用本连接新会话的元数据只读重试，不等待首轮完成，
                // 也不重放创建、发送或标题生成请求。重试范围与时限统一由 runtime::Connection::read 控制。
                let raw = self.runtime.read("thread/read", json!({"threadId":id,"includeTurns":false})).await?;
                let thread = crate::codex_service::normalize_thread_response(&raw)?;
                let page = self.runtime.read("thread/turns/list", json!({
                    "threadId":id,"limit":if initial {1} else {8},"itemsView":"full",
                    "sortDirection":if initial {"asc"} else {"desc"}
                })).await?;
                let mut turns = page["data"].as_array().cloned().unwrap_or_default();
                if !initial { turns.reverse(); }
                let name = raw["thread"]["name"].as_str().unwrap_or("");
                Ok(json!({"title":thread["title"],"expectedName":name,"eligible":name.trim().is_empty(),
                    "messages":super::super::conversation_naming::messages(&turns)}))
            }).await
    }
}
