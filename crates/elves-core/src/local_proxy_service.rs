//! 单客户端安装后的本地协议代理生命周期，不启动 launcher、管理器或 Codex App。
use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::Context;
use fs2::FileExt;

use crate::settings::SettingsStore;

pub const PROXY_SERVICE_ARG: &str = "--proxy-service";
const PROXY_SERVICE_LOCK_FILE: &str = "local-proxy-service.lock";
static LAN_LISTENING: AtomicBool = AtomicBool::new(false);
static ERROR: OnceLock<Mutex<Option<String>>> = OnceLock::new();

pub fn lan_listening() -> bool {
    LAN_LISTENING.load(Ordering::Acquire)
}

pub fn last_error() -> Option<String> {
    ERROR
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

fn remember_error(message: Option<String>) {
    *ERROR
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = message;
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
    if !desired_state(&settings).0 {
        return Ok(EnsureServiceResult::Disabled);
    }
    if crate::ports::loopback_listener_is_running(
        crate::protocol_proxy::DEFAULT_PROTOCOL_PROXY_PORT,
    )
    .await
    {
        return Ok(EnsureServiceResult::AlreadyRunning);
    }
    let child = Command::new(executable)
        .arg(PROXY_SERVICE_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("启动独立本地代理服务失败：{}", executable.display()))?;
    Ok(EnsureServiceResult::Started {
        process_id: child.id(),
    })
}

/// 运行独立本地代理服务。进程使用锁文件保证 Elves、管理器重复启动时只保留一个实例。
pub fn run_service_process() -> anyhow::Result<()> {
    let state_dir = crate::paths::default_app_state_dir();
    fs::create_dir_all(&state_dir)?;
    let lock_path = state_dir.join(PROXY_SERVICE_LOCK_FILE);
    let lock = File::create(&lock_path)?;
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

pub fn start_monitor() -> ProxyMonitor {
    let (shutdown, mut receiver) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut runtime: Option<crate::protocol_server::LocalProxyRuntime> = None;
        let mut active: Option<(bool, bool)> = None;
        loop {
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
                                    LAN_LISTENING.store(desired.1, Ordering::Release);
                                    remember_error(None);
                                }
                                Err(error) => {
                                    remember_error(Some(format!("本地代理未启动：{error:#}")))
                                }
                            }
                        } else {
                            active = Some(desired);
                            remember_error(None);
                        }
                    }
                }
                Err(error) => remember_error(Some(format!("无法读取本地代理配置：{error:#}"))),
            }
            tokio::select! {
                _ = &mut receiver => break,
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
        if let Some(runtime) = runtime {
            runtime.shutdown().await;
        }
        LAN_LISTENING.store(false, Ordering::Release);
    });
    ProxyMonitor { shutdown, task }
}

fn desired_state(settings: &crate::settings::BackendSettings) -> (bool, bool) {
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
}
