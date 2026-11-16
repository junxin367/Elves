//! Elves 本机入口独立于网页和 Android 的远程授权、端口与只读策略。
use super::*;

pub(super) const DEVICE_KEY_ID: &str = "elves-desktop";
const COOKIE: &str = "elves_desktop";
pub const DEFAULT_PORT: u16 = 45223;

pub(super) struct DesktopRuntime {
    handle: axum_server::Handle<SocketAddr>,
    server: tokio::task::JoinHandle<()>,
    pump: tokio::task::JoinHandle<()>,
}

impl DesktopRuntime {
    pub(super) async fn shutdown(self) {
        self.handle.shutdown();
        self.server.abort();
        self.pump.abort();
        let _ = self.server.await;
        let _ = self.pump.await;
    }
}

impl HarnessController {
    pub(super) async fn start_desktop(self: &Arc<Self>, port: u16) -> anyhow::Result<()> {
        let settings = RemoteAccessSettings {
            enabled: true,
            bind_host: "127.0.0.1".into(),
            port,
            read_only: false,
            allow_approvals: true,
            ..Default::default()
        };
        // 固定独立 Origin 保留客户端外观和草稿，不借用可关闭、可改端口的远程会话监听。
        let (handle, server, pump) = self.start_server_scoped(settings, true).await?;
        self.desktop_running.store(true, Ordering::Release);
        *self.desktop_runtime.lock().await = Some(DesktopRuntime {
            handle,
            server,
            pump,
        });
        self.desktop_port.store(port, Ordering::Release);
        self.harness.enabled.store(true, Ordering::Release);
        self.claude.set_reconnect_enabled(true);
        self.claude.reconnect();
        Ok(())
    }

    pub(super) fn desktop_bootstrap(&self) -> anyhow::Result<Value> {
        anyhow::ensure!(
            self.desktop_running.load(Ordering::Acquire),
            "Harness 本机入口尚未就绪"
        );
        let mut value = management::issue_local_proof(&self.state_dir);
        value["url"] = json!(format!(
            "http://127.0.0.1:{}/",
            self.desktop_port.load(Ordering::Acquire)
        ));
        Ok(value)
    }

    pub(super) async fn stop_desktop(&self) {
        self.desktop_running.store(false, Ordering::Release);
        if let Some(runtime) = self.desktop_runtime.lock().await.take() {
            runtime.shutdown().await;
        }
    }
}

pub(super) async fn authorize(state: &WebState, headers: &HeaderMap) -> Result<Device, ApiError> {
    if !state.controller.desktop_running.load(Ordering::Acquire)
        || state.controller.closing.load(Ordering::Acquire)
    {
        return Err(ApiError::new(503, "Harness 正在关闭"));
    }
    if headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        != Some(format!("127.0.0.1:{}", state.settings.port).as_str())
    {
        return Err(ApiError::new(403, "Elves 本机授权只能用于本机工作区"));
    }
    let token = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let (name, value) = cookie.trim().split_once('=')?;
                (name == COOKIE && value.len() <= 256).then_some(value)
            })
        })
        .ok_or_else(|| ApiError::new(401, "Elves 本机授权尚未就绪"))?;
    state
        .controller
        .desktop_device
        .lock()
        .await
        .as_ref()
        .filter(|device| constant_time_eq(&device.token_hash, &hash(token)))
        .cloned()
        .ok_or_else(|| ApiError::new(401, "Elves 本机授权已失效，请重试启动"))
}

pub(super) async fn connect(state: WebState) -> Result<Response, ApiError> {
    let token = random_token();
    let mut device = state.controller.desktop_device.lock().await;
    let mut current = device.clone().unwrap_or_else(|| Device {
        id: uuid::Uuid::new_v4().to_string(),
        name: "Elves".into(),
        token_hash: String::new(),
        created_at_ms: now_ms(),
        expires_at_ms: u64::MAX,
        last_seen_at_ms: now_ms(),
        key_id: DEVICE_KEY_ID.into(),
        public_origin: None,
        client_id_hash: String::new(),
    });
    current.token_hash = hash(&token);
    current.last_seen_at_ms = now_ms();
    crate::settings::atomic_write(
        &state
            .controller
            .state_dir
            .join("harness-desktop-device.json"),
        &serde_json::to_vec(&current).map_err(|_| ApiError::new(503, "本机授权无法编码"))?,
    )
    .map_err(|_| ApiError::new(503, "本机授权无法保存"))?;
    *device = Some(current.clone());
    // 本机身份不加入远程设备列表；远程换密钥、撤销设备和只读开关均不能使 Elves 退出。
    Ok((
        [(
            header::SET_COOKIE,
            format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=315360000"),
        )],
        Json(json!({"ok":true,"device":current.public()})),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, WebState) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../temp");
        std::fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("elves-desktop-")
            .tempdir_in(root)
            .unwrap();
        let controller = HarnessController::new(directory.path().to_owned()).unwrap();
        controller.desktop_running.store(true, Ordering::Release);
        controller
            .desktop_port
            .store(DEFAULT_PORT, Ordering::Release);
        controller.harness.enabled.store(true, Ordering::Release);
        let state = WebState {
            desktop: true,
            controller,
            settings: RemoteAccessSettings {
                port: DEFAULT_PORT,
                ..Default::default()
            },
        };
        (directory, state)
    }

    async fn login(state: &WebState) -> HeaderMap {
        let response = connect(state.clone()).await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(
                response.headers()[header::SET_COOKIE]
                    .to_str()
                    .unwrap()
                    .split(';')
                    .next()
                    .unwrap(),
            )
            .unwrap(),
        );
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:45223"));
        headers
    }

    #[tokio::test]
    async fn desktop_works_without_remote_access_or_access_key() {
        let (_directory, state) = fixture();
        let headers = login(&state).await;
        assert!(!state.settings.enabled);
        assert!(state.controller.auth.lock().await.access_key.is_none());
        let device = authorized(&state, &headers).await.unwrap();
        assert_eq!(device.key_id, DEVICE_KEY_ID);
        assert!(state.controller.auth.lock().await.devices.is_empty());
        state
            .controller
            .authorization(device, Some("send"))
            .check()
            .await
            .unwrap();
        let mut remote = state.clone();
        remote.desktop = false;
        assert_eq!(authorized(&remote, &headers).await.err().unwrap().code, 503);
    }

    #[tokio::test]
    async fn remote_restrictions_and_key_rotation_do_not_revoke_desktop() {
        let (directory, state) = fixture();
        let headers = login(&state).await;
        let device = authorized(&state, &headers).await.unwrap();
        configure_access_key(directory.path(), "desktop-independent-key").unwrap();
        state.controller.reload_access_key().await.unwrap();
        state
            .controller
            .reconcile(RemoteAccessSettings {
                enabled: false,
                read_only: true,
                allow_approvals: false,
                ..Default::default()
            })
            .await;
        // 远程开关的真实生命周期不能停止本机执行调度。
        assert!(state.controller.harness.enabled.load(Ordering::Acquire));
        state.controller.auth.lock().await.devices.clear();
        for action in ["send", "approve", "answer", "management"] {
            state
                .controller
                .authorization(device.clone(), Some(action))
                .check()
                .await
                .unwrap();
        }
        assert_eq!(authorized(&state, &headers).await.unwrap().id, device.id);
    }

    #[tokio::test]
    async fn desktop_proof_is_single_use_and_rejected_on_remote_transport() {
        let (directory, state) = fixture();
        let proof = management::issue_local_proof(directory.path());
        let payload = json!({"localProof": proof["proof"]});
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:45223"));
        let mut remote = state.clone();
        remote.desktop = false;
        let peer = "127.0.0.1:50000".parse().unwrap();
        assert_eq!(
            management::consume_local_proof(
                &remote,
                &headers,
                RequestTrustContext::default(),
                peer,
                &payload
            )
            .unwrap_err()
            .code,
            403
        );
        assert!(
            management::consume_local_proof(
                &state,
                &headers,
                RequestTrustContext::default(),
                peer,
                &payload
            )
            .unwrap()
        );
        assert_eq!(
            management::consume_local_proof(
                &state,
                &headers,
                RequestTrustContext::default(),
                peer,
                &payload
            )
            .unwrap_err()
            .code,
            401
        );
    }

    #[tokio::test]
    async fn desktop_channels_never_publish_public_or_secondary_origins() {
        let (_directory, state) = fixture();
        let headers = login(&state).await;
        *state.controller.public_origin.write().unwrap() = Some("https://public.example".into());
        *state.controller.channels.secondary.write().unwrap() =
            Some("https://secondary.example".into());
        state.controller.configure_secondary_channel(true);
        let Json(value) = channels::discover(State(state), headers).await.unwrap();
        assert_eq!(value["origins"], json!([]));
        assert_eq!(value["secondaryEnabled"], false);
        assert!(value["primaryOrigin"].is_null());
    }

    #[tokio::test]
    async fn desktop_identity_cannot_be_used_with_a_public_or_other_loopback_host() {
        let (_directory, state) = fixture();
        let mut headers = login(&state).await;
        for host in ["public.example", "localhost:45223", "127.0.0.1:45222"] {
            headers.insert(header::HOST, HeaderValue::from_str(host).unwrap());
            assert_eq!(authorized(&state, &headers).await.err().unwrap().code, 403);
        }
    }
}
