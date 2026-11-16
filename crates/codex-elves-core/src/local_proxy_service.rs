//! 单客户端安装后的本地协议代理生命周期，不启动 launcher、管理器或 Codex App。
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use crate::settings::SettingsStore;

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

pub fn start_monitor() -> ProxyMonitor {
    let (shutdown, mut receiver) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut runtime: Option<crate::launcher::LocalProxyRuntime> = None;
        let mut active: Option<(bool, bool)> = None;
        loop {
            match SettingsStore::default().load() {
                Ok(settings) => {
                    // 用户仍由“启用本地代理”控制代理；仅启动客户端或保存普通供应商不擅自启用代理。
                    let enabled = settings.relay_profiles_enabled
                        && settings.active_relay_uses_protocol_proxy();
                    let desired = (enabled, enabled && settings.lan_proxy_enabled);
                    if active != Some(desired) {
                        if let Some(previous) = runtime.take() {
                            previous.shutdown().await;
                        }
                        LAN_LISTENING.store(false, Ordering::Release);
                        if enabled {
                            match crate::launcher::start_local_proxy_only(&settings).await {
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
