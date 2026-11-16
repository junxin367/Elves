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
        let mut runtime: Option<crate::protocol_server::LocalProxyRuntime> = None;
        let mut active: Option<(bool, bool)> = None;
        loop {
            match SettingsStore::default().load() {
                Ok(settings) => {
                    // 用户仍由“启用本地代理”控制代理；仅启动客户端或保存普通供应商不擅自启用代理。
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
    let enabled = settings.relay_profiles_enabled && settings.active_relay_uses_protocol_proxy();
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
    }
}
