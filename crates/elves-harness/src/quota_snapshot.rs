//! 会话额度只持久保留各实例、当前已确认账号和模型分组的最后一对真实窗口。
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
};

const LIMIT: u64 = 256 * 1024;
const WINDOWS: [&str; 2] = ["fiveHour", "sevenDay"];

pub(crate) fn binding(backend: &str, config: &impl Serialize) -> String {
    crate::hash(&json!([backend, config]).to_string())
}
fn reset(v: &Value) -> Option<i64> {
    v.as_i64()
        .and_then(|n| {
            if n > 0 && n < 1_000_000_000_000 {
                n.checked_mul(1000)
            } else {
                Some(n)
            }
        })
        .or_else(|| {
            chrono::DateTime::parse_from_rfc3339(v.as_str()?)
                .ok()
                .map(|t| t.timestamp_millis())
        })
}
fn pair(v: &Value, at: u64) -> bool {
    at > 0
        && WINDOWS.iter().all(|k| {
            let b = &v[*k];
            b["usedPercentage"]
                .as_f64()
                .is_some_and(|n| (0.0..=100.0).contains(&n))
                && (b["resetsAt"].is_null()
                    || reset(&b["resetsAt"]).is_some_and(|n| n > 0 && n as u64 > at))
        })
}
pub(crate) fn historical(record: &Value, now: u64) -> bool {
    record["historical"] == true
        || WINDOWS
            .iter()
            .any(|k| reset(&record[*k]["resetsAt"]).is_some_and(|t| t <= now as i64))
}
fn merge_metrics(target: &mut Value, source: &Value, key: &str, backend: &str) {
    let (a, b) = (&target[key], &source[key]);
    let Some(end) = reset(&a["resetsAt"]) else {
        return;
    };
    if reset(&b["resetsAt"]) != Some(end) {
        return;
    }
    let m = &b["metrics"];
    let expected = if backend == "antigravity-native-usage" {
        "antigravity-local-journal"
    } else {
        "claude-wsl-local-journal"
    };
    let start = end
        - if key == "fiveHour" {
            18_000_000
        } else {
            604_800_000
        };
    if m["source"] != expected || m["windowEndMs"] != end || m["windowStartMs"] != start {
        return;
    }
    let mut out = a["metrics"].clone();
    if !out.is_object() {
        out = json!({"source":expected,"windowEndMs":end,"windowStartMs":start});
    }
    for (field, time, basis) in [
        ("usedTokens", "tokensObservedAtMs", "tokensUsedPercentage"),
        ("usedCostUsd", "costObservedAtMs", "costUsedPercentage"),
    ] {
        let at = m[time]
            .as_u64()
            .or_else(|| m["observedAtMs"].as_u64())
            .unwrap_or(0);
        if at > 0
            && at <= crate::now_ms() + 30_000
            && (at > out[time].as_u64().unwrap_or(0) || out[field].is_null())
            && m[field].as_f64().is_some_and(|n| n.is_finite() && n >= 0.)
        {
            out[field] = m[field].clone();
            out[time] = json!(at);
            out[basis] = m[basis]
                .as_f64()
                .filter(|p| (0.0..=100.0).contains(p))
                .map_or_else(|| b["usedPercentage"].clone(), |p| json!(p));
            out["observedAtMs"] = json!(at.max(out["observedAtMs"].as_u64().unwrap_or(0)));
        }
    }
    if out["observedAtMs"].is_u64() {
        target[key]["metrics"] = out;
    }
}
fn merge_group(old: &Value, new: &Value, at: u64, source: &str, details: bool) -> Value {
    let mut result = if details {
        old.clone()
    } else if pair(new, at) && at >= old["observedAtMs"].as_u64().unwrap_or(0) {
        let mut v = new.clone();
        v["observedAtMs"] = json!(at);
        v["historical"] = json!(false);
        for key in WINDOWS {
            if let Some(obj) = v[key].as_object_mut() {
                obj.remove("metrics");
            }
            merge_metrics(&mut v, old, key, source);
        }
        v
    } else if old.is_object() {
        let mut v = old.clone();
        v["historical"] = json!(true);
        v
    } else {
        return Value::Null;
    };
    if result.is_object() {
        for key in WINDOWS {
            merge_metrics(&mut result, new, key, source);
        }
    }
    result
}

fn merge(old: &Value, new: &Value, details: bool) -> Value {
    let source = new["source"]
        .as_str()
        .or_else(|| old["source"].as_str())
        .unwrap_or("");
    let at = new["observedAtMs"]
        .as_u64()
        .filter(|v| *v <= crate::now_ms() + 30_000)
        .unwrap_or(0);
    if source == "claude-native-usage" {
        return merge_group(old, new, at, source, details);
    }
    if source != "antigravity-native-usage" {
        return Value::Null;
    }
    let items = |v: &Value| v["groups"].as_array().cloned().unwrap_or_default();
    let previous = items(old);
    let incoming = items(new);
    let mut ids = Vec::new();
    for g in incoming.iter().chain(&previous) {
        if let Some(id) = g["id"].as_str().filter(|s| !s.is_empty() && s.len() <= 80)
            && !ids.contains(&id)
            && ids.len() < 32
        {
            ids.push(id);
        }
    }
    let groups: Vec<_> = ids
        .iter()
        .filter_map(|id| {
            let o = previous
                .iter()
                .find(|g| g["id"] == *id)
                .unwrap_or(&Value::Null);
            let n = incoming
                .iter()
                .find(|g| g["id"] == *id)
                .unwrap_or(&Value::Null);
            let v = merge_group(o, n, at, source, details);
            v.is_object().then_some(v)
        })
        .collect();
    let Some(primary) = groups
        .iter()
        .find(|g| g["id"] == "gemini")
        .or_else(|| groups.first())
    else {
        return Value::Null;
    };
    let mut result = if new.is_object() && !details {
        new.clone()
    } else {
        old.clone()
    };
    for key in WINDOWS {
        result[key] = primary[key].clone();
    }
    result["observedAtMs"] = json!(
        groups
            .iter()
            .filter_map(|g| g["observedAtMs"].as_u64())
            .max()
            .unwrap_or(0)
    );
    result["groups"] = json!(groups);
    result
}

struct Entry {
    scope: String,
    usage: Value,
    durable: bool,
}
pub(crate) struct Snapshots {
    root: PathBuf,
    slots: Mutex<HashMap<String, Entry>>,
}
impl Snapshots {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.into(),
            slots: Mutex::new(HashMap::new()),
        }
    }
    fn path(&self, id: &str) -> PathBuf {
        self.root
            .join(format!("quota-snapshot-v1-{}.json", crate::hash(id)))
    }
    fn disk(&self, id: &str, scope: &str) -> Option<Value> {
        let path = self.path(id);
        let meta = path.symlink_metadata().ok()?;
        if !meta.is_file() || meta.len() > LIMIT {
            return None;
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .ok()?
            .take(LIMIT + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > LIMIT {
            return None;
        }
        let v: Value = serde_json::from_slice(&bytes).ok()?;
        if v["version"] != 1 || v["binding"] != id || v["scope"] != scope {
            return None;
        }
        let mut usage = v["usage"].clone();
        let valid = |g: &Value| {
            g["observedAtMs"]
                .as_u64()
                .is_some_and(|at| at <= crate::now_ms() + 30_000 && pair(g, at))
        };
        match usage["source"].as_str()? {
            "claude-native-usage" if valid(&usage) => {
                usage["historical"] = json!(true);
            }
            "antigravity-native-usage" => {
                let groups = usage.get_mut("groups")?.as_array_mut()?;
                groups.retain(valid);
                if groups.is_empty() {
                    return None;
                }
                for g in groups {
                    g["historical"] = json!(true);
                }
            }
            _ => return None,
        }
        Some(usage)
    }
    fn save(&self, id: &str, e: &Entry) {
        let record = json!({"version":1,"binding":id,"scope":if e.durable{&e.scope}else{""},"usage":if e.durable{&e.usage}else{&Value::Null}});
        let ok = serde_json::to_vec(&record).is_ok_and(|b| {
            b.len() as u64 <= LIMIT && crate::settings::atomic_write(&self.path(id), &b).is_ok()
        });
        if !ok {
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "quota.snapshot_write_failed",
                json!({}),
            );
        }
    }
    // 只由实例已核对的身份选择磁盘记录；无可核对账号时不跨进程恢复，也不从网页接收缓存路径或账号。
    pub(crate) fn bind(&self, id: &str, scope: &str, durable: bool) -> Value {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = slots.get(id)
            && e.scope == scope
            && e.durable == durable
        {
            return e.usage.clone();
        }
        let on_disk = durable.then(|| self.disk(id, scope)).flatten();
        let e = Entry {
            scope: scope.into(),
            usage: on_disk.unwrap_or(Value::Null),
            durable,
        };
        if durable && e.usage.is_null() && self.path(id).exists() {
            self.save(id, &e);
        }
        let result = e.usage.clone();
        slots.insert(id.into(), e);
        result
    }
    pub(crate) fn get(&self, id: &str, scope: &str) -> Value {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .filter(|e| e.scope == scope)
            .map(|e| e.usage.clone())
            .unwrap_or(Value::Null)
    }
    pub(crate) fn matches(&self, id: &str, scope: &str) -> bool {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .is_some_and(|e| e.scope == scope)
    }
    pub(crate) fn clear(&self, id: &str, scope: &str) {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = slots.get_mut(id).filter(|e| e.scope == scope) {
            e.usage = Value::Null;
            if e.durable || self.path(id).exists() {
                self.save(id, e);
            }
        }
    }
    // 身份改变后的迟到刷新和明细不能写入新账号；同一窗口的金额与 Token 可分别补齐。
    pub(crate) fn publish(&self, id: &str, scope: &str, new: &Value, details: bool) -> Value {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let Some(e) = slots.get_mut(id).filter(|e| e.scope == scope) else {
            return Value::Null;
        };
        let next = merge(&e.usage, new, details);
        if next != e.usage {
            e.usage = next;
            if e.durable {
                self.save(id, e);
            }
        }
        e.usage.clone()
    }
    pub(crate) fn has_group(v: &Value, group: &str) -> bool {
        v["groups"]
            .as_array()
            .is_some_and(|g| g.iter().any(|x| x["id"] == group))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_snapshot_survives_restart_without_mixing_periods_accounts_or_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let id = "wsl-one";
        let a = "user-one";
        let b = "user-two";
        let at = crate::now_ms() - 10 * 86_400_000;
        let end = at + 18_000_000;
        let record = json!({"source":"claude-native-usage","observedAtMs":at,
            "fiveHour":{"usedPercentage":10,"resetsAt":end},
            "sevenDay":{"usedPercentage":20,"resetsAt":at+604_800_000}});
        let half = json!({"source":"claude-native-usage","observedAtMs":at+500,
            "sevenDay":{"usedPercentage":70,"resetsAt":at+604_800_000}});
        let store = Snapshots::new(dir.path());
        assert!(store.bind(id, a, true).is_null());
        assert!(store.publish(id, a, &half, false).is_null());
        let full = store.publish(id, a, &record, false);
        assert_eq!(full["historical"], false);
        let mut details = record.clone();
        details["fiveHour"]["metrics"] = json!({"source":"claude-wsl-local-journal","observedAtMs":at,
            "windowStartMs":at,"windowEndMs":end,"usedTokens":0});
        store.publish(id, a, &details, true);
        let old = store.publish(id, a, &half, false);
        assert_eq!(old["observedAtMs"], at);
        assert_eq!(old["historical"], true);
        assert_eq!(old["sevenDay"]["usedPercentage"], 20);
        drop(store);
        let store = Snapshots::new(dir.path());
        let restored = store.bind(id, a, true);
        assert_eq!(restored, old);
        assert_eq!(restored["fiveHour"]["metrics"]["usedTokens"], 0);
        assert!(restored["fiveHour"]["metrics"]["usedCostUsd"].is_null());
        let mut next = record;
        next["observedAtMs"] = json!(end + 10);
        next["fiveHour"]["resetsAt"] = json!(end + 18_000_000);
        let next = store.publish(id, a, &next, false);
        assert_eq!(next["historical"], false);
        assert!(next["fiveHour"]["metrics"].is_null());
        assert!(store.bind(id, b, true).is_null());
        assert!(
            store.publish(id, a, &details, true).is_null(),
            "换号后的迟到结果不能写回"
        );
        assert!(
            Snapshots::new(dir.path()).bind(id, a, true).is_null(),
            "换号后磁盘不恢复旧授权"
        );
    }

    #[test]
    fn quota_snapshot_agy_publishes_each_model_group_as_an_indivisible_pair() {
        let now = crate::now_ms();
        let payload = |at, groups: Value| json!({"source":"antigravity-native-usage","observedAtMs":at,"groups":groups});
        let group = |name: &str, n| {
            json!({"id":name,"fiveHour":{"usedPercentage":n,"resetsAt":null},
            "sevenDay":{"usedPercentage":0,"resetsAt":null}})
        };
        let initial = merge(
            &Value::Null,
            &payload(
                now - 200,
                json!([group("gemini", 10), group("thirdParty", 90)]),
            ),
            false,
        );
        let later = merge(
            &initial,
            &payload(
                now,
                json!([{"id":"gemini","sevenDay":{"usedPercentage":42}},group("thirdParty",25)]),
            ),
            false,
        );
        assert_eq!(later["groups"][0]["observedAtMs"], now - 200);
        assert_eq!(later["groups"][0]["historical"], true);
        assert_eq!(later["groups"][0]["sevenDay"]["usedPercentage"], 0);
        assert_eq!(later["groups"][1]["observedAtMs"], now);
        assert_eq!(later["groups"][1]["historical"], false);
        assert_eq!(later["groups"][1]["fiveHour"]["usedPercentage"], 25);
    }
}
