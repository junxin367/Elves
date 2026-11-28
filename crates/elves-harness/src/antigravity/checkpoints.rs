//! AGY 自动压缩只认原生转录的完成检查点，不从上下文用量变化或消息正文猜测。

use serde_json::{Value, json};

#[derive(Clone, Debug)]
struct NativeAnchor {
    step: u64,
    at: u64,
    user_hash: Option<String>,
}

#[derive(Clone, Default)]
pub(super) struct Origin {
    owner: Option<NativeAnchor>,
    manual: bool,
    last_checkpoint: Option<u64>,
    last_model: Option<(u64, String)>,
    pending_model: Option<(u64, String)>,
}

pub(super) enum Boundary {
    Manual,
    Automatic(Checkpoint),
    Model(u64, String),
}

impl Origin {
    pub fn invalidate_input(&mut self) {
        // 损坏行之后重新确认所属输入，但保留已处理的步骤号，避免同一检查点又显示一次。
        self.owner = None;
        self.manual = false;
        self.last_model = None;
        self.pending_model = None;
    }

    pub fn observe(&mut self, row: &Value) -> Option<Boundary> {
        if row["type"] == "ERROR_MESSAGE"
            || matches!(row["status"].as_str(), Some("ERROR" | "INVALID"))
        {
            self.last_model = None;
            self.pending_model = None;
        }
        if row["type"] == "PLANNER_RESPONSE" {
            // 自动压缩不应用手动模型配置。只用同次输入两侧连续的实际调用推定，跨切模、错误或缺失证据不填。
            let previous = self.pending_model.take();
            self.last_model = row["step_index"].as_u64().zip(
                row["model"]
                    .as_str()
                    .and_then(crate::compaction_facts::model)
                    .filter(|_| row["source"] == "MODEL" && row["status"] == "DONE")
                    .map(str::to_owned),
            );
            return previous.and_then(|(step, left)| {
                let (next, right) = self.last_model.as_ref()?;
                (*next > step && *right == left)
                    .then(|| Boundary::Model(step, super::local_store::split_model_effort(right).0))
            });
        }
        // 原生系统也能从第 0 步开启独立会话，全程没有用户气泡。只认该会话的起点，不把截断的尾读当起点。
        if row["type"] == "SYSTEM_MESSAGE"
            && row["source"] == "SYSTEM"
            && row["status"] == "DONE"
            && row["step_index"] == 0
            && self.owner.is_none()
            && !self.manual
        {
            self.owner = Some(NativeAnchor {
                step: 0,
                at: time(row),
                user_hash: None,
            });
            return None;
        }
        if row["type"] == "USER_INPUT" || row["source"] == "USER" {
            self.last_model = None;
            self.pending_model = None;
            let text = super::local_store::extract_user_request_text(
                row["content"].as_str().unwrap_or(""),
            );
            self.manual = text == "/compact";
            if !self.manual {
                self.owner = row["step_index"]
                    .as_u64()
                    .filter(|_| !text.is_empty())
                    .map(|step| NativeAnchor {
                        step,
                        at: time(row),
                        user_hash: Some(crate::hash(&text)),
                    });
            }
            return None;
        }
        if row["type"] != "CHECKPOINT"
            || row["status"] != "DONE"
            || !matches!(row["source"].as_str(), Some("SYSTEM" | "MODEL"))
        {
            return None;
        }
        let step = row["step_index"].as_u64()?;
        if self.last_checkpoint.is_some_and(|seen| step <= seen) {
            return None;
        }
        self.last_checkpoint = Some(step);
        // 一条 /compact 只认领一次完成边界；重复记录不能变成第二次自动压缩。
        if std::mem::take(&mut self.manual) {
            return Some(Boundary::Manual);
        }
        let cp = Checkpoint::read(row, self.owner.as_ref())?;
        self.pending_model = self
            .last_model
            .take()
            .filter(|(s, _)| *s < step && cp.item["compactionModel"].is_null())
            .map(|(_, model)| (step, model));
        Some(Boundary::Automatic(cp))
    }
}

fn time(row: &Value) -> u64 {
    row["created_at"]
        .as_str()
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .and_then(|t| u64::try_from(t.timestamp_millis()).ok())
        .unwrap_or(0)
}

#[derive(Clone, Debug)]
pub(super) struct Checkpoint {
    pub step: u64,
    at: u64,
    owner: NativeAnchor,
    item: Value,
}

impl Checkpoint {
    fn read(row: &Value, owner: Option<&NativeAnchor>) -> Option<Self> {
        // 尾读看不到所属输入、手动 /compact、未结束或来源不明时，不发布“自动压缩”。
        let owner = owner?;
        if row["source"] != "SYSTEM" {
            return None;
        }
        let step = row["step_index"].as_u64()?;
        let at = time(row);
        if step <= owner.step || owner.at == 0 || at < owner.at {
            return None;
        }
        let mut item = json!({
            "id":format!("agy-local-compact-{step}"), "type":"contextCompaction", "backend":"antigravity",
            "source":"automatic","status":"completed","completed":true,"recordedAtMs":at,
            "nativeCheckpointStep":step,"remoteInlineEvent":true
        });
        if let Some(model) = row["model"]
            .as_str()
            .and_then(crate::compaction_facts::model)
        {
            crate::compaction_facts::set_model(
                &mut item,
                Some(&super::local_store::split_model_effort(model).0),
                "native",
            );
        }
        if let Some(ms) = row["duration_ms"].as_u64() {
            item["durationMs"] = json!(ms);
            item["compactionDurationSource"] = json!("native");
        }
        // 这是摘要调用的输入和输出，不是压缩后的上下文容量；不计算缩减比例或虚构起止耗时。
        if let Some(input) = row["input_tokens"]
            .as_u64()
            .zip(row["cache_read_tokens"].as_u64())
            .and_then(|(a, b)| a.checked_add(b))
        {
            item["summaryInputTokens"] = json!(input);
        }
        if let Some(output) = row["output_tokens"].as_u64() {
            item["summaryOutputTokens"] = json!(output);
        }
        Some(Self {
            step,
            at,
            owner: owner.clone(),
            item,
        })
    }

    pub fn item(&self) -> Value {
        self.item.clone()
    }
    pub fn signature(&self) -> (u64, bool) {
        (self.step, self.item["compactionModel"].is_string())
    }

    pub fn infer(&mut self, model: &str) {
        crate::compaction_facts::set_model(&mut self.item, Some(model), "inferred");
    }

    fn owns(&self, turn: &Value) -> bool {
        let Some(fingerprint) = &self.owner.user_hash else {
            return false;
        };
        let start = turn["turnStartedAtMs"]
            .as_u64()
            .or_else(|| turn["createdAt"].as_u64());
        if start.is_some_and(|s| s / 1000 > self.owner.at / 1000) {
            return false;
        }
        let local = turn["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("agy-local-"));
        if !local
            && turn["turnCompletedAtMs"]
                .as_u64()
                .is_some_and(|end| end / 1000 < self.at / 1000)
        {
            return false;
        }
        turn["items"].as_array().into_iter().flatten().any(|i| {
            if i["type"] != "userMessage" {
                return false;
            }
            if local
                && i["id"]
                    .as_str()
                    .and_then(|s| s.strip_prefix("agy-local-user-"))
                    .and_then(|s| s.parse::<u64>().ok())
                    != Some(self.owner.step)
            {
                return false;
            }
            let Some(at) = i["sentAtMs"].as_u64().or(start) else {
                return false;
            };
            if at / 1000 > self.owner.at / 1000 || local && at / 1000 != self.owner.at / 1000 {
                return false;
            }
            let text = i["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            crate::hash(text.trim()) == *fingerprint
        })
    }
}

/// 只合并已核实的检查点。托管轮把标记单独保存，不能打断 ACP 正在拼接的最后一段思考或回复。
pub(super) fn merge(turns: &mut [Value], checkpoints: &[Checkpoint]) -> bool {
    merge_evidence(turns, checkpoints, |_, _| {})
}

pub(super) fn recent_updates(turns: &mut [Value], checkpoints: &[Checkpoint]) -> Vec<Value> {
    let Some(last) = turns.last().filter(|t| {
        t["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("agy-turn-"))
    }) else {
        return vec![];
    };
    // 前台只检查末轮新增的边界；同秒重复输入仍需经过全部可能轮次的唯一归属验证。
    let fresh: Vec<_> = checkpoints
        .iter()
        .filter(|cp| {
            !last["_agyCheckpoints"]
                .as_array()
                .is_some_and(|items| items.contains(&cp.item))
                && cp.owns(last)
        })
        .collect();
    let mut changed = Vec::new();
    merge_evidence(turns, fresh, |index, item| changed.push((index, item.clone())));
    changed.into_iter().map(|(index, mut item)| {
        let turn = &turns[index];
        if let Some(items) = turn["items"].as_array() {
            // 与历史投影共用同一个插入位置，只随事件发下一个正文 ID，不复制整轮回复。
            let at = insertion_index(items, &item);
            item["beforeItemId"] = items[at..].iter().find(|i| !i["nativeCheckpointStep"].is_u64())
                .map_or(Value::Null, |i| i["id"].clone());
        }
        json!({"turnId":turn["id"],"item":item})
    }).collect()
}

fn merge_evidence<'a>(
    turns: &mut [Value],
    checkpoints: impl IntoIterator<Item = &'a Checkpoint>,
    mut on_change: impl FnMut(usize, &Value),
) -> bool {
    let mut changed = false;
    for cp in checkpoints {
        // 已从转录导入的条目按原生步骤和时间对齐，不重复生成，也不移动任何已有消息。
        if let Some((index,item)) = turns
            .iter_mut().enumerate()
            .filter(|(_,t)| {
                t["_agyInheritedHistory"] != true
                    && t["id"]
                        .as_str()
                        .is_some_and(|id| id.starts_with("agy-local-"))
            })
            .filter_map(|(index,t)| Some((index,t.get_mut("items")?.as_array_mut()?)))
            .flat_map(|(index,items)| items.iter_mut().map(move |i| (index,i)))
            .find(|(_,i)| {
                i["id"] == cp.item["id"]
                    && i["type"] == "contextCompaction"
                    && i["recordedAtMs"]
                        .as_u64()
                        .or_else(|| i["completedAtMs"].as_u64())
                        == Some(cp.at)
            })
        {
            if replace_checkpoint(item, cp) { changed = true; on_change(index, item); }
            continue;
        }
        let mut matches = turns
            .iter()
            .enumerate()
            .filter(|(_, t)| t["_agyInheritedHistory"] != true && cp.owns(t));
        let Some((index, _)) = matches.next() else {
            continue;
        };
        if matches.next().is_some() {
            continue;
        }
        let turn = &mut turns[index];
        if !turn["_agyCheckpoints"].is_array() {
            turn["_agyCheckpoints"] = json!([]);
        }
        let events = turn["_agyCheckpoints"].as_array_mut().unwrap();
        if let Some(old) = events.iter_mut().find(|i| i["id"] == cp.item["id"]) {
            if replace_checkpoint(old, cp) { changed = true; on_change(index, old); }
        } else {
            events.push(cp.item());
            on_change(index, events.last().unwrap());
            changed = true;
        }
    }
    changed
}

fn replace_checkpoint(old: &mut Value, cp: &Checkpoint) -> bool {
    let mut next = cp.item();
    // 增量尾读可能暂缺另一侧模型；同一步骤、同一原生时间才允许保留之前已证实的来源。
    if old["nativeCheckpointStep"] == cp.step && old["recordedAtMs"] == cp.at {
        crate::compaction_facts::set_model(
            &mut next,
            old["compactionModel"].as_str(),
            old["compactionModelSource"].as_str().unwrap_or(""),
        );
    }
    if *old == next {
        return false;
    }
    *old = next;
    true
}

fn local_step(id: &str) -> Option<u64> {
    let (_, suffix) = id.strip_prefix("agy-local-")?.split_once('-')?;
    suffix.split('-').next()?.parse().ok()
}

fn insertion_index(items: &[Value], event: &Value) -> usize {
    let step = event["nativeCheckpointStep"].as_u64().unwrap_or(0);
    let at = event["recordedAtMs"].as_u64().unwrap_or(0);
    items.iter().position(|i| {
        if let Some(native) = i["id"].as_str().and_then(local_step) { return native > step; }
        i["startedAtMs"].as_u64().or_else(|| i["sentAtMs"].as_u64()).is_some_and(|ms| ms / 1000 > at / 1000)
    }).unwrap_or(items.len())
}

pub(super) fn display(turn: &mut Value) {
    let Some(mut events) = turn
        .as_object_mut()
        .and_then(|obj| obj.remove("_agyCheckpoints"))
    else {
        return;
    };
    let (Some(events), Some(items)) = (events.as_array_mut(), turn["items"].as_array_mut()) else {
        return;
    };
    events.sort_by_key(|e| e["nativeCheckpointStep"].as_u64());
    for event in events.iter() {
        if !event["nativeCheckpointStep"].is_u64() || !event["recordedAtMs"].is_u64()
            || items.iter().any(|i| i["id"] == event["id"]) {
            continue;
        }
        // 原生导入项有步骤号，托管 ACP 项有接收时间；同秒不倒置已显示的项。没有后项时就停在当前位置。
        let index = insertion_index(items, event);
        items.insert(index, event.clone());
    }
}
