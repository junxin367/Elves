//! 只在原生历史已有压缩条目时补同次证据，不产生正文、启动模型或读取桌面连接。
use super::CodexHarness;
use crate::compaction_facts::{model, rank, set_model, timing};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

const MAX_READ: u64 = 64 * 1024 * 1024;
const FIELDS: &[&str] = &[
    "compactionModel",
    "compactionModelSource",
    "recordedAtMs",
    "recordedAtSource",
    "startedAtMs",
    "completedAtMs",
    "durationMs",
    "compactionDurationSource",
    "preTokens",
    "postTokens",
];
type Receipts = BTreeMap<(String, String), Option<String>>;

// 转录中的 replacement_history 可能有数 MiB。只反序列化证据字段，不把任何正文、工具或替换历史装进 Value。
#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct RolloutPayload {
    id: Option<String>,
    turn_id: Option<String>,
    thread_id: Option<String>,
    item_id: Option<String>,
    response_id: Option<String>,
    model: Option<String>,
    compaction_model: Option<String>,
    compaction_response_id: Option<String>,
    latest_token_usage_record: Option<UsageEvidence>,
    resume_metadata: Option<ResumeEvidence>,
    duration_ms: Option<u64>,
    pre_tokens: Option<u64>,
    post_tokens: Option<u64>,
}
#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct UsageEvidence {
    thread_id: Option<String>,
    turn_id: Option<String>,
    response_id: Option<String>,
    model: Option<String>,
}
#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct ResumeEvidence {
    last_started_turn_id: Option<String>,
    previous_turn_settings: Option<ModelEvidence>,
}
#[derive(Default, serde::Deserialize)]
struct ModelEvidence {
    model: Option<String>,
}
#[derive(serde::Deserialize)]
struct Row {
    #[serde(rename = "type")]
    kind: String,
    timestamp: Option<String>,
    payload: RolloutPayload,
}
#[derive(serde::Deserialize)]
struct RowKind<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
}

#[derive(Clone, Default)]
struct Cut {
    item: Option<String>,
    response: Option<String>,
    facts: Value,
}
#[derive(Default)]
struct Trace {
    turns: HashMap<String, Vec<Cut>>,
    complete: bool,
    from_start: HashSet<String>,
}

// 写入中的转录按打开时的长度读取。只追加新行时，原长度处的尾部内容应保持不变。
fn prefix_tail(reader: &mut (impl Read + Seek), len: u64) -> Option<Vec<u8>> {
    let start = len.saturating_sub(128);
    reader.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = vec![0; (len - start) as usize];
    reader.read_exact(&mut tail).ok()?;
    Some(tail)
}

fn bounded(s: &Value) -> Option<&str> {
    s.as_str().and_then(model)
}
pub(super) fn merge(target: &mut Value, facts: &Value) {
    timing(target);
    crate::compaction_facts::merge_recorded_time(target, facts);
    let original = target["compactionModel"]
        .as_str()
        .or_else(|| target["model"].as_str())
        .and_then(model)
        .map(str::to_owned);
    if let Some(name) = original.as_deref() {
        let scope = target["compactionModelSource"]
            .as_str()
            .unwrap_or("native")
            .to_owned();
        set_model(target, Some(name), &scope);
    }
    set_model(
        target,
        facts["compactionModel"].as_str(),
        facts["compactionModelSource"].as_str().unwrap_or("native"),
    );
    for &k in &["startedAtMs", "completedAtMs", "preTokens", "postTokens"] {
        if target[k].is_null() && facts[k].is_u64() {
            target[k] = facts[k].clone();
        }
    }
    if facts["durationMs"].is_u64()
        && (target["durationMs"].is_null()
            || target["compactionDurationSource"] == "observed"
                && facts["compactionDurationSource"] == "native")
    {
        target["durationMs"] = facts["durationMs"].clone();
        target["compactionDurationSource"] = facts["compactionDurationSource"].clone();
    }
    timing(target);
}

fn trace(path: &Path, thread: &str) -> Option<Arc<Trace>> {
    type Entry = (u64, SystemTime, Arc<Trace>);
    static CACHE: OnceLock<Mutex<HashMap<(PathBuf, String), Entry>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let (len, mtime) = (meta.len(), meta.modified().ok()?);
    let key = (path.to_owned(), thread.to_owned());
    if let Some((l, m, result)) = cache.lock().ok()?.get(&key)
        && *l == len
        && *m == mtime
    {
        return Some(result.clone());
    }
    let mut reader = BufReader::new(File::open(path).ok()?);
    let witness = prefix_tail(&mut reader, len)?;
    reader.rewind().ok()?;
    let mut header = String::new();
    (&mut reader)
        .take(2 * 1024 * 1024)
        .read_line(&mut header)
        .ok()?;
    let h: Value = serde_json::from_str(&header).ok()?;
    if h["type"] != "session_meta" || h["payload"]["id"] != thread {
        return None;
    }
    let mut out = Trace {
        complete: len <= MAX_READ,
        ..Default::default()
    };
    if !out.complete {
        reader.seek(SeekFrom::Start(len - MAX_READ)).ok()?;
        reader.skip_until(b'\n').ok()?;
    }
    let mut active = (String::new(), String::new());
    let mut actual: Receipts = BTreeMap::new();
    let remaining = len.saturating_sub(reader.stream_position().ok()?);
    let mut lines = reader.take(remaining);
    let mut buf = Vec::new();
    while {
        buf.clear();
        lines.read_until(b'\n', &mut buf).ok()? != 0
    } {
        if buf.last() != Some(&b'\n') {
            out.complete = false;
            out.from_start.clear();
            break;
        }
        // 普通消息只解顶层类型，不依赖 JSON 的键序，也不会把正文中引用的压缩事件当作记录。
        let Ok(tag) = serde_json::from_slice::<RowKind<'_>>(&buf) else {
            out.complete = false;
            out.from_start.clear();
            active = Default::default();
            continue;
        };
        if tag.kind == "event_msg" && !out.complete {
            // 尾读也可能完整包含某轮；只有原生 task_started 才是该轮起点，重复的 turn_context 不是。
            #[derive(serde::Deserialize)]
            struct Event {
                payload: Start,
            }
            #[derive(serde::Deserialize)]
            struct Start {
                #[serde(rename = "type")]
                kind: String,
                turn_id: Option<String>,
                thread_id: Option<String>,
            }
            if let Ok(event) = serde_json::from_slice::<Event>(&buf)
                && event.payload.kind == "task_started"
                && event
                    .payload
                    .thread_id
                    .as_deref()
                    .is_none_or(|id| id == thread)
                && let Some(id) = event.payload.turn_id.as_deref().and_then(model)
                && !out.turns.contains_key(id)
            {
                out.from_start.insert(id.to_owned());
            }
            continue;
        }
        if !matches!(
            tag.kind,
            "session_meta"
                | "compacted"
                | "turn_context"
                | "token_usage_record"
                | "response.completed"
        ) {
            continue;
        }
        let Ok(row) = serde_json::from_slice::<Row>(&buf) else {
            out.complete = false;
            out.from_start.clear();
            active = Default::default();
            continue;
        };
        let v = &row.payload;
        let text = |s: &Option<String>| s.as_deref().and_then(model).map(str::to_owned);
        match row.kind.as_str() {
            "session_meta" if v.id.as_deref() != Some(thread) => return None,
            "turn_context" => {
                active = (
                    text(&v.turn_id).unwrap_or_default(),
                    text(&v.model).unwrap_or_default(),
                )
            }
            "token_usage_record" | "response.completed"
                if v.thread_id.as_deref() == Some(thread) =>
            {
                if let (Some(turn), Some(resp), Some(name)) = (
                    v.turn_id.as_deref().and_then(model),
                    v.response_id.as_deref().and_then(model),
                    v.model.as_deref().and_then(model),
                ) {
                    actual
                        .entry((turn.into(), resp.into()))
                        .and_modify(|old| {
                            if old.as_deref() != Some(name) {
                                *old = None
                            }
                        })
                        .or_insert_with(|| Some(name.into()));
                }
            }
            "compacted" => {
                let usage = v.latest_token_usage_record.as_ref();
                let resume = v.resume_metadata.as_ref();
                let response = v.compaction_response_id.as_deref().and_then(model);
                let paired = usage.filter(|u| {
                    response.is_some()
                        && u.thread_id.as_deref() == Some(thread)
                        && u.response_id.as_deref() == response
                });
                let recorded_turn = resume
                    .and_then(|r| r.last_started_turn_id.as_deref())
                    .and_then(model);
                let turn = paired
                    .and_then(|u| u.turn_id.as_deref())
                    .and_then(model)
                    .or(recorded_turn);
                let Some(turn) = turn.filter(|t| recorded_turn.is_none_or(|r| r == *t)) else {
                    out.complete = false;
                    out.from_start.clear();
                    continue;
                };
                let mut facts = json!({});
                set_model(
                    &mut facts,
                    v.compaction_model
                        .as_deref()
                        .and_then(model)
                        .or_else(|| v.model.as_deref().and_then(model)),
                    "native",
                );
                let paired = paired.filter(|u| u.turn_id.as_deref() == Some(turn));
                set_model(
                    &mut facts,
                    paired.and_then(|u| u.model.as_deref()),
                    "native",
                );
                let prev = resume
                    .and_then(|r| r.previous_turn_settings.as_ref()?.model.as_deref())
                    .and_then(model);
                let current = (active.0 == turn)
                    .then_some(active.1.as_str())
                    .and_then(model);
                if !matches!((prev,current),(Some(a),Some(b)) if a!=b) {
                    set_model(&mut facts, prev.or(current), "inferred");
                }
                if let Some(at) = row
                    .timestamp
                    .as_deref()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.timestamp_millis())
                    .filter(|t| *t > 0)
                {
                    facts["recordedAtMs"] = json!(at);
                }
                for (n, dest) in [
                    (v.duration_ms, "durationMs"),
                    (v.pre_tokens, "preTokens"),
                    (v.post_tokens, "postTokens"),
                ] {
                    if let Some(n) = n {
                        facts[dest] = json!(n);
                    }
                }
                timing(&mut facts);
                let group = out.turns.entry(turn.into()).or_default();
                group.push(Cut {
                    item: text(&v.item_id),
                    response: response
                        .filter(|_| paired.is_some() || usage.is_none())
                        .map(str::to_owned),
                    facts,
                });
            }
            _ => {}
        }
    }
    for (turn, cuts) in &mut out.turns {
        for cut in cuts {
            if let Some(resp) = &cut.response
                && let Some(Some(m)) = actual.get(&(turn.clone(), resp.clone()))
            {
                set_model(&mut cut.facts, Some(m), "native");
            }
        }
    }
    let out = Arc::new(out);
    // 不把追加后才写出的行混进本次快照；同长度改写、截短或旧数据末端不同都不能复用。
    let current = std::fs::symlink_metadata(path).ok()?;
    if !current.is_file()
        || current.len() < len
        || current.len() == len && current.modified().ok()? != mtime
    {
        return None;
    }
    if current.len() > len {
        return (prefix_tail(&mut File::open(path).ok()?, len)? == witness).then_some(out);
    }
    let mut cache = cache.lock().ok()?;
    if cache.len() >= 16 && !cache.contains_key(&key) {
        cache.clear();
    }
    cache.insert(key, (len, mtime, out.clone()));
    Some(out)
}

fn decorate(
    items: &mut [Value],
    turn: &str,
    complete: bool,
    saved: &[Value],
    trace: &Trace,
    receipts: &Receipts,
) {
    let mut entries: Vec<_> = items
        .iter_mut()
        .filter(|i| i["type"] == "contextCompaction")
        .collect();
    let cuts = trace.turns.get(turn).map(Vec::as_slice).unwrap_or_default();
    let count = entries.len();
    // 任何原生条目自带响应 ID 时都走直接身份；不能用相同数量把不匹配的响应按位置配回来。
    let ordered = complete
        && (trace.complete || trace.from_start.contains(turn))
        && cuts.len() == count
        && cuts.iter().all(|c| c.item.is_none())
        && entries.iter().all(|i| {
            i["responseId"].is_null()
                && !cuts
                    .iter()
                    .any(|c| c.response.as_deref() == i["id"].as_str())
        });
    for (ordinal, item) in entries.iter_mut().enumerate() {
        let Some(id) = bounded(&item["id"]).map(str::to_owned) else {
            continue;
        };
        if let Some(f) = saved.iter().find(|s| s["turn"] == turn && s["id"] == id) {
            merge(item, f);
        }
        let direct: Vec<_> = cuts
            .iter()
            .filter(|c| {
                c.item.as_deref().is_none_or(|i| i == id)
                    && item["responseId"]
                        .as_str()
                        .is_none_or(|r| c.response.as_deref() == Some(r))
                    && (c.item.as_deref() == Some(&id)
                        || c.response
                            .as_deref()
                            .is_some_and(|r| item["responseId"] == r || id == r))
            })
            .collect();
        // 无直接 item/response ID 时，只在完整原生轮次与完整转录数量、顺序都能对上时按序关联。
        let cut = match direct.as_slice() {
            [v] => Some(*v),
            [] if ordered => cuts.get(ordinal),
            _ => None,
        };
        if let Some(c) = cut {
            merge(item, &c.facts);
            if let Some(resp) = &c.response
                && let Some(Some(m)) = receipts.get(&(turn.to_owned(), resp.clone()))
            {
                set_model(item, Some(m), "native");
            }
        }
        timing(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "只读回放显式指定的同一实例转录，不显示正文"]
    fn replays_compaction_facts_from_native_rollout() {
        let path = std::env::var_os("ELVES_TEST_CODEX_ROLLOUT").unwrap();
        let thread = std::env::var("ELVES_TEST_CODEX_THREAD").unwrap();
        let tr = trace(Path::new(&path), &thread).unwrap();
        let mut counts = [0; 4];
        for (turn, cuts) in &tr.turns {
            let mut items: Vec<_> = cuts
                .iter()
                .enumerate()
                .map(|(ix, c)| {
                    json!({"type":"contextCompaction",
                "id":c.item.clone().unwrap_or_else(||format!("event-{ix}"))})
                })
                .collect();
            decorate(&mut items, turn, true, &[], &tr, &Receipts::new());
            for i in items {
                counts[0] += 1;
                counts[1] += usize::from(i["recordedAtMs"].is_u64());
                counts[2] += usize::from(i["compactionModelSource"] == "inferred");
                counts[3] += usize::from(i["durationMs"].is_u64());
            }
        }
        assert!(counts[0] > 0 && counts[0] == counts[1]);
        eprintln!("原生压缩条数/记录时间/推定型号/耗时：{counts:?}");
    }

    #[test]
    fn compaction_trace_only_joins_matching_identities_and_complete_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let lines = [
            json!({"type":"session_meta","payload":{"id":"thread"}}),
            json!({"type":"turn_context","payload":{"turn_id":"turn","model":"at-that-time"}}),
            json!({"timestamp":"2026-11-27T00:00:00Z","type":"compacted","payload":{"compaction_response_id":"resp",
                "resume_metadata":{"last_started_turn_id":"turn","previous_turn_settings":{"model":"at-that-time"}},
                "latest_token_usage_record":{"response_id":"resp","thread_id":"thread","turn_id":"turn","output_tokens":90}}}),
        ];
        std::fs::write(
            &path,
            lines.iter().map(|v| format!("{v}\n")).collect::<String>(),
        )
        .unwrap();
        assert!(trace(&path, "other-thread").is_none());
        let tr = trace(&path, "thread").unwrap();
        let apply = |turn: &str, resp: Option<&str>, full: bool, receipts: &Receipts| {
            let mut items = [json!({"id":"item","type":"contextCompaction","responseId":resp})];
            decorate(&mut items, turn, full, &[], &tr, receipts);
            items[0].clone()
        };
        let receipts = BTreeMap::from([(("turn".into(), "resp".into()), Some("actual".into()))]);
        for v in [
            apply("turn", None, false, &receipts),
            apply("another", None, true, &receipts),
            apply("turn", Some("other"), true, &receipts),
        ] {
            assert!(v["compactionModel"].is_null() && v["recordedAtMs"].is_null());
        }
        let inferred = apply("turn", None, true, &Receipts::new());
        assert_eq!(inferred["compactionModelSource"], "inferred");
        assert_eq!(inferred["compactionModel"], "at-that-time");
        assert!(
            inferred["recordedAtMs"].is_u64()
                && inferred["durationMs"].is_null()
                && inferred["postTokens"].is_null()
        );
        assert_eq!(
            apply("turn", Some("resp"), false, &receipts)["compactionModelSource"],
            "native"
        );
        assert_eq!(
            apply("turn", None, true, &receipts)["compactionModel"],
            "actual"
        );
        let mut f = inferred;
        set_model(&mut f, Some("selected"), "specified");
        merge(&mut f, &apply("turn", None, true, &receipts));
        set_model(&mut f, Some("stale"), "specified");
        assert_eq!(f["compactionModel"], "actual");
        let mut early = json!({"hoverObservedStartedAtMs":123});
        timing(&mut early);
        merge(&mut early, &apply("turn", None, true, &Receipts::new()));
        assert_eq!(early["recordedAtSource"], "record");
        assert!(early["recordedAtMs"].as_u64().unwrap() > 123 && early["durationMs"].is_null());
        // 断裂的前缀不能据数量猜测；真实轮次起点在断点之后，后续压缩才有完整所属范围。
        for (started, expected) in [(false, false), (true, true)] {
            let partial = dir.path().join(format!("partial-{started}.jsonl"));
            let boundary = if started {
                json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn"}})
                    .to_string()
            } else {
                String::new()
            };
            std::fs::write(
                &partial,
                format!(
                    "{}\ninvalid\n{boundary}\n{}\n{}\n",
                    lines[0], lines[1], lines[2]
                ),
            )
            .unwrap();
            let tr = trace(&partial, "thread").unwrap();
            assert!(!tr.complete);
            let mut items = [json!({"id":"id-only","type":"contextCompaction"})];
            decorate(&mut items, "turn", true, &[], &tr, &Receipts::new());
            assert_eq!(items[0]["compactionModel"].is_string(), expected);
        }
    }
}

impl CodexHarness {
    pub(super) async fn save_compaction_detail(
        &self,
        id: &str,
        turn: &str,
        item: &Value,
    ) -> anyhow::Result<()> {
        self.save_compaction_details(id, std::iter::once((turn, item)))
            .await
    }

    async fn save_compaction_details<'a>(
        &self,
        id: &str,
        items: impl Iterator<Item = (&'a str, &'a Value)>,
    ) -> anyhow::Result<()> {
        let mut store = self.store.lock().await;
        if store
            .data
            .deletions
            .get(id)
            .is_some_and(|d| d.state == "deleted")
        {
            return Ok(());
        }
        let mut rows = store
            .data
            .compaction_details
            .get(id)
            .cloned()
            .unwrap_or_default();
        let mut changed = false;
        for (turn, item) in items {
            if turn.is_empty() || bounded(&item["id"]).is_none() {
                continue;
            }
            let mut f = json!({"id":item["id"],"turn":turn});
            for &k in FIELDS {
                if !item[k].is_null() {
                    f[k] = item[k].clone();
                }
            }
            if let Some(index) = rows
                .iter()
                .position(|r| r["turn"] == turn && r["id"] == item["id"])
            {
                merge(&mut f, &rows[index]);
                if rows[index] == f {
                    continue;
                }
                rows.remove(index);
            }
            rows.push(f);
            if rows.len() > 512 {
                rows.remove(0);
            }
            changed = true;
        }
        if !changed {
            return Ok(());
        }
        // 一页历史至多一次原子写入；只保存型号/时间/数值，绝不复制会话正文或代理响应。
        store.commit(|d| {
            d.compaction_details.insert(id.into(), rows);
            Ok(())
        })
    }

    pub(super) async fn compaction_history_details(
        &self,
        id: &str,
        mut h: Value,
        from_first_item_page: bool,
    ) -> Value {
        let is_items = h["scope"] == "items";
        let has = h["transcriptEvents"]
            .as_array()
            .is_some_and(|rs| rs.iter().any(|r| r["item"]["type"] == "contextCompaction"))
            || h["data"].as_array().is_some_and(|rs| {
                rs.iter().any(|r| {
                    r["type"] == "contextCompaction"
                        || r["items"]
                            .as_array()
                            .is_some_and(|a| a.iter().any(|i| i["type"] == "contextCompaction"))
                })
            });
        if !has {
            return h;
        }
        let saved = self
            .store
            .lock()
            .await
            .data
            .compaction_details
            .get(id)
            .cloned()
            .unwrap_or_default();
        let home = self.codex_home_dir().await;
        let thread = id.to_owned();
        let (trace, receipts) = tokio::task::spawn_blocking(move || {
            let tr = crate::context_usage::rollout_path(&home, &thread)
                .and_then(|p| trace(&p, &thread))
                .unwrap_or_default();
            let receipts = if tr.turns.values().flatten().any(|c| c.response.is_some()) {
                crate::proxy_log::confirmed_response_models(&thread)
            } else {
                BTreeMap::new()
            };
            (tr, receipts)
        })
        .await
        .unwrap_or_default();
        let mut known = HashMap::new();
        let mut apply = |items: &mut [Value], turn: &str, full: bool| {
            decorate(items, turn, full, &saved, &trace, &receipts);
            for i in items.iter().filter(|i| i["type"] == "contextCompaction") {
                if let Some(key) = i["id"].as_str() {
                    known.insert((turn.to_owned(), key.to_owned()), i.clone());
                }
            }
        };
        if is_items {
            let turn = h["turnId"].as_str().unwrap_or("").to_owned();
            let full = h["itemsComplete"] == true && from_first_item_page;
            if let Some(items) = h["data"].as_array_mut() {
                apply(items, &turn, full);
            }
        } else if let Some(turns) = h["data"].as_array_mut() {
            for t in turns {
                let turn = t["id"].as_str().unwrap_or("").to_owned();
                let full = t["itemsComplete"] == true;
                if let Some(items) = t["items"].as_array_mut() {
                    apply(items, &turn, full);
                }
            }
        }
        if let Some(events) = h["transcriptEvents"].as_array_mut() {
            for e in events
                .iter_mut()
                .filter(|e| e["item"]["type"] == "contextCompaction")
            {
                let turn = e["turnId"].as_str().unwrap_or("").to_owned();
                if let Some(detail) = e["item"]["id"]
                    .as_str()
                    .and_then(|key| known.get(&(turn.clone(), key.into())))
                {
                    merge(&mut e["item"], detail);
                } else {
                    decorate(
                        std::slice::from_mut(&mut e["item"]),
                        &turn,
                        false,
                        &saved,
                        &trace,
                        &receipts,
                    );
                }
            }
        }
        // 经 rollout + 原生身份确认的资料也保存，后续代理日志淘汰或更换当前模型不改变已确认结果。
        let details = known
            .iter()
            .filter(|(_, item)| {
                rank(item["compactionModelSource"].as_str().unwrap_or("")) > 0
                    || item["recordedAtMs"].is_u64()
            })
            .map(|((turn, _), item)| (turn.as_str(), item));
        if let Err(e) = self.save_compaction_details(id, details).await {
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "harness.compaction_facts_save_failed",
                json!({"error":e.to_string()}),
            );
        }
        h
    }
}
