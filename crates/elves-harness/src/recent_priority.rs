//! 同一 Harness 的本机和远程入口共用优先事项；客户端目录和已读状态不是成员清单。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use serde_json::{Value, json};

use super::{ApiError, notification_thread_id};

// 每次只发送有限数量的会话 ID，不限制持久名单容量；与 sidebar/list.js 的操作批量一致。
pub(crate) const MAX_CLEAR_IDS: usize = 4096;

pub(crate) struct RecentPriority {
    path: PathBuf,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    ids: Vec<String>,
    revision: u64,
    load_failed: bool,
}

fn main_id(item: &Value) -> Option<&str> {
    let id = notification_thread_id(&item["id"])?;
    (item["archived"] != true
        && item["ephemeral"] != true
        && item["serviceSide"] != true
        && item["parentThreadId"]
            .as_str()
            .is_none_or(|parent| parent == id)
        && item["attachedToThreadId"].is_null()
        && item["threadSource"] != "subagent"
        && item["source"]["subAgent"].is_null()
        && item["source"]["subagent"].is_null()
        && item["priority"]["eligible"] != false)
        .then_some(id)
}

fn status(item: &Value) -> Option<&str> {
    item["status"]
        .as_str()
        .or_else(|| item["status"]["type"].as_str())
}

fn attention(item: &Value) -> Option<u8> {
    let side = &item["sideThreadAttention"];
    let own = item["priority"]["attentionState"].as_str();
    if own == Some("waiting")
        || status(item) == Some("waiting")
        || side["waiting"] == true
        || item["requests"].as_array().is_some_and(|r| !r.is_empty())
        || item["status"]["activeFlags"]
            .as_array()
            .is_some_and(|flags| {
                flags
                    .iter()
                    .any(|flag| flag == "waitingOnApproval" || flag == "waitingOnUserInput")
            })
    {
        Some(0)
    } else if own == Some("unread") || item["hasUnreadTurn"] == true || side["unread"] == true {
        Some(1)
    } else if own == Some("active")
        || item["priority"]["active"] == true
        || side["active"] == true
        || matches!(status(item), Some("active" | "inProgress" | "running"))
    {
        Some(2)
    } else {
        None
    }
}

fn rows(sidebar: &Value) -> impl Iterator<Item = &Value> {
    sidebar["recent"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(sidebar["pinnedThreads"].as_array().into_iter().flatten())
}

fn clearable(sidebar: &Value) -> HashSet<&str> {
    let mut eligible = HashSet::new();
    let mut blocked = HashSet::new();
    for item in rows(sidebar) {
        let Some(id) = main_id(item) else { continue };
        // 用户要求安卓打开时不清出优先事项。只有点击清除时才使用这组条件，
        // 且必须有明确的空闲和已读证据；侧边关系未读完整时不能判为没有待处理事项。
        if matches!(status(item), Some("idle" | "notLoaded"))
            && item["hasUnreadTurn"] == false
            && item["requestsKnown"] != false
            && attention(item).is_none()
            && (matches!(item["backend"].as_str(), Some("claude" | "antigravity"))
                || sidebar["sideThreadsComplete"] != false)
        {
            eligible.insert(id);
        } else {
            blocked.insert(id);
        }
    }
    eligible.retain(|id| !blocked.contains(id));
    eligible
}

impl RecentPriority {
    pub(crate) fn new(directory: &Path) -> Self {
        let path = directory.join("harness-recent-priority.json");
        let loaded = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Vec<String>>(&bytes).context("优先事项名单损坏"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        };
        let state = match loaded {
            Ok(mut ids) => {
                let mut seen = HashSet::new();
                ids.retain(|id| {
                    notification_thread_id(&json!(id)).is_some() && seen.insert(id.clone())
                });
                State {
                    ids,
                    ..State::default()
                }
            }
            Err(error) => {
                Self::log_failure(&error);
                // 损坏的清单不按空清单覆盖；仍可正常使用会话和列表。
                State {
                    load_failed: true,
                    ..State::default()
                }
            }
        };
        Self {
            path,
            state: Mutex::new(state),
        }
    }

    fn log_failure(error: &anyhow::Error) {
        let _ = crate::diagnostic_log::append_diagnostic_log(
            "harness.recent_priority_storage_failed",
            json!({"error":format!("{error:#}")}),
        );
    }

    fn save(&self, state: &mut State, ids: Vec<String>) -> anyhow::Result<()> {
        crate::settings::atomic_write(&self.path, &serde_json::to_vec(&ids)?)?;
        state.ids = ids;
        state.revision += 1;
        Ok(())
    }

    pub(crate) fn revision(&self) -> u64 {
        self.state.lock().unwrap().revision
    }

    pub(crate) fn runtime_changed(&self) {
        self.state.lock().unwrap().revision += 1;
    }

    pub(crate) fn promote(&self, id: &str) {
        if notification_thread_id(&json!(id)).is_none() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        state.revision += 1;
        if state.load_failed || state.ids.iter().any(|current| current == id) {
            return;
        }
        let mut ids = state.ids.clone();
        ids.insert(0, id.to_owned());
        if let Err(error) = self.save(&mut state, ids) {
            Self::log_failure(&error);
        }
    }

    pub(crate) fn forget(&self, id: &str) {
        let mut state = self.state.lock().unwrap();
        state.revision += 1;
        if !state.load_failed && state.ids.iter().any(|current| current == id) {
            let ids = state
                .ids
                .iter()
                .filter(|current| *current != id)
                .cloned()
                .collect();
            if let Err(error) = self.save(&mut state, ids) {
                Self::log_failure(&error);
            }
        }
    }

    pub(crate) fn annotate(&self, sidebar: &mut Value, observed: u64) -> bool {
        let mut state = self.state.lock().unwrap();
        // 启动、分页、筛选和已读刷新都不会删除成员。旧目录即使带有未读，
        // 也不得在另一端清除之后重新加入；只返回当前已保存的清单。
        let current = state.revision == observed;
        let mut changed = false;
        if current && !state.load_failed {
            let mut known: HashSet<_> = state.ids.iter().map(String::as_str).collect();
            let mut added: Vec<_> = rows(sidebar)
                .filter_map(|item| {
                    let id = main_id(item)?;
                    let weight = attention(item)?;
                    known.insert(id).then_some((
                        weight,
                        item["recencyAt"]
                            .as_u64()
                            .or_else(|| item["updatedAt"].as_u64())
                            .unwrap_or(0),
                        id,
                    ))
                })
                .collect();
            if !added.is_empty() {
                added.sort_unstable_by_key(|(weight, time, id)| {
                    (*weight, std::cmp::Reverse(*time), *id)
                });
                let mut ids = state.ids.clone();
                ids.extend(added.into_iter().map(|(_, _, id)| id.to_owned()));
                match self.save(&mut state, ids) {
                    Ok(()) => changed = true,
                    Err(error) => Self::log_failure(&error),
                }
            }
        }
        if !state.load_failed {
            let allowed = if current {
                let eligible = clearable(sidebar);
                state
                    .ids
                    .iter()
                    .filter(|id| eligible.contains(id.as_str()))
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            sidebar["recentPriorityClearableIds"] = json!(allowed);
            sidebar["recentPriorityIds"] = json!(state.ids);
        }
        changed
    }

    pub(crate) fn clear(
        &self,
        sidebar: &Value,
        requested: &HashSet<&str>,
        observed: u64,
    ) -> anyhow::Result<usize> {
        let mut state = self.state.lock().unwrap();
        if state.load_failed {
            return Err(ApiError::new(503, "优先事项存储暂不可用，未移出会话").into());
        }
        if state.revision != observed {
            return Err(ApiError::new(409, "会话状态已变化，请刷新优先事项后重试").into());
        }
        let eligible = clearable(sidebar);
        let retained = state
            .ids
            .iter()
            .filter(|id| !requested.contains(id.as_str()) || !eligible.contains(id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let removed = state.ids.len() - retained.len();
        if removed != 0 {
            self.save(&mut state, retained).map_err(|error| {
                Self::log_failure(&error);
                ApiError::new(503, "无法保存优先事项，未移出会话")
            })?;
        }
        Ok(removed)
    }
}
