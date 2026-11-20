use std::fs::{File, OpenOptions};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::extract::{DefaultBodyLimit, Path as RoutePath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const ENDPOINT_FILE: &str = "harness-service.json";
const LOCK_FILE: &str = "harness-service.lock";
// 协议 12 将管理能力限定到本机身份；新客户端必须更换旧后台，不能沿用仍向远程开放管理的服务。
const PROTOCOL_VERSION: u32 = 12;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(windows)]
static OWNED_SERVICE_JOB: OnceLock<Mutex<Option<std::os::windows::io::OwnedHandle>>> =
    OnceLock::new();
static SERVICE_EXECUTABLE: OnceLock<std::path::PathBuf> = OnceLock::new();

pub fn set_service_executable(path: std::path::PathBuf) {
    let _ = SERVICE_EXECUTABLE.set(path);
}

pub fn owns_service() -> bool {
    #[cfg(windows)]
    {
        OWNED_SERVICE_JOB.get().is_some_and(|job| {
            job.lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_some()
        })
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn shutdown_response_is_safe(value: &Value) -> bool {
    matches!(value["status"].as_str(), Some("stopped" | "not_running"))
}

fn client_process_running() -> bool {
    #[cfg(windows)]
    {
        let current_process_id = std::process::id();
        return elves_core::windows_integration::enumerate_processes()
            .into_iter()
            .any(|process| {
                process.process_id != current_process_id
                    && matches!(
                        process.exe_file.to_ascii_lowercase().as_str(),
                        "elves.exe"
                            | "codex-elves.exe"
                            | "codex-elves-manager.exe"
                            | "codex-elves-task-board.exe"
                            | "codex-elves-remote.exe"
                    )
            });
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn descendant_process_running(parent_process_id: u32, executable: &str) -> bool {
    #[cfg(windows)]
    {
        let processes = elves_core::windows_integration::enumerate_processes();
        let mut parents = std::collections::HashSet::from([parent_process_id]);
        let mut changed = true;
        while changed {
            changed = false;
            for process in &processes {
                if !parents.contains(&process.parent_process_id) {
                    continue;
                }
                if process.exe_file.eq_ignore_ascii_case(executable) {
                    return true;
                }
                if parents.insert(process.process_id) {
                    changed = true;
                }
            }
        }
        false
    }
    #[cfg(not(windows))]
    {
        let _ = (parent_process_id, executable);
        false
    }
}

fn claude_execution_active(directory: &Path) -> bool {
    let Ok(bytes) = std::fs::read(directory.join("remote-claude-sessions.json")) else {
        return false;
    };
    let Ok(saved) = serde_json::from_slice::<Value>(&bytes) else {
        return true;
    };
    saved["conversations"]
        .as_object()
        .into_iter()
        .flat_map(|conversations| conversations.values())
        .filter(|conversation| conversation["shared"] == true && conversation["deleted"] != true)
        .any(|conversation| {
            matches!(
                conversation["nativeStatus"]["tempo"].as_str(),
                Some("active" | "blocked")
            ) || conversation["nativeStatus"]["sessionStatus"] == "waiting"
                || conversation["turns"]
                    .as_array()
                    .and_then(|turns| turns.last())
                    .is_some_and(|turn| {
                        matches!(
                            turn["status"].as_str(),
                            Some("pending" | "inProgress" | "unknown")
                        )
                    })
        })
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Endpoint {
    protocol_version: u32,
    process_id: u32,
    port: u16,
    token: String,
}

#[derive(Clone)]
struct ServiceState {
    endpoint: Endpoint,
    handle: axum_server::Handle<std::net::SocketAddr>,
}

fn acquire_service_lock(directory: &Path) -> anyhow::Result<Option<File>> {
    std::fs::create_dir_all(directory)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(LOCK_FILE))?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(error)
            if error.kind() == std::io::ErrorKind::WouldBlock
                || matches!(error.raw_os_error(), Some(32 | 33)) =>
        {
            Ok(None)
        }
        Err(error) => Err(error).context("无法锁定Harness 后台"),
    }
}

fn load_endpoint_any(directory: &Path) -> anyhow::Result<Option<Endpoint>> {
    // 旧安装端点仅用于安全退出和迁移；新运行时始终写 Harness 自己的端点。
    let bytes = match std::fs::read(directory.join(ENDPOINT_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::read(directory.join("remote-access-service.json")) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error).context("无法读取旧版本后台状态"),
            }
        }
        Err(error) => return Err(error).context("无法读取 Harness 后台状态"),
    };
    Ok(serde_json::from_slice::<Endpoint>(&bytes)
        .ok()
        .filter(|endpoint| {
            endpoint.protocol_version > 0
                && endpoint.process_id > 0
                && endpoint.port > 0
                && endpoint.token.len() == 64
                && endpoint.token.bytes().all(|byte| byte.is_ascii_hexdigit())
        }))
}

fn load_endpoint(directory: &Path) -> anyhow::Result<Option<Endpoint>> {
    Ok(load_endpoint_any(directory)?
        .filter(|endpoint| endpoint.protocol_version == PROTOCOL_VERSION))
}

async fn endpoint_is_active(endpoint: &Endpoint) -> anyhow::Result<bool> {
    Ok(endpoint_health(endpoint).await?.is_some_and(|value| {
        value["protocolVersion"] == endpoint.protocol_version
            && value["processId"] == endpoint.process_id
    }))
}

async fn endpoint_health(endpoint: &Endpoint) -> anyhow::Result<Option<Value>> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(1))
        .build()?;
    let Ok(response) = client
        .get(format!("http://127.0.0.1:{}/health", endpoint.port))
        .bearer_auth(&endpoint.token)
        .send()
        .await
    else {
        return Ok(None);
    };
    if !response.status().is_success() {
        return Ok(None);
    }
    Ok(response.json::<Value>().await.ok())
}

async fn active_endpoint(directory: &Path) -> anyhow::Result<Option<Endpoint>> {
    let Some(endpoint) = load_endpoint(directory)? else {
        return Ok(None);
    };
    Ok(endpoint_is_active(&endpoint).await?.then_some(endpoint))
}

async fn retire_obsolete_service(directory: &Path) -> anyhow::Result<()> {
    let Some(endpoint) = load_endpoint_any(directory)? else {
        return Ok(());
    };
    if endpoint.protocol_version == PROTOCOL_VERSION || !endpoint_is_active(&endpoint).await? {
        return Ok(());
    }
    if endpoint.protocol_version >= 9 {
        // 菜单能力升级也先走旧服务已有的安全退出接口，不能为更换私有协议强制中断原生任务。
        let value = request(
            &endpoint,
            "prepareShutdown",
            &json!({"reason":"client_upgrade"}),
        )
        .await?;
        anyhow::ensure!(
            shutdown_response_is_safe(&value),
            "旧版 Harness 未确认安全退出，请先等待当前任务完成后重试"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !endpoint_is_active(&endpoint).await? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("旧版 Harness 正在安全退出，请稍后重试");
    }
    #[cfg(windows)]
    {
        // 旧服务没有受控退出接口；先保存受管身份，下次启动补偿，不能把强制终止当作已回收。
        super::claude::remember_shutdown(directory, "legacy_service_upgrade")?;
        anyhow::ensure!(
            crate::windows_integration::terminate_process(endpoint.process_id),
            "旧版Harness 后台仍在运行，请退出 Elves 后重试"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !endpoint_is_active(&endpoint).await.unwrap_or(false) {
                let _ = std::fs::remove_file(directory.join(ENDPOINT_FILE));
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("旧版Harness 后台未能停止，请退出 Elves 后重试");
    }
    #[cfg(not(windows))]
    anyhow::bail!("旧版Harness 后台仍在运行，请退出 Elves 后重试")
}

pub async fn ensure_running() -> anyhow::Result<()> {
    let directory = crate::paths::default_app_state_dir();
    retire_obsolete_service(&directory).await?;
    if active_endpoint(&directory).await?.is_some() {
        return Ok(());
    }
    let available = acquire_service_lock(&directory)?;
    let mut child = if available.is_some() {
        drop(available);
        // 用户要求单客户端安装，独立后台由客户端自身承载，不再依赖管理器程序。
        let program = SERVICE_EXECUTABLE.get().cloned().unwrap_or_else(|| {
            std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("elves.exe"))
        });
        anyhow::ensure!(program.is_file(), "未找到 Elves 程序，无法启动Harness 后台");
        let mut command = Command::new(program);
        command
            .arg("--harness-service")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        command.creation_flags(crate::windows_integration::CREATE_NO_WINDOW);
        Some(command.spawn().context("无法启动独立Harness 后台")?)
    } else {
        None
    };
    #[cfg(windows)]
    let mut owned_job = if let Some(child) = child.as_mut() {
        match remote_service_job(child) {
            Ok(job) => Some(job),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    } else {
        None
    };
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        if active_endpoint(&directory).await?.is_some() {
            #[cfg(windows)]
            if let Some(job) = owned_job.take() {
                *OWNED_SERVICE_JOB
                    .get_or_init(|| Mutex::new(None))
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(job);
            }
            return Ok(());
        }
        if let Some(child) = child.as_mut()
            && let Some(status) = child.try_wait()?
            && !status.success()
        {
            anyhow::bail!("Harness 后台启动失败（{status}），请查看诊断日志");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("Harness 后台尚未就绪，请稍后重试；现有隧道未被重启")
}

fn supported_action(action: &str) -> bool {
    matches!(
        action,
        "status"
            | "localBootstrap"
            | "desktopBootstrap"
            | "addProjectFolder"
            | "prepareShutdown"
            | "apply"
            | "revoke"
            | "diagnostics"
            | "downloadZrok"
            | "registerZrok"
            | "startZrok"
            | "testZrok"
            | "saveNgrok"
            | "saveFrp"
            | "testPrimaryRoute"
            | "applyPrimaryRoute"
    )
}

async fn request(endpoint: &Endpoint, action: &str, payload: &Value) -> anyhow::Result<Value> {
    anyhow::ensure!(supported_action(action), "不支持的远程管理操作");
    let timeout = if action == "prepareShutdown" {
        // 与服务端 60 秒清理预算同步，另留 5 秒返回已保存的结果。
        Duration::from_secs(65)
    } else if matches!(action, "testPrimaryRoute" | "applyPrimaryRoute")
        && payload["provider"] == "frp"
    {
        Duration::from_secs(420)
    } else {
        Duration::from_secs(150)
    };
    let response = reqwest::Client::builder()
        .no_proxy()
        .timeout(timeout)
        .build()?
        .post(format!("http://127.0.0.1:{}/admin/{action}", endpoint.port))
        .bearer_auth(&endpoint.token)
        .json(payload)
        .send()
        .await
        .context("Harness 后台未返回结果，请先刷新状态，不要重复执行操作")?
        .error_for_status()
        .context("Harness 后台拒绝了管理请求")?;
    response.json().await.context("Harness 后台返回了无效状态")
}

pub async fn prepare_shutdown_status(reason: &str) -> anyhow::Result<Value> {
    let directory = crate::paths::default_app_state_dir();
    // 安装 handoff 不登记 Claude 回收意图；新 Harness 会按 native job 重新 attach。
    // 普通退出仍保留原有回收记录，空闲会话可安全停止，未知状态继续补偿。
    if reason != "installer" {
        super::claude::remember_shutdown(&directory, reason)?;
    }
    let Some(endpoint) = load_endpoint_any(&directory)? else {
        if client_process_running() {
            // 端点缺失但仍有客户端进程时，状态无法确认；禁止安装器继续强制结束进程。
            anyhow::bail!("Elves 后台端点不可用且仍有客户端进程运行，请先退出 Elves 后重试");
        }
        return Ok(json!({"status":"not_running","cleanup":"pending_startup_check"}));
    };
    if !endpoint_is_active(&endpoint).await? {
        if client_process_running() {
            // 端点失效但仍有客户端进程时，不能把未知状态当作“未运行”。
            anyhow::bail!("Elves 后台端点已失效且仍有客户端进程运行，请先退出 Elves 后重试");
        }
        return Ok(json!({"status":"not_running","cleanup":"pending_startup_check"}));
    }
    if reason == "installer"
        && !endpoint_health(&endpoint)
            .await?
            .is_some_and(|value| value["buildId"] == crate::assets::DIAGNOSTIC_BUILD_ID)
        && (descendant_process_running(endpoint.process_id, "codex.exe")
            || claude_execution_active(&directory))
    {
        // 首次从旧直连 stdio 版本升级时不能伪造无损接管；检测到旧 Harness 仍持有 Codex 时必须中止。
        return Ok(json!({
            "status":"handoff_unavailable",
            "message":"当前 Harness 版本不支持执行中无损升级交接，请等待任务完成后重试安装",
            "cleanup":{"complete":false,"pendingStartupCheck":false}
        }));
    }
    if endpoint.protocol_version < 9 {
        // 安装包用新 helper 请求旧服务时不恢复旧桥接；旧版无法响应的清理保留到新服务启动。
        return Ok(json!({"status":"legacy_service","cleanup":"pending_startup_check"}));
    }
    request(&endpoint, "prepareShutdown", &json!({"reason":reason})).await
}

pub async fn prepare_shutdown(reason: &str) -> anyhow::Result<Value> {
    let value = prepare_shutdown_status(reason).await?;
    let message = value["message"]
        .as_str()
        .unwrap_or("远程后台未确认安全退出，请等待当前任务完成后重试")
        .to_owned();
    anyhow::ensure!(shutdown_response_is_safe(&value), "{message}");
    Ok(value)
}

pub(super) async fn try_request(action: &str, payload: &Value) -> anyhow::Result<Option<Value>> {
    let Some(endpoint) = active_endpoint(&crate::paths::default_app_state_dir()).await? else {
        return Ok(None);
    };
    request(&endpoint, action, payload).await.map(Some)
}

pub(super) async fn request_or_start(action: &str, payload: Value) -> anyhow::Result<Value> {
    anyhow::ensure!(supported_action(action), "不支持的远程管理操作");
    ensure_running().await?;
    try_request(action, &payload)
        .await?
        .context("Harness 后台连接已断开，请刷新状态后重试")
}

fn authorized(headers: &HeaderMap, endpoint: &Endpoint) -> bool {
    !headers.contains_key(header::ORIGIN)
        && headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|token| super::constant_time_eq(token, &endpoint.token))
}

async fn health(
    State(state): State<ServiceState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    let endpoint = &state.endpoint;
    if !authorized(&headers, endpoint) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(Json(json!({
        "status":"ok",
        "protocolVersion":endpoint.protocol_version,
        "processId":endpoint.process_id,
        "version":crate::version::VERSION,
        "buildId":crate::assets::DIAGNOSTIC_BUILD_ID,
    })))
}

async fn admin(
    State(state): State<ServiceState>,
    RoutePath(action): RoutePath<String>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    let endpoint = &state.endpoint;
    if !authorized(&headers, endpoint) {
        return Err(StatusCode::FORBIDDEN);
    }
    if !supported_action(&action) {
        return Err(StatusCode::NOT_FOUND);
    }
    if action == "prepareShutdown" {
        let reason = payload["reason"].as_str().unwrap_or("client_exit");
        let mut response = match tokio::time::timeout(
            SHUTDOWN_TIMEOUT,
            super::prepare_controller_shutdown(reason),
        )
        .await
        {
            Ok(value) => value,
            Err(_) => json!({"status":"closing","cleanup":{
                "complete":false,"reason":"outcome_unknown","pendingStartupCheck":true
            }}),
        };
        response["serviceProcessId"] = json!(endpoint.process_id);
        // 只有明确确认已停止时才关闭管理监听；忙碌、超时或未知结果必须保留后台，等待用户稍后重试。
        if shutdown_response_is_safe(&response) {
            state.handle.graceful_shutdown(Some(Duration::from_secs(2)));
        }
        return Ok(Json(response));
    }
    let mut response = super::admin_request(&action, payload).await;
    response["serviceProcessId"] = json!(endpoint.process_id);
    Ok(Json(response))
}

async fn save_frp(
    state: State<ServiceState>,
    headers: HeaderMap,
    payload: Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    admin(state, RoutePath("saveFrp".into()), headers, payload).await
}

pub async fn run() -> anyhow::Result<()> {
    let directory = crate::paths::default_app_state_dir();
    let Some(_lock) = acquire_service_lock(&directory)? else {
        return Ok(());
    };
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let endpoint = Endpoint {
        protocol_version: PROTOCOL_VERSION,
        process_id: std::process::id(),
        port: listener.local_addr()?.port(),
        token: super::random_token(),
    };
    let state = ServiceState {
        endpoint: endpoint.clone(),
        handle: axum_server::Handle::new(),
    };
    let handle = state.handle.clone();
    let router = Router::new()
        .route("/health", get(health))
        .route("/admin/{action}", post(admin))
        .route(
            "/admin/saveFrp",
            post(save_frp).layer(DefaultBodyLimit::max(160 * 1024)),
        )
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(state);
    let server = axum_server::from_tcp(listener)?
        .handle(handle)
        .serve(router.into_make_service());
    // Harness 先建立独立本机工作区及代理，再对外报告就绪；远程会话可以始终关闭。
    super::management::register_management();
    let controller = super::shared_controller()?;
    let settings = crate::settings::SettingsStore::default().load()?;
    controller
        .start_desktop(settings.harness.desktop_port)
        .await?;
    crate::settings::atomic_write(
        &directory.join(ENDPOINT_FILE),
        &serde_json::to_vec(&endpoint)?,
    )?;
    let monitor = super::start_monitor()?;
    let proxy_monitor = crate::local_proxy_service::start_monitor();
    let result = server.await;
    proxy_monitor.shutdown().await;
    let shutdown = tokio::time::timeout(SHUTDOWN_TIMEOUT, monitor.shutdown()).await;
    std::fs::remove_file(directory.join(ENDPOINT_FILE))
        .context("无法清理已停止的Harness 后台状态")?;
    shutdown.context("Harness 后台清理超时，进程将退出并回收隧道")?;
    result.context("Harness 后台管理接口已停止")
}

#[cfg(windows)]
fn remote_service_job(
    child: &std::process::Child,
) -> anyhow::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows::core::PCWSTR;

    let handle = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
        .context("无法创建Harness 后台生命周期保护")?;
    let job = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // Harness 退出时只回收自身与 proxy；持久 app-server daemon 允许静默脱离 Job。
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
    unsafe {
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&limits).cast(),
            std::mem::size_of_val(&limits) as u32,
        )?;
        AssignProcessToJobObject(handle, HANDLE(child.as_raw_handle()))?;
    }
    Ok(job)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_response_only_allows_confirmed_stopped_or_not_running() {
        for status in ["stopped", "not_running"] {
            assert!(shutdown_response_is_safe(&json!({"status":status})));
        }
        for status in ["busy", "closing", "legacy_service", "failed"] {
            assert!(!shutdown_response_is_safe(&json!({"status":status})));
        }
    }

    #[test]
    fn claude_execution_active_distinguishes_idle_and_running_sessions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("remote-claude-sessions.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "conversations": {
                    "idle": {"shared":true,"deleted":false,"nativeStatus":{"tempo":"idle"},"turns":[]},
                    "running": {"shared":true,"deleted":false,"nativeStatus":{"tempo":"active"},"turns":[]}
                }
            })
            .to_string(),
        )
        .unwrap();
        assert!(claude_execution_active(directory.path()));
        std::fs::write(
            &path,
            serde_json::json!({
                "conversations": {
                    "idle": {"shared":true,"deleted":false,"nativeStatus":{"tempo":"idle"},"turns":[]}
                }
            })
            .to_string(),
        )
        .unwrap();
        assert!(!claude_execution_active(directory.path()));
    }

    #[tokio::test]
    async fn protocol_upgrade_requests_controlled_shutdown_before_replacing_service() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let directory = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Endpoint {
            protocol_version: PROTOCOL_VERSION - 1,
            process_id: u32::MAX,
            port: listener.local_addr().unwrap().port(),
            token: "a".repeat(64),
        };
        let closed = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = closed.clone();
        let shutdown = closed.clone();
        let counted = calls.clone();
        let protocol = endpoint.protocol_version;
        let router = Router::new()
            .route(
                "/health",
                get(move || {
                    let observed = observed.clone();
                    async move {
                        if observed.load(Ordering::Acquire) {
                            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})))
                        } else {
                            (
                                StatusCode::OK,
                                Json(json!({"processId":u32::MAX,"protocolVersion":protocol})),
                            )
                        }
                    }
                }),
            )
            .route(
                "/admin/prepareShutdown",
                post(move || {
                    let shutdown = shutdown.clone();
                    let counted = counted.clone();
                    async move {
                        counted.fetch_add(1, Ordering::SeqCst);
                        shutdown.store(true, Ordering::Release);
                        Json(json!({"status":"stopped"}))
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        std::fs::write(
            directory.path().join(ENDPOINT_FILE),
            serde_json::to_vec(&endpoint).unwrap(),
        )
        .unwrap();
        retire_obsolete_service(directory.path()).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(closed.load(Ordering::Acquire));
        server.abort();
        let _ = server.await;
    }

    const TEST_DIRECTORY_ENV: &str = "CODEX_ELVES_REMOTE_SERVICE_TEST_DIRECTORY";

    fn test_directory() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("remote-service-")
            .tempdir_in(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../temp"))
            .unwrap()
    }

    fn configure_test_paths() -> Option<std::path::PathBuf> {
        let directory = std::env::var_os(TEST_DIRECTORY_ENV).map(std::path::PathBuf::from)?;
        crate::paths::set_settings_path_for_tests(Some(directory.join("settings.json")));
        crate::paths::set_app_state_dir_for_tests(Some(directory.clone()));
        Some(directory)
    }

    #[test]
    fn only_one_service_can_own_a_state_directory() {
        let directory = test_directory();
        let first = acquire_service_lock(directory.path()).unwrap().unwrap();
        assert!(acquire_service_lock(directory.path()).unwrap().is_none());
        drop(first);
        assert!(acquire_service_lock(directory.path()).unwrap().is_some());
    }

    #[test]
    fn rejects_invalid_service_endpoints() {
        let directory = test_directory();
        for value in [
            json!({"protocolVersion":PROTOCOL_VERSION,"processId":1,"port":0,"token":"a".repeat(64)}),
            json!({"protocolVersion":PROTOCOL_VERSION + 1,"processId":1,"port":12345,"token":"a".repeat(64)}),
            json!({"protocolVersion":PROTOCOL_VERSION,"processId":1,"port":12345,"token":"bad\r\nheader"}),
        ] {
            std::fs::write(
                directory.path().join(ENDPOINT_FILE),
                serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
            assert!(load_endpoint(directory.path()).unwrap().is_none());
        }
    }

    #[test]
    fn legacy_dual_mode_service_is_not_reused() {
        let directory = test_directory();
        std::fs::write(
            directory.path().join(ENDPOINT_FILE),
            serde_json::to_vec(&json!({
                "protocolVersion":5,"processId":12345,"port":12345,"token":"a".repeat(64)
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(
            load_endpoint_any(directory.path()).unwrap().is_some(),
            "旧后台仍可识别，以便只退役已验证的旧进程"
        );
        assert!(
            load_endpoint(directory.path()).unwrap().is_none(),
            "保留双模式路由的旧后台不能被新版服务模式复用"
        );
    }

    #[test]
    fn management_requires_token_and_rejects_browser_origins() {
        let endpoint = Endpoint {
            protocol_version: PROTOCOL_VERSION,
            process_id: 1,
            port: 12345,
            token: "a".repeat(64),
        };
        let mut headers = HeaderMap::new();
        assert!(!authorized(&headers, &endpoint));
        headers.insert(header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert!(!authorized(&headers, &endpoint));
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", endpoint.token).parse().unwrap(),
        );
        assert!(authorized(&headers, &endpoint));
        headers.insert(header::ORIGIN, "http://127.0.0.1:12345".parse().unwrap());
        assert!(!authorized(&headers, &endpoint));
    }

    #[tokio::test]
    async fn controlled_shutdown_exits_service_without_changing_saved_settings() {
        let directory = test_directory();
        let remote_port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut settings = crate::settings::BackendSettings::default();
        settings.remote_access.enabled = true;
        settings.remote_access.port = remote_port;
        let settings_path = directory.path().join("settings.json");
        crate::settings::SettingsStore::new(settings_path.clone())
            .save(&settings)
            .unwrap();
        let before = std::fs::read(&settings_path).unwrap();
        let mut child = test_command("runtime_worker", directory.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        #[cfg(windows)]
        let _job = remote_service_job(&child).unwrap();
        let endpoint = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(endpoint) = active_endpoint(directory.path()).await.unwrap() {
                    return endpoint;
                }
                assert!(child.try_wait().unwrap().is_none());
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        let report = request(&endpoint, "prepareShutdown", &json!({"reason":"test_exit"}))
            .await
            .unwrap();
        assert_eq!(report["status"], "stopped");
        assert_eq!(report["cleanup"]["complete"], true);
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success());
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        assert!(active_endpoint(directory.path()).await.unwrap().is_none());
        assert_eq!(std::fs::read(settings_path).unwrap(), before);
        assert!(acquire_service_lock(directory.path()).unwrap().is_some());
        assert!(std::net::TcpListener::bind(("127.0.0.1", remote_port)).is_ok());
        assert!(std::net::TcpListener::bind(("127.0.0.1", endpoint.port)).is_ok());
    }

    #[tokio::test]
    #[ignore = "仅由隔离后台进程测试调用"]
    async fn runtime_worker() {
        if configure_test_paths().is_some() {
            run().await.unwrap();
        }
    }

    fn test_command(name: &str, directory: &Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                &format!("service::tests::{name}"),
                "--ignored",
                "--nocapture",
            ])
            .env(TEST_DIRECTORY_ENV, directory)
            .env("PATH", "")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        command.creation_flags(crate::windows_integration::CREATE_NO_WINDOW);
        command
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn owned_service_job_stops_service_and_releases_ports() {
        let directory = test_directory();
        let remote_port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut settings = crate::settings::BackendSettings::default();
        settings.remote_access.enabled = true;
        settings.remote_access.port = remote_port;
        crate::settings::SettingsStore::new(directory.path().join("settings.json"))
            .save(&settings)
            .unwrap();

        let manager_guard = crate::ports::LoopbackPortGuard::listener(
            std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap(),
        );
        let manager_wake = manager_guard.shared_listener().unwrap();
        let launcher_guard = crate::ports::LoopbackPortGuard::listener(
            std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap(),
        );
        let launcher_control = launcher_guard.shared_listener().unwrap();
        let helper = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let starter_ports = [
            manager_wake.local_addr().unwrap().port(),
            launcher_control.local_addr().unwrap().port(),
            helper.local_addr().unwrap().port(),
        ];

        let mut child = test_command("runtime_worker", directory.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let job = remote_service_job(&child).unwrap();
        let endpoint = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(endpoint) = active_endpoint(directory.path()).await.unwrap() {
                    return endpoint;
                }
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "service exited before startup"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();

        drop(manager_wake);
        drop(manager_guard);
        drop(launcher_control);
        drop(launcher_guard);
        drop(helper);
        for port in starter_ports {
            assert!(
                std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(),
                "remote service retained starter port {port}"
            );
        }

        let status = request(&endpoint, "status", &json!({})).await.unwrap();
        assert_eq!(status["status"], "ok");
        assert_eq!(status["running"], true);
        assert_eq!(status["serviceProcessId"], endpoint.process_id);
        assert_eq!(status["zrok"]["installed"], false);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let health_url = format!("http://127.0.0.1:{}/health", endpoint.port);
        assert_eq!(
            client.get(&health_url).send().await.unwrap().status(),
            reqwest::StatusCode::FORBIDDEN
        );
        assert_eq!(
            client
                .get(&health_url)
                .bearer_auth(&endpoint.token)
                .header(reqwest::header::ORIGIN, "http://localhost")
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::FORBIDDEN
        );

        drop(job);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if child.try_wait().unwrap().is_some() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        assert!(active_endpoint(directory.path()).await.unwrap().is_none());
        assert!(acquire_service_lock(directory.path()).unwrap().is_some());
        assert!(std::net::TcpListener::bind(("127.0.0.1", remote_port)).is_ok());
        assert!(std::net::TcpListener::bind(("127.0.0.1", endpoint.port)).is_ok());
    }
}
