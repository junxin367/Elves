//! 变更页共享只读快照：文件通知使缓存失效，遗漏通知由可见页面定时核对。
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::Mutex;

use super::*;

// 用户要求隐藏页不持续扫描。45 秒租约与 15 秒清理间隔，使闲置监听在 60 秒内释放；
// 每个服务最多 8 个工作目录，每目录最多 8 MiB 只读响应，避免随标签数增长。
const LEASE_MS: u64 = 45_000;
const CACHE_BYTES: usize = 8 * 1024 * 1024;
const CHECK_MS: u64 = 15_000;
const FALLBACK_MS: u64 = 5_000;

#[derive(Default)]
pub(crate) struct Registry {
    inner: Arc<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    entries: Mutex<HashMap<PathBuf, Arc<Entry>>>,
    sweeping: AtomicBool,
}

struct Cached {
    value: Value,
    generation: u64,
    at: Instant,
    bytes: usize,
    max_age_ms: u64,
}

struct Entry {
    id: String,
    root: PathBuf,
    last_used: AtomicU64,
    generation: AtomicU64,
    pending_event: AtomicBool,
    runtime: tokio::runtime::Handle,
    topology: AtomicU64,
    watch_ready: AtomicBool,
    watch_dirty: AtomicBool,
    watch_starting: AtomicBool,
    watcher: StdMutex<Option<RecommendedWatcher>>,
    cache: Mutex<HashMap<String, Cached>>,
    discovery: Mutex<Option<(u64, u64, WorkspaceRepositories)>>,
    controller: std::sync::Weak<crate::HarnessController>,
}

fn now() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

impl Registry {
    pub(super) async fn repositories(
        &self,
        root: PathBuf,
    ) -> Result<WorkspaceRepositories, ApiError> {
        let entry = self.inner.entries.lock().await.get(&root).cloned();
        if let Some(entry) = entry {
            let mut discovery = entry.discovery.lock().await;
            let generation = entry.topology.load(Ordering::Acquire);
            if let Some((at, version, value)) = discovery.as_ref()
                && *version == generation
                && now().saturating_sub(*at) < CHECK_MS
            {
                return Ok(value.clone());
            }
            let value = tokio::task::spawn_blocking(move || WorkspaceRepositories::scan(root))
                .await
                .map_err(|_| ApiError::new(503, "无法发现项目内的 Git 仓库"))??;
            *discovery = Some((now(), generation, value.clone()));
            Ok(value)
        } else {
            tokio::task::spawn_blocking(move || WorkspaceRepositories::scan(root))
                .await
                .map_err(|_| ApiError::new(503, "无法发现项目内的 Git 仓库"))?
        }
    }

    pub(super) async fn read<F, Fut>(
        &self,
        root: PathBuf,
        key: String,
        fresh: bool,
        controller: std::sync::Weak<crate::HarnessController>,
        load: F,
    ) -> Result<Value, ApiError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value, ApiError>>,
    {
        if !self.inner.sweeping.swap(true, Ordering::Relaxed) {
            let weak = Arc::downgrade(&self.inner);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(15)).await;
                    let Some(inner) = weak.upgrade() else { return };
                    inner.entries.lock().await.retain(|_, entry| {
                        now().saturating_sub(entry.last_used.load(Ordering::Relaxed)) < LEASE_MS
                    });
                }
            });
        }
        let entry = {
            let mut entries = self.inner.entries.lock().await;
            if !entries.contains_key(&root) && entries.len() >= 8 {
                if let Some(oldest) = entries
                    .iter()
                    .min_by_key(|(_, e)| e.last_used.load(Ordering::Relaxed))
                    .map(|(root, _)| root.clone())
                {
                    entries.remove(&oldest);
                }
            }
            entries
                .entry(root.clone())
                .or_insert_with(|| {
                    Arc::new(Entry {
                        id: uuid::Uuid::new_v4().to_string(),
                        root,
                        last_used: AtomicU64::new(now()),
                        generation: AtomicU64::new(0),
                        pending_event: AtomicBool::new(false),
                        runtime: tokio::runtime::Handle::current(),
                        topology: AtomicU64::new(0),
                        watch_ready: AtomicBool::new(false),
                        watch_dirty: AtomicBool::new(true),
                        watch_starting: AtomicBool::new(false),
                        watcher: StdMutex::new(None),
                        cache: Mutex::new(HashMap::new()),
                        discovery: Mutex::new(None),
                        controller,
                    })
                })
                .clone()
        };
        entry.last_used.store(now(), Ordering::Relaxed);
        entry.start_watch();
        // 同目录请求串行合并；持锁等待者复用已完成快照，其他目录互不阻塞。
        let requested_at = Instant::now();
        let mut cache = entry.cache.lock().await;
        let generation = entry.generation.load(Ordering::Acquire);
        let interval = if entry.watch_ready.load(Ordering::Acquire) {
            CHECK_MS
        } else {
            FALLBACK_MS
        };
        let reusable = cache.get(&key).filter(|value| {
            value.generation == generation
                && value.at.elapsed() < Duration::from_millis(value.max_age_ms)
                && (!fresh || value.at >= requested_at)
        });
        let mut value = if let Some(value) = reusable {
            value.value.clone()
        } else {
            // 主动核对失败后不能再次把上一份服务缓存作为新鲜成功回执；页面自行保留带说明的旧内容。
            cache.remove(&key);
            let value = load().await?;
            let bytes = serde_json::to_vec(&value).map_or(CACHE_BYTES + 1, |value| value.len());
            if bytes <= CACHE_BYTES {
                while cache.values().map(|value| value.bytes).sum::<usize>() + bytes > CACHE_BYTES {
                    let Some(oldest) = cache
                        .iter()
                        .min_by_key(|(_, value)| value.at)
                        .map(|(key, _)| key.clone())
                    else {
                        break;
                    };
                    cache.remove(&oldest);
                }
                cache.insert(
                    key,
                    Cached {
                        value: value.clone(),
                        generation,
                        at: Instant::now(),
                        bytes,
                        max_age_ms: interval,
                    },
                );
            }
            value
        };
        value["watchId"] = json!(entry.id);
        value["watchVersion"] = json!(generation);
        value["watchDirty"] = json!(generation != entry.generation.load(Ordering::Acquire));
        value["watchIntervalMs"] = json!(interval);
        Ok(value)
    }
}

impl Entry {
    fn changed(self: &Arc<Self>, topology: bool) {
        if topology {
            self.watch_dirty.store(true, Ordering::Release);
            self.topology.fetch_add(1, Ordering::AcqRel);
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
        // 通知只含随机监听 ID，不广播目录、文件名或所属会话；300ms 合并写入风暴。
        if !self.pending_event.swap(true, Ordering::AcqRel) {
            let weak = Arc::downgrade(self);
            self.runtime.spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let Some(entry) = weak.upgrade() else { return };
                entry.pending_event.store(false, Ordering::Release);
                if now().saturating_sub(entry.last_used.load(Ordering::Relaxed)) < LEASE_MS
                    && let Some(controller) = entry.controller.upgrade()
                {
                    controller.publish_event(json!({
                        "method": "workspace/changed", "params": {"watchId": entry.id}
                    }));
                }
            });
        }
    }

    fn start_watch(self: &Arc<Self>) {
        if !self.watch_dirty.load(Ordering::Acquire)
            || self.watch_starting.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::task::spawn_blocking(move || {
            let Some(entry) = weak.upgrade() else { return };
            let generation = entry.generation.load(Ordering::Acquire);
            let callback = Arc::downgrade(&entry);
            let watcher = RecommendedWatcher::new(
                move |event: notify::Result<notify::Event>| {
                    let Some(entry) = callback.upgrade() else {
                        return;
                    };
                    match event {
                        Ok(event) if !matches!(event.kind, EventKind::Access(_)) => {
                            if event.paths.is_empty()
                                || event
                                    .paths
                                    .iter()
                                    .any(|path| relevant_path(&entry.root, path))
                            {
                                entry.changed(!matches!(
                                    event.kind,
                                    EventKind::Modify(notify::event::ModifyKind::Data(_))
                                ));
                            }
                        }
                        Err(_) => {
                            entry.watch_ready.store(false, Ordering::Release);
                            entry.changed(true);
                        }
                        _ => {}
                    }
                },
                notify::Config::default().with_follow_symlinks(false),
            );
            let mut ready = false;
            if let Ok(mut watcher) = watcher {
                let (paths, complete) = watch_paths(&entry.root);
                ready = complete;
                for (path, recursive) in paths {
                    if watcher.watch(&path, recursive).is_err() {
                        ready = false;
                    }
                }
                // WSL/网络目录的通知不能保证完整，始终保留 5 秒共享核对。
                if entry
                    .root
                    .to_string_lossy()
                    .trim_start_matches(r"\\?\")
                    .starts_with(r"\\")
                    || entry.root.to_string_lossy().starts_with(r"\\?\UNC\")
                {
                    ready = false;
                }
                if let Ok(mut slot) = entry.watcher.lock() {
                    *slot = Some(watcher);
                }
            }
            entry.watch_ready.store(ready, Ordering::Release);
            entry.watch_dirty.store(
                generation != entry.generation.load(Ordering::Acquire),
                Ordering::Release,
            );
            entry.watch_starting.store(false, Ordering::Release);
        });
    }
}

fn relevant_path(root: &Path, path: &Path) -> bool {
    // 工作树外只注册 Git 确认的 metadata/refs；不能按其宿主目录名 temp 再次过滤。
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    let parts: Vec<_> = relative
        .components()
        .map(|p| p.as_os_str().to_string_lossy())
        .collect();
    if let Some(git) = parts
        .iter()
        .position(|part| part.eq_ignore_ascii_case(".git"))
    {
        return parts.get(git + 1).is_none_or(|part| {
            matches!(
                part.as_ref(),
                "HEAD" | "index" | "refs" | "packed-refs" | "config" | "commondir"
            )
        });
    }
    !parts.iter().any(|part| {
        is_search_skipped_directory(part)
            || matches!(
                part.as_ref(),
                ".venv" | "venv" | "__pycache__" | ".gradle" | ".next" | "coverage" | "out" | "tmp"
            )
    })
}

fn watch_paths(root: &Path) -> (Vec<(PathBuf, RecursiveMode)>, bool) {
    let mut paths = Vec::new();
    let mut pending = VecDeque::from([root.to_path_buf()]);
    let mut seen = HashSet::new();
    let started = Instant::now();
    let mut complete = true;
    // 不递归注册依赖和构建目录，也不跟随符号链接；目录树超限仍由定时读取补齐。
    while let Some(directory) = pending.pop_front() {
        if paths.len() >= 2048 || started.elapsed() > Duration::from_secs(2) {
            complete = false;
            break;
        }
        if !seen.insert(directory.clone()) {
            continue;
        }
        paths.push((directory.clone(), RecursiveMode::NonRecursive));
        let marker = directory.join(".git");
        if let Ok(metadata) = std::fs::symlink_metadata(&marker) {
            if metadata.is_file() && !is_link(&metadata) && metadata.len() <= 16_384 {
                if let Ok(text) = std::fs::read_to_string(&marker) {
                    if let Some(path) = text.trim().strip_prefix("gitdir: ") {
                        if let Ok(git_dir) = std::fs::canonicalize(directory.join(path)) {
                            add_git_paths(&git_dir, &mut paths);
                            let common = git_dir.join("commondir");
                            if std::fs::metadata(&common).is_ok_and(|m| m.len() <= 16_384) {
                                if let Ok(text) = std::fs::read_to_string(common) {
                                    if let Ok(common) =
                                        std::fs::canonicalize(git_dir.join(text.trim()))
                                    {
                                        add_git_paths(&common, &mut paths);
                                    }
                                }
                            }
                        }
                    }
                }
            } else if metadata.is_dir() && !is_link(&metadata) {
                add_git_paths(&marker, &mut paths);
            }
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            complete = false;
            continue;
        };
        for child in entries {
            if started.elapsed() > Duration::from_secs(2) {
                complete = false;
                break;
            }
            let Ok(child) = child else {
                complete = false;
                continue;
            };
            let path = child.path();
            if relevant_path(root, &path)
                && child.file_name() != ".git"
                && std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir() && !is_link(&m))
            {
                pending.push_back(path);
            }
        }
    }
    if paths.len() > 2048 {
        paths.truncate(2048);
        complete = false;
    }
    (paths, complete)
}

fn add_git_paths(root: &Path, paths: &mut Vec<(PathBuf, RecursiveMode)>) {
    paths.push((root.to_path_buf(), RecursiveMode::NonRecursive));
    let refs = root.join("refs");
    if std::fs::symlink_metadata(&refs).is_ok_and(|m| m.is_dir() && !is_link(&m)) {
        paths.push((refs, RecursiveMode::Recursive));
    }
}
