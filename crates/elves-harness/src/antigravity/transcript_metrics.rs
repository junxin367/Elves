//! AGY 会话统计只读当前原生转录：首次最多尾读 8 MiB，之后只消费完整追加行。
//! 不启动 CLI、不改写会话正文，按原生 step_index 去重模型调用。

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use serde_json::{Value, json};

use super::local_store;
use crate::session_metrics_cost as billing;

const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;
const CACHE_CAPACITY: usize = 64;

#[derive(Clone, Default)]
struct Ledger {
    users: BTreeMap<u64, (u64, String)>,
    calls: BTreeMap<u64, (Option<u64>, Value)>,
    complete: bool,
    carried: Value,
    carried_by_user: BTreeMap<u64, Value>,
    retired_step: Option<u64>,
}

#[derive(Clone)]
pub(crate) struct Snapshot {
    pub turns: Vec<Value>,
    pub usage: Value,
    pub complete: bool,
}

#[derive(Clone)]
struct Cached {
    len: u64,
    modified: Option<SystemTime>,
    offset: u64,
    tail: Vec<u8>,
    ledger: Arc<Ledger>,
    snapshot: Arc<Snapshot>,
}

fn timestamp(value: &Value) -> u64 {
    value
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .and_then(|t| u64::try_from(t.timestamp_millis()).ok())
        .unwrap_or(0)
}

pub(crate) fn total(usage: &Value) -> Option<u64> {
    usage["totalTokens"]
        .as_u64()
        .or_else(|| input(usage)?.checked_add(usage["outputTokens"].as_u64()?))
}

pub(crate) fn input(usage: &Value) -> Option<u64> {
    let input = usage["inputTokens"].as_u64()?;
    if usage.get("uncachedInputTokens").is_some() {
        Some(input)
    } else {
        input.checked_add(
            usage["cachedReadTokens"]
                .as_u64()
                .or_else(|| usage["cachedInputTokens"].as_u64())?,
        )
    }
}

/// 输入与缓存互斥；缺失字段保持未知，不能把一次不完整的调用当作零。
pub(crate) fn combine(usages: &[Value]) -> Value {
    if usages.is_empty() {
        return Value::Null;
    }
    let sum = |read: fn(&Value) -> Option<u64>| {
        usages
            .iter()
            .try_fold(0_u64, |sum, usage| sum.checked_add(read(usage)?))
    };
    let input = sum(input);
    let cached = sum(|u| {
        u["cachedReadTokens"]
            .as_u64()
            .or_else(|| u["cachedInputTokens"].as_u64())
    });
    let output = sum(|u| u["outputTokens"].as_u64());
    let steps = sum(|u| u["steps"].as_u64());
    let mut parts = Vec::new();
    let mut complete = true;
    for usage in usages {
        if let Some(items) = usage["billingParts"].as_array()
            && parts.len() + items.len() <= billing::MAX_PARTS
        {
            parts.extend(items.iter().cloned());
            complete &= usage["billingComplete"] != false;
        } else {
            complete = false;
        }
    }
    let last = usages.last().unwrap();
    json!({
        "inputTokens": input,
        "uncachedInputTokens": input.zip(cached).and_then(|(i,c)| i.checked_sub(c)),
        "cachedReadTokens": cached, "outputTokens": output, "totalTokens": sum(total),
        "thoughtTokens": sum(|u| u["thoughtTokens"].as_u64()),
        "steps": steps, "model": last["model"], "lastObservedAtMs": last["lastObservedAtMs"],
        "lastStepTokens": last.get("lastStepTokens").cloned().unwrap_or_else(|| json!(total(last))),
        "lastStepIndex": last["lastStepIndex"],
        "billingParts": parts, "billingComplete": complete, "source": "transcript",
    })
}

impl Ledger {
    fn consume(&mut self, line: &[u8]) {
        if line.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        let Ok(entry) = serde_json::from_slice::<Value>(line) else {
            self.complete = false;
            return;
        };
        let kind = entry["type"].as_str().unwrap_or("");
        if kind != "USER_INPUT" && entry["source"] != "USER" && kind != "PLANNER_RESPONSE" {
            return;
        }
        let Some(step) = entry["step_index"].as_u64() else {
            self.complete = false;
            return;
        };
        let at = timestamp(&entry["created_at"]);
        if kind == "USER_INPUT" || entry["source"] == "USER" {
            self.users.insert(
                step,
                (
                    at,
                    crate::hash(&local_store::extract_user_request_text(
                        entry["content"].as_str().unwrap_or(""),
                    )),
                ),
            );
            // 输入身份只需保留近期 128 条；旧模型调用继续计入会话累计，不保留旧消息正文。
            while self.users.len() > 128 {
                if let Some((old, _)) = self.users.pop_first() {
                    self.carried_by_user.remove(&old);
                }
            }
            return;
        }
        if self.retired_step.is_some_and(|retired| step <= retired) {
            return;
        }
        let uncached = entry["input_tokens"].as_u64();
        let cached = entry["cache_read_tokens"].as_u64();
        let output = entry["output_tokens"].as_u64();
        let input = uncached.zip(cached).and_then(|(i, c)| i.checked_add(c));
        let total = input.zip(output).and_then(|(i, o)| i.checked_add(o));
        let model = entry["model"]
            .as_str()
            .map(local_store::split_model_effort)
            .map(|(model, _)| model)
            .unwrap_or_default();
        let usage = json!({
            "inputTokens":input, "uncachedInputTokens":uncached, "cachedReadTokens":cached,
            "outputTokens":output, "totalTokens":total,
            "thoughtTokens":entry.get("thinking_tokens").or_else(||entry.get("thought_tokens")),
            "steps":1, "model":model, "lastObservedAtMs":at, "lastStepIndex":step,
            "billingParts":[billing::responses_part(&model,
                &json!({"inputTokens":input,"outputTokens":output,"cachedInputTokens":cached}),
                true, &Value::Null)],
            "billingComplete":total.is_some(), "source":"transcript",
        });
        // 原生同一步骤的重复/补全记录替换原值，不追加成新的用量或计费项。
        // 旧步骤在新输入之后再次出现时仍保留原归属，不按日志追加顺序转移到新轮次。
        let owner = self
            .calls
            .get(&step)
            .and_then(|(owner, _)| *owner)
            .or_else(|| self.users.range(..step).next_back().map(|(step, _)| *step));
        self.calls.insert(step, (owner, usage));
        // 与费用明细的 2048 项上限一致；更早调用只保留计数，重复日志不再次累计。
        while self.calls.len() > billing::MAX_PARTS {
            let (old, (owner, usage)) = self.calls.pop_first().unwrap();
            self.carried = carry(&self.carried, &usage);
            if let Some(owner) = owner.filter(|owner| self.users.contains_key(owner)) {
                let previous = self.carried_by_user.get(&owner).unwrap_or(&Value::Null);
                self.carried_by_user.insert(owner, carry(previous, &usage));
            }
            self.retired_step = Some(old);
        }
    }

    fn snapshot(&self) -> Snapshot {
        let mut groups: BTreeMap<u64, Vec<Value>> = self
            .carried_by_user
            .iter()
            .map(|(owner, usage)| (*owner, vec![usage.clone()]))
            .collect();
        let mut all = Vec::new();
        if self.carried.is_object() {
            all.push(self.carried.clone());
        }
        for (owner, usage) in self.calls.values() {
            all.push(usage.clone());
            if let Some(owner) = owner {
                groups.entry(*owner).or_default().push(usage.clone());
            }
        }
        let turns = self
            .users
            .iter()
            .map(|(step, (at, fingerprint))| {
                let usage = combine(groups.get(step).map(Vec::as_slice).unwrap_or_default());
                json!({
                    "id":format!("agy-local-turn-{step}"), "turnStartedAtMs":at,
                    "_metricsUserHash":fingerprint,
                    "_acpUsage":usage,
                })
            })
            .collect();
        let usage = combine(&all);
        let complete = self.complete && (self.calls.is_empty() || total(&usage).is_some());
        Snapshot {
            turns,
            usage,
            complete,
        }
    }
}

fn carry(previous: &Value, usage: &Value) -> Value {
    let mut combined = combine(&if previous.is_object() {
        vec![previous.clone(), usage.clone()]
    } else {
        vec![usage.clone()]
    });
    combined["billingParts"] = Value::Null;
    combined["billingComplete"] = json!(false);
    combined
}

pub(crate) fn read(root: &Path, native_id: &str) -> std::io::Result<Option<Arc<Snapshot>>> {
    let Some((path, _, _)) = local_store::find_metrics_transcript_file(root, native_id) else {
        return Ok(None);
    };
    read_file(&path).map(Some)
}

pub(crate) fn read_file(path: &Path) -> std::io::Result<Arc<Snapshot>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Cached>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    let (len, modified) = (metadata.len(), metadata.modified().ok());
    let cached = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)
        .cloned();
    if let Some(entry) = &cached
        && entry.len == len
        && entry.modified == modified
    {
        return Ok(entry.snapshot.clone());
    }
    let mut offset = len.saturating_sub(MAX_READ_BYTES);
    let mut ledger = Arc::new(Ledger {
        complete: offset == 0,
        ..Default::default()
    });
    if let Some(entry) = cached
        && len > entry.len
        && len.saturating_sub(entry.offset) <= MAX_READ_BYTES
    {
        file.seek(SeekFrom::Start(
            entry.offset.saturating_sub(entry.tail.len() as u64),
        ))?;
        let mut tail = vec![0; entry.tail.len()];
        file.read_exact(&mut tail)?;
        if tail == entry.tail {
            offset = entry.offset;
            ledger = entry.ledger;
        }
    }
    let skip_first = if offset > 0 {
        file.seek(SeekFrom::Start(offset - 1))?;
        let mut previous = [0];
        file.read_exact(&mut previous)?;
        previous[0] != b'\n'
    } else {
        false
    };
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(len.saturating_sub(offset).min(MAX_READ_BYTES))
        .read_to_end(&mut bytes)?;
    let start = if skip_first {
        bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1)
    } else {
        0
    };
    let end = bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(start, |i| i + 1);
    for line in bytes[start..end].split(|b| *b == b'\n') {
        Arc::make_mut(&mut ledger).consume(line);
    }
    let snapshot = Arc::new(ledger.snapshot());
    let entry = Cached {
        len,
        modified,
        offset: offset + end as u64,
        tail: bytes[end.saturating_sub(128)..end].to_vec(),
        ledger,
        snapshot: snapshot.clone(),
    };
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= CACHE_CAPACITY && !cache.contains_key(path) {
        cache.clear();
    }
    // 同一文件并发读取时，先发起的旧快照不能覆盖已经缓存的新文件版本。
    if cache
        .get(path)
        .is_none_or(|current| current.modified <= entry.modified)
    {
        cache.insert(path.to_owned(), entry);
    }
    Ok(snapshot)
}
