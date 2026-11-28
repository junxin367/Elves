//! 在官方 CLI 的本地会话转录中核对刚投递的普通文字。不把文件变大或管道写入当成送达。

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

#[derive(Clone)]
pub(super) struct Source {
    root: PathBuf,
    relative: PathBuf,
    new_session: bool,
}

// 只读末尾的一条旧记录和本次增量；巨大的工具结果或图片不应让一次发送扫描全部会话。
const READ_LIMIT: usize = 256 * 1024;
const ANCHOR_BYTES: usize = 256;

pub(super) fn unique_text(blocks: &[Value], turns: &[Value]) -> Option<String> {
    // 只核对单个完整文字块；图片不能凭配文确认，同文旧消息、未结案输入和包装标记不能按文字猜归属。
    let [block] = blocks else { return None };
    let text = block["text"].as_str()?;
    if block["type"] != "text"
        || text.len() > READ_LIMIT
        || text.trim_start().starts_with('/')
        || text.contains("<USER_REQUEST>")
        || text.contains("</USER_REQUEST>")
    {
        return None;
    }
    let repeated = turns.iter().any(|turn| {
        turn["items"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(turn.get("_pendingUserItem"))
            .any(|item| {
                item["type"] == "userMessage"
                    && item["content"]
                        .as_array()
                        .is_some_and(|parts| parts.iter().any(|p| p["text"].as_str() == Some(text)))
            })
    });
    (!repeated).then(|| text.to_owned())
}

impl Source {
    pub fn new(root: PathBuf, product: &str, native: &str, new_session: bool) -> Option<Self> {
        if !root.is_absolute()
            || !super::local_store::valid_app_data_dir(product)
            || !super::local_store::valid_native_id(native)
        {
            return None;
        }
        Some(Self {
            root,
            relative: Path::new(product)
                .join("brain")
                .join(native)
                .join(".system_generated/logs/transcript_full.jsonl"),
            new_session,
        })
    }

    fn open(&self) -> std::io::Result<File> {
        // 固定运行时实际使用的产品目录及 native ID；不跨实例寻找同名文件，不跟随目录链接读取其他文件。
        let path = self.root.join(&self.relative);
        let root = self.root.canonicalize()?;
        if path.canonicalize()? != root.join(&self.relative)
            || !fs::symlink_metadata(&self.root)?.is_dir()
        {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
        if !fs::symlink_metadata(&path)?.is_file() {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        File::open(path)
    }

    pub async fn baseline(self, first: bool) -> Option<Cursor> {
        // WSL 路径不可用时及时降级，不能为了提速反而让普通派发等待慢磁盘。
        tokio::time::timeout(
            Duration::from_millis(100),
            tokio::task::spawn_blocking(move || {
                let (offset, step, anchor) = match self.open() {
                    Ok(mut file) => {
                        let end = file.metadata().ok()?.len();
                        if end == 0 && first && self.new_session {
                            (0, None, vec![])
                        } else {
                            let start = end.saturating_sub(READ_LIMIT as u64);
                            file.seek(SeekFrom::Start(start)).ok()?;
                            let mut bytes = vec![0; (end - start) as usize];
                            file.read_exact(&mut bytes).ok()?;
                            let rows = bytes.strip_suffix(b"\n")?;
                            let last = rows.rsplit(|&b| b == b'\n').next()?;
                            if start > 0 && last.len() == rows.len() {
                                return None;
                            }
                            let step = serde_json::from_slice::<Value>(last).ok()?["step_index"]
                                .as_u64()?;
                            let anchor = bytes[bytes.len().saturating_sub(ANCHOR_BYTES)..].to_vec();
                            (end, Some(step), anchor)
                        }
                    }
                    // 只有本运行时刚创建的空会话允许没有基线；恢复的旧会话缺历史时不能误认后来回放的记录。
                    Err(e)
                        if e.kind() == std::io::ErrorKind::NotFound
                            && first
                            && self.new_session =>
                    {
                        (0, None, vec![])
                    }
                    Err(_) => return None,
                };
                Some(Cursor {
                    source: self,
                    offset,
                    step,
                    anchor,
                })
            }),
        )
        .await
        .ok()?
        .ok()?
    }
}

pub(super) struct Cursor {
    source: Source,
    offset: u64,
    step: Option<u64>,
    anchor: Vec<u8>,
}

impl Cursor {
    // Some(false) 表示证据有歧义，立即退出快速路径，不再跳过可疑记录继续寻找同名输入。
    fn inspect(&self, text: &str, since: u64) -> Option<bool> {
        // 常见的“还没落盘”只读一次元数据；文件增长才打开并校验路径与原末尾，避免反复读取 WSL 文件。
        match fs::symlink_metadata(self.source.root.join(&self.source.relative)) {
            Ok(meta) if !meta.is_file() => return Some(false),
            Ok(meta) if meta.len() == self.offset => return None,
            Err(e) if self.offset == 0 && e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => return Some(false),
            _ => {}
        }
        let mut file = match self.source.open() {
            Ok(file) => file,
            Err(e) if self.offset == 0 && e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => return Some(false),
        };
        let end = file.metadata().ok()?.len();
        let Some(delta) = end
            .checked_sub(self.offset)
            .filter(|&n| n <= READ_LIMIT as u64)
        else {
            return Some(false);
        };
        if delta == 0 {
            return None;
        }
        file.seek(SeekFrom::Start(self.offset - self.anchor.len() as u64))
            .ok()?;
        let mut bytes = vec![0; self.anchor.len() + delta as usize];
        file.read_exact(&mut bytes).ok()?;
        let Some(appended) = bytes.strip_prefix(self.anchor.as_slice()) else {
            return Some(false);
        };
        // 同次写入可能分段。不要消费半行；多条用户输入、步骤回退、历史重写均不能按正文猜归属。
        if !appended.ends_with(b"\n") {
            return None;
        }
        let mut matched = false;
        let mut previous = self.step;
        for line in appended[..appended.len() - 1].split(|&b| b == b'\n') {
            let Ok(row) = serde_json::from_slice::<Value>(line) else {
                return Some(false);
            };
            let Some(step) = row["step_index"].as_u64() else {
                return Some(false);
            };
            if previous.is_some_and(|s| step <= s) || previous.is_none() && step != 0 {
                return Some(false);
            }
            previous = Some(step);
            if !matched {
                let created = row["created_at"]
                    .as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .and_then(|t| u64::try_from(t.timestamp()).ok());
                // 官方转录按整秒记时，身份依赖已核对的追加位置、用户角色和完整输入，不能只按时刻或子串匹配。
                if row["type"] != "USER_INPUT"
                    || row["source"] != "USER_EXPLICIT"
                    || row["status"] != "DONE"
                    || !created.is_some_and(|t| t >= since / 1000 && t <= super::now_ms() / 1000)
                    || !row["content"].as_str().is_some_and(|raw| {
                        raw.strip_prefix("<USER_REQUEST>\n")
                            .and_then(|s| s.split_once("\n</USER_REQUEST>"))
                            .is_some_and(|(body, rest)| {
                                body == text
                                    && !rest.contains("</USER_REQUEST>")
                                    && !rest.contains("<USER_REQUEST>")
                                    && (rest.is_empty()
                                        || rest.starts_with("\n<ADDITIONAL_METADATA>"))
                            })
                    })
                {
                    return Some(false);
                }
                matched = true;
            } else if row["type"] == "USER_INPUT" {
                return Some(false);
            }
        }
        Some(matched)
    }

    pub async fn observe(self, text: String, since: u64) -> bool {
        let mut state = (self, text);
        loop {
            // 只在本条直接发送的原有 15 秒观察期内轮询；收到其他原生证据或失败后调用端立即取消。
            tokio::time::sleep(Duration::from_millis(200)).await;
            let Ok((back, result)) = tokio::task::spawn_blocking(move || {
                let result = state.0.inspect(&state.1, since);
                (state, result)
            })
            .await
            else {
                return false;
            };
            if let Some(confirmed) = result {
                return confirmed;
            }
            state = back;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn agy_input_receipt_does_not_guess_repeated_input_or_attachments() {
        let text = json!({"type":"text","text":"继续"});
        let item = json!({"type":"userMessage","content":[text.clone()]});
        assert_eq!(
            unique_text(std::slice::from_ref(&text), &[]).as_deref(),
            Some("继续")
        );
        for turn in [
            json!({"items":[item.clone()]}),
            json!({"items":[],"_pendingUserItem":item}),
        ] {
            assert!(unique_text(std::slice::from_ref(&text), &[turn]).is_none());
        }
        assert!(unique_text(&[text, json!({"type":"image","data":"fixture"})], &[]).is_none());
        assert!(unique_text(&[json!({"type":"text","text":"/compact"})], &[]).is_none());
    }

    #[tokio::test]
    async fn agy_input_receipt_requires_unambiguous_new_complete_native_record() {
        let dir = tempfile::tempdir().unwrap();
        let source = Source::new(dir.path().into(), "antigravity-cli", "audit", true).unwrap();
        let path = dir.path().join(&source.relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert!(
            source.clone().baseline(false).await.is_none(),
            "旧会话缺文件不允许从零核对"
        );
        let fresh = source.clone().baseline(true).await.unwrap();
        let base = "{\"step_index\":5,\"type\":\"GENERIC\"}\n";
        fs::write(&path, base).unwrap();
        let cursor = source.baseline(false).await.unwrap();
        let now = super::super::now_ms();
        let row = json!({"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","step_index":6,
            "created_at":chrono::Utc::now().to_rfc3339(), "content":"<USER_REQUEST>\n完整内容\n</USER_REQUEST>"});
        assert_eq!(
            cursor.inspect("完整内容", now),
            None,
            "基线本身不是接收证据"
        );
        fs::write(&path, format!("{base}{row}")).unwrap();
        assert_eq!(cursor.inspect("完整内容", now), None, "不能读半行");
        fs::write(&path, format!("{base}{row}\n")).unwrap();
        assert_eq!(cursor.inspect("完整内容", now), Some(true));
        assert_eq!(cursor.inspect("内容", now), Some(false));
        assert_eq!(
            fresh.inspect("完整内容", now),
            Some(false),
            "新会话不认旧步骤"
        );
        for (key, value) in [
            ("source", json!("MODEL")),
            ("step_index", json!(5)),
            ("type", json!("SYSTEM_MESSAGE")),
            ("status", json!("ERROR")),
            ("created_at", json!("2001-01-01T00:00:00Z")),
            (
                "content",
                json!("<USER_REQUEST>\n完整内容\n</USER_REQUEST>\n篡改"),
            ),
        ] {
            let mut changed = row.clone();
            changed[key] = value;
            fs::write(&path, format!("{base}{changed}\n")).unwrap();
            assert_eq!(cursor.inspect("完整内容", now), Some(false), "{key}");
        }
        let mut second = row.clone();
        second["step_index"] = json!(7);
        fs::write(&path, format!("{base}{row}\n{second}\n")).unwrap();
        assert_eq!(
            cursor.inspect("完整内容", now),
            Some(false),
            "多条新输入不猜归属"
        );
        fs::write(&path, format!("{}{row}\n", base.replace('5', "4"))).unwrap();
        assert_eq!(
            cursor.inspect("完整内容", now),
            Some(false),
            "即使长度相同也不能跨历史改写"
        );
        let mut first = row;
        first["step_index"] = json!(0);
        fs::write(&path, format!("{first}\n")).unwrap();
        assert_eq!(fresh.inspect("完整内容", now), Some(true));
    }
}
