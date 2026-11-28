use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::Context;
use serde_json::{Value, json};

use super::{CodexHarness, error, required};

fn default_mode() -> Value {
    json!({"mode":"default","settings":{"developer_instructions":null}})
}

fn mode_from_context(context: &Value) -> anyhow::Result<Value> {
    // 旧原生记录未保存 collaboration_mode 时沿用原生 Default；存在却无法识别时不能静默降级。
    if context.is_null() {
        return Ok(default_mode());
    }
    if !matches!(context["mode"].as_str(), Some("default" | "plan")) {
        return Err(error(503, "无法识别主会话的原生协作模式"));
    }
    if !context["settings"].is_null() && !context["settings"].is_object() {
        return Err(error(503, "主会话的原生协作模式设置无效"));
    }
    let instructions = &context["settings"]["developer_instructions"];
    if !instructions.is_null() && !instructions.is_string() {
        return Err(error(503, "主会话的原生协作模式指令无效"));
    }
    Ok(json!({"mode":context["mode"],"settings":{"developer_instructions":instructions}}))
}

fn read_mode(path: &Path, turn_id: &str) -> anyhow::Result<Option<Value>> {
    if !path.is_absolute() {
        return Err(error(503, "主会话缺少有效的原生协作模式记录路径"));
    }
    let file = File::open(path).context("无法读取主会话的原生协作模式记录")?;
    let mut mode = None;
    // 用户要求继承源会话真实 Plan/Default。只读原生接口返回的该会话路径，并精确匹配仍有效的轮次；
    // 回退后遗留的后续记录不能重新带回已撤销的模式，也不读取 Codex App 内部状态或客户端提供的路径。
    for line in BufReader::new(file).lines() {
        let line = line.context("读取主会话原生协作模式记录失败")?;
        if !line.contains("\"turn_context\"") || !line.contains(turn_id) {
            continue;
        }
        let record: Value =
            serde_json::from_str(&line).context("主会话的原生协作模式记录不完整")?;
        if record["type"] == "turn_context" && record["payload"]["turn_id"] == turn_id {
            mode = Some(mode_from_context(&record["payload"]["collaboration_mode"])?);
        }
    }
    Ok(mode)
}

impl CodexHarness {
    pub(super) async fn side_collaboration_mode(
        &self,
        parent: &str,
        source: &Value,
        generation: &str,
    ) -> anyhow::Result<Value> {
        let mut cursor = Value::Null;
        let mut seen = HashSet::new();
        loop {
            let page = self
                .runtime
                .request(
                    "thread/turns/list",
                    json!({"threadId":parent,"cursor":cursor,"limit":1,
                        "itemsView":"summary","sortDirection":"desc"}),
                    None,
                )
                .await?;
            let turns = page["data"]
                .as_array()
                .ok_or_else(|| error(503, "主会话原生轮次返回无效"))?;
            if let Some(turn) = turns.first() {
                let compaction_only = turn["items"].as_array().is_some_and(|items| {
                    !items.is_empty()
                        && items.iter().all(|item| item["type"] == "contextCompaction")
                });
                // 独立压缩不产生新的模式选择；沿用它之前最近一个实际轮次的原生设置。
                if !compaction_only {
                    let turn_id = required(turn, "id")?;
                    let lookup_id = turn_id.to_owned();
                    let path = source["path"]
                        .as_str()
                        .filter(|path| !path.is_empty())
                        .ok_or_else(|| error(503, "主会话缺少原生协作模式记录路径"))?
                        .to_owned();
                    let mode = tokio::task::spawn_blocking(move || {
                        read_mode(Path::new(&path), &lookup_id)
                    })
                    .await
                    .context("读取主会话原生协作模式的任务失败")??;
                    if let Some(mode) = mode {
                        return Ok(mode);
                    }
                    // 压缩轮次可能先开始、结束时才写 turn_context。仅在确实缺记录且同一连接、
                    // 主会话和轮次仍有原生压缩开始事件时提示等待，不把其他读取错误或排队压缩误报为正在压缩。
                    let projection = self.projection.lock().await;
                    if projection.generation == generation
                        && projection
                            .compactions
                            .get(parent)
                            .is_some_and(|op| op["state"] == "running" && op["turnId"] == turn_id)
                    {
                        return Err(error(
                            409,
                            "主会话正在压缩上下文，请在压缩完成后再创建侧边会话。",
                        ));
                    }
                    return Err(error(
                        503,
                        "尚未取得主会话最近轮次的原生协作模式，请稍后重试",
                    ));
                }
            }
            let Some(next) = page["nextCursor"]
                .as_str()
                .filter(|cursor| !cursor.is_empty())
            else {
                // 尚无实际轮次的新会话使用原生 Default，不从历史正文推断模式。
                return Ok(default_mode());
            };
            if !seen.insert(next.to_owned()) {
                return Err(error(503, "主会话原生轮次分页游标未前进"));
            }
            cursor = json!(next);
        }
    }
}
