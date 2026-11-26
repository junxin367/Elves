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
        let mut value = local_proof::issue_local_proof(&self.state_dir);
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

    #[derive(Default)]
    struct DesktopActionsBackend {
        sends: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl TestRemoteBackend for DesktopActionsBackend {
        async fn call(&self, action: &str, payload: Value) -> anyhow::Result<Value> {
            anyhow::ensure!(action == "send", "unexpected test action: {action}");
            self.sends.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"ok":true,"threadId":payload["threadId"],"deliveryState":"accepted"}))
        }
    }

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
            .authorization(device.clone(), Some("send"))
            .check()
            .await
            .unwrap();
        let remote_lock = state.controller.auth.lock().await;
        let recovered = tokio::time::timeout(
            Duration::from_secs(1),
            state
                .controller
                .resolve_device(&device.id, Some(DEVICE_KEY_ID)),
        )
        .await
        .expect("本机身份恢复不能等待远程授权锁");
        assert_eq!(recovered.unwrap().id, device.id);
        drop(remote_lock);
        let mut remote = state.clone();
        remote.desktop = false;
        assert_eq!(authorized(&remote, &headers).await.err().unwrap().code, 503);
    }

    #[tokio::test]
    async fn desktop_actions_accept_local_identity_and_keep_idempotent_receipts() {
        let (_directory, state) = fixture();
        let backend = Arc::new(DesktopActionsBackend::default());
        assert!(state.controller.test_backend.set(backend.clone()).is_ok());
        let mut headers = login(&state).await;
        let payload = json!({
            "threadId":"e56d7c7d-645e-43d5-ae5b-e097fe1f9bdc",
            "prompt":"desktop action regression"
        });
        for request in ["desktop-send-a", "desktop-send-b", "desktop-send-a"] {
            headers.insert("idempotency-key", HeaderValue::from_str(request).unwrap());
            let Json(receipt) = action(
                State(state.clone()),
                Path("send".into()),
                headers.clone(),
                Json(payload.clone()),
            )
            .await
            .unwrap();
            assert_eq!(receipt["state"], "completed");
        }
        assert_eq!(backend.sends.load(Ordering::SeqCst), 2);
        assert!(state.controller.auth.lock().await.devices.is_empty());
        let Json(saved) = receipt(
            State(state.clone()),
            Path("desktop-send-a".into()),
            headers.clone(),
        )
        .await
        .unwrap();
        assert_eq!(saved["state"], "completed");
        let device = authorized(&state, &headers).await.unwrap();
        assert_eq!(
            state
                .controller
                .execute(
                    device,
                    "desktop-send-a".into(),
                    "send".into(),
                    json!({"threadId":payload["threadId"],"prompt":"different"}),
                )
                .await
                .unwrap_err()
                .code,
            409
        );
    }

    #[tokio::test]
    async fn desktop_receipts_survive_reconnect_and_restart_without_remote_credentials() {
        let (directory, state) = fixture();
        let backend = Arc::new(DesktopActionsBackend::default());
        assert!(state.controller.test_backend.set(backend.clone()).is_ok());
        let headers = login(&state).await;
        let device = authorized(&state, &headers).await.unwrap();
        let payload = json!({"threadId":"desktop-thread","prompt":"persisted request"});
        state
            .controller
            .execute(
                device.clone(),
                "desktop-restart".into(),
                "send".into(),
                payload.clone(),
            )
            .await
            .unwrap();
        let controller = HarnessController::new(directory.path().to_owned()).unwrap();
        controller.desktop_running.store(true, Ordering::Release);
        assert!(controller.test_backend.set(backend.clone()).is_ok());
        let restarted = WebState {
            controller,
            ..state
        };
        let next_headers = login(&restarted).await;
        let next_device = authorized(&restarted, &next_headers).await.unwrap();
        assert_eq!(next_device.id, device.id);
        assert!(
            restarted
                .controller
                .authorization(device, Some("send"))
                .check()
                .await
                .is_err(),
            "重连更新 Cookie 后不能沿用旧令牌"
        );
        let repeated = restarted
            .controller
            .execute(
                next_device,
                "desktop-restart".into(),
                "send".into(),
                payload,
            )
            .await
            .unwrap();
        assert_eq!(repeated["state"], "completed");
        assert_eq!(backend.sends.load(Ordering::SeqCst), 1);
        assert!(restarted.controller.auth.lock().await.access_key.is_none());
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
        let proof = local_proof::issue_local_proof(directory.path());
        let payload = json!({"localProof": proof["proof"]});
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:45223"));
        let mut remote = state.clone();
        remote.desktop = false;
        let peer = "127.0.0.1:50000".parse().unwrap();
        assert_eq!(
            local_proof::consume_local_proof(
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
            local_proof::consume_local_proof(
                &state,
                &headers,
                RequestTrustContext::default(),
                peer,
                &payload
            )
            .unwrap()
        );
        assert_eq!(
            local_proof::consume_local_proof(
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

    #[tokio::test]
    async fn management_is_native_only_on_http_even_with_remote_credentials_or_forged_client_headers()
     {
        let (_directory, remote_authorization) = crate::tests::service_dispatch_fixture().await;
        let controller = remote_authorization.controller;
        let local = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_port = local.local_addr().unwrap().port();
        let remote_port = remote.local_addr().unwrap().port();
        controller.desktop_running.store(true, Ordering::Release);
        controller.desktop_port.store(local_port, Ordering::Release);
        let local_router = scoped_router(
            controller.clone(),
            RemoteAccessSettings {
                port: local_port,
                ..Default::default()
            },
            true,
        );
        let remote_router = scoped_router(
            controller.clone(),
            RemoteAccessSettings {
                enabled: true,
                port: remote_port,
                ..Default::default()
            },
            false,
        );
        let local_server = tokio::spawn(async move {
            axum::serve(
                local,
                local_router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let remote_server = tokio::spawn(async move {
            axum::serve(
                remote,
                remote_router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let local_url = format!("http://127.0.0.1:{local_port}");
        let remote_url = format!("http://127.0.0.1:{remote_port}");
        assert_eq!(
            client
                .get(format!("{local_url}/api/session"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            client
                .get(format!("{remote_url}/api/session"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            client
                .post(format!("{local_url}/api/connect"))
                .header("Origin", &local_url)
                .json(&json!({"key":"remote-password-cannot-authorize-desktop"}))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        let proof = local_proof::issue_local_proof(&controller.state_dir);
        let connected = client
            .post(format!("{local_url}/api/connect"))
            .header("Origin", &local_url)
            .json(&json!({"localProof":proof["proof"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(connected.status(), 200);
        let local_cookie = connected.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let remote_cookie = format!("{COOKIE_NAME}=isolated-service-dispatch-token");
        let local_session: Value = client
            .get(format!("{local_url}/api/session"))
            .header("Cookie", &local_cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(local_session["transport"], "desktop");
        assert_eq!(
            local_session["capabilities"]["workspaceOpenIn"],
            cfg!(windows)
        );
        let remote_session: Value = client
            .get(format!("{remote_url}/api/session"))
            .header("Cookie", &remote_cookie)
            .header("User-Agent", "Elves")
            .header("X-Elves-Client", "desktop")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(remote_session["transport"], "remote");
        assert_eq!(remote_session["capabilities"]["management"], false);
        assert_eq!(remote_session["capabilities"]["workspaceOpenIn"], false);
        // 用户要求“关闭其他占用”在本机客户端、网页、PWA、手机和 Android 上一样可用：两种设备都声明该能力，
        // 直接请求也按同一规则校验和路由，不再按设备类型拒绝（与上面的宿主机启动能力相反，不能把它改回本机专属）。
        assert_eq!(
            local_session["capabilities"]["releaseExternalHolders"],
            true
        );
        assert_eq!(
            remote_session["capabilities"]["releaseExternalHolders"],
            true
        );
        for (label, url, cookie) in [
            ("local", &local_url, &local_cookie),
            ("remote", &remote_url, &remote_cookie),
        ] {
            // 只接受会话标识：多余字段、缺少标识都在进入引擎前被拒绝。
            for body in [
                json!({}),
                json!({"threadId":"claude-unknown","pid":1}),
                json!({"threadId":"../etc"}),
            ] {
                let response = client
                    .post(format!("{url}/api/action/releaseExternalHolders"))
                    .header("Origin", url)
                    .header("Cookie", cookie)
                    .header("Idempotency-Key", format!("{label}-release-invalid"))
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), 400, "{label} {body}");
            }
            // 合法请求会进入各自引擎：不存在的 Claude 会话由 Claude 引擎报告，非 Claude 会话明确拒绝，
            // 都不会被白名单、授权或 Codex 路径悄悄吞掉。
            for (thread, message) in [
                (
                    "claude-00000000-0000-4000-8000-000000000000",
                    "Claude 会话不存在或已删除",
                ),
                (
                    "019a0000-0000-7000-8000-000000000000",
                    "仅 Claude 会话支持关闭其他占用",
                ),
            ] {
                let routed: Value = client
                    .post(format!("{url}/api/action/releaseExternalHolders"))
                    .header("Origin", url)
                    .header("Cookie", cookie)
                    .header("Idempotency-Key", format!("{label}-release-{thread}"))
                    .json(&json!({"threadId":thread}))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                assert_eq!(routed["state"], "failed", "{label} {thread}");
                assert_eq!(routed["result"]["message"], message, "{label} {thread}");
            }
        }
        // 用户要求远程 PC、手机和 Android 均无宿主机启动能力；伪造原生标记与旧缓存直接请求也不能绕过。
        for path in ["/api/workspace/open-in-targets", "/api/workspace/open-in"] {
            for user_agent in [
                "Mozilla/5.0 Windows",
                "Mozilla/5.0 iPhone Mobile",
                "Elves Android",
            ] {
                let denied = client
                    .post(format!("{remote_url}{path}"))
                    .header("Origin", &remote_url)
                    .header("Cookie", &remote_cookie)
                    .header("User-Agent", user_agent)
                    .header("X-Elves-Client", "desktop")
                    .header("Idempotency-Key", "remote-open-in-denied")
                    .json(&json!({"threadId":"unknown-session","target":"fileManager"}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(denied.status(), 403, "{path} {user_agent}");
            }
            // 本机请求继续进入原有参数校验；使用空参数避免真的读取会话或启动应用。
            let local = client
                .post(format!("{local_url}{path}"))
                .header("Origin", &local_url)
                .header("Cookie", &local_cookie)
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            assert_eq!(local.status(), 400, "{path}");
        }
        assert_eq!(
            client
                .get(format!("{local_url}/api/session"))
                .header("Cookie", &remote_cookie)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            client
                .get(format!("{remote_url}/api/session"))
                .header("Cookie", &local_cookie)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            client
                .get(format!("{local_url}/management/index.html"))
                .header("Cookie", &local_cookie)
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        for path in ["/management", "/management/", "/management/index.html"] {
            let denied = client
                .get(format!("{remote_url}{path}"))
                .header("User-Agent", "Elves")
                .header("X-Elves-Client", "desktop")
                .header("If-None-Match", "*")
                .header("Cookie", &remote_cookie)
                .send()
                .await
                .unwrap();
            assert_eq!(denied.status(), 404, "{path}");
        }
        for command in [
            "load_settings",
            "save_settings",
            "read_relay_files",
            "list_local_sessions",
            "read_local_proxy_logs",
            "read_local_proxy_log_detail",
            "remote_access_command",
        ] {
            // 旧管理 API 已从 Harness 删除；即使携带远程授权也只能得到未找到。
            assert_eq!(
                client
                    .post(format!("{remote_url}/api/management/{command}"))
                    .header("Origin", &remote_url)
                    .header("Cookie", &remote_cookie)
                    .header("Content-Type", "application/json")
                    .body("invalid-json")
                    .send()
                    .await
                    .unwrap()
                    .status(),
                404,
                "{command}"
            );
        }
        // 用户要求远程会话配置保留在 Elves 本机 Harness 中；网页、PWA、Android 必须一律拒绝。
        let local_session: Value = client
            .get(format!("{local_url}/api/session"))
            .header("Cookie", &local_cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let remote_session: Value = client
            .get(format!("{remote_url}/api/session"))
            .header("Cookie", &remote_cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(local_session["capabilities"]["desktopRemoteAccess"], true);
        assert_eq!(local_session["capabilities"]["management"], false);
        assert_eq!(remote_session["capabilities"]["desktopRemoteAccess"], false);
        assert_eq!(remote_session["capabilities"]["management"], false);
        for action in [
            "status",
            "credentials",
            "generateKey",
            "setKey",
            "saveSettings",
            "openFrpDirectory",
        ] {
            let denied = client
                .post(format!("{remote_url}/api/desktop/remote-access/{action}"))
                .header("Origin", &remote_url)
                .header("Cookie", &remote_cookie)
                .header("User-Agent", "Elves")
                .header("X-Elves-Client", "desktop")
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            assert_eq!(denied.status(), 403, "remote {action}");
        }
        assert_eq!(
            client
                .post(format!("{local_url}/api/desktop/remote-access/status"))
                .header("Origin", &local_url)
                .header("Cookie", &remote_cookie)
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let generated: Value = client
            .post(format!("{local_url}/api/desktop/remote-access/generateKey"))
            .header("Origin", &local_url)
            .header("Cookie", &local_cookie)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(generated["status"], "ok");
        assert!(generated["key"].as_str().is_some_and(|key| key.len() >= 16));
        let credentials: Value = client
            .post(format!("{local_url}/api/desktop/remote-access/credentials"))
            .header("Origin", &local_url)
            .header("Cookie", &local_cookie)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(credentials["status"], "ok");
        assert_eq!(credentials["accessKeyConfigured"], true);
        let saved: Value = client
            .post(format!("{local_url}/api/desktop/remote-access/saveSettings"))
            .header("Origin", &local_url)
            .header("Cookie", &local_cookie)
            .json(&json!({
                "apply": false,
                "settings": {
                    "enabled": false,
                    "ngrokEnabled": false,
                    "bindHost": "127.0.0.1",
                    "port": 45222,
                    "zrokSetupValue": "demo-token",
                    "deviceSessionHours": 24,
                    "readOnly": false,
                    "allowApprovals": true
                }
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(saved["status"], "ok");
        assert_eq!(saved["settings"]["deviceSessionHours"], 24);
        assert_eq!(saved["settings"]["zrokSetupValue"], "demo-token");
        let status_body: Value = client
            .post(format!("{local_url}/api/desktop/remote-access/status"))
            .header("Origin", &local_url)
            .header("Cookie", &local_cookie)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status_body["status"], "ok");
        assert_eq!(status_body["settings"]["deviceSessionHours"], 24);
        for reserved in [
            crate::protocol_proxy::DEFAULT_PROTOCOL_PROXY_PORT,
            crate::settings::DEFAULT_DESKTOP_PORT,
        ] {
            let rejected: Value = client
                .post(format!("{local_url}/api/desktop/remote-access/saveSettings"))
                .header("Origin", &local_url)
                .header("Cookie", &local_cookie)
                .json(&json!({
                    "apply": false,
                    "settings": {
                        "enabled": false,
                        "ngrokEnabled": false,
                        "bindHost": "127.0.0.1",
                        "port": reserved,
                        "zrokSetupValue": "",
                        "deviceSessionHours": 0,
                        "readOnly": false,
                        "allowApprovals": true
                    }
                }))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(rejected["status"], "failed", "reserved port {reserved}");
        }
        let local_manifest: Value = client
            .get(format!("{local_url}/app-resources.json"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let remote_manifest: Value = client
            .get(format!("{remote_url}/app-resources.json"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            !remote_manifest["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file["path"].as_str().is_some_and(|path| path.starts_with("/management")))
        );
        assert!(
            !local_manifest["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file["path"].as_str().is_some_and(|path| path.starts_with("/management")))
        );
        assert_ne!(local_manifest["version"], remote_manifest["version"]);
        local_server.abort();
        remote_server.abort();
        let _ = local_server.await;
        let _ = remote_server.await;
    }
}
