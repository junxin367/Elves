//! 安装包阶段的旧版目录识别与单向同步。
//!
//! 用户要求：
//! 1. 安装包不得删除、终止或修改与 CodexElves 相关的程序、快捷方式、注册表及旧目录；
//! 2. 新版 Elves 的共享配置、桌面端数据与 CLI 目录统一收敛到 `~/.elves`、`~/.elves/desktop`、`~/.elves/cli`；
//! 3. 在安装包阶段识别旧版目录（`~/.codex-session-delete`、`%LOCALAPPDATA%\com.elves.app` /
//!    `%LOCALAPPDATA%\com.codexelves.remote`、`~/.codex-elves-cli`），以只读方式将旧数据单向复制到新版目录，
//!    跳过运行态锁、服务端点、私有套接字与临时缓存，且不覆盖新版目录中已存在的文件。

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use fs2::FileExt;
use serde::Serialize;

const MIGRATIONS_DIR: &str = "migrations";
const MIGRATION_LOCK_FILE: &str = "legacy-data-sync-v1.lock";
const STATE_MIGRATION_MARKER: &str = "legacy-state-sync-v1.done";
const DESKTOP_MIGRATION_MARKER: &str = "legacy-desktop-sync-v1.done";
const CLI_MIGRATION_MARKER: &str = "legacy-cli-sync-v1.done";

const DESKTOP_SETTINGS_FILE: &str = "settings.json";
const DESKTOP_WEBVIEW_DIR: &str = "webview";
const DESKTOP_RESOURCES_DIR: &str = "web-resources";

/// 共享状态目录根部需要跳过的运行态/自动生成/目标子目录，避免把旧进程锁或套接字带入新版。
const STATE_SKIP_ROOT_DIRS: &[&str] = &[
    "locks",
    "codex-app-server",
    "tool-runtime",
    "desktop-input-files",
    "remote-access-runtime",
    "desktop",
    "cli",
];

/// 共享状态目录根部需要跳过的运行态端点、锁文件与内置临时脚本。
const STATE_SKIP_ROOT_FILES: &[&str] = &[
    "harness-service.json",
    "harness-service.lock",
    "remote-access-service.json",
    "remote-access-service.lock",
    "local-proxy-service.json",
    "local-proxy-service.lock",
    "remote-access-key.lock",
    "claude-shared.py",
    "claude-project-trust.py",
    "claude-sdk-host.py",
];

/// 日志文件可能正被运行中的旧版 CodexElves 持有锁；读取被占用时按可跳过处理，不阻塞迁移。
const STATE_BEST_EFFORT_ROOT_FILES: &[&str] = &["codex-elves.log", "proxy-requests.jsonl"];
const CLI_BEST_EFFORT_ROOT_FILES: &[&str] = &["codex-wrapper.log"];

/// WebView2 缓存与崩溃转储目录体积大、易被旧进程占用且可自动重建，同步时直接跳过。
const WEBVIEW_SKIP_DIRS: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "GrShaderCache",
    "GraphiteDawnCache",
    "ShaderCache",
    "DawnCache",
    "Crashpad",
    "Service Worker",
    "blob_storage",
    "Session Storage",
    "OptimizationGuidePredictionModels",
    "Component CRX Cache",
    "BrowserMetrics",
];

const WEBVIEW_SKIP_FILES: &[&str] = &[
    "LOCK",
    "lockfile",
    "SingletonLock",
    "SingletonSocket",
    "SingletonCookie",
];

const MAX_RECORDED_SKIPPED_PATHS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyDataSources {
    pub state_dir: Option<PathBuf>,
    pub desktop_candidates: Vec<PathBuf>,
    pub cli_dir: Option<PathBuf>,
}

impl LegacyDataSources {
    pub fn discover_default() -> Self {
        Self {
            state_dir: crate::paths::legacy_app_state_dir(),
            desktop_candidates: crate::paths::legacy_desktop_data_candidates(),
            cli_dir: crate::paths::legacy_cli_home_dir(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeOutcome {
    AlreadyMigrated,
    NoLegacySource,
    Migrated,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeReport {
    pub name: &'static str,
    pub source: Option<PathBuf>,
    pub target: PathBuf,
    pub outcome: ScopeOutcome,
    pub copied_files: usize,
    pub skipped_existing_files: usize,
    pub skipped_locked_files: usize,
    pub failed_files: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationReport {
    pub target_state_dir: PathBuf,
    pub state: ScopeReport,
    pub desktop: ScopeReport,
    pub cli: ScopeReport,
}

impl MigrationReport {
    pub fn has_failures(&self) -> bool {
        !self.state.failed_files.is_empty()
            || !self.desktop.failed_files.is_empty()
            || !self.cli.failed_files.is_empty()
    }

    /// 输出给 NSIS 安装详情日志的简体中文逐行摘要（每行保持简短，避免详情框截断）。
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "Elves 数据目录：{}",
            self.target_state_dir.display()
        )];
        for scope in [&self.state, &self.desktop, &self.cli] {
            match scope.outcome {
                ScopeOutcome::AlreadyMigrated => {
                    lines.push(format!("{}：已完成历史同步，保留当前数据", scope.name));
                }
                ScopeOutcome::NoLegacySource => {
                    lines.push(format!("{}：未检测到旧版目录，直接使用新版目录", scope.name));
                }
                ScopeOutcome::Migrated | ScopeOutcome::Partial => {
                    let source = scope
                        .source
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default();
                    let status = if scope.outcome == ScopeOutcome::Migrated {
                        "已同步"
                    } else {
                        "部分同步"
                    };
                    let mut detail = format!(
                        "{}：{}（复制 {} 个文件，保留已有 {} 个）",
                        scope.name, status, scope.copied_files, scope.skipped_existing_files
                    );
                    if scope.skipped_locked_files > 0 {
                        detail.push_str(&format!(
                            "，跳过被占用 {} 个",
                            scope.skipped_locked_files
                        ));
                    }
                    if !scope.failed_files.is_empty() {
                        detail.push_str(&format!("，失败 {} 个", scope.failed_files.len()));
                    }
                    lines.push(detail);
                    if !source.is_empty() {
                        lines.push(format!("  来源（原样保留）：{source}"));
                    }
                    for note in &scope.notes {
                        lines.push(format!("  说明：{note}"));
                    }
                    for failed in scope.failed_files.iter().take(5) {
                        lines.push(format!("  未复制：{failed}"));
                    }
                }
            }
        }
        lines
    }
}

/// 共享状态目录是否已完成过旧版同步标记。
pub fn legacy_state_migration_completed(target_state_dir: &Path) -> bool {
    target_state_dir
        .join(MIGRATIONS_DIR)
        .join(STATE_MIGRATION_MARKER)
        .is_file()
}

/// 安装包入口调用：使用默认旧版目录与当前目标 `~/.elves` 目录执行一次旧数据识别与单向同步。
pub fn migrate_default_legacy_data() -> anyhow::Result<MigrationReport> {
    let target_state_dir = crate::paths::default_app_state_dir();
    let sources = LegacyDataSources::discover_default();
    migrate_legacy_data_at(&sources, &target_state_dir)
}

pub fn migrate_legacy_data_at(
    sources: &LegacyDataSources,
    target_state_dir: &Path,
) -> anyhow::Result<MigrationReport> {
    let migrations_dir = target_state_dir.join(MIGRATIONS_DIR);
    fs::create_dir_all(&migrations_dir)
        .with_context(|| format!("无法创建迁移目录：{}", migrations_dir.display()))?;
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(migrations_dir.join(MIGRATION_LOCK_FILE))
        .context("无法打开旧版数据迁移锁文件")?;
    lock_file
        .lock_exclusive()
        .context("无法锁定旧版数据迁移流程")?;

    let state = migrate_state_scope(sources.state_dir.as_deref(), target_state_dir, &migrations_dir)?;
    let desktop = migrate_desktop_scope(
        &sources.desktop_candidates,
        &target_state_dir.join("desktop"),
        &migrations_dir,
    )?;
    let cli = migrate_cli_scope(
        sources.cli_dir.as_deref(),
        &target_state_dir.join("cli"),
        &migrations_dir,
    )?;

    Ok(MigrationReport {
        target_state_dir: target_state_dir.to_path_buf(),
        state,
        desktop,
        cli,
    })
}

fn migrate_state_scope(
    source: Option<&Path>,
    target: &Path,
    migrations_dir: &Path,
) -> anyhow::Result<ScopeReport> {
    let marker = migrations_dir.join(STATE_MIGRATION_MARKER);
    if marker.is_file() {
        return Ok(ScopeReport {
            name: "共享配置与会话状态",
            source: None,
            target: target.to_path_buf(),
            outcome: ScopeOutcome::AlreadyMigrated,
            copied_files: 0,
            skipped_existing_files: 0,
            skipped_locked_files: 0,
            failed_files: Vec::new(),
            notes: Vec::new(),
        });
    }

    let Some(source) = source.filter(|path| is_usable_source_dir(path, target)) else {
        write_marker(&marker, "no_legacy_source")?;
        return Ok(ScopeReport {
            name: "共享配置与会话状态",
            source: None,
            target: target.to_path_buf(),
            outcome: ScopeOutcome::NoLegacySource,
            copied_files: 0,
            skipped_existing_files: 0,
            skipped_locked_files: 0,
            failed_files: Vec::new(),
            notes: Vec::new(),
        });
    };

    let mut stats = CopyStats::default();
    copy_state_dir(source, target, Path::new(""), &mut stats)?;

    let outcome = if stats.failed_files.is_empty() {
        write_marker(&marker, &format!("migrated_from={}", source.display()))?;
        ScopeOutcome::Migrated
    } else {
        // 核心状态文件出现非预期失败时不写完成标记，便于下次安装自动补齐。
        ScopeOutcome::Partial
    };

    Ok(ScopeReport {
        name: "共享配置与会话状态",
        source: Some(source.to_path_buf()),
        target: target.to_path_buf(),
        outcome,
        copied_files: stats.copied_files,
        skipped_existing_files: stats.skipped_existing_files,
        skipped_locked_files: stats.skipped_locked_files,
        failed_files: stats.failed_files,
        notes: stats.notes,
    })
}

fn migrate_desktop_scope(
    candidates: &[PathBuf],
    target: &Path,
    migrations_dir: &Path,
) -> anyhow::Result<ScopeReport> {
    let marker = migrations_dir.join(DESKTOP_MIGRATION_MARKER);
    if marker.is_file() {
        return Ok(ScopeReport {
            name: "桌面客户端配置与缓存",
            source: None,
            target: target.to_path_buf(),
            outcome: ScopeOutcome::AlreadyMigrated,
            copied_files: 0,
            skipped_existing_files: 0,
            skipped_locked_files: 0,
            failed_files: Vec::new(),
            notes: Vec::new(),
        });
    }

    let Some(source) = select_legacy_desktop_source(candidates, target) else {
        write_marker(&marker, "no_legacy_source")?;
        return Ok(ScopeReport {
            name: "桌面客户端配置与缓存",
            source: None,
            target: target.to_path_buf(),
            outcome: ScopeOutcome::NoLegacySource,
            copied_files: 0,
            skipped_existing_files: 0,
            skipped_locked_files: 0,
            failed_files: Vec::new(),
            notes: Vec::new(),
        });
    };

    fs::create_dir_all(target)
        .with_context(|| format!("无法创建新版桌面数据目录：{}", target.display()))?;

    let mut stats = CopyStats::default();

    // 1. 窗口位置与尺寸配置（settings.json）
    let source_settings = source.join(DESKTOP_SETTINGS_FILE);
    if is_regular_file(&source_settings) {
        copy_single_file(
            &source_settings,
            &target.join(DESKTOP_SETTINGS_FILE),
            DESKTOP_SETTINGS_FILE,
            false,
            &mut stats,
        );
    }

    // 2. WebView2 配置文件（webview/）：若新版目录已有独立 webview，不混入旧配置以免损坏 LevelDB/IndexedDB。
    let source_webview = source.join(DESKTOP_WEBVIEW_DIR);
    let target_webview = target.join(DESKTOP_WEBVIEW_DIR);
    if is_regular_dir(&source_webview) {
        if target_webview.exists() {
            stats
                .notes
                .push("新版 webview 目录已存在，保留现有登录与界面状态，未混入旧版缓存".into());
        } else {
            copy_webview_dir(&source_webview, &target_webview, Path::new("webview"), &mut stats)?;
            if stats.skipped_locked_files > 0 {
                stats.notes.push(
                    "旧版程序仍在运行，已跳过被占用的 WebView2 文件；如需完整迁移可退出旧程序后清理新版 desktop 目录再安装"
                        .into(),
                );
            }
        }
    }

    // 3. 公开网页资源缓存（web-resources/）
    let source_resources = source.join(DESKTOP_RESOURCES_DIR);
    let target_resources = target.join(DESKTOP_RESOURCES_DIR);
    if is_regular_dir(&source_resources) {
        copy_generic_dir(
            &source_resources,
            &target_resources,
            Path::new(DESKTOP_RESOURCES_DIR),
            true,
            &mut stats,
        )?;
    }

    // 桌面迁移只要核心设置未失败即落完成标记，防止后续升级把旧版 WebView2 文件混入已在运行的新版配置。
    let outcome = if stats.failed_files.is_empty() {
        write_marker(&marker, &format!("migrated_from={}", source.display()))?;
        if stats.skipped_locked_files > 0 {
            ScopeOutcome::Partial
        } else {
            ScopeOutcome::Migrated
        }
    } else {
        ScopeOutcome::Partial
    };

    Ok(ScopeReport {
        name: "桌面客户端配置与缓存",
        source: Some(source),
        target: target.to_path_buf(),
        outcome,
        copied_files: stats.copied_files,
        skipped_existing_files: stats.skipped_existing_files,
        skipped_locked_files: stats.skipped_locked_files,
        failed_files: stats.failed_files,
        notes: stats.notes,
    })
}

fn migrate_cli_scope(
    source: Option<&Path>,
    target: &Path,
    migrations_dir: &Path,
) -> anyhow::Result<ScopeReport> {
    let marker = migrations_dir.join(CLI_MIGRATION_MARKER);
    if marker.is_file() {
        return Ok(ScopeReport {
            name: "CLI Wrapper 配置",
            source: None,
            target: target.to_path_buf(),
            outcome: ScopeOutcome::AlreadyMigrated,
            copied_files: 0,
            skipped_existing_files: 0,
            skipped_locked_files: 0,
            failed_files: Vec::new(),
            notes: Vec::new(),
        });
    }

    let Some(source) = source.filter(|path| is_usable_source_dir(path, target)) else {
        write_marker(&marker, "no_legacy_source")?;
        return Ok(ScopeReport {
            name: "CLI Wrapper 配置",
            source: None,
            target: target.to_path_buf(),
            outcome: ScopeOutcome::NoLegacySource,
            copied_files: 0,
            skipped_existing_files: 0,
            skipped_locked_files: 0,
            failed_files: Vec::new(),
            notes: Vec::new(),
        });
    };

    let mut stats = CopyStats::default();
    copy_cli_dir(source, target, Path::new(""), &mut stats)?;

    let outcome = if stats.failed_files.is_empty() {
        write_marker(&marker, &format!("migrated_from={}", source.display()))?;
        ScopeOutcome::Migrated
    } else {
        ScopeOutcome::Partial
    };

    Ok(ScopeReport {
        name: "CLI Wrapper 配置",
        source: Some(source.to_path_buf()),
        target: target.to_path_buf(),
        outcome,
        copied_files: stats.copied_files,
        skipped_existing_files: stats.skipped_existing_files,
        skipped_locked_files: stats.skipped_locked_files,
        failed_files: stats.failed_files,
        notes: stats.notes,
    })
}

/// 选择实际承载过桌面端配置的旧目录：
/// 只认包含 `settings.json`、`webview/` 或 `web-resources/` 的目录，避免被“关于 Elves”窗口留下的空 `EBWebView/` 误导。
fn select_legacy_desktop_source(candidates: &[PathBuf], target: &Path) -> Option<PathBuf> {
    candidates.iter().find_map(|candidate| {
        if !is_usable_source_dir(candidate, target) {
            return None;
        }
        let has_desktop_data = is_regular_file(&candidate.join(DESKTOP_SETTINGS_FILE))
            || is_regular_dir(&candidate.join(DESKTOP_WEBVIEW_DIR))
            || is_regular_dir(&candidate.join(DESKTOP_RESOURCES_DIR));
        has_desktop_data.then(|| candidate.clone())
    })
}

#[derive(Default)]
struct CopyStats {
    copied_files: usize,
    skipped_existing_files: usize,
    skipped_locked_files: usize,
    failed_files: Vec<String>,
    notes: Vec<String>,
}

fn copy_state_dir(
    source: &Path,
    target: &Path,
    relative: &Path,
    stats: &mut CopyStats,
) -> anyhow::Result<()> {
    let entries = match fs::read_dir(source) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("读取旧目录失败：{}", source.display()));
        }
    };
    let is_root = relative.as_os_str().is_empty();
    let in_migrations = relative == Path::new(MIGRATIONS_DIR);

    for entry in entries {
        let entry = entry?;
        let name_os = entry.file_name();
        let name = name_os.to_string_lossy();
        let source_path = entry.path();
        if is_reparse_or_symlink(&source_path)? {
            continue;
        }
        let metadata = match fs::symlink_metadata(&source_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let child_relative = if is_root {
            PathBuf::from(&name_os)
        } else {
            relative.join(&name_os)
        };
        let target_path = target.join(&name_os);

        if metadata.is_dir() {
            if is_root && matches_any_ignore_case(&name, STATE_SKIP_ROOT_DIRS) {
                continue;
            }
            copy_state_dir(&source_path, &target_path, &child_relative, stats)?;
        } else if metadata.is_file() {
            if is_temp_or_lock_filename(&name) {
                continue;
            }
            if is_root && matches_any_ignore_case(&name, STATE_SKIP_ROOT_FILES) {
                continue;
            }
            if in_migrations && name.to_ascii_lowercase().starts_with("legacy-") {
                continue;
            }
            let allow_locked_skip =
                is_root && matches_any_ignore_case(&name, STATE_BEST_EFFORT_ROOT_FILES);
            copy_single_file(
                &source_path,
                &target_path,
                &child_relative.to_string_lossy(),
                allow_locked_skip,
                stats,
            );
        }
    }
    Ok(())
}

fn copy_cli_dir(
    source: &Path,
    target: &Path,
    relative: &Path,
    stats: &mut CopyStats,
) -> anyhow::Result<()> {
    let entries = match fs::read_dir(source) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("读取旧 CLI 目录失败：{}", source.display()));
        }
    };
    let is_root = relative.as_os_str().is_empty();

    for entry in entries {
        let entry = entry?;
        let name_os = entry.file_name();
        let name = name_os.to_string_lossy();
        let source_path = entry.path();
        if is_reparse_or_symlink(&source_path)? {
            continue;
        }
        let metadata = match fs::symlink_metadata(&source_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let child_relative = if is_root {
            PathBuf::from(&name_os)
        } else {
            relative.join(&name_os)
        };
        let target_path = target.join(&name_os);

        if metadata.is_dir() {
            copy_cli_dir(&source_path, &target_path, &child_relative, stats)?;
        } else if metadata.is_file() {
            if is_temp_or_lock_filename(&name) {
                continue;
            }
            let allow_locked_skip =
                is_root && matches_any_ignore_case(&name, CLI_BEST_EFFORT_ROOT_FILES);
            copy_single_file(
                &source_path,
                &target_path,
                &child_relative.to_string_lossy(),
                allow_locked_skip,
                stats,
            );
        }
    }
    Ok(())
}

fn copy_webview_dir(
    source: &Path,
    target: &Path,
    relative: &Path,
    stats: &mut CopyStats,
) -> anyhow::Result<()> {
    let entries = match fs::read_dir(source) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound || is_file_locked_error(&error) => {
            stats.skipped_locked_files += 1;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };

    for entry in entries {
        let entry = entry?;
        let name_os = entry.file_name();
        let name = name_os.to_string_lossy();
        let source_path = entry.path();
        if is_reparse_or_symlink(&source_path)? {
            continue;
        }
        let metadata = match fs::symlink_metadata(&source_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) if is_file_locked_error(&error) => {
                stats.skipped_locked_files += 1;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let child_relative = relative.join(&name_os);
        let target_path = target.join(&name_os);

        if metadata.is_dir() {
            if matches_any_ignore_case(&name, WEBVIEW_SKIP_DIRS) {
                continue;
            }
            copy_webview_dir(&source_path, &target_path, &child_relative, stats)?;
        } else if metadata.is_file() {
            if is_temp_or_lock_filename(&name)
                || matches_any_ignore_case(&name, WEBVIEW_SKIP_FILES)
            {
                continue;
            }
            copy_single_file(
                &source_path,
                &target_path,
                &child_relative.to_string_lossy(),
                true,
                stats,
            );
        }
    }
    Ok(())
}

fn copy_generic_dir(
    source: &Path,
    target: &Path,
    relative: &Path,
    allow_locked_skip: bool,
    stats: &mut CopyStats,
) -> anyhow::Result<()> {
    let entries = match fs::read_dir(source) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) if allow_locked_skip && is_file_locked_error(&error) => {
            stats.skipped_locked_files += 1;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };

    for entry in entries {
        let entry = entry?;
        let name_os = entry.file_name();
        let name = name_os.to_string_lossy();
        let source_path = entry.path();
        if is_reparse_or_symlink(&source_path)? {
            continue;
        }
        let metadata = match fs::symlink_metadata(&source_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let child_relative = relative.join(&name_os);
        let target_path = target.join(&name_os);

        if metadata.is_dir() {
            copy_generic_dir(
                &source_path,
                &target_path,
                &child_relative,
                allow_locked_skip,
                stats,
            )?;
        } else if metadata.is_file() {
            if is_temp_or_lock_filename(&name) {
                continue;
            }
            copy_single_file(
                &source_path,
                &target_path,
                &child_relative.to_string_lossy(),
                allow_locked_skip,
                stats,
            );
        }
    }
    Ok(())
}

struct TempFileGuard {
    path: PathBuf,
    armed: bool,
}

impl TempFileGuard {
    fn create_in(parent: &Path) -> io::Result<(File, Self)> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let pid = std::process::id();
        for _ in 0..32 {
            let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(".elves-migrate-{pid}-{sequence}.tmp"));
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(file) => return Ok((file, Self { path, armed: true })),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "无法创建唯一迁移临时文件",
        ))
    }

    fn persist_noclobber(mut self, target: &Path) -> io::Result<()> {
        if target.symlink_metadata().is_ok() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "target exists"));
        }
        fs::rename(&self.path, target)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn copy_single_file(
    source: &Path,
    target: &Path,
    label: &str,
    allow_locked_skip: bool,
    stats: &mut CopyStats,
) {
    if target.symlink_metadata().is_ok() {
        stats.skipped_existing_files += 1;
        return;
    }

    let Some(parent) = target.parent() else {
        record_failure(stats, label, "目标父目录无效");
        return;
    };
    if let Err(error) = fs::create_dir_all(parent) {
        record_failure(stats, label, &error.to_string());
        return;
    }

    let mut reader = match File::open(source) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) if allow_locked_skip && is_file_locked_error(&error) => {
            stats.skipped_locked_files += 1;
            return;
        }
        Err(error) => {
            record_failure(stats, label, &error.to_string());
            return;
        }
    };

    let (mut temp, guard) = match TempFileGuard::create_in(parent) {
        Ok(created) => created,
        Err(error) => {
            record_failure(stats, label, &error.to_string());
            return;
        }
    };

    if let Err(error) = io::copy(&mut reader, &mut temp) {
        drop(temp);
        if allow_locked_skip && is_file_locked_error(&error) {
            stats.skipped_locked_files += 1;
            return;
        }
        record_failure(stats, label, &error.to_string());
        return;
    }
    drop(temp);

    match guard.persist_noclobber(target) {
        Ok(()) => stats.copied_files += 1,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            stats.skipped_existing_files += 1;
        }
        Err(error) => {
            record_failure(stats, label, &error.to_string());
        }
    }
}

fn record_failure(stats: &mut CopyStats, label: &str, reason: &str) {
    if stats.failed_files.len() < MAX_RECORDED_SKIPPED_PATHS {
        stats.failed_files.push(format!("{label}（{reason}）"));
    }
}

fn write_marker(path: &Path, content: &str) -> anyhow::Result<()> {
    let parent = path.parent().context("迁移标记父目录无效")?;
    fs::create_dir_all(parent)?;
    let mut bytes = content.as_bytes().to_vec();
    bytes.push(b'\n');
    crate::settings::atomic_write(path, &bytes)
        .with_context(|| format!("写入迁移标记失败：{}", path.display()))
}

fn is_usable_source_dir(source: &Path, target: &Path) -> bool {
    if !is_regular_dir(source) {
        return false;
    }
    if let (Ok(source_canon), Ok(target_canon)) = (fs::canonicalize(source), fs::canonicalize(target))
        && source_canon == target_canon
    {
        return false;
    }
    true
}

fn is_regular_dir(path: &Path) -> bool {
    is_reparse_or_symlink(path).is_ok_and(|is_link| !is_link) && path.is_dir()
}

fn is_regular_file(path: &Path) -> bool {
    is_reparse_or_symlink(path).is_ok_and(|is_link| !is_link) && path.is_file()
}

fn is_reparse_or_symlink(path: &Path) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_temp_or_lock_filename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".lock") || lower.ends_with(".tmp")
}

fn matches_any_ignore_case(name: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

fn is_file_locked_error(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(32 | 33)) || error.kind() == io::ErrorKind::PermissionDenied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_state_desktop_and_cli_without_modifying_legacy_sources_and_skips_runtime_files() {
        let temp = tempfile::tempdir().unwrap();
        let legacy_state = temp.path().join(".codex-session-delete");
        let legacy_desktop = temp.path().join("LocalAppData").join("com.codexelves.remote");
        let legacy_cli = temp.path().join(".codex-elves-cli");
        let target_state = temp.path().join(".elves");

        // 构造旧版共享状态目录（含持久配置和应跳过的运行态文件）
        fs::create_dir_all(legacy_state.join("claude-session-turns")).unwrap();
        fs::create_dir_all(legacy_state.join("codex-app-server")).unwrap();
        fs::create_dir_all(legacy_state.join("locks")).unwrap();
        fs::create_dir_all(legacy_state.join("tool-runtime")).unwrap();
        fs::create_dir_all(legacy_state.join("desktop-input-files")).unwrap();
        fs::write(legacy_state.join("settings.json"), r#"{"theme":"dark"}"#).unwrap();
        fs::write(
            legacy_state.join("remote-claude-sessions.json"),
            r#"{"conversations":{}}"#,
        )
        .unwrap();
        fs::write(
            legacy_state.join("claude-session-turns").join("c1.json"),
            r#"[]"#,
        )
        .unwrap();
        fs::write(legacy_state.join("harness-service.json"), r#"{"pid":1}"#).unwrap();
        fs::write(legacy_state.join("remote-access-service.json"), r#"{"pid":2}"#).unwrap();
        fs::write(legacy_state.join("harness-service.lock"), "").unwrap();
        fs::write(legacy_state.join("local-proxy-service.lock"), "").unwrap();
        fs::write(legacy_state.join("claude-shared.py"), "# runtime helper").unwrap();
        fs::write(legacy_state.join("codex-app-server").join("manifest.json"), "{}").unwrap();
        fs::write(legacy_state.join("locks").join("45221.lock"), "").unwrap();
        fs::write(legacy_state.join("tool-runtime").join("rg.exe"), "bin").unwrap();
        fs::write(legacy_state.join("desktop-input-files").join("tmp.bin"), "tmp").unwrap();

        // 构造旧版桌面客户端目录（含设置、WebView2 持久数据和应跳过的缓存）
        fs::create_dir_all(legacy_desktop.join("webview/EBWebView/Default/Local Storage")).unwrap();
        fs::create_dir_all(legacy_desktop.join("webview/EBWebView/Default/Cache")).unwrap();
        fs::create_dir_all(legacy_desktop.join("webview/EBWebView/Crashpad")).unwrap();
        fs::create_dir_all(legacy_desktop.join("web-resources/origin1")).unwrap();
        fs::write(legacy_desktop.join("settings.json"), r#"{"window":{"width":1200}}"#).unwrap();
        fs::write(
            legacy_desktop.join("webview/EBWebView/Default/Local Storage/data.ldb"),
            "storage",
        )
        .unwrap();
        fs::write(
            legacy_desktop.join("webview/EBWebView/Default/Cache/cache.bin"),
            "cache",
        )
        .unwrap();
        fs::write(
            legacy_desktop.join("webview/EBWebView/Default/LOCK"),
            "lock",
        )
        .unwrap();
        fs::write(
            legacy_desktop.join("webview/EBWebView/Crashpad/report.dmp"),
            "dump",
        )
        .unwrap();
        fs::write(
            legacy_desktop.join("web-resources/origin1/index.html"),
            "<html></html>",
        )
        .unwrap();

        // 构造旧版 CLI Wrapper 目录
        fs::create_dir_all(&legacy_cli).unwrap();
        fs::write(legacy_cli.join("config.toml"), "model = \"gpt-5\"").unwrap();
        fs::write(legacy_cli.join("temp.lock"), "lock").unwrap();

        let sources = LegacyDataSources {
            state_dir: Some(legacy_state.clone()),
            desktop_candidates: vec![
                temp.path().join("LocalAppData").join("com.elves.app"),
                legacy_desktop.clone(),
            ],
            cli_dir: Some(legacy_cli.clone()),
        };

        let report = migrate_legacy_data_at(&sources, &target_state).unwrap();
        assert!(!report.has_failures());
        assert_eq!(report.state.outcome, ScopeOutcome::Migrated);
        assert_eq!(report.desktop.outcome, ScopeOutcome::Migrated);
        assert_eq!(report.cli.outcome, ScopeOutcome::Migrated);
        assert!(legacy_state_migration_completed(&target_state));

        // 新版共享状态已复制持久文件，跳过运行态文件
        assert_eq!(
            fs::read_to_string(target_state.join("settings.json")).unwrap(),
            r#"{"theme":"dark"}"#
        );
        assert!(target_state.join("claude-session-turns/c1.json").is_file());
        assert!(!target_state.join("harness-service.json").exists());
        assert!(!target_state.join("remote-access-service.json").exists());
        assert!(!target_state.join("harness-service.lock").exists());
        assert!(!target_state.join("local-proxy-service.lock").exists());
        assert!(!target_state.join("claude-shared.py").exists());
        assert!(!target_state.join("codex-app-server").exists());
        assert!(!target_state.join("locks").exists());
        assert!(!target_state.join("tool-runtime").exists());
        assert!(!target_state.join("desktop-input-files").exists());

        // 新版桌面目录已复制配置与 WebView2 持久存储，跳过缓存和锁
        assert_eq!(
            fs::read_to_string(target_state.join("desktop/settings.json")).unwrap(),
            r#"{"window":{"width":1200}}"#
        );
        assert!(
            target_state
                .join("desktop/webview/EBWebView/Default/Local Storage/data.ldb")
                .is_file()
        );
        assert!(!target_state.join("desktop/webview/EBWebView/Default/Cache").exists());
        assert!(!target_state.join("desktop/webview/EBWebView/Default/LOCK").exists());
        assert!(!target_state.join("desktop/webview/EBWebView/Crashpad").exists());
        assert!(
            target_state
                .join("desktop/web-resources/origin1/index.html")
                .is_file()
        );

        // 新版 CLI 目录已复制配置，跳过锁文件
        assert_eq!(
            fs::read_to_string(target_state.join("cli/config.toml")).unwrap(),
            "model = \"gpt-5\""
        );
        assert!(!target_state.join("cli/temp.lock").exists());

        // 旧版目录必须完整保留，不做任何删除或改写
        assert!(legacy_state.join("settings.json").is_file());
        assert!(legacy_state.join("harness-service.json").is_file());
        assert!(legacy_desktop.join("settings.json").is_file());
        assert!(legacy_cli.join("config.toml").is_file());

        // 再次运行安装迁移必须幂等，不覆盖用户后续修改的新版文件
        fs::write(target_state.join("settings.json"), r#"{"theme":"light"}"#).unwrap();
        let second = migrate_legacy_data_at(&sources, &target_state).unwrap();
        assert_eq!(second.state.outcome, ScopeOutcome::AlreadyMigrated);
        assert_eq!(second.desktop.outcome, ScopeOutcome::AlreadyMigrated);
        assert_eq!(second.cli.outcome, ScopeOutcome::AlreadyMigrated);
        assert_eq!(
            fs::read_to_string(target_state.join("settings.json")).unwrap(),
            r#"{"theme":"light"}"#
        );
    }

    #[test]
    fn desktop_source_selection_ignores_about_only_ebwebview_and_prefers_real_desktop_data() {
        let temp = tempfile::tempdir().unwrap();
        let com_elves = temp.path().join("LocalAppData").join("com.elves.app");
        let com_codexelves = temp.path().join("LocalAppData").join("com.codexelves.remote");
        let target_state = temp.path().join(".elves");

        // 场景 1：com.elves.app 里只有“关于 Elves”留下的 EBWebView，实际配置仍在 com.codexelves.remote
        fs::create_dir_all(com_elves.join("EBWebView")).unwrap();
        fs::write(com_elves.join("EBWebView/dummy"), "about").unwrap();
        fs::create_dir_all(&com_codexelves).unwrap();
        fs::write(com_codexelves.join("settings.json"), r#"{"window":{"width":1400}}"#).unwrap();

        let sources = LegacyDataSources {
            state_dir: None,
            desktop_candidates: vec![com_elves.clone(), com_codexelves.clone()],
            cli_dir: None,
        };
        let report = migrate_legacy_data_at(&sources, &target_state).unwrap();
        assert_eq!(report.desktop.source.as_deref(), Some(com_codexelves.as_path()));
        assert_eq!(
            fs::read_to_string(target_state.join("desktop/settings.json")).unwrap(),
            r#"{"window":{"width":1400}}"#
        );
        assert!(!target_state.join("desktop/EBWebView").exists());

        // 场景 2：com.elves.app 拥有真实的 settings.json 时优先同步 com.elves.app
        let target_state_2 = temp.path().join(".elves-2");
        fs::write(com_elves.join("settings.json"), r#"{"window":{"width":1600}}"#).unwrap();
        let report_2 = migrate_legacy_data_at(&sources, &target_state_2).unwrap();
        assert_eq!(report_2.desktop.source.as_deref(), Some(com_elves.as_path()));
        assert_eq!(
            fs::read_to_string(target_state_2.join("desktop/settings.json")).unwrap(),
            r#"{"window":{"width":1600}}"#
        );
        assert!(!target_state_2.join("desktop/EBWebView").exists());
    }

    #[test]
    fn preserves_existing_target_files_and_existing_webview_profile_without_clobbering() {
        let temp = tempfile::tempdir().unwrap();
        let legacy_state = temp.path().join(".codex-session-delete");
        let legacy_desktop = temp.path().join("LocalAppData").join("com.codexelves.remote");
        let target_state = temp.path().join(".elves");

        fs::create_dir_all(&legacy_state).unwrap();
        fs::write(legacy_state.join("settings.json"), "legacy-settings").unwrap();
        fs::write(legacy_state.join("remote-access-key.json"), "legacy-key").unwrap();

        fs::create_dir_all(legacy_desktop.join("webview/EBWebView/Default")).unwrap();
        fs::write(legacy_desktop.join("settings.json"), "legacy-window").unwrap();
        fs::write(
            legacy_desktop.join("webview/EBWebView/Default/Preferences"),
            "legacy-pref",
        )
        .unwrap();

        // 目标目录在安装前已有新版文件与新版 webview
        fs::create_dir_all(target_state.join("desktop/webview/EBWebView/Default")).unwrap();
        fs::write(target_state.join("settings.json"), "new-settings").unwrap();
        fs::write(
            target_state.join("desktop/webview/EBWebView/Default/Preferences"),
            "new-pref",
        )
        .unwrap();

        let sources = LegacyDataSources {
            state_dir: Some(legacy_state),
            desktop_candidates: vec![legacy_desktop],
            cli_dir: None,
        };
        let report = migrate_legacy_data_at(&sources, &target_state).unwrap();
        assert_eq!(report.state.copied_files, 1);
        assert_eq!(report.state.skipped_existing_files, 1);
        assert_eq!(
            fs::read_to_string(target_state.join("settings.json")).unwrap(),
            "new-settings"
        );
        assert_eq!(
            fs::read_to_string(target_state.join("remote-access-key.json")).unwrap(),
            "legacy-key"
        );
        assert_eq!(
            fs::read_to_string(target_state.join("desktop/settings.json")).unwrap(),
            "legacy-window"
        );
        assert_eq!(
            fs::read_to_string(
                target_state.join("desktop/webview/EBWebView/Default/Preferences")
            )
            .unwrap(),
            "new-pref"
        );
        assert!(
            report
                .desktop
                .notes
                .iter()
                .any(|note| note.contains("新版 webview 目录已存在"))
        );
    }

    #[test]
    fn windows_installer_script_never_removes_or_kills_codex_elves_and_runs_migration() {
        let script_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/installer/windows/Elves.nsi");
        let content = fs::read_to_string(&script_path)
            .unwrap_or_else(|error| panic!("读取 {} 失败：{error}", script_path.display()));
        let active_code: String = content
            .lines()
            .filter(|line| !line.trim_start().starts_with(';'))
            .collect::<Vec<_>>()
            .join("\n");
        let lower = active_code.to_ascii_lowercase();

        for forbidden in [
            "codex-elves",
            "codexelves",
            "legacyinstalldir",
            r"software\codexelves",
            r"uninstall\codexelves",
        ] {
            assert!(
                !lower.contains(forbidden),
                "Elves.nsi 不应包含针对 CodexElves 的删除、终止或注册表操作：{forbidden}"
            );
        }
        assert!(
            active_code.contains("--migrate-legacy-data"),
            "Elves.nsi 安装阶段必须触发旧版数据识别与同步（--migrate-legacy-data）"
        );
    }
}
