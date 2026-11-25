//! Claude 会话存档的分文件持久化与改动追踪。
//!
//! 用户要求缩短操作等待：原先每次状态变更都整份重写 `remote-claude-sessions.json`，
//! 实测该文件已达 62 MB（逼近 64 MiB 上限），克隆、序列化和同步落盘合计约 1.1 秒，持锁期间其他请求全部排队。
//! 现在的约定（改动前先核对这里，不要把正文放回主存档）：
//! - 主存档仍是 `remote-claude-sessions.json`，保存全部会话的元数据，格式不变，回退旧版本仍可读取；
//!   旧版本读到空正文只会重新读取原生历史。
//! - 共享会话（`shared`）的正文只是原生 JSONL 的投影缓存，单独放在 `claude-session-turns/<会话 ID>.json`，
//!   仅在该会话正文改动后才重写；非共享的旧会话没有原生日志可重建，正文仍留在主存档。
//! - 改动由 `Turns` 与 `Conversations` 的可变访问自动记录，调用方不需要手工标记；
//!   每个文件各自限制 64 MiB，不再由所有会话共用一个上限。
//! - 写入统一经单个写入任务按提交顺序执行，序列化发生在持锁期间，较晚的内容不会被较早的内容覆盖。

use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::{Conversation, Saved};

const MAIN_FILE: &str = "remote-claude-sessions.json";
const TURNS_DIRECTORY: &str = "claude-session-turns";
// 迁移前的完整单文件备份：回退到旧版本时可手工换回，保留迁移前的全部正文。
const LEGACY_BACKUP: &str = "remote-claude-sessions.json.pre-sharding";
/// 单个文件的大小上限；主存档与每个正文文件各自计算。
pub(super) const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;

/// 会话正文。可变访问（`DerefMut`、`&mut` 迭代）都会递增 `rev`，`flushed` 记录已写入侧文件的版本，
/// 两者不同即表示侧文件落后。`apart` 为真时主存档只写空数组，正文由侧文件保存。
#[derive(Default, Clone)]
pub(super) struct Turns {
    items: Vec<Value>,
    rev: u64,
    flushed: Option<u64>,
    apart: bool,
}

impl Turns {
    /// 以新内容整体替换并记入改动；调用方已确认内容有变化时使用，省去再比较一遍整份正文。
    pub(super) fn set(&mut self, items: Vec<Value>) {
        self.items = items;
        self.rev += 1;
    }

    /// 不记入改动的可变访问：启动归一化（每次加载都会重复且幂等）和只补内存数字的统计回填使用，
    /// 它们自己不应触发写盘，改动会随后续任意一次落盘一并写出。
    pub(super) fn untracked(&mut self) -> &mut Vec<Value> {
        &mut self.items
    }
}

impl From<Vec<Value>> for Turns {
    fn from(items: Vec<Value>) -> Self {
        Self {
            items,
            ..Self::default()
        }
    }
}

impl Deref for Turns {
    type Target = Vec<Value>;

    fn deref(&self) -> &Vec<Value> {
        &self.items
    }
}

impl DerefMut for Turns {
    fn deref_mut(&mut self) -> &mut Vec<Value> {
        self.rev += 1;
        &mut self.items
    }
}

impl<'a> IntoIterator for &'a Turns {
    type Item = &'a Value;
    type IntoIter = std::slice::Iter<'a, Value>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

impl<'a> IntoIterator for &'a mut Turns {
    type Item = &'a mut Value;
    type IntoIter = std::slice::IterMut<'a, Value>;

    fn into_iter(self) -> Self::IntoIter {
        self.rev += 1;
        self.items.iter_mut()
    }
}

impl PartialEq for Turns {
    fn eq(&self, other: &Self) -> bool {
        self.items == other.items
    }
}

impl PartialEq<Vec<Value>> for Turns {
    fn eq(&self, other: &Vec<Value>) -> bool {
        &self.items == other
    }
}

impl PartialEq<Turns> for Vec<Value> {
    fn eq(&self, other: &Turns) -> bool {
        self == &other.items
    }
}

impl std::fmt::Debug for Turns {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.items.fmt(formatter)
    }
}

impl Serialize for Turns {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.apart {
            // 旧版本要求 `turns` 字段必须存在；写空数组，回退后由原生历史重新投影。
            serializer.collect_seq(std::iter::empty::<&Value>())
        } else {
            self.items.serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for Turns {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::<Value>::deserialize(deserializer).map(Self::from)
    }
}

/// 一次持久修改开始后各会话改动前的样子；`None` 表示该会话原先不存在。
#[derive(Clone)]
struct Undo {
    rev: u64,
    touched: BTreeMap<String, Option<Conversation>>,
}

/// 会话表。只读访问沿用 `BTreeMap` 的方法；修改必须经本类型的方法，才能被持久化与回滚逻辑看到
/// （没有实现 `DerefMut`，漏掉的写法会在编译期报错）。
#[derive(Default, Clone)]
pub(super) struct Conversations {
    map: BTreeMap<String, Conversation>,
    /// 主存档版本：任何会话被可变访问、新增或显式 `touch` 时递增。
    rev: u64,
    flushed: Option<u64>,
    undo: Option<Undo>,
}

impl Deref for Conversations {
    type Target = BTreeMap<String, Conversation>;

    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl Serialize for Conversations {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.map.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Conversations {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        BTreeMap::deserialize(deserializer).map(|map| Self {
            map,
            ..Self::default()
        })
    }
}

impl Conversations {
    /// 取得可变会话并记入改动；持久修改进行中还会保存改动前的副本，写盘失败时据此回滚。
    pub(super) fn get_mut(&mut self, id: &str) -> Option<&mut Conversation> {
        if !self.map.contains_key(id) {
            return None;
        }
        self.rev += 1;
        if let Some(undo) = &mut self.undo
            && !undo.touched.contains_key(id)
        {
            undo.touched
                .insert(id.to_owned(), self.map.get(id).cloned());
        }
        self.map.get_mut(id)
    }

    /// 不记入改动的可变访问，只用于只改内存视图的字段（如终端预览）或调用方自己精确判断是否改动的场合；
    /// 确有需要落盘的改动时，调用方必须再调用 `touch`。
    pub(super) fn get_mut_silent(&mut self, id: &str) -> Option<&mut Conversation> {
        self.map.get_mut(id)
    }

    /// 标记主存档需要重写，不保存回滚副本；与 `get_mut_silent` 配合使用。
    pub(super) fn touch(&mut self) {
        self.rev += 1;
    }

    pub(super) fn insert(
        &mut self,
        id: String,
        mut conversation: Conversation,
    ) -> Option<Conversation> {
        // 放入的对象可能是较早读出的副本，版本号与磁盘上的正文已经对不上：一律按落盘状态未知处理，下次写出。
        conversation.turns.flushed = None;
        self.rev += 1;
        if let Some(undo) = &mut self.undo
            && !undo.touched.contains_key(&id)
        {
            undo.touched.insert(id.clone(), self.map.get(&id).cloned());
        }
        self.map.insert(id, conversation)
    }

    /// 全部会话的可变访问（记入改动，持久修改进行中会保存每个会话的回滚副本，代价与全部正文相当）。
    /// 生产路径应按 ID 取单个会话；这里只给批量修改的测试使用，不进入正式构建。
    #[cfg(test)]
    pub(super) fn values_mut(&mut self) -> impl Iterator<Item = &mut Conversation> {
        self.rev += 1;
        if let Some(undo) = &mut self.undo {
            for (id, conversation) in &self.map {
                undo.touched
                    .entry(id.clone())
                    .or_insert_with(|| Some(conversation.clone()));
            }
        }
        self.map.values_mut()
    }

    /// 启动归一化专用：不记入改动，归一化每次加载都会重复执行，不需要因此重写存档。
    pub(super) fn values_mut_untracked(&mut self) -> impl Iterator<Item = &mut Conversation> {
        self.map.values_mut()
    }

    /// 开始一次持久修改：此后被修改的会话会保存改动前副本。
    pub(super) fn begin(&mut self) {
        self.undo = Some(Undo {
            rev: self.rev,
            touched: BTreeMap::new(),
        });
    }

    /// 本次持久修改触碰过的会话 ID，用于只写这些会话的正文。
    pub(super) fn touched_ids(&self) -> BTreeSet<String> {
        self.undo
            .as_ref()
            .map(|undo| undo.touched.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub(super) fn commit(&mut self) {
        self.undo = None;
    }

    /// 放弃本次持久修改：被触碰的会话恢复为改动前，新增的会话移除。
    pub(super) fn rollback(&mut self) {
        if let Some(undo) = self.undo.take() {
            for (id, before) in undo.touched {
                match before {
                    Some(conversation) => {
                        self.map.insert(id, conversation);
                    }
                    None => {
                        self.map.remove(&id);
                    }
                }
            }
            self.rev = undo.rev;
        }
    }

    /// 读取共享会话的正文侧文件，并把刚从主存档读出的内容标记为已落盘。
    /// 返回 true 表示主存档里仍有旧布局内联的共享会话正文，需要调用 `migrate_legacy_layout`。
    pub(super) fn load_turns(&mut self, directory: &Path) -> bool {
        self.rev = 0;
        self.flushed = Some(0);
        self.undo = None;
        let mut legacy = false;
        for conversation in self.map.values_mut() {
            conversation.turns.rev = 0;
            conversation.turns.flushed = Some(0);
            let Some(path) =
                turns_path(directory, &conversation.id).filter(|_| conversation.shared)
            else {
                continue;
            };
            if !conversation.turns.items.is_empty() {
                // 旧布局，或回退旧版本后写回的主存档：内联正文更新，以它为准并在迁移时重写侧文件。
                legacy = true;
                continue;
            }
            conversation.turns.apart = true;
            match std::fs::read(&path) {
                Ok(bytes) => match serde_json::from_slice::<Vec<Value>>(&bytes) {
                    Ok(items) => conversation.turns.items = items,
                    Err(failure) => {
                        // 侧文件只是缓存：损坏时按空正文处理，等原生历史重新投影后覆盖。
                        conversation.turns.flushed = None;
                        report(
                            "claude.store.turns-corrupt",
                            json!({"conversation":conversation.id,"error":failure.to_string()}),
                        );
                    }
                },
                // 没有侧文件等同空正文（新发现的会话，或尚未读取过历史）。
                Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {}
                Err(failure) => {
                    conversation.turns.flushed = None;
                    report(
                        "claude.store.turns-unreadable",
                        json!({"conversation":conversation.id,"error":failure.to_string()}),
                    );
                }
            }
        }
        legacy
    }
}

/// 会话正文侧文件的路径；只接受规范小写 UUID 的会话 ID，文件名不可能含路径分隔符或保留字。
fn turns_path(directory: &Path, id: &str) -> Option<PathBuf> {
    let uuid = id.strip_prefix("claude-")?;
    (uuid::Uuid::parse_str(uuid).ok()?.hyphenated().to_string() == uuid)
        .then(|| directory.join(TURNS_DIRECTORY).join(format!("{id}.json")))
}

fn report(event: &str, detail: Value) {
    super::diagnose_event(event, detail);
}

/// 已序列化、等待写入的一批文件，以及写入成功后需要更新的落盘版本。
pub(super) struct Prepared {
    pub(super) files: Vec<(PathBuf, Vec<u8>)>,
    pub(super) marks: Marks,
}

pub(super) struct Marks {
    main: Option<u64>,
    turns: Vec<(String, u64)>,
}

impl Saved {
    /// 序列化尚未落盘的改动。`only` 为 `Some` 时只写这些会话的正文（持久修改只等自己触碰的会话，
    /// 其余会话的正文留给合并落盘），为 `None` 时写出全部。必须在持有会话锁时调用。
    pub(super) fn prepare(
        &mut self,
        directory: &Path,
        only: Option<&BTreeSet<String>>,
    ) -> anyhow::Result<Option<Prepared>> {
        let conversations = &mut self.conversations;
        let main_rev = conversations.rev;
        let mut main_dirty = conversations.flushed != Some(main_rev);
        let mut files = Vec::new();
        let mut turns = Vec::new();
        for (key, conversation) in conversations.map.iter_mut() {
            let path = turns_path(directory, &conversation.id).filter(|_| conversation.shared);
            let apart = path.is_some();
            let moved = conversation.turns.apart != apart;
            if moved {
                conversation.turns.apart = apart;
                main_dirty = true;
            }
            // 供独立服务判断安装前是否仍有执行中的会话：正文不在主存档时只能读这个字段。
            let last = conversation
                .turns
                .items
                .last()
                .and_then(|turn| turn["status"].as_str())
                .unwrap_or("");
            if conversation.last_turn_status != last {
                conversation.last_turn_status = last.to_owned();
                main_dirty = true;
            }
            let Some(path) = path else {
                continue;
            };
            // 刚改为侧文件存放的会话，以及落盘状态未知（从未确认、上次写入失败）的会话，必须随任何一次主存档写入
            // 一起写出，否则主存档已清空正文而侧文件缺失或过期；只有已确认落盘且仅本次未触碰的会话才能留给合并落盘。
            let unconfirmed = conversation.turns.flushed.is_none();
            let dirty = conversation.turns.flushed != Some(conversation.turns.rev);
            if !(moved || unconfirmed || dirty && only.is_none_or(|ids| ids.contains(key))) {
                continue;
            }
            let bytes = serde_json::to_vec(&conversation.turns.items)?;
            anyhow::ensure!(
                bytes.len() <= MAX_FILE_BYTES,
                "Claude 会话正文已达 64 MiB，未截断历史"
            );
            files.push((path, bytes));
            turns.push((key.clone(), conversation.turns.rev));
        }
        if main_dirty {
            let bytes = serde_json::to_vec(&*self)?;
            anyhow::ensure!(
                bytes.len() <= MAX_FILE_BYTES,
                "Claude 会话记录已达 64 MiB，未截断历史"
            );
            // 主存档放最后：侧文件先落盘，崩溃时不会出现主存档已清空正文而侧文件缺失的状态。
            files.push((directory.join(MAIN_FILE), bytes));
        }
        if files.is_empty() {
            return Ok(None);
        }
        Ok(Some(Prepared {
            files,
            marks: Marks {
                main: main_dirty.then_some(main_rev),
                turns,
            },
        }))
    }

    /// 写入结束后更新落盘版本。失败时把对应文件标为“落盘状态未知”，下次一定重写。
    pub(super) fn finish(&mut self, marks: &Marks, written: bool) {
        if let Some(rev) = marks.main {
            self.conversations.flushed = written.then_some(rev);
        }
        for (id, rev) in &marks.turns {
            if let Some(conversation) = self.conversations.map.get_mut(id) {
                conversation.turns.flushed = written.then_some(*rev);
            }
        }
    }
}

/// 把旧布局（共享会话正文内联在主存档）迁移到分文件布局。迁移前保留完整单文件备份。
pub(super) fn migrate_legacy_layout(directory: &Path, saved: &mut Saved) -> anyhow::Result<()> {
    let main = directory.join(MAIN_FILE);
    let backup = directory.join(LEGACY_BACKUP);
    if !backup.exists() {
        std::fs::copy(&main, &backup)?;
    }
    let Some(prepared) = saved.prepare(directory, None)? else {
        return Ok(());
    };
    let outcome = write_files(&prepared.files);
    saved.finish(&prepared.marks, outcome.is_ok());
    outcome
}

pub(super) fn write_files(files: &[(PathBuf, Vec<u8>)]) -> anyhow::Result<()> {
    for (path, bytes) in files {
        crate::settings::atomic_write(path, bytes)?;
    }
    Ok(())
}

struct Batch {
    files: Vec<(PathBuf, Vec<u8>)>,
    ack: oneshot::Sender<anyhow::Result<()>>,
}

/// 单个写入任务：所有批次按提交顺序逐个写入，同一文件不会被两个线程同时替换。
#[derive(Default)]
pub(super) struct Writer {
    queue: StdMutex<Option<mpsc::UnboundedSender<Batch>>>,
}

impl Writer {
    /// 排队写入并返回完成回执。必须在持有会话锁时提交，写入顺序才与序列化顺序一致。
    pub(super) fn submit(
        &self,
        files: Vec<(PathBuf, Vec<u8>)>,
    ) -> oneshot::Receiver<anyhow::Result<()>> {
        let (ack, receiver) = oneshot::channel();
        let mut batch = Batch { files, ack };
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for _ in 0..2 {
            if let Some(sender) = queue.as_ref() {
                match sender.send(batch) {
                    Ok(()) => return receiver,
                    Err(failed) => batch = failed.0,
                }
            }
            // 首次使用，或原先承载写入任务的运行时已经退出：在当前运行时重新启动写入任务。
            *queue = Some(spawn_writer());
        }
        receiver
    }
}

fn spawn_writer() -> mpsc::UnboundedSender<Batch> {
    let (sender, mut queue) = mpsc::unbounded_channel::<Batch>();
    tokio::spawn(async move {
        while let Some(Batch { files, ack }) = queue.recv().await {
            let result = match tokio::task::spawn_blocking(move || write_files(&files)).await {
                Ok(result) => result,
                Err(failure) => Err(anyhow::anyhow!("Claude 会话存档写入线程异常：{failure}")),
            };
            let _ = ack.send(result);
        }
    });
    sender
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "11111111-1111-4111-8111-111111111111";
    const B: &str = "22222222-2222-4222-8222-222222222222";

    fn conversation(uuid: &str, shared: bool, turns: Value) -> Conversation {
        serde_json::from_value(json!({
            "id":format!("claude-{uuid}"),"nativeId":uuid,"projectId":null,"cwd":"/w","linuxCwd":"/w",
            "config":{},"title":"t","model":"m","effort":"e","createdAt":1,"updatedAt":2,
            "pinned":false,"unread":false,"deleted":false,"resumed":true,"turns":turns,"shared":shared
        }))
        .unwrap()
    }

    fn turns(status: &str) -> Value {
        json!([{"id":"turn-1","status":status,"items":[{"id":"item-1","type":"agentMessage","text":"正文"}]}])
    }

    fn state(conversations: Vec<Conversation>) -> Saved {
        let mut saved = Saved::default();
        for conversation in conversations {
            saved
                .conversations
                .insert(conversation.id.clone(), conversation);
        }
        saved
    }

    /// 与 `Claude::new` 的读取步骤一致：解析主存档，再读取侧文件。
    fn reopen(directory: &Path) -> (Saved, bool) {
        let mut saved: Saved =
            serde_json::from_slice(&std::fs::read(directory.join(MAIN_FILE)).unwrap()).unwrap();
        let legacy = saved.conversations.load_turns(directory);
        (saved, legacy)
    }

    fn write_all(directory: &Path, saved: &mut Saved) {
        let prepared = saved.prepare(directory, None).unwrap().unwrap();
        write_files(&prepared.files).unwrap();
        saved.finish(&prepared.marks, true);
    }

    fn file_names(prepared: &Prepared) -> Vec<String> {
        prepared
            .files
            .iter()
            .map(|(path, _)| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    fn sidecar(directory: &Path, uuid: &str) -> PathBuf {
        directory
            .join(TURNS_DIRECTORY)
            .join(format!("claude-{uuid}.json"))
    }

    #[test]
    fn clean_state_and_reads_prepare_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let mut saved = state(vec![conversation(A, true, turns("completed"))]);
        write_all(directory.path(), &mut saved);

        let (mut loaded, legacy) = reopen(directory.path());
        assert!(!legacy);
        // 只读访问（遍历、取末项、长度、整体比较）不能让存档变脏。
        let id = format!("claude-{A}");
        let conversation = &loaded.conversations[&id];
        assert_eq!(conversation.turns.len(), 1);
        assert!(conversation.turns.last().is_some());
        assert_eq!(
            conversation.turns,
            turns("completed").as_array().unwrap().clone()
        );
        assert!(loaded.prepare(directory.path(), None).unwrap().is_none());
    }

    #[test]
    fn turns_change_writes_that_sidecar_and_main_without_shared_turns() {
        let directory = tempfile::tempdir().unwrap();
        let mut saved = state(vec![
            conversation(A, true, turns("completed")),
            conversation(B, true, turns("completed")),
        ]);
        write_all(directory.path(), &mut saved);
        assert!(sidecar(directory.path(), A).exists() && sidecar(directory.path(), B).exists());

        saved
            .conversations
            .get_mut(&format!("claude-{A}"))
            .unwrap()
            .turns[0]["status"] = json!("inProgress");
        let prepared = saved.prepare(directory.path(), None).unwrap().unwrap();
        assert_eq!(
            file_names(&prepared),
            [format!("claude-{A}.json"), MAIN_FILE.to_owned()],
            "只写正文有变的会话，主存档放最后"
        );
        write_files(&prepared.files).unwrap();
        saved.finish(&prepared.marks, true);
        assert!(saved.prepare(directory.path(), None).unwrap().is_none());

        let main: Value =
            serde_json::from_slice(&std::fs::read(directory.path().join(MAIN_FILE)).unwrap())
                .unwrap();
        for uuid in [A, B] {
            let stored = &main["conversations"][format!("claude-{uuid}")];
            assert_eq!(stored["turns"], json!([]), "共享会话的正文不写入主存档");
        }
        assert_eq!(
            main["conversations"][format!("claude-{A}")]["lastTurnStatus"],
            "inProgress"
        );
        let (loaded, _) = reopen(directory.path());
        assert_eq!(
            loaded.conversations[&format!("claude-{A}")].turns[0]["status"],
            "inProgress"
        );
        assert_eq!(
            loaded.conversations[&format!("claude-{B}")].turns[0]["status"],
            "completed"
        );
    }

    #[test]
    fn metadata_change_rewrites_main_only() {
        let directory = tempfile::tempdir().unwrap();
        let mut saved = state(vec![conversation(A, true, turns("completed"))]);
        write_all(directory.path(), &mut saved);
        saved
            .conversations
            .get_mut(&format!("claude-{A}"))
            .unwrap()
            .pinned = true;
        let prepared = saved.prepare(directory.path(), None).unwrap().unwrap();
        assert_eq!(file_names(&prepared), [MAIN_FILE]);
    }

    #[test]
    fn scoped_prepare_leaves_other_dirty_turns_for_the_next_flush() {
        let directory = tempfile::tempdir().unwrap();
        let mut saved = state(vec![
            conversation(A, true, turns("completed")),
            conversation(B, true, turns("completed")),
        ]);
        write_all(directory.path(), &mut saved);
        for uuid in [A, B] {
            saved
                .conversations
                .get_mut(&format!("claude-{uuid}"))
                .unwrap()
                .turns[0]["status"] = json!("failed");
        }
        // 持久修改只等自己触碰的会话；另一个会话的正文留给合并落盘，不拖慢这次操作。
        let only = BTreeSet::from([format!("claude-{A}")]);
        let prepared = saved
            .prepare(directory.path(), Some(&only))
            .unwrap()
            .unwrap();
        assert_eq!(
            file_names(&prepared),
            [format!("claude-{A}.json"), MAIN_FILE.to_owned()]
        );
        write_files(&prepared.files).unwrap();
        saved.finish(&prepared.marks, true);

        let rest = saved.prepare(directory.path(), None).unwrap().unwrap();
        assert_eq!(file_names(&rest), [format!("claude-{B}.json")]);
    }

    #[test]
    fn failed_write_keeps_everything_dirty() {
        let directory = tempfile::tempdir().unwrap();
        let mut saved = state(vec![conversation(A, true, turns("completed"))]);
        write_all(directory.path(), &mut saved);
        saved
            .conversations
            .get_mut(&format!("claude-{A}"))
            .unwrap()
            .turns[0]["status"] = json!("failed");
        let first = saved.prepare(directory.path(), None).unwrap().unwrap();
        saved.finish(&first.marks, false);
        let again = saved.prepare(directory.path(), None).unwrap().unwrap();
        assert_eq!(
            file_names(&first),
            file_names(&again),
            "写入失败后下一次必须重写同样的文件"
        );
    }

    #[test]
    fn unconfirmed_sidecars_ride_along_with_any_main_write() {
        let directory = tempfile::tempdir().unwrap();
        let mut saved = state(vec![
            conversation(A, true, turns("completed")),
            conversation(B, true, turns("completed")),
        ]);
        // 第一次整体写入失败：两个会话的正文都还没有确认落盘。
        let failed = saved.prepare(directory.path(), None).unwrap().unwrap();
        saved.finish(&failed.marks, false);
        saved
            .conversations
            .get_mut(&format!("claude-{B}"))
            .unwrap()
            .pinned = true;
        // 之后只触碰 B 的持久修改仍要带上 A 的正文，否则主存档会指向不存在的侧文件。
        let only = BTreeSet::from([format!("claude-{B}")]);
        let prepared = saved
            .prepare(directory.path(), Some(&only))
            .unwrap()
            .unwrap();
        assert_eq!(
            file_names(&prepared),
            [
                format!("claude-{A}.json"),
                format!("claude-{B}.json"),
                MAIN_FILE.to_owned()
            ]
        );
    }

    #[test]
    fn inserting_a_stale_copy_forces_its_turns_to_be_rewritten() {
        let directory = tempfile::tempdir().unwrap();
        let mut saved = state(vec![conversation(A, true, turns("completed"))]);
        write_all(directory.path(), &mut saved);
        let id = format!("claude-{A}");
        let stale = saved.conversations[&id].clone();
        saved.conversations.get_mut(&id).unwrap().turns[0]["status"] = json!("failed");
        write_all(directory.path(), &mut saved);
        // 把较早读出的副本放回去：版本号相同但内容不同，必须重写正文。
        saved.conversations.insert(id.clone(), stale);
        let prepared = saved.prepare(directory.path(), None).unwrap().unwrap();
        assert_eq!(
            file_names(&prepared),
            [format!("{id}.json"), MAIN_FILE.to_owned()]
        );
    }

    #[test]
    fn changes_made_while_a_batch_is_being_written_stay_dirty() {
        let directory = tempfile::tempdir().unwrap();
        let mut saved = state(vec![conversation(A, true, turns("completed"))]);
        write_all(directory.path(), &mut saved);
        let id = format!("claude-{A}");
        saved.conversations.get_mut(&id).unwrap().turns[0]["status"] = json!("failed");
        let batch = saved.prepare(directory.path(), None).unwrap().unwrap();
        // 合并落盘等待写入时锁已放开，其间到达的新快照把正文又改了一次。
        saved.conversations.get_mut(&id).unwrap().turns[0]["status"] = json!("interrupted");
        saved.finish(&batch.marks, true);
        let rest = saved.prepare(directory.path(), None).unwrap().unwrap();
        assert_eq!(
            file_names(&rest),
            [format!("{id}.json"), MAIN_FILE.to_owned()]
        );
    }

    #[test]
    fn rollback_restores_conversations_new_entries_and_dirty_state() {
        let directory = tempfile::tempdir().unwrap();
        let mut saved = state(vec![conversation(A, true, turns("completed"))]);
        write_all(directory.path(), &mut saved);
        let id = format!("claude-{A}");

        saved.conversations.begin();
        saved.conversations.get_mut(&id).unwrap().pinned = true;
        saved.conversations.get_mut(&id).unwrap().turns[0]["status"] = json!("failed");
        saved.conversations.insert(
            format!("claude-{B}"),
            conversation(B, true, turns("completed")),
        );
        assert_eq!(
            saved.conversations.touched_ids(),
            BTreeSet::from([id.clone(), format!("claude-{B}")])
        );
        saved.conversations.rollback();

        assert!(!saved.conversations[&id].pinned);
        assert_eq!(saved.conversations[&id].turns[0]["status"], "completed");
        assert!(!saved.conversations.contains_key(&format!("claude-{B}")));
        assert!(
            saved.prepare(directory.path(), None).unwrap().is_none(),
            "回滚后与磁盘一致，不应再写任何文件"
        );
    }

    #[test]
    fn non_shared_and_non_canonical_ids_keep_turns_in_main() {
        let directory = tempfile::tempdir().unwrap();
        let mut odd = conversation(A, true, turns("completed"));
        odd.id = "claude-ABCDEF00-0000-4000-8000-000000000000".into();
        let mut saved = state(vec![
            conversation(B, false, turns("completed")),
            odd.clone(),
        ]);
        write_all(directory.path(), &mut saved);
        assert!(!directory.path().join(TURNS_DIRECTORY).exists());
        let (loaded, legacy) = reopen(directory.path());
        assert!(!legacy);
        assert_eq!(loaded.conversations[&format!("claude-{B}")].turns.len(), 1);
        assert_eq!(loaded.conversations[&odd.id].turns.len(), 1);
    }

    #[test]
    fn legacy_inline_layout_is_migrated_with_a_backup() {
        let directory = tempfile::tempdir().unwrap();
        let legacy_main = json!({"conversations":{
            format!("claude-{A}"): serde_json::to_value(conversation(A, true, turns("completed"))).unwrap(),
            format!("claude-{B}"): serde_json::to_value(conversation(B, false, turns("failed"))).unwrap(),
        }});
        let legacy_bytes = serde_json::to_vec(&legacy_main).unwrap();
        std::fs::write(directory.path().join(MAIN_FILE), &legacy_bytes).unwrap();

        let (mut saved, legacy) = reopen(directory.path());
        assert!(legacy, "共享会话的正文还内联在主存档里");
        assert_eq!(saved.conversations[&format!("claude-{A}")].turns.len(), 1);
        migrate_legacy_layout(directory.path(), &mut saved).unwrap();

        assert_eq!(
            std::fs::read(directory.path().join(LEGACY_BACKUP)).unwrap(),
            legacy_bytes,
            "迁移前的完整单文件原样备份"
        );
        assert!(sidecar(directory.path(), A).exists());
        assert!(
            !sidecar(directory.path(), B).exists(),
            "非共享会话没有原生日志可重建，正文留在主存档"
        );
        let (reloaded, legacy) = reopen(directory.path());
        assert!(!legacy);
        assert_eq!(
            reloaded.conversations[&format!("claude-{A}")].turns[0]["status"],
            "completed"
        );
        assert_eq!(
            reloaded.conversations[&format!("claude-{B}")].turns[0]["status"],
            "failed"
        );
        assert!(saved.prepare(directory.path(), None).unwrap().is_none());

        // 备份只做一次：再次迁移不能覆盖第一次留下的完整备份。
        std::fs::write(directory.path().join(LEGACY_BACKUP), b"first").unwrap();
        migrate_legacy_layout(directory.path(), &mut saved).unwrap();
        assert_eq!(
            std::fs::read(directory.path().join(LEGACY_BACKUP)).unwrap(),
            b"first"
        );
    }

    #[test]
    fn inline_turns_win_over_a_stale_sidecar() {
        let directory = tempfile::tempdir().unwrap();
        let mut saved = state(vec![conversation(A, true, turns("completed"))]);
        write_all(directory.path(), &mut saved);
        // 回退到旧版本后，旧版本把较新的正文写回主存档，侧文件已经过期。
        let mut main: Value =
            serde_json::from_slice(&std::fs::read(directory.path().join(MAIN_FILE)).unwrap())
                .unwrap();
        main["conversations"][format!("claude-{A}")]["turns"] = turns("failed");
        std::fs::write(
            directory.path().join(MAIN_FILE),
            serde_json::to_vec(&main).unwrap(),
        )
        .unwrap();

        let (mut loaded, legacy) = reopen(directory.path());
        assert!(legacy);
        assert_eq!(
            loaded.conversations[&format!("claude-{A}")].turns[0]["status"],
            "failed"
        );
        migrate_legacy_layout(directory.path(), &mut loaded).unwrap();
        let (reloaded, _) = reopen(directory.path());
        assert_eq!(
            reloaded.conversations[&format!("claude-{A}")].turns[0]["status"],
            "failed"
        );
    }

    #[test]
    fn corrupt_or_missing_sidecars_load_as_empty_and_are_rewritten() {
        let directory = tempfile::tempdir().unwrap();
        let mut saved = state(vec![
            conversation(A, true, turns("completed")),
            conversation(B, true, turns("completed")),
        ]);
        write_all(directory.path(), &mut saved);
        std::fs::write(sidecar(directory.path(), A), b"{broken").unwrap();
        std::fs::remove_file(sidecar(directory.path(), B)).unwrap();

        let (mut loaded, legacy) = reopen(directory.path());
        assert!(!legacy);
        for uuid in [A, B] {
            assert!(
                loaded.conversations[&format!("claude-{uuid}")]
                    .turns
                    .is_empty()
            );
        }
        // 缺失的侧文件等同空正文，不必补写；损坏的侧文件在下一次落盘时用空正文覆盖，
        // 主存档里的最后一轮状态同步更新为空（两者都会在原生历史重新投影后恢复）。
        let prepared = loaded.prepare(directory.path(), None).unwrap().unwrap();
        assert_eq!(
            file_names(&prepared),
            [format!("claude-{A}.json"), MAIN_FILE.to_owned()]
        );
        loaded
            .conversations
            .get_mut(&format!("claude-{A}"))
            .unwrap()
            .turns
            .set(turns("completed").as_array().unwrap().clone());
        write_all(directory.path(), &mut loaded);
        let (reloaded, _) = reopen(directory.path());
        assert_eq!(
            reloaded.conversations[&format!("claude-{A}")].turns.len(),
            1
        );
    }

    #[test]
    fn oversized_turns_are_refused_per_file_instead_of_truncated() {
        let directory = tempfile::tempdir().unwrap();
        let mut saved = state(vec![
            conversation(A, true, json!([])),
            conversation(B, true, turns("completed")),
        ]);
        let huge = "x".repeat(MAX_FILE_BYTES + 1);
        saved
            .conversations
            .get_mut(&format!("claude-{A}"))
            .unwrap()
            .turns
            .push(json!({"id":"huge","text":huge}));
        let failure = saved
            .prepare(directory.path(), None)
            .err()
            .expect("超过单文件上限必须拒绝");
        assert!(failure.to_string().contains("64 MiB"), "{failure}");
        // 只有超限的那个会话受影响；上限按文件计算，其他会话仍可保存。
        saved
            .conversations
            .get_mut(&format!("claude-{A}"))
            .unwrap()
            .turns
            .clear();
        assert!(saved.prepare(directory.path(), None).unwrap().is_some());
    }

    #[tokio::test]
    async fn writer_applies_batches_in_submission_order() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ordered.json");
        let writer = Writer::default();
        let mut receivers = Vec::new();
        for index in 0..20 {
            receivers.push(writer.submit(vec![(path.clone(), format!("{index}").into_bytes())]));
        }
        for receiver in receivers {
            receiver.await.unwrap().unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "19",
            "较晚提交的内容不能被较早的覆盖"
        );
    }

    #[tokio::test]
    async fn writer_reports_failures_to_the_waiting_caller() {
        let directory = tempfile::tempdir().unwrap();
        let blocked = directory.path().join("blocked.json");
        std::fs::create_dir(blocked.with_extension("json.tmp")).unwrap();
        let writer = Writer::default();
        let failure = writer
            .submit(vec![(blocked, b"x".to_vec())])
            .await
            .unwrap()
            .unwrap_err();
        assert!(failure.to_string().contains("temp file"), "{failure:#}");
    }
}
