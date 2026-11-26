//! 单客户端安装后的本地协议代理生命周期，不启动 launcher、管理器或 Codex App。
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::settings::{BackendSettings, SettingsStore};

pub const PROXY_SERVICE_ARG: &str = "--proxy-service";
const PROXY_SERVICE_LOCK_FILE: &str = "local-proxy-service.lock";
const PROXY_SERVICE_STATUS_FILE: &str = "local-proxy-service.json";
const MONITOR_POLL_INTERVAL: Duration = Duration::from_millis(300);
const MONITOR_ERROR_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const SYNC_WAIT_TIMEOUT: Duration = Duration::from_millis(2500);
const SYNC_POLL_INTERVAL: Duration = Duration::from_millis(100);

static LAN_LISTENING: AtomicBool = AtomicBool::new(false);
static ERROR: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// 独立 `--proxy-service` 子进程持久化的运行态快照。
/// 用户要求在代理管理器中准确展示局域网监听状态、地址及真实启动错误，不能依赖仅在子进程内有效的内存静态值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyServiceRuntimeState {
    pub process_id: u32,
    pub listening: bool,
    pub lan_listening: bool,
    pub port: u16,
    #[serde(default)]
    pub error: Option<String>,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncServiceOutcome {
    Synced {
        listening: bool,
        lan_listening: bool,
    },
    Failed(String),
    Pending,
}

pub fn lan_listening() -> bool {
    let port = crate::protocol_proxy::DEFAULT_PROTOCOL_PROXY_PORT;
    if LAN_LISTENING.load(Ordering::Acquire) {
        return true;
    }
    read_runtime_state()
        .is_some_and(|state| state.listening && state.lan_listening && state.port == port)
}

pub fn last_error() -> Option<String> {
    let in_process = ERROR
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    in_process.or_else(|| read_runtime_state().and_then(|state| state.error))
}

fn remember_error(message: Option<String>) {
    *ERROR
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = message;
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn service_lock_is_held(state_dir: &Path) -> bool {
    let lock_path = state_dir.join(PROXY_SERVICE_LOCK_FILE);
    let Ok(file) = OpenOptions::new().read(true).write(true).open(&lock_path) else {
        return false;
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(error)
            if error.kind() == std::io::ErrorKind::WouldBlock
                || matches!(error.raw_os_error(), Some(32 | 33)) =>
        {
            true
        }
        Err(_) => false,
    }
}

fn write_runtime_state_to(
    state_dir: &Path,
    listening: bool,
    lan_listening: bool,
    error: Option<&str>,
) {
    let state = ProxyServiceRuntimeState {
        process_id: std::process::id(),
        listening,
        lan_listening,
        port: crate::protocol_proxy::DEFAULT_PROTOCOL_PROXY_PORT,
        error: error.map(ToOwned::to_owned),
        updated_at_ms: now_ms(),
    };
    if let Ok(bytes) = serde_json::to_vec(&state) {
        let _ = crate::settings::atomic_write(&state_dir.join(PROXY_SERVICE_STATUS_FILE), &bytes);
    }
}

pub fn read_runtime_state() -> Option<ProxyServiceRuntimeState> {
    read_runtime_state_from(&crate::paths::default_app_state_dir())
}

fn read_runtime_state_from(state_dir: &Path) -> Option<ProxyServiceRuntimeState> {
    // 只有独立代理服务仍持有独占锁时才采信状态文件，避免读取异常退出遗留的过期快照。
    if !service_lock_is_held(state_dir) {
        return None;
    }
    let bytes = fs::read(state_dir.join(PROXY_SERVICE_STATUS_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub struct ProxyMonitor {
    shutdown: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl ProxyMonitor {
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = self.task.await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureServiceResult {
    Disabled,
    AlreadyRunning,
    Started { process_id: u32 },
}

/// 由 Elves 客户端或独立管理器调用，确保本地代理以独立进程运行。
///
/// 代理进程只读取共享设置并自行监视供应商切换；调用方退出、管理器关闭或页面不参与请求
/// 都不会回收该进程。用户要求“随 Elves 启动”开关只控制这里是否拉起服务，不改变供应商路由。
pub async fn ensure_service_process(executable: &Path) -> anyhow::Result<EnsureServiceResult> {
    let settings = SettingsStore::default()
        .load()
        .context("读取本地代理设置失败")?;
    ensure_service_process_for_settings(executable, &settings).await
}

pub async fn ensure_service_process_for_settings(
    executable: &Path,
    settings: &BackendSettings,
) -> anyhow::Result<EnsureServiceResult> {
    if !desired_state(settings).0 {
        return Ok(EnsureServiceResult::Disabled);
    }
    let state_dir = crate::paths::default_app_state_dir();
    if service_lock_is_held(&state_dir)
        || crate::ports::loopback_listener_is_running(
            crate::protocol_proxy::DEFAULT_PROTOCOL_PROXY_PORT,
        )
        .await
    {
        return Ok(EnsureServiceResult::AlreadyRunning);
    }
    let mut command = Command::new(executable);
    command
        .arg(PROXY_SERVICE_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(crate::windows_integration::CREATE_NO_WINDOW);
    }
    let child = command
        .spawn()
        .with_context(|| format!("启动独立本地代理服务失败：{}", executable.display()))?;
    Ok(EnsureServiceResult::Started {
        process_id: child.id(),
    })
}

/// 在管理器保存代理/供应商设置后同步独立代理服务状态：
/// - 期望运行时确保 `--proxy-service` 已启动；
/// - 短暂等待独立代理完成重绑（例如 127.0.0.1 ↔ 0.0.0.0 切换或启停），使管理器界面立即看到真实结果。
pub async fn sync_after_settings_change(
    executable: &Path,
    settings: &BackendSettings,
) -> SyncServiceOutcome {
    let desired = desired_state(settings);
    let state_dir = crate::paths::default_app_state_dir();
    let port = crate::protocol_proxy::DEFAULT_PROTOCOL_PROXY_PORT;
    if desired.0 {
        if let Err(error) = ensure_service_process_for_settings(executable, settings).await {
            return SyncServiceOutcome::Failed(format!("{error:#}"));
        }
    }
    if !desired.0 && !service_lock_is_held(&state_dir) {
        let listening = crate::ports::loopback_listener_is_running(port).await;
        return if listening {
            SyncServiceOutcome::Pending
        } else {
            SyncServiceOutcome::Synced {
                listening: false,
                lan_listening: false,
            }
        };
    }

    let deadline = Instant::now() + SYNC_WAIT_TIMEOUT;
    loop {
        if let Some(runtime) = read_runtime_state_from(&state_dir) {
            if let Some(error) = runtime.error {
                return SyncServiceOutcome::Failed(error);
            }
            if runtime.port == port
                && runtime.listening == desired.0
                && runtime.lan_listening == desired.1
            {
                let port_matches = if desired.0 {
                    crate::ports::loopback_listener_is_running(port).await
                } else {
                    !crate::ports::loopback_listener_is_running(port).await
                };
                if port_matches {
                    return SyncServiceOutcome::Synced {
                        listening: runtime.listening,
                        lan_listening: runtime.lan_listening,
                    };
                }
            }
        }
        if Instant::now() >= deadline {
            return SyncServiceOutcome::Pending;
        }
        tokio::time::sleep(SYNC_POLL_INTERVAL).await;
    }
}

/// 运行独立本地代理服务。进程使用锁文件保证 Elves、管理器重复启动时只保留一个实例。
pub fn run_service_process() -> anyhow::Result<()> {
    let state_dir = crate::paths::default_app_state_dir();
    fs::create_dir_all(&state_dir)?;
    let lock_path = state_dir.join(PROXY_SERVICE_LOCK_FILE);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let _lock = lock;
        let monitor = start_monitor();
        if std::env::var_os("ELVES_PROXY_SERVICE_ONESHOT").is_some() {
            monitor.shutdown().await;
            return Ok::<(), anyhow::Error>(());
        }
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    })
}

fn settings_file_fingerprint(path: &Path) -> Option<(u64, SystemTime)> {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}

pub fn start_monitor() -> ProxyMonitor {
    let (shutdown, mut receiver) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let state_dir = crate::paths::default_app_state_dir();
        let settings_path = crate::paths::default_settings_path();
        let mut runtime: Option<crate::protocol_server::LocalProxyRuntime> = None;
        let mut active: Option<(bool, bool)> = None;
        let mut last_fingerprint: Option<(u64, SystemTime)> = None;
        let mut retry_after: Option<Instant> = Some(Instant::now());
        loop {
            let current_fingerprint = settings_file_fingerprint(&settings_path);
            let fingerprint_changed = current_fingerprint != last_fingerprint;
            let retry_due = retry_after.is_some_and(|deadline| Instant::now() >= deadline);
            if fingerprint_changed || retry_due {
                last_fingerprint = current_fingerprint;
                match SettingsStore::default().load() {
                    Ok(settings) => {
                        // 用户分别控制供应商路由和“随 Elves 启动”策略；关闭启动策略时保留独立监视进程但不监听端口。
                        let desired = desired_state(&settings);
                        let enabled = desired.0;
                        if active != Some(desired) {
                            if let Some(previous) = runtime.take() {
                                previous.shutdown().await;
                            }
                            LAN_LISTENING.store(false, Ordering::Release);
                            if enabled {
                                match crate::protocol_server::start(&settings).await {
                                    Ok(started) => {
                                        runtime = Some(started);
                                        active = Some(desired);
                                        retry_after = None;
                                        LAN_LISTENING.store(desired.1, Ordering::Release);
                                        remember_error(None);
                                        write_runtime_state_to(&state_dir, true, desired.1, None);
                                    }
                                    Err(error) => {
                                        let message = format!("本地代理未启动：{error:#}");
                                        active = None;
                                        retry_after =
                                            Some(Instant::now() + MONITOR_ERROR_RETRY_INTERVAL);
                                        remember_error(Some(message.clone()));
                                        write_runtime_state_to(
                                            &state_dir,
                                            false,
                                            false,
                                            Some(&message),
                                        );
                                    }
                                }
                            } else {
                                active = Some(desired);
                                retry_after = None;
                                remember_error(None);
                                write_runtime_state_to(&state_dir, false, false, None);
                            }
                        } else {
                            retry_after = None;
                        }
                    }
                    Err(error) => {
                        let message = format!("无法读取本地代理配置：{error:#}");
                        retry_after = Some(Instant::now() + MONITOR_ERROR_RETRY_INTERVAL);
                        remember_error(Some(message.clone()));
                        write_runtime_state_to(
                            &state_dir,
                            runtime.is_some(),
                            LAN_LISTENING.load(Ordering::Acquire),
                            Some(&message),
                        );
                    }
                }
            }
            tokio::select! {
                _ = &mut receiver => break,
                _ = tokio::time::sleep(MONITOR_POLL_INTERVAL) => {}
            }
        }
        if let Some(runtime) = runtime {
            runtime.shutdown().await;
        }
        LAN_LISTENING.store(false, Ordering::Release);
        write_runtime_state_to(&state_dir, false, false, None);
    });
    ProxyMonitor { shutdown, task }
}

pub fn desired_state(settings: &BackendSettings) -> (bool, bool) {
    // 用户要求启动策略与供应商 localProxyEnabled 分离；这里只决定服务生命周期，不改写供应商配置。
    let enabled = settings.local_proxy_startup_enabled
        && settings.relay_profiles_enabled
        && settings.active_relay_uses_protocol_proxy();
    (enabled, enabled && settings.lan_proxy_enabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{BackendSettings, RelayMode, RelayProfile};

    #[test]
    fn saved_proxy_switch_controls_startup_independently_from_remote_access() {
        let profile = RelayProfile {
            id: "proxy".into(),
            relay_mode: RelayMode::PureApi,
            local_proxy_enabled: Some(true),
            ..Default::default()
        };
        let mut settings = BackendSettings {
            relay_profiles: vec![profile],
            active_relay_id: "proxy".into(),
            relay_profiles_enabled: true,
            local_proxy_startup_enabled: true,
            ..Default::default()
        };
        for enabled in [false, true] {
            settings.remote_access.enabled = enabled;
            settings.remote_access.read_only = true;
            assert_eq!(desired_state(&settings), (true, false));
        }
        settings.lan_proxy_enabled = true;
        assert_eq!(desired_state(&settings), (true, true));
        settings.relay_profiles[0].local_proxy_enabled = Some(false);
        assert_eq!(desired_state(&settings), (false, false));
        settings.relay_profiles[0].local_proxy_enabled = Some(true);
        settings.local_proxy_startup_enabled = false;
        assert_eq!(desired_state(&settings), (false, false));
        assert!(settings.active_relay_uses_protocol_proxy());
    }

    #[test]
    fn runtime_state_file_is_only_trusted_while_service_lock_is_held() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path();
        write_runtime_state_to(state_dir, true, true, None);
        assert!(
            read_runtime_state_from(state_dir).is_none(),
            "没有持有服务锁时不应把遗留状态文件当成运行中的局域网监听"
        );

        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state_dir.join(PROXY_SERVICE_LOCK_FILE))
            .unwrap();
        lock.lock_exclusive().unwrap();
        let loaded = read_runtime_state_from(state_dir).expect("runtime state while locked");
        assert!(loaded.listening);
        assert!(loaded.lan_listening);
        assert_eq!(
            loaded.port,
            crate::protocol_proxy::DEFAULT_PROTOCOL_PROXY_PORT
        );

        drop(lock);
        assert!(read_runtime_state_from(state_dir).is_none());
    }
}
