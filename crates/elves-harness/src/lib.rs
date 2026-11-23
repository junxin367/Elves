//! Harness：Elves 的独立执行、会话与管理产品。远程会话仅是可选的网页/Android 访问能力。
pub use elves_core::settings;
mod codex_server;
mod codex_service;
pub use elves_core::assets;
pub use elves_core::cli_wrapper;
pub use elves_core::codex_home;
pub use elves_core::codex_sqlite;
pub use elves_core::diagnostic_log;
pub use elves_core::lan_proxy;
pub use elves_core::local_proxy_service;
pub use elves_core::model_capabilities;
pub use elves_core::model_catalog;
pub use elves_core::paths;
pub use input_files::stage_desktop_files;
pub use elves_core::ports;
pub use elves_core::prompt_optimize;
pub use elves_core::protocol_proxy;
pub use elves_core::proxy_log;
pub use elves_core::relay_config;
pub use elves_core::tool_runtime;
pub use elves_core::version;
mod service_history;
mod service_transport;
pub use elves_core::windows_create_hidden_process;
#[cfg(windows)]
pub use elves_core::windows_integration;
use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::io::{Cursor, Read, Write};
use std::net::SocketAddr;
use std::path::{Component, Path as FsPath, PathBuf, Prefix};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use async_trait::async_trait;
use axum::body::{Body, Bytes};
use axum::extract::ws::{CloseFrame, Message as WebSocketMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Extension, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use flate2::Compression;
use flate2::write::GzEncoder;
use futures_util::{SinkExt, stream};
use image::{ImageFormat, ImageReader, Limits};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, broadcast};

use crate::codex_service::{ServiceDispatchGuard, ServiceDispatchPermit};
use crate::settings::{RemoteAccessSettings, SettingsStore};

mod browser;
mod channels;
mod claude;
mod codex;
mod context_usage;
mod conversation_naming;
mod credentials;
mod desktop;
mod desktop_images;
mod diagnostics;
mod document_preview;
pub(crate) mod execution;
mod frp_recovery;
mod frp_tunnel;
mod image_preview;
mod input_files;
mod instance_settings;
mod instances;
mod local_proof;
mod ngrok_tunnel;
mod open_in;
mod permissions;
mod preferences;
mod primary_route;
mod prompt_optimization;
mod recent_priority;
mod scheduled_tasks;
mod session_metrics;
mod shared_settings;

mod service;
mod video;
mod workspace;
mod zrok_tunnel;
use credentials::{AccessKey, AccessKeyStore, lock_access_key_store};
pub use credentials::{access_credentials_status, configure_access_key};
pub use frp_tunnel::program_directory as frp_program_directory;
pub use service::{
    ensure_running as ensure_service_running, owns_service,
    prepare_shutdown as prepare_service_shutdown,
    prepare_shutdown_status as prepare_service_shutdown_status, run as run_service,
    set_service_executable,
};
use zrok_tunnel::ZrokTunnelService;

// 用户要求加快编译：网页字节统一由 elves-assets 内嵌，Harness 与客户端不再重复生成资源表。
// 缓存恢复页仍独立提供为 no-store；远程管理权限继续由本服务校验。
const COOKIE_NAME: &str = "codex_elves_device";
const HARNESS_READ_GUIDANCE: &str = "Harness 执行引擎暂时不可用，请稍后重试。";
const MAX_DEVICES: usize = 32;
const MAX_RECEIPTS: usize = 20_000;
const MAX_REMOTE_IMAGES: usize = 1_024;
const MAX_REMOTE_INLINE_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_REMOTE_IMAGE_BYTES: u64 = 25 * 1024 * 1024;
const MAX_REMOTE_IMAGE_DIMENSION: u32 = 16_384;
const MAX_REMOTE_IMAGE_PIXELS: u64 = 100_000_000;
// 用户要求 Codex/Claude 单条固定最多 8 张，含队列保留与新增；字节额度仅适用于内联图片。
const MAX_INPUT_IMAGE_COUNT: usize = 8;
const MAX_INPUT_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_INPUT_IMAGES_TOTAL_BYTES: usize = 12 * 1024 * 1024;
// 原有图片最多 12 MiB，新增本地文件/目录另限 12 MiB；base64 为 4/3，再为目录清单和消息保留 4 MiB。
const MAX_ACTION_BODY_BYTES: usize = 36 * 1024 * 1024;
static CONTROLLER: OnceLock<Arc<HarnessController>> = OnceLock::new();
// 安装器只对“明确仍在执行”的结果重试；连接断开、旧服务和未知结果继续中止，避免覆盖运行中的程序。
pub const SERVICE_SHUTDOWN_BUSY_EXIT_CODE: i32 = 10;
static STARTUP_ERROR: OnceLock<std::sync::Mutex<Option<String>>> = OnceLock::new();
static WEB_ASSETS: OnceLock<HashMap<&'static str, StaticWebAsset>> = OnceLock::new();

#[cfg(test)]
#[async_trait]
trait TestRemoteBackend: Send + Sync {
    async fn call(&self, action: &str, payload: Value) -> anyhow::Result<Value>;

    async fn call_authorized(
        &self,
        action: &str,
        payload: Value,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        authorization.check().await?;
        self.call(action, payload).await
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Device {
    id: String,
    name: String,
    token_hash: String,
    created_at_ms: u64,
    expires_at_ms: u64,
    last_seen_at_ms: u64,
    #[serde(default)]
    key_id: String,
    #[serde(default)]
    #[serde(alias = "quickTunnelOrigin")]
    public_origin: Option<String>,
    // 用户要求按持久随机客户端标识去重；仅存摘要，不能用它代替 Cookie/密钥或按 IP、UA 合并设备。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    client_id_hash: String,
}

impl Device {
    fn public(&self) -> Value {
        json!({
            "id": self.id, "name": self.name, "createdAtMs": self.created_at_ms,
            "expiresAtMs": self.expires_at_ms, "lastSeenAtMs": self.last_seen_at_ms
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Receipt {
    device_id: String,
    request_id: String,
    payload_hash: String,
    created_at_ms: u64,
    state: String,
    result: Option<Value>,
}

#[derive(Default)]
struct AuthState {
    devices: Vec<Device>,
    access_key: Option<AccessKey>,
    login_locked: bool,
    login_storage_failed_for: Option<String>,
    receipts: HashMap<String, Receipt>,
}

impl AuthState {
    fn resolve_device<'a>(
        &'a self,
        desktop: Option<&'a Device>,
        id: &str,
        key_id: Option<&str>,
    ) -> Option<&'a Device> {
        let matches = |device: &&Device| {
            device.id == id && key_id.is_none_or(|key_id| device.key_id == key_id)
        };
        // Elves 本机身份独立保存；队列、命名与回执恢复不能要求它存在于远程设备清单。
        desktop
            .filter(|device| device.key_id == desktop::DEVICE_KEY_ID)
            .filter(matches)
            .or_else(|| {
                self.devices.iter().find(|device| {
                    matches(device)
                        && device.expires_at_ms > now_ms()
                        && self
                            .access_key
                            .as_ref()
                            .is_some_and(|key| device.key_id == key.id)
                })
            })
    }
}

#[derive(Clone, Copy)]
struct AccessPolicy {
    enabled: bool,
    read_only: bool,
    allow_approvals: bool,
}

impl Default for AccessPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            read_only: true,
            allow_approvals: false,
        }
    }
}

#[derive(Clone)]
pub struct HarnessAuthorization {
    controller: Arc<HarnessController>,
    device: Device,
    write_action: Option<String>,
    request_id: Option<String>,
}

impl HarnessAuthorization {
    pub async fn check(&self) -> anyhow::Result<()> {
        self.prepare_dispatch().await?.commit(|| Ok(()))
    }

    async fn prepare_dispatch(&self) -> anyhow::Result<HarnessDispatchPermit> {
        if self.device.key_id == desktop::DEVICE_KEY_ID {
            let guard = self.controller.desktop_device.clone().lock_owned().await;
            if !guard.as_ref().is_some_and(|device| {
                device.id == self.device.id
                    && constant_time_eq(&device.token_hash, &self.device.token_hash)
            }) {
                return Err(ApiError::new(401, "Elves 本机授权已失效").into());
            }
            return Ok(HarnessDispatchPermit {
                authorization: self.clone(),
                expires_at_ms: u64::MAX,
                _auth: None,
                _key_store: None,
                _desktop: Some(guard),
            });
        }

        let mut auth = self.controller.auth.clone().lock_owned().await;
        let (store, key, locked) = self
            .controller
            .access_key_snapshot()
            .await
            .map_err(|_| ApiError::new(401, "设备授权无法确认，请重新连接"))?;
        self.controller.sync_access_key(&mut auth, key, locked);
        let current_key = auth.access_key.as_ref().map(|key| key.id.as_str());
        let current = auth
            .devices
            .iter()
            .find(|device| {
                device.id == self.device.id
                    && device.key_id == self.device.key_id
                    && current_key == Some(device.key_id.as_str())
                    && constant_time_eq(&device.token_hash, &self.device.token_hash)
                    && device.expires_at_ms > now_ms()
            })
            .ok_or_else(|| ApiError::new(401, "设备授权已撤销或过期"))?;
        let expires_at_ms = current.expires_at_ms;
        Ok(HarnessDispatchPermit {
            authorization: self.clone(),
            expires_at_ms,
            _auth: Some(auth),
            _key_store: Some(store),
            _desktop: None,
        })
    }
}

#[async_trait]
impl ServiceDispatchGuard for HarnessAuthorization {
    async fn prepare_dispatch(&self) -> anyhow::Result<Box<dyn ServiceDispatchPermit>> {
        Ok(Box::new(
            HarnessAuthorization::prepare_dispatch(self).await?,
        ))
    }
}

struct HarnessDispatchPermit {
    authorization: HarnessAuthorization,
    expires_at_ms: u64,
    _auth: Option<tokio::sync::OwnedMutexGuard<AuthState>>,
    _key_store: Option<AccessKeyStore>,
    _desktop: Option<tokio::sync::OwnedMutexGuard<Option<Device>>>,
}

impl ServiceDispatchPermit for HarnessDispatchPermit {
    fn commit(
        self: Box<Self>,
        dispatch: Box<dyn FnOnce() -> anyhow::Result<()> + Send>,
    ) -> anyhow::Result<()> {
        // 原生执行引擎在同步入队前复核所属本机或远程授权锁与当前策略。
        (*self).commit(dispatch)
    }
}

impl HarnessDispatchPermit {
    fn commit<T>(self, send: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        let controller = &self.authorization.controller;
        if self.authorization.device.key_id == desktop::DEVICE_KEY_ID {
            if controller.closing.load(Ordering::Acquire)
                || !controller.desktop_running.load(Ordering::Acquire)
            {
                return Err(ApiError::new(503, "Harness 本机入口已停止").into());
            }
            return send();
        }

        let policy = controller
            .access_policy
            .read()
            .unwrap_or_else(|error| error.into_inner());
        if controller.closing.load(Ordering::Acquire)
            || !policy.enabled
            || !controller.running.load(Ordering::SeqCst)
        {
            return Err(ApiError::new(503, "远程访问服务已停止").into());
        }
        if self.expires_at_ms <= now_ms() {
            return Err(ApiError::new(401, "设备授权已撤销或过期").into());
        }
        if let Some(action) = &self.authorization.write_action {
            if policy.read_only {
                return Err(ApiError::new(403, "当前为只读访问").into());
            }
            if matches!(action.as_str(), "approve" | "answer") && !policy.allow_approvals {
                return Err(ApiError::new(403, "远程审批与询问回答已关闭，请在桌面处理").into());
            }
        }
        send()
    }
}

#[derive(Clone)]
struct RegisteredImage {
    source: RegisteredImageSource,
}

#[derive(Clone)]
enum RegisteredImageSource {
    Path(PathBuf),
    Inline {
        bytes: Arc<[u8]>,
        mime: &'static str,
    },
}

#[derive(Clone)]
struct LoadedImage {
    bytes: Vec<u8>,
    mime: &'static str,
}

#[derive(Default)]
struct ImageRegistry {
    images: HashMap<String, RegisteredImage>,
    paths: HashMap<PathBuf, String>,
    order: VecDeque<String>,
    inline_bytes: usize,
}

impl ImageRegistry {
    fn register(&mut self, path: PathBuf) -> String {
        if let Some(id) = self.paths.get(&path).cloned() {
            if let Some(position) = self.order.iter().position(|entry| entry == &id) {
                self.order.remove(position);
            }
            self.order.push_back(id.clone());
            return id;
        }
        while self.images.len() >= MAX_REMOTE_IMAGES {
            let Some(id) = self.order.pop_front() else {
                break;
            };
            if let Some(image) = self.images.remove(&id) {
                match image.source {
                    RegisteredImageSource::Path(path) => {
                        self.paths.remove(&path);
                    }
                    RegisteredImageSource::Inline { bytes, .. } => {
                        self.inline_bytes = self.inline_bytes.saturating_sub(bytes.len());
                    }
                }
            }
        }
        let id = loop {
            let id = random_token();
            if !self.images.contains_key(&id) {
                break id;
            }
        };
        self.paths.insert(path.clone(), id.clone());
        self.images.insert(
            id.clone(),
            RegisteredImage {
                source: RegisteredImageSource::Path(path),
            },
        );
        self.order.push_back(id.clone());
        id
    }

    fn register_inline(&mut self, bytes: Vec<u8>, mime: &'static str) -> String {
        let id = format!("{:x}", Sha256::digest(&bytes));
        if self.images.contains_key(&id) {
            if let Some(position) = self.order.iter().position(|entry| entry == &id) {
                self.order.remove(position);
            }
            self.order.push_back(id.clone());
            return id;
        }
        while self.images.len() >= MAX_REMOTE_IMAGES
            || self.inline_bytes.saturating_add(bytes.len()) > MAX_REMOTE_INLINE_IMAGE_BYTES
        {
            let Some(expired_id) = self.order.pop_front() else {
                break;
            };
            if let Some(image) = self.images.remove(&expired_id) {
                match image.source {
                    RegisteredImageSource::Path(path) => {
                        self.paths.remove(&path);
                    }
                    RegisteredImageSource::Inline { bytes, .. } => {
                        self.inline_bytes = self.inline_bytes.saturating_sub(bytes.len());
                    }
                }
            }
        }
        self.inline_bytes += bytes.len();
        self.images.insert(
            id.clone(),
            RegisteredImage {
                source: RegisteredImageSource::Inline {
                    bytes: bytes.into(),
                    mime,
                },
            },
        );
        self.order.push_back(id.clone());
        id
    }
}

struct LiveService {
    settings: Option<RemoteAccessSettings>,
    handle: Option<axum_server::Handle<std::net::SocketAddr>>,
    server: Option<tokio::task::JoinHandle<()>>,
    pump: Option<tokio::task::JoinHandle<()>>,
    error: Option<String>,
    zrok: ZrokTunnelService,
    ngrok: ngrok_tunnel::NgrokTunnelService,
    frp: Option<frp_tunnel::Running>,
    frp_start: Option<tokio::task::JoinHandle<anyhow::Result<frp_tunnel::Running>>>,
    frp_error: Option<String>,
    frp_retry_at: u64,
}

pub struct HarnessController {
    #[cfg(test)]
    test_backend: OnceLock<Arc<dyn TestRemoteBackend>>,
    harness: Arc<codex::CodexHarness>,
    claude: Arc<claude::Claude>,
    permissions: permissions::SessionPermissions,
    shared_settings: shared_settings::SharedSettings,
    scheduled_tasks: Arc<scheduled_tasks::ScheduledTasks>,
    conversation_naming: Arc<conversation_naming::ConversationNaming>,
    harness_connected: AtomicBool,
    prompt_optimization: Arc<prompt_optimization::RemotePromptOptimization>,
    state_dir: PathBuf,
    desktop_running: AtomicBool,
    desktop_port: AtomicU16,
    desktop_runtime: Mutex<Option<desktop::DesktopRuntime>>,
    desktop_device: Arc<Mutex<Option<Device>>>,
    auth: Arc<Mutex<AuthState>>,
    access_policy: RwLock<AccessPolicy>,
    live: Mutex<LiveService>,
    running: AtomicBool,
    closing: AtomicBool,
    shutdown_complete: AtomicBool,
    shutdown_serial: Mutex<()>,
    events: broadcast::Sender<Value>,
    key_checks: Arc<tokio::sync::Semaphore>,
    public_origin: Arc<RwLock<Option<String>>>,
    primary: primary_route::Primary,
    channels: channels::Channels,
    diagnostics: diagnostics::Diagnostics,
    images: RwLock<ImageRegistry>,
    videos: RwLock<video::Registry>,
    image_previews: std::sync::Mutex<image_preview::PreviewCache>,
    image_preview_slots: Arc<tokio::sync::Semaphore>,
    browser_tunnels: browser::Registry,
}

impl HarnessController {
    pub fn new(state_dir: PathBuf) -> anyhow::Result<Arc<Self>> {
        let devices =
            read_json_or_default(&state_dir.join("remote-access-devices.json")).unwrap_or_default();
        let (access_key, login_locked) = (|| -> anyhow::Result<_> {
            let mut store = lock_access_key_store(&state_dir)?;
            let key = store.load()?;
            let locked = store.login_locked(key.as_ref())?;
            Ok((key, locked))
        })()
        .unwrap_or_else(|error| {
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "harness.remote_credentials_unavailable",
                json!({"message":format!("{error:#}")}),
            );
            (None, true)
        });
        let mut receipts: HashMap<String, Receipt> =
            read_json_or_default(&state_dir.join("remote-access-receipts.json"))?;
        let mut recovered = false;
        for receipt in receipts
            .values_mut()
            .filter(|receipt| receipt.state == "pending")
        {
            receipt.state = "unknown".into();
            receipt.result = Some(
                json!({"message":"服务曾重启，消息送达状态未知，请查看原生会话，不要重复发送"}),
            );
            recovered = true;
        }
        if recovered {
            crate::settings::atomic_write(
                &state_dir.join("remote-access-receipts.json"),
                &serde_json::to_vec(&receipts)?,
            )?;
        }
        let (events, _) = broadcast::channel(512);
        let public_origin = Arc::new(RwLock::new(None));
        let zrok = ZrokTunnelService::new(state_dir.clone(), public_origin.clone());
        let channels = channels::Channels::new();
        let ngrok =
            ngrok_tunnel::NgrokTunnelService::new(state_dir.clone(), channels.secondary.clone());
        let primary = primary_route::Primary::new(&state_dir)?;
        let harness = codex::CodexHarness::new(&state_dir)?;
        let claude = claude::Claude::new(&state_dir)?;
        let permissions = permissions::SessionPermissions::open(&state_dir)?;
        // 用户要求 Codex 实例仅保留服务模式；旧设备模式文件不再读取，也不能恢复桌面注入路由。
        harness.enabled.store(true, Ordering::Release);
        let controller = Arc::new(Self {
            #[cfg(test)]
            test_backend: OnceLock::new(),
            harness: harness.clone(),
            claude: claude.clone(),
            permissions,
            shared_settings: shared_settings::SharedSettings::new(&state_dir),
            scheduled_tasks: Arc::new(scheduled_tasks::ScheduledTasks::new(&state_dir)?),
            conversation_naming: conversation_naming::ConversationNaming::new(&state_dir)?,
            harness_connected: AtomicBool::new(false),
            prompt_optimization: Arc::new(prompt_optimization::RemotePromptOptimization::default()),
            desktop_device: Arc::new(Mutex::new(read_json_or_default(
                &state_dir.join("harness-desktop-device.json"),
            )?)),
            state_dir,
            desktop_running: AtomicBool::new(false),
            desktop_port: AtomicU16::new(0),
            desktop_runtime: Mutex::new(None),
            auth: Arc::new(Mutex::new(AuthState {
                devices,
                access_key,
                login_locked,
                receipts,
                ..AuthState::default()
            })),
            access_policy: RwLock::new(AccessPolicy::default()),
            live: Mutex::new(LiveService {
                settings: None,
                handle: None,
                server: None,
                pump: None,
                error: None,
                zrok,
                ngrok,
                frp: None,
                frp_start: None,
                frp_error: None,
                frp_retry_at: 0,
            }),
            running: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            shutdown_complete: AtomicBool::new(false),
            shutdown_serial: Mutex::new(()),
            events,
            key_checks: Arc::new(tokio::sync::Semaphore::new(4)),
            public_origin,
            primary,
            channels,
            diagnostics: diagnostics::Diagnostics::default(),
            images: RwLock::new(ImageRegistry::default()),
            videos: RwLock::new(video::Registry::default()),
            image_previews: std::sync::Mutex::new(image_preview::PreviewCache::default()),
            image_preview_slots: Arc::new(tokio::sync::Semaphore::new(2)),
            browser_tunnels: browser::Registry::new(),
        });
        harness.attach(Arc::downgrade(&controller));
        claude.attach(Arc::downgrade(&controller));
        controller
            .conversation_naming
            .attach(Arc::downgrade(&controller));
        Ok(controller)
    }

    #[cfg(test)]
    fn with_test_backend(
        native: Arc<dyn TestRemoteBackend>,
        state_dir: PathBuf,
    ) -> anyhow::Result<Arc<Self>> {
        let controller = Self::new(state_dir)?;
        // 仅隔离接口测试可替换服务结果；正式程序没有可注入的桌面执行后端。
        let _ = controller.test_backend.set(native);
        Ok(controller)
    }

    fn public_origin(&self) -> Option<String> {
        if self.primary.provider() == primary_route::Provider::Frp {
            return self.primary.frp_origin();
        }
        self.public_origin
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .or_else(|| {
                self.channels
                    .primary
                    .read()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone()
            })
    }

    fn attach_item_images(&self, item: &mut Value) {
        video::attach_item(self, item);
        self.attach_item_files(item);
        let sources = canonical_item_image_sources(item);
        if let Some(object) = item.as_object_mut() {
            object.remove("remoteImages");
        }
        if sources.is_empty() {
            return;
        }
        let mut registry = self
            .images
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let mut replacements = HashMap::new();
        let images = sources
            .into_iter()
            .filter_map(|source| {
                let image = register_remote_image(&mut registry, &source)?;
                replacements.insert(source, image.source.clone());
                Some(json!({
                    "source": image.source,
                    "url": format!("/api/images/{}", image.id),
                    "mime": image.mime
                }))
            })
            .collect::<Vec<_>>();
        if !images.is_empty() {
            replace_item_inline_image_sources(item, &replacements);
            item["remoteImages"] = Value::Array(images);
        }
    }

    fn attach_item_files(&self, item: &mut Value) {
        if item["type"] != "userMessage" {
            return;
        }
        let historical = item["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|content| content["type"] == "text")
            .filter_map(|content| content["text"].as_str())
            .flat_map(input_files::mentioned_file_attachments)
            .collect::<Vec<_>>();
        if !historical.is_empty() {
            // Codex App 历史消息把附件写在用户正文的兼容块中；保留原正文供模型和编辑使用，
            // 另提供独立附件列表供各端按文件芯片显示，不能再把路径当普通正文渲染。
            item["fileAttachments"] = Value::Array(historical);
        }
        for content in item["content"].as_array_mut().into_iter().flatten() {
            if content["type"] != "mention" || content["kind"].is_string() {
                continue;
            }
            let Some(path) = content["path"].as_str() else {
                continue;
            };
            let kind = std::fs::symlink_metadata(path)
                .ok()
                .map(|metadata| if metadata.is_dir() { "folder" } else { "file" });
            if let Some(kind) = kind {
                content["kind"] = json!(kind);
            }
        }
    }

    fn attach_queue_message_images(&self, message: &mut Value) {
        let sources = canonical_queue_image_sources(message);
        if let Some(object) = message.as_object_mut() {
            object.remove("remoteImages");
        }
        if sources.is_empty() {
            return;
        }
        let mut registry = self
            .images
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let mut replacements = HashMap::new();
        let images = sources
            .into_iter()
            .filter_map(|source| {
                let image = register_remote_image(&mut registry, &source)?;
                replacements.insert(source, image.source.clone());
                Some(json!({
                    "source": image.source,
                    "url": format!("/api/images/{}", image.id),
                    "mime": image.mime
                }))
            })
            .collect::<Vec<_>>();
        if !images.is_empty() {
            replace_queue_inline_image_sources(message, &replacements);
            message["remoteImages"] = Value::Array(images);
        }
    }

    fn attach_queue_images(&self, value: &mut Value) {
        for pointer in [
            "/queuedMessages",
            "/thread/queuedMessages",
            "/params/queuedMessages",
            "/result/queuedMessages",
        ] {
            for message in value
                .pointer_mut(pointer)
                .and_then(Value::as_array_mut)
                .into_iter()
                .flatten()
            {
                self.attach_queue_message_images(message);
            }
        }
    }

    fn attach_history_images(&self, value: &mut Value) {
        if value.get("scope").and_then(Value::as_str) == Some("items") {
            for item in value["data"].as_array_mut().into_iter().flatten() {
                self.attach_item_images(item);
            }
            return;
        }
        for turn in value["data"].as_array_mut().into_iter().flatten() {
            for item in turn["items"].as_array_mut().into_iter().flatten() {
                self.attach_item_images(item);
            }
        }
    }

    fn attach_event_images(&self, event: &mut Value) {
        self.attach_queue_images(event);
        if !matches!(
            event["method"].as_str(),
            Some("item/started" | "item/completed")
        ) {
            return;
        }
        if let Some(item) = event.pointer_mut("/params/item") {
            self.attach_item_images(item);
        }
    }

    fn attach_response_images(&self, value: &mut Value) {
        self.attach_queue_images(value);
        if let Some(result) = value.get_mut("result") {
            self.attach_queue_images(result);
        }
    }

    fn registered_image(&self, id: &str) -> Option<RegisteredImage> {
        self.images
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .images
            .get(id)
            .cloned()
    }

    async fn save_devices(&self, state: &AuthState) -> anyhow::Result<()> {
        let path = self.state_dir.join("remote-access-devices.json");
        let devices = state.devices.clone();
        tokio::task::spawn_blocking(move || {
            crate::settings::atomic_write(&path, &serde_json::to_vec(&devices)?)
        })
        .await?
    }

    async fn save_receipts(&self, state: &AuthState) -> anyhow::Result<()> {
        crate::settings::atomic_write(
            &self.state_dir.join("remote-access-receipts.json"),
            &serde_json::to_vec(&state.receipts)?,
        )
    }

    async fn reload_access_key(&self) -> anyhow::Result<bool> {
        let mut auth = self.auth.lock().await;
        let (_store, key, locked) = self.access_key_snapshot().await?;
        self.sync_access_key(&mut auth, key, locked);
        Ok(auth.access_key.is_some())
    }

    fn publish_access_policy(&self, settings: &RemoteAccessSettings) {
        *self
            .access_policy
            .write()
            .unwrap_or_else(|error| error.into_inner()) = AccessPolicy {
            enabled: settings.enabled,
            read_only: settings.read_only,
            allow_approvals: settings.allow_approvals,
        };
    }

    fn accepting_requests(&self) -> bool {
        !self.closing.load(Ordering::Acquire)
            && (self.desktop_running.load(Ordering::Acquire)
                || self.running.load(Ordering::SeqCst)
                    && self
                        .access_policy
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .enabled)
    }

    fn authorization(
        self: &Arc<Self>,
        device: Device,
        write_action: Option<&str>,
    ) -> HarnessAuthorization {
        HarnessAuthorization {
            controller: self.clone(),
            device,
            write_action: write_action.map(str::to_owned),
            request_id: None,
        }
    }

    async fn resolve_device(&self, id: &str, key_id: Option<&str>) -> Option<Device> {
        let desktop = self.desktop_device.lock().await;
        // 恢复本机队列与命名时不等待远程密钥文件或设备授权锁。
        if let Some(device) = desktop.as_ref().filter(|device| {
            device.key_id == desktop::DEVICE_KEY_ID
                && device.id == id
                && key_id.is_none_or(|key_id| device.key_id == key_id)
        }) {
            return Some(device.clone());
        }
        drop(desktop);
        self.auth
            .lock()
            .await
            .resolve_device(None, id, key_id)
            .cloned()
    }

    async fn access_key_snapshot(
        &self,
    ) -> anyhow::Result<(AccessKeyStore, Option<AccessKey>, bool)> {
        let state_dir = self.state_dir.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = lock_access_key_store(&state_dir)?;
            let key = store.load()?;
            let locked = store.login_locked(key.as_ref())?;
            Ok((store, key, locked))
        })
        .await?
    }

    fn sync_access_key(&self, auth: &mut AuthState, key: Option<AccessKey>, locked: bool) {
        let changed =
            auth.access_key.as_ref().map(|key| &key.id) != key.as_ref().map(|key| &key.id);
        if changed {
            auth.devices
                .retain(|device| key.as_ref().is_some_and(|key| device.key_id == key.id));
            auth.login_storage_failed_for = None;
            self.publish_event(json!({"method":"remote/authorizationChanged"}));
        }
        auth.login_locked = locked
            || key
                .as_ref()
                .is_some_and(|key| auth.login_storage_failed_for.as_ref() == Some(&key.id));
        auth.access_key = key;
    }

    async fn device_active(&self, id: &str) -> bool {
        if self.desktop_running.load(Ordering::Acquire)
            && self
                .desktop_device
                .lock()
                .await
                .as_ref()
                .is_some_and(|device| device.id == id)
        {
            return !self.closing.load(Ordering::Acquire);
        }
        if !self.running.load(Ordering::SeqCst)
            || !self
                .access_policy
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .enabled
        {
            return false;
        }

        if self.reload_access_key().await.is_err() {
            return false;
        }
        let auth = self.auth.lock().await;
        auth.devices.iter().any(|device| {
            device.id == id
                && device.expires_at_ms > now_ms()
                && auth
                    .access_key
                    .as_ref()
                    .is_some_and(|key| device.key_id == key.id)
        })
    }

    async fn prune_stale_public_devices(&self) {
        let Some(current_origin) = self.public_origin() else {
            return;
        };
        let mut auth = self.auth.lock().await;
        let before = auth.devices.len();
        auth.devices.retain(|device| {
            device.public_origin.is_none()
                || device.public_origin.as_deref() == Some(current_origin.as_str())
        });
        if auth.devices.len() == before {
            return;
        }
        let _ = self.save_devices(&auth).await;
        self.publish_event(json!({"method":"remote/authorizationChanged"}));
    }

    pub async fn reconcile(self: &Arc<Self>, mut settings: RemoteAccessSettings) {
        if self.shutdown_complete.load(Ordering::Acquire) {
            return;
        }
        if self.closing.load(Ordering::Acquire) {
            // 用户要求计划退出先停止派发和重连；后台配置监视不能在回收途中重新开启服务。
            settings.enabled = false;
        }
        let desktop_active = self.desktop_running.load(Ordering::Acquire);
        // Publish restrictions before any file lock, native call, or tunnel await.
        self.publish_access_policy(&settings);
        // 用户要求远程开关只影响网页和 Android；本机入口运行时继续队列派发与原生连接。
        self.harness
            .enabled
            .store(desktop_active || settings.enabled, Ordering::Release);
        self.claude
            .set_reconnect_enabled(desktop_active || settings.enabled);
        let zrok_target = zrok_target(&settings);
        let key_error = self
            .reload_access_key()
            .await
            .err()
            .map(|error| error.to_string());
        let mut live = self.live.lock().await;
        if self.shutdown_complete.load(Ordering::Acquire) {
            return;
        }
        self.configure_secondary_channel(settings.enabled && settings.ngrok_enabled);
        let finished = live.server.as_ref().is_some_and(|task| task.is_finished());
        let was_enabled = live
            .settings
            .as_ref()
            .is_some_and(|settings| settings.enabled);
        let listener_settings_unchanged = live.settings.as_ref().is_some_and(|previous| {
            let mut previous = previous.clone();
            previous.ngrok_enabled = settings.ngrok_enabled;
            previous == settings
        });
        if listener_settings_unchanged
            && !finished
            && key_error.is_none()
            && (live.server.is_some() || !settings.enabled)
        {
            live.settings = Some(settings.clone());
            self.reconcile_primary(&mut live, &settings).await;
            drop(live);
            if !settings.enabled && !desktop_active {
                self.harness.stop_preparation().await;
                self.claude.release().await;
            }
            self.prune_stale_public_devices().await;
            return;
        }
        self.running.store(false, Ordering::SeqCst);
        if !desktop_active {
            self.prompt_optimization.cancel_all();
        }
        self.primary.generation.fetch_add(1, Ordering::SeqCst);
        self.stop_frp(&mut live).await;
        if !desktop_active {
            self.harness_connected.store(false, Ordering::SeqCst);
            self.publish_event(json!({"method":"remote/disconnected"}));
        }
        if let Some(handle) = live.handle.take() {
            handle.shutdown();
        }
        if let Some(task) = live.server.take() {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = live.pump.take() {
            task.abort();
            let _ = task.await;
        }
        live.settings = Some(settings.clone());
        live.error = None;
        if !settings.enabled {
            live.ngrok.stop().await;
            live.zrok.reconcile(false, &zrok_target).await;
            if finished || was_enabled {}
            drop(live);
            // 用户要求关闭即撤下公网入口；Claude 回收可等待，不能持有监听锁让隧道继续运行。
            if !desktop_active {
                self.harness.stop_preparation().await;
                self.claude.release().await;
            }
            self.prune_stale_public_devices().await;
            return;
        }
        if let Some(error) = key_error {
            if !desktop_active {
                self.harness.stop_preparation().await;
            }
            live.ngrok.stop().await;
            live.error = Some(error);
            live.zrok.reconcile(false, &zrok_target).await;
            drop(live);
            self.prune_stale_public_devices().await;
            return;
        }
        match self.start_server(settings.clone()).await {
            Ok((handle, server, pump)) => {
                live.handle = Some(handle);
                live.server = Some(server);
                live.pump = Some(pump);
                self.running.store(true, Ordering::SeqCst);
            }
            Err(error) => live.error = Some(error.to_string()),
        }
        self.reconcile_primary(&mut live, &settings).await;
        if settings.enabled && (!was_enabled || finished) {
            // 服务重新启用或监听器重建后，恢复 Claude 连接端以接收仍在运行的原生会话事件。
            self.claude.reconnect();
        }
        drop(live);
        self.prune_stale_public_devices().await;
    }

    async fn start_server(
        self: &Arc<Self>,
        settings: RemoteAccessSettings,
    ) -> anyhow::Result<(
        axum_server::Handle<std::net::SocketAddr>,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    )> {
        self.start_server_scoped(settings, false).await
    }

    async fn start_server_scoped(
        self: &Arc<Self>,
        settings: RemoteAccessSettings,
        desktop: bool,
    ) -> anyhow::Result<(
        axum_server::Handle<std::net::SocketAddr>,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    )> {
        settings.validate()?;
        if !desktop {
            anyhow::ensure!(
                settings.port != self.desktop_port.load(Ordering::Acquire)
                    && settings.port != crate::protocol_proxy::DEFAULT_PROTOCOL_PROXY_PORT,
                "远程会话不能占用 Harness 本机入口或 Elves 本地代理端口"
            );
        }
        let listener = std::net::TcpListener::bind((settings.bind_host.as_str(), settings.port))
            .context("远程访问端口无法监听，请检查端口是否占用")?;
        listener.set_nonblocking(true)?;
        let prewarm_enabled = settings.enabled;
        let router = scoped_router(self.clone(), settings, desktop);
        let handle = axum_server::Handle::new();
        let server_handle = handle.clone();
        let controller = self.clone();
        let server = axum_server::from_tcp(listener)?.handle(server_handle);
        let server = tokio::spawn(async move {
            let _ = server
                .serve(router.into_make_service_with_connect_info::<SocketAddr>())
                .await;
            if desktop {
                controller.desktop_running.store(false, Ordering::SeqCst);
            } else {
                controller.running.store(false, Ordering::SeqCst);
            }
        });
        let controller = self.clone();
        let pump = tokio::spawn(async move {
            if !desktop && controller.desktop_running.load(Ordering::Acquire) {
                return;
            }
            let prewarm = async {
                if prewarm_enabled {
                    // 用户要求 Codex 仅使用服务模式；目录预热与服务事件循环并行，不阻塞网页监听。
                    if let Err(error) = controller.harness.prewarm_for_listener().await {
                        let _ = crate::diagnostic_log::append_diagnostic_log(
                            "remote_access.harness_prewarm_failed",
                            json!({"message":format!("{error:#}")}),
                        );
                    }
                }
            };
            let harness_pump = async {
                let mut cursor = 0;
                let mut epoch = String::new();
                loop {
                    if controller.harness.enabled.load(Ordering::Acquire) {
                        let result = controller
                            .harness
                            .call("events", json!({"cursor":cursor,"epoch":epoch}), None)
                            .await;
                        match result {
                            Ok(response) if response["ok"] == true => {
                                if !controller.harness_connected.swap(true, Ordering::SeqCst) {
                                    controller.publish_backend_event(
                                        "harness",
                                        json!({"method":"remote/reconnected","params":{"serviceConnected":true}}),
                                    );
                                }
                                epoch = response["epoch"].as_str().unwrap_or("").to_owned();
                                cursor = response["cursor"].as_u64().unwrap_or(0);
                                if response["reset"] == true {
                                    controller.publish_backend_event(
                                        "harness",
                                        json!({"method":"remote/resync"}),
                                    );
                                }
                                for event in response["events"].as_array().into_iter().flatten() {
                                    let mut event = event.clone();
                                    controller.attach_event_images(&mut event);
                                    controller.publish_backend_event("harness", event);
                                }
                            }
                            _ => {
                                if controller.harness_connected.swap(false, Ordering::SeqCst) {
                                    controller.publish_backend_event(
                                        "harness",
                                        json!({"method":"remote/disconnected"}),
                                    );
                                }
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(650)).await;
                }
            };
            tokio::join!(harness_pump, prewarm);
        });
        Ok((handle, server, pump))
    }

    async fn admin_status(&self, settings: &RemoteAccessSettings) -> Value {
        let live = self.live.lock().await;
        let error = live.error.clone();
        let zrok = live.zrok.status();
        let ngrok = live.ngrok.status();
        let primary = self.primary_status(&live);
        drop(live);
        let cleanup = self.claude.cleanup_report();
        let auth = self.auth.lock().await;
        json!({
            "status":"ok", "enabled":settings.enabled,
            "running":self.running.load(Ordering::SeqCst),
            "serviceConnected":self.harness_connected.load(Ordering::SeqCst),
            "closing":self.closing.load(Ordering::Acquire),"claudeCleanup":cleanup,
            "error":error, "addresses":access_addresses(settings),
            "devices":auth.devices.iter().filter(|device|device.expires_at_ms>now_ms())
                .filter(|device|auth.access_key.as_ref().is_some_and(|key|device.key_id==key.id))
                .map(Device::public).collect::<Vec<_>>(),
            "settings":settings, "zrok":zrok, "ngrok":ngrok, "primary":primary,
            "accessKeyConfigured":auth.access_key.is_some(),
            "loginLocked":auth.login_locked
        })
    }

    pub async fn admin(self: &Arc<Self>, action: &str, payload: Value) -> anyhow::Result<Value> {
        if self.closing.load(Ordering::Acquire) {
            anyhow::ensure!(action == "status", "远程后台正在关闭，管理操作尚未执行");
            let settings = SettingsStore::default().load()?.remote_access;
            return Ok(self.admin_status(&settings).await);
        }
        match action {
            "localBootstrap" | "desktopBootstrap" => self.desktop_bootstrap(),
            "addProjectFolder" => {
                // 仅供带本机私有管理令牌的原生菜单调用；不开放接受任意路径的远程写入口。
                let path = payload["path"].as_str().context("缺少所选文件夹")?;
                let project = self.harness.add_local_folder(path).await?;
                self.publish_event(json!({"method":"remote/sidebarChanged"}));
                Ok(json!({"ok":true,"project":project}))
            }
            "saveFrp" | "testPrimaryRoute" | "applyPrimaryRoute" => {
                self.primary_command(action, payload).await
            }
            "status" | "apply" => {
                let settings = SettingsStore::default().load()?.remote_access;
                self.reconcile(settings.clone()).await;
                Ok(self.admin_status(&settings).await)
            }
            "downloadZrok" => {
                let settings = SettingsStore::default().load()?.remote_access;
                self.reconcile(settings.clone()).await;
                self.live.lock().await.zrok.begin_download().await?;
                Ok(self.admin_status(&settings).await)
            }
            "registerZrok" => {
                let email = payload["email"].as_str().context("缺少注册邮箱")?;
                let settings = SettingsStore::default().load()?.remote_access;
                self.reconcile(settings).await;
                self.live.lock().await.zrok.register(email).await
            }
            "startZrok" => {
                let _operation = self.primary.try_operation()?;
                let settings = SettingsStore::default().load()?.remote_access;
                self.reconcile(settings.clone()).await;
                anyhow::ensure!(
                    settings.enabled && self.running.load(Ordering::SeqCst),
                    "请先启用远程会话网页"
                );
                let token = payload["token"].as_str();
                let fixed_name = payload["fixedName"].as_str().context("缺少固定公网名称")?;
                let mut live = self.live.lock().await;
                live.zrok
                    .start_public(token, fixed_name, true, &zrok_target(&settings))
                    .await?;
                if self.primary.provider() == primary_route::Provider::Zrok {
                    live.ngrok
                        .reconcile(settings.ngrok_enabled, &zrok_target(&settings))
                        .await;
                } else {
                    live.zrok.reconcile(false, &zrok_target(&settings)).await;
                }
                drop(live);
                Ok(self.admin_status(&settings).await)
            }
            "saveNgrok" => {
                let settings = SettingsStore::default().load()?.remote_access;
                self.reconcile(settings.clone()).await;
                anyhow::ensure!(settings.ngrok_enabled, "请先启用 ngrok 第二线路");
                anyhow::ensure!(self.running.load(Ordering::SeqCst), "请先启用远程会话网页");
                let token = payload["token"].as_str().context("缺少 ngrok Token")?;
                self.live
                    .lock()
                    .await
                    .ngrok
                    .save_and_test(token, &zrok_target(&settings))
                    .await
            }
            "testZrok" => {
                let settings = SettingsStore::default().load()?.remote_access;
                self.reconcile(settings).await;
                self.live.lock().await.zrok.check().await
            }
            "revoke" => {
                let id = payload["deviceId"].as_str().context("缺少设备 ID")?;
                let mut auth = self.auth.lock().await;
                auth.devices.retain(|device| device.id != id);
                self.save_devices(&auth).await?;
                drop(auth);
                self.publish_event(json!({"method":"remote/authorizationChanged"}));
                Ok(json!({"status":"ok"}))
            }
            "diagnostics" => self.diagnostics_admin(payload).await,
            _ => anyhow::bail!("不支持的远程管理操作"),
        }
    }

    async fn authorize(&self, headers: &HeaderMap) -> Option<Device> {
        if !self.running.load(Ordering::SeqCst)
            || !self
                .access_policy
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .enabled
        {
            return None;
        }
        if channels::bearer(headers).is_some() {
            let id = self.channels.device_id(headers)?;
            if !self.device_active(&id).await {
                return None;
            }
            return self
                .auth
                .lock()
                .await
                .devices
                .iter()
                .find(|device| device.id == id)
                .cloned();
        }
        let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
        let token = cookies.split(';').find_map(|cookie| {
            let (name, value) = cookie.trim().split_once('=')?;
            (name == COOKIE_NAME).then_some(value)
        })?;
        if token.is_empty() || token.len() > 256 {
            return None;
        }
        let digest = hash(token);
        // Anonymous and random-cookie requests must not reach key-file/KDF work.
        {
            let auth = self.auth.lock().await;
            if !auth.devices.iter().any(|device| {
                device.expires_at_ms > now_ms() && constant_time_eq(&device.token_hash, &digest)
            }) {
                return None;
            }
        }
        if !self.reload_access_key().await.ok()? {
            return None;
        }
        let mut auth = self.auth.lock().await;
        let key_id = auth.access_key.as_ref()?.id.clone();
        let device = auth.devices.iter_mut().find(|device| {
            device.key_id == key_id
                && constant_time_eq(&device.token_hash, &digest)
                && device.expires_at_ms > now_ms()
        })?;
        device.last_seen_at_ms = now_ms();
        Some(device.clone())
    }

    async fn call_authorized(
        &self,
        action: &str,
        mut payload: Value,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        // 手动命名与自动标题写回共用锁；用户开始手动改名即停止该会话的后续自动更新。
        let _naming_serial = if action == "rename" {
            let id = payload["threadId"].as_str().unwrap_or("");
            let lock = self.conversation_naming.rename_lock(id).await;
            let guard = lock.lock_owned().await;
            authorization
                .prepare_dispatch()
                .await?
                .commit(|| self.conversation_naming.manual(id))?;
            Some(guard)
        } else {
            None
        };
        if action == "openWorkspace" {
            // 工作区打开使用独立原生实现，但沿用持久回执，浏览器/网关重试不能重复启动应用。
            return open_in::dispatch(payload, authorization)
                .await
                .map_err(Into::into);
        }
        if action == "harnessDiagnostics" {
            return self.harness_diagnostics(&payload, authorization).await;
        }
        if action.ends_with("ScheduledTask") {
            return self
                .scheduled_tasks
                .action(self, action, &payload, authorization)
                .await;
        }
        if action == "clearRecentPriority" {
            let invalid = || ApiError::new(400, "待清除的优先事项标识无效");
            let ids = payload["threadIds"]
                .as_array()
                .filter(|ids| !ids.is_empty() && ids.len() <= recent_priority::MAX_CLEAR_IDS)
                .ok_or_else(invalid)?
                .iter()
                .map(|id| notification_thread_id(id).ok_or_else(invalid))
                .collect::<Result<std::collections::HashSet<_>, _>>()?;
            let priority = &self.harness.recent_priority;
            let observed = priority.revision();
            // 各端传的是点击时看到的候选会话；服务重新读取原生状态，不信任页面报告的已读、空闲或归属。
            let mut sidebar = self
                .call_backend("sidebar", json!({"enabledInstanceIds":payload["enabledInstanceIds"]}), authorization)
                .await
                .and_then(|value| native_result(value).map_err(Into::into))
                .map_err(|error| {
                    error.downcast_ref::<ApiError>().cloned()
                        .unwrap_or_else(|| ApiError::new(503, "无法核对会话状态，优先事项未清除"))
                })?;
            self.hydrate_priority_unread_state(&mut sidebar, &ids, authorization)
                .await;
            let clear_result = authorization
                .prepare_dispatch()
                .await?
                .commit(|| priority.clear(&sidebar, &ids, observed));
            let mut requested_sample = ids.iter().copied().take(32).collect::<Vec<_>>();
            requested_sample.sort_unstable();
            match &clear_result {
                Ok(removed) => {
                    let _ = crate::diagnostic_log::append_diagnostic_log(
                        "remote_access.priority_clear",
                        json!({
                            "requestId":authorization.request_id,
                            "deviceId":authorization.device.id,
                            "requestedCount":ids.len(),
                            "requestedIdsSample":requested_sample,
                            "removed":removed,
                            "observedRevision":observed,
                            "currentRevision":priority.revision(),
                            "sideThreadsComplete":sidebar["sideThreadsComplete"],
                            "sideThreadsError":sidebar["sideThreadsError"],
                            "outcome":"completed",
                        }),
                    );
                }
                Err(error) => {
                    let message = error.to_string().chars().take(512).collect::<String>();
                    let _ = crate::diagnostic_log::append_diagnostic_log(
                        "remote_access.priority_clear",
                        json!({
                            "requestId":authorization.request_id,
                            "deviceId":authorization.device.id,
                            "requestedCount":ids.len(),
                            "requestedIdsSample":requested_sample,
                            "observedRevision":observed,
                            "currentRevision":priority.revision(),
                            "sideThreadsComplete":sidebar["sideThreadsComplete"],
                            "sideThreadsError":sidebar["sideThreadsError"],
                            "outcome":"failed",
                            "error":message,
                        }),
                    );
                }
            }
            let removed = clear_result?;
            if removed != 0 {
                self.publish_event(json!({"method":"remote/sidebarChanged","params":{"reason":"priority"}}));
            }
            return Ok(json!({"ok":true,"removed":removed}));
        }
        if action == "setPermissions" {
            let id = payload["threadId"]
                .as_str()
                .filter(|id| notification_thread_id(&json!(id)).is_some())
                .ok_or_else(|| ApiError::new(400, "会话 ID 无效"))?;
            if claude::Claude::is_subagent_id(id) {
                return Err(ApiError::new(403, "子智能体仅支持只读查看").into());
            }
            let _codex_serial = if !claude::Claude::owns_id(id) {
                Some(self.harness.thread_lock(id).await.lock_owned().await)
            } else {
                None
            };
            if !claude::Claude::owns_id(id) {
                self.harness.check_deletion_barrier(id).await?;
            }
            // 用户要求侧边继承主会话权限且没有独立切换入口；接口同样禁止保存侧边权限覆盖。
            if self.harness.any_side(id).await {
                return Err(ApiError::new(409, "侧边会话沿用创建时继承的主会话权限").into());
            }
            // 只确认原会话身份，不恢复执行；实例来自服务端会话，不能由客户端改投环境。
            let context = self
                .call_backend("workspaceContext", payload.clone(), authorization)
                .await?;
            let backend = if claude::Claude::owns_id(id) {
                "claude"
            } else {
                "codex"
            };
            let instance = context["instanceId"].as_str().unwrap_or("codex:local");
            let mode = permissions::validate_mode(backend, &payload["permissionMode"])?;
            if backend == "claude" {
                return self
                    .claude
                    .set_permissions(id, mode, authorization)
                    .await
                    .map_err(claude::api_error);
            }
            let revision = self
                .permissions
                .save(id, instance, mode, authorization)
                .await?;
            let result = json!({"ok":true,"threadId":id,"instanceId":instance,
                "permissionMode":mode,"permissionRevision":revision,"appliesTo":"nextTurn"});
            self.publish_event(json!({"method":"thread/settings/updated","params":result}));
            return Ok(result);
        }
        if matches!(
            action,
            "send" | "setGoal" | "editMessage" | "steerQueued" | "createSideThread" | "forkThread"
        ) {
            if let Some(selection) = match payload["threadId"].as_str() {
                Some(id) if claude::Claude::owns_id(id) => {
                    self.claude.permission_selection(id).await?
                }
                Some(id) => self.permissions.get(id).await,
                None => None,
            } {
                if payload.get("instanceId").is_some_and(|instance| {
                    instance.as_str() != Some(selection.instance_id.as_str())
                }) {
                    return Err(ApiError::new(409, "会话权限所属实例已变化").into());
                }
                // 用户要求会话选择只影响该会话；发送前带入覆盖，不改写实例全局配置。
                payload["permissionMode"] = json!(selection.mode);
            }
        }
        if let Some(mode) = payload.get("permissionMode") {
            let backend = if payload["backend"] == "claude"
                || payload["instanceId"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("claude:"))
                || payload["threadId"]
                    .as_str()
                    .is_some_and(claude::Claude::owns_id)
            {
                "claude"
            } else {
                "codex"
            };
            permissions::validate_mode(backend, mode)?;
            // 覆盖值由服务器重新解析，不能信任客户端提供的任意批准策略或文件目录。
            payload.as_object_mut().unwrap().remove("permissionConfig");
            if backend == "codex"
                && !matches!(action, "create" | "createSideThread")
                && payload["permissionMode"] == "configured"
            {
                payload["permissionConfig"] = self.harness.configured_permissions().await?;
            }
        }
        if matches!(
            action,
            "optimizePrompt" | "promptOptimization" | "cancelPromptOptimize"
        ) {
            // 用户要求优化独立于 Codex/Claude 实例；必须在会话后端路由前处理，不进入会话执行引擎准入。
            return execution::Scope::new(
                self.harness.audit.clone(),
                action,
                &payload,
                Some(authorization),
            )
            .recorded(async {
                execution::transport("http-model", action, false);
                self.prompt_optimization
                    .call(action, payload, &self.state_dir, authorization)
                    .await
            })
            .await;
        }
        let context_thread_id = payload["threadId"]
            .as_str()
            .filter(|id| action == "thread" && !claude::Claude::owns_id(id))
            .map(str::to_owned);
        let codex_creation = matches!(action, "create" | "forkThread")
            && payload["backend"] != "claude"
            && !payload["instanceId"]
                .as_str()
                .is_some_and(|id| id.starts_with("claude:"));
        let creation_mode = codex_creation
            .then(|| payload.get("permissionMode").cloned())
            .flatten();
        let creation_instance = payload["instanceId"]
            .as_str()
            .unwrap_or("codex:local")
            .to_owned();
        let codex_request = payload["backend"] != "claude"
            && !creation_instance.starts_with("claude:")
            && !payload["threadId"]
                .as_str()
                .is_some_and(claude::Claude::owns_id);
        if codex_request
            && payload["reasoningEffort"] == "ultra"
            && !instance_settings::ultra_enabled(&self.state_dir, &creation_instance)?
        {
            return Err(
                ApiError::new(409, "当前实例未开放 Ultra 思考深度，请先在设置中开启").into(),
            );
        }
        let detail_id = (action == "thread")
            .then(|| payload["threadId"].as_str().map(str::to_owned))
            .flatten();
        let naming_input = matches!(action, "create" | "send")
            .then(|| json!({"threadId":payload["threadId"],"prompt":payload["prompt"]}));
        let priority = &self.harness.recent_priority;
        let observed_priority = (action == "sidebar").then(|| priority.revision());
        let mut value = self.call_backend(action, payload, authorization).await?;
        if let Some(observed) = observed_priority
            && value["ok"] == true
            && priority.annotate(&mut value, observed)
        {
            self.publish_event(json!({"method":"remote/sidebarChanged","params":{"reason":"priority"}}));
        }
        // 没有首轮的主会话分支也应出现在所有端；仅浏览和恢复旧会话不产生创建回执。
        if value["ok"] == true
            && matches!(action, "create" | "forkThread")
            && let Some(id) = value["conversationId"].as_str().or_else(|| value["threadId"].as_str())
        {
            priority.promote(id);
            self.publish_event(json!({"method":"remote/sidebarChanged","params":{"reason":"priority"}}));
        }
        if let Some(input) = naming_input {
            self.conversation_naming
                .accepted(action, &input, &value, &authorization.device.id);
        }
        if codex_request && action == "models" {
            instance_settings::filter_models(
                &mut value,
                instance_settings::ultra_enabled(&self.state_dir, &creation_instance)?,
            );
        }
        if let Some(mode) = creation_mode
            .or_else(|| codex_creation.then(|| value["permissionMode"].clone()))
            .and_then(|mode| mode.as_str().map(str::to_owned))
        {
            if let Some(id) = value["conversationId"]
                .as_str()
                .or_else(|| value["threadId"].as_str())
            {
                if let Err(error) = self
                    .permissions
                    .save(id, &creation_instance, &mode, authorization)
                    .await
                {
                    let _ = crate::diagnostic_log::append_diagnostic_log(
                        "remote_access.permissions.persist_failed",
                        json!({"threadId":id,"error":error.to_string()}),
                    );
                    value["permissionWarning"] = json!(
                        "本轮已使用所选权限，但会话权限保存失败，请重新选择；不要重复发送消息"
                    );
                }
            }
        }
        if let Some(id) = detail_id
            && let Some(selection) = self.permissions.get(&id).await
            && value["thread"].is_object()
            && value["thread"]["serviceSide"] != true
        {
            value["thread"]["permissionMode"] = json!(selection.mode);
            value["thread"]["permissionRevision"] = json!(selection.revision);
        }
        if let Some(thread_id) = context_thread_id
            && context_usage::needs_usage(&value, &thread_id)
        {
            let settings_path = self.state_dir.join("settings.json");
            let fallback = value.clone();
            let value = tokio::task::spawn_blocking(move || {
                let Ok(settings) = SettingsStore::new(settings_path).load() else {
                    return value;
                };
                let home = crate::codex_home::codex_home_dir_for_settings(&settings);
                context_usage::fill_missing(&home, &thread_id, value)
            })
            .await
            .unwrap_or(fallback);
            authorization.check().await?;
            return Ok(value);
        }
        Ok(value)
    }

    async fn harness_diagnostics(
        &self,
        payload: &Value,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        let thread_id = payload["threadId"]
            .as_str()
            .filter(|id| notification_thread_id(&json!(id)).is_some())
            .map(str::to_owned);
        let mut enabled_instance_ids = vec!["codex:local".to_owned()];
        // 目标是 Claude 会话时只补查该会话所属实例，避免诊断入口默认扫描所有 WSL。
        if let Some(id) = thread_id
            .as_deref()
            .filter(|id| claude::Claude::owns_id(id))
            && let Ok(instance_id) = self.claude.instance_id(id).await
        {
            enabled_instance_ids.push(instance_id);
        }
        let sidebar = self
            .call_backend(
                "sidebar",
                json!({"enabledInstanceIds":enabled_instance_ids}),
                authorization,
            )
            .await?;
        let mut target = Value::Null;
        let mut detail = Value::Null;
        let mut related = Value::Null;
        let mut thread_read_error = Value::Null;
        let mut related_threads_error = Value::Null;
        let mut related_for_id = Value::Null;
        if let Some(id) = thread_id.as_deref() {
            target = ["recent", "pinnedThreads"]
                .into_iter()
                .flat_map(|key| sidebar[key].as_array().into_iter().flatten())
                .find(|row| row["id"].as_str() == Some(id))
                .cloned()
                .unwrap_or(Value::Null);
            match self
                .call_backend("thread", json!({"threadId":id}), authorization)
                .await
            {
                Ok(value) => {
                    detail = value;
                    related_for_id = detail["thread"]["parentThreadId"]
                        .as_str()
                        .or_else(|| detail["thread"]["attachedToThreadId"].as_str())
                        .unwrap_or(id)
                        .into();
                }
                Err(error) => {
                    thread_read_error = json!(format!("{error:#}"));
                    related_for_id = json!(id);
                }
            }
            let related_id = related_for_id.as_str().unwrap_or(id);
            match self
                .call_backend(
                    "relatedThreads",
                    json!({"threadId":related_id}),
                    authorization,
                )
                .await
            {
                Ok(value) => related = value,
                Err(error) => related_threads_error = json!(format!("{error:#}")),
            }
        }
        let unknown_unread_ids = ["recent", "pinnedThreads"]
            .into_iter()
            .flat_map(|key| sidebar[key].as_array().into_iter().flatten())
            .filter_map(|row| {
                row["hasUnreadTurn"]
                    .is_null()
                    .then(|| row["id"].as_str())
                    .flatten()
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "ok":true,
            "capturedAtMs":now_ms(),
            "buildId":crate::assets::DIAGNOSTIC_BUILD_ID,
            "version":crate::version::VERSION,
            "runtime":{
                "desktop":true,
                "desktopRunning":self.desktop_running.load(Ordering::Acquire),
                "serviceConnected":self.harness_connected.load(Ordering::Acquire),
                "codexHarnessEnabled":self.harness.enabled.load(Ordering::Acquire),
                "closing":self.closing.load(Ordering::Acquire),
                "desktopPort":self.desktop_port.load(Ordering::Acquire)
            },
            "priority":{
                "ids":sidebar["recentPriorityIds"],
                "clearableIds":sidebar["recentPriorityClearableIds"],
                "unknownUnreadIds":unknown_unread_ids,
                "sideThreadsComplete":sidebar["sideThreadsComplete"],
                "sideThreadsError":sidebar["sideThreadsError"]
            },
            "persistentSides":{
                "ids":sidebar["sideThreadIds"],
                "unread":sidebar["unreadSideThreads"]
            },
            "target":{
                "requestedId":thread_id,
                "foundInSidebar":!target.is_null(),
                "relatedForId":related_for_id,
                "sidebar":target,
                "threadRead":detail["thread"],
                "threadReadError":thread_read_error,
                "relatedThreads":related,
                "relatedThreadsError":related_threads_error
            }
        }))
    }

    async fn call_backend(
        &self,
        action: &str,
        mut payload: Value,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        if self.closing.load(Ordering::Acquire) {
            return Err(ApiError::new(503, "远程服务正在关闭，操作尚未派发").into());
        }
        let instance_id = payload
            .get("instanceId")
            .map(|value| {
                value
                    .as_str()
                    .filter(|id| !id.is_empty() && id.len() <= 160)
                    .map(str::to_owned)
                    .ok_or_else(|| ApiError::new(400, "会话实例标识无效"))
            })
            .transpose()?;
        if let Some(id) = &instance_id {
            let backend = id.split(':').next().unwrap_or("");
            if !instances::TYPES.iter().any(|kind| kind.id == backend) {
                return Err(ApiError::new(400, "未知会话实例类型").into());
            }
            if payload.get("backend").is_some_and(|value| value != backend) {
                return Err(ApiError::new(409, "会话类型与实例不匹配").into());
            }
            payload["backend"] = json!(backend);
        }
        let thread_id_owned = payload["threadId"].as_str().map(str::to_owned);
        let thread_id = thread_id_owned.as_deref();
        let claude_thread = thread_id.is_some_and(claude::Claude::owns_id);
        if action == "releaseExternalHolders" && !claude_thread {
            // 只有 Claude 会话能核对并关闭别的进程的持有；Codex 的占用提示仍只提供重试。
            return Err(ApiError::new(409, "仅 Claude 会话支持关闭其他占用").into());
        }
        let requested_owned = payload["backend"].as_str().map(str::to_owned);
        let requested = requested_owned.as_deref();
        if requested.is_some_and(|backend| !instances::TYPES.iter().any(|kind| kind.id == backend))
        {
            return Err(ApiError::new(400, "会话后端无效").into());
        }
        if requested.is_some_and(|backend| !matches!(backend, "codex" | "claude")) {
            // 新类型可先注册检测和设置，但必须提供执行适配器后才能派发，不能默认落到 Codex。
            return Err(ApiError::new(409, "该会话类型的执行接入尚未支持").into());
        }
        // 用户要求创建后类型固定；以已保存会话身份路由，不能由页面改写。
        if thread_id.is_some()
            && requested.is_some_and(|backend| (backend == "claude") != claude_thread)
        {
            return Err(ApiError::new(409, "已创建的会话不能切换 Codex / Claude").into());
        }
        // 用户要求每个实例独立控制；已有会话的实例来自保存身份，客户端不能通过参数改投另一个环境。
        if let Some(id) = &instance_id {
            let expected = if let Some(thread_id) = thread_id {
                if claude_thread {
                    self.claude.instance_id(thread_id).await?
                } else {
                    "codex:local".into()
                }
            } else {
                id.clone()
            };
            if *id != expected {
                return Err(ApiError::new(409, "已创建的会话不能切换运行实例").into());
            }
            if requested == Some("codex") && id != "codex:local" {
                return Err(ApiError::new(409, "该 Codex 实例的执行接入尚未支持").into());
            }
        }
        if desktop_images::has_refs(&payload) {
            // 已解析所属会话及实例后才接收本机图片，不能把引用送到 WSL 或从远程指定本机文件。
            if authorization.device.key_id != desktop::DEVICE_KEY_ID || claude_thread
                || requested == Some("claude")
                || !matches!(action, "send" | "create" | "setGoal" | "editQueued")
            {
                return Err(ApiError::new(403, "本机原图仅支持 Elves 的本机 Codex 会话").into());
            }
            desktop_images::resolve(self.state_dir.clone(), &mut payload, authorization).await?;
        }
        if payload.get("files").is_some() {
            if !matches!(action, "send" | "create" | "setGoal" | "editQueued") {
                return Err(ApiError::new(400, "当前操作不接受文件和文件夹附件").into());
            }
            let paths = input_files::stage(self.state_dir.clone(), &payload, authorization).await?;
            if claude_thread || requested == Some("claude") {
                let config = if !claude_thread {
                    match &instance_id {
                        Some(id) => Some(self.claude_instance_config(id).await?),
                        None => None,
                    }
                } else {
                    None
                };
                let target_paths = self.claude
                    .local_input_paths(thread_id, config, paths)
                    .await
                    .map_err(|error| {
                        ApiError::new(
                            409,
                            format!("附件无法映射到目标 WSL，消息尚未发送：{error}"),
                        )
                    })?
                    ;
                // Claude 原生 CLI 当前仍通过正文接收目标 WSL 路径；Codex 走下方结构化 mention。
                input_files::append_paths(&mut payload, target_paths);
            } else {
                input_files::replace_paths(&mut payload, paths)?;
            }
        }
        if action == "sessionMetrics" {
            let id = thread_id.ok_or_else(|| ApiError::new(400, "缺少会话 ID"))?;
            authorization.check().await?;
            let metrics = if claude_thread {
                self.claude.session_metrics(id).await?
            } else if let Some(metrics) = self.harness.side_session_metrics(&id).await {
                metrics
            } else {
                let settings_path = self.state_dir.join("settings.json");
                let first_token = self.harness.session_first_token(&id).await;
                let attached = self.harness.any_side(&id).await;
                let id = id.to_owned();
                tokio::task::spawn_blocking(move || {
                    let Ok(settings) = SettingsStore::new(settings_path).load() else {
                        return Value::Null;
                    };
                    let home = crate::codex_home::codex_home_dir_for_settings(&settings);
                    if attached { session_metrics::codex_thread(&home, &id, first_token, true) }
                    else { session_metrics::codex(&home, &id, first_token) }
                })
                .await
                .unwrap_or(Value::Null)
            };
            let metrics = session_metrics::with_cache_expiry(metrics, id);
            return Ok(json!({"ok":true,"threadId":id,
                "backend":if claude_thread {"claude"} else {"codex"},"metrics":metrics}));
        }
        if action == "claudeUsage" {
            // 本地消费扫描只在用户展开额度详情时按需执行，不进入默认额度轮询或消息发送链路。
            let include_local = payload["includeLocal"] == true;
            let allow_probe = authorization.device.key_id == desktop::DEVICE_KEY_ID
                || !self
                    .access_policy
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .read_only;
            // 用户要求只能经会话所属 WSL 的 Claude 读取额度；已有会话始终使用保存的实例身份。
            return if claude_thread {
                self.claude
                    .session_subscription_usage(thread_id.unwrap(), allow_probe, include_local)
                    .await
            } else if requested == Some("claude") {
                let config = if let Some(id) = &instance_id {
                    self.claude_instance_config(id).await?
                } else {
                    self.claude.config()?
                };
                self.claude
                    .subscription_usage(config, allow_probe, include_local)
                    .await
            } else {
                Err(ApiError::new(400, "额度读取仅支持 Claude 实例").into())
            };
        }
        if matches!(action, "models" | "skills")
            && (claude_thread || requested == Some("claude"))
            && authorization.device.key_id != desktop::DEVICE_KEY_ID
            && self
                .access_policy
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .read_only
        {
            // 只读设备不启动可能加载 Hooks / MCP 的目录探测 CLI；模型和 Skill 共用此边界。
            return Ok(
                json!({"ok":true,"backend":"claude","readOnly":true,"data":[],
                "defaultModel":"","defaultReasoningEffort":""}),
            );
        }
        if claude_thread {
            let id = thread_id.unwrap();
            // Claude 子代理只复用现有查看页；任何发送、停止、审批或设置写入都不能改投其主会话。
            if claude::Claude::is_subagent_id(id)
                && !matches!(
                    action,
                    "thread"
                        | "history"
                        | "workspaceContext"
                        | "models"
                        | "skills"
                        | "queue"
                        | "relatedThreads"
                )
            {
                return Err(
                    ApiError::new(403, "Claude 子代理仅支持只读查看，请返回主会话操作").into(),
                );
            }
            return execution::Scope::new(
                self.harness.audit.clone(),
                action,
                &payload,
                Some(authorization),
            )
            .recorded(async {
                execution::transport("claude-wsl-stdio", action, action == "send");
                match action {
                    "thread" => self.claude.detail_authorized(id, Some(authorization)).await,
                    "history" => {
                        self.claude
                            .history_authorized(&payload, Some(authorization))
                            .await
                    }
                    "workspaceContext" => self.claude.workspace(id).await,
                    "models" => {
                        self.claude
                            .session_models(
                                id,
                                payload["includeAvailable"] == true,
                                payload["forceReload"] == true,
                            )
                            .await
                    }
                    "skills" => {
                        self.claude
                            .session_skills(id, payload["forceReload"] == true)
                            .await
                    }
                    "queue" => self.claude.queue(id).await,
                    "queueEdit" => self.claude.queue_edit_state(&payload).await,
                    "sendState" => self.claude.queue_send_state(&payload).await,
                    "relatedThreads" => self.claude.related_threads(id).await,
                    _ => self.claude.command(action, &payload, authorization).await,
                }
            })
            .await
            .map_err(claude::api_error);
        }
        if requested == Some("claude") {
            let selected_config = if let Some(id) = &instance_id {
                Some(self.claude_instance_config(id).await?)
            } else {
                None
            };
            let cwd =
                if let Some(project_id) = payload["projectId"].as_str().filter(|s| !s.is_empty()) {
                    let context = self
                        .call_codex_backend(
                            "workspaceContext",
                            json!({"projectId":project_id}),
                            authorization,
                        )
                        .await?;
                    let context = native_result(context)?;
                    anyhow::ensure!(
                        context["hostId"] == "local" && context["projectId"] == project_id,
                        "Claude 仅支持已确认的本机项目"
                    );
                    context["cwd"]
                        .as_str()
                        .context("项目工作目录不可用")?
                        .to_owned()
                } else {
                    String::new()
                };
            return execution::Scope::new(
                self.harness.audit.clone(),
                action,
                &payload,
                Some(authorization),
            )
            .recorded(async {
                execution::transport("claude-wsl-stdio", action, action == "send");
                match action {
                    "projectTrust" => {
                        let config = match selected_config {
                            Some(config) => config,
                            None => self.claude.config()?,
                        };
                        self.claude.project_trust(config, &cwd, None, None).await
                    }
                    "models" => {
                        if let Some(config) = selected_config {
                            self.claude
                                .models_in(config, &cwd, payload["forceReload"] == true)
                                .await
                        } else {
                            self.claude
                                .models(&cwd, payload["forceReload"] == true)
                                .await
                        }
                    }
                    "skills" => {
                        let config = match selected_config {
                            Some(config) => config,
                            None => self.claude.config()?,
                        };
                        self.claude
                            .skills_in(config, &cwd, payload["forceReload"] == true)
                            .await
                    }
                    "create" => {
                        if let Some(config) = selected_config {
                            self.claude
                                .create_in(config, &payload, &cwd, authorization)
                                .await
                        } else {
                            self.claude.create(&payload, &cwd, authorization).await
                        }
                    }
                    _ => Err(ApiError::new(400, "此操作需要已创建的 Claude 会话").into()),
                }
            })
            .await
            .map_err(claude::api_error);
        }
        // 用户要求默认仅 Codex；旧客户端或后台读取未显式启用 Claude 时也不能启动 WSL 扫描。
        let include_claude = payload["includeClaude"] == true;
        let enabled_instances: Option<Vec<String>> = payload
            .get("enabledInstanceIds")
            .map(|value| {
                let values = value
                    .as_array()
                    .filter(|values| values.len() <= 128)
                    .ok_or_else(|| ApiError::new(400, "启用实例列表无效"))?;
                values
                    .iter()
                    .map(|value| {
                        value
                            .as_str()
                            .filter(|id| !id.is_empty() && id.len() <= 160)
                            .map(str::to_owned)
                            .ok_or_else(|| ApiError::new(400, "启用实例标识无效"))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        if let Some(object) = payload.as_object_mut() {
            object.remove("instanceId");
            object.remove("enabledInstanceIds");
            object.remove("backend");
        }
        // 实例显示开关不改变既有项目发现；仅在 Codex 未启用且目录确实不可用时保留 Harness 自有项目。
        let mut result = match self
            .call_codex_backend(action, payload, authorization)
            .await
        {
            Ok(value) => value,
            Err(failure)
                if action == "sidebar"
                    && enabled_instances
                        .as_ref()
                        .is_some_and(|ids| !ids.iter().any(|id| id == "codex:local")) =>
            {
                let mut value = self.harness.folder_sidebar().await;
                value["projectPreferencesWarning"] = json!(format!(
                    "Codex 项目目录暂不可用：{failure}。已保留 Harness 登记及本机已保存的项目。"
                ));
                value
            }
            Err(failure) => return Err(failure),
        };
        instances::annotate_codex(&mut result);
        if action == "sidebar" {
            let projects = result["projects"].as_array().cloned().unwrap_or_default();
            result["projects"] = json!(self.harness.merge_local_projects(projects).await);
        }
        if action == "sidebar"
            && let Some(ids) = &enabled_instances
        {
            // 按实例筛选只改变返回列表；关闭实例不停止原生任务，也不删除会话。
            instances::filter_sidebar(&mut result, ids);
            // 本机 Claude 尚无执行/发现适配器；启用偏好不能触发默认 WSL，也不把未接通状态当扫描故障。
            for id in ids.iter().filter(|id| id.starts_with("claude:wsl:")) {
                let outcome = async {
                    let config = self.claude_instance_config(id).await?;
                    self.claude.discover_in(&config, &result).await
                }
                .await;
                if let Err(failure) = outcome {
                    result["claudeDiscoveryError"] =
                        json!(format!("Claude 实例发现暂不可用：{failure}"));
                }
            }
            self.claude
                .merge_sidebar_instances(&mut result, Some(ids))
                .await;
        } else if action == "sidebar" && include_claude {
            // 用户要求默认仅启用 Codex；网页明确关闭 Claude 时不触发 WSL 原生会话发现。
            // 用户要求在正常列表刷新中只读发现指定 WSL 原生会话；不能复制其他服务登记文件或启动/恢复模型来“发现”。
            if let Err(failure) = self.claude.discover(&result).await {
                result["claudeDiscoveryError"] =
                    json!(format!("Claude 原生会话发现暂不可用：{failure}"));
            }
            // 两种后端共用同一项目目录；在同一服务目录中合并 Claude 会话。
            self.claude.merge_sidebar(&mut result).await;
        }
        Ok(result)
    }

    async fn claude_instance_config(&self, id: &str) -> anyhow::Result<claude::WslConfig> {
        if let Ok(default) = self.claude.config()
            && id == default.instance_id()
        {
            return Ok(default);
        }
        // 新实例只能从服务端检测结果解析；不接受请求携带的发行版、用户或程序路径。
        let environments = preferences::detect(&self.state_dir, false).await;
        let instance = instances::catalog(&environments)
            .into_iter()
            .find(|instance| instance.id == id && instance.backend == "claude" && instance.ready)
            .ok_or_else(|| ApiError::new(409, "所选 Claude 实例暂不可用或执行接入尚未支持"))?;
        Ok(claude::WslConfig {
            distribution: instance.distribution,
            user: instance.user,
            executable: instance.executable,
        })
    }

    async fn call_codex_backend(
        &self,
        action: &str,
        payload: Value,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        if action == "workspaceContext"
            && payload["threadId"].is_null()
            && let Some(id) = payload["projectId"].as_str()
            && let Some(context) = self.harness.local_project_context(id).await
        {
            return Ok(context);
        }
        // 用户要求移除远程桌面注入路由：所有 Codex 读写都由服务管理，失败只保留原错误与回执，不换通道重发。
        #[cfg(test)]
        if let Some(bridge) = self.test_backend.get() {
            return bridge.call_authorized(action, payload, authorization).await;
        }
        execution::Scope::service(
            self.harness.audit.clone(),
            action,
            &payload,
            Some(authorization),
        )
        .recorded(self.harness.call(action, payload, Some(authorization)))
        .await
    }

    async fn hydrate_priority_unread_state(
        &self,
        sidebar: &mut Value,
        requested: &HashSet<&str>,
        authorization: &HarnessAuthorization,
    ) {
        let targets = ["recent", "pinnedThreads"]
            .into_iter()
            .flat_map(|key| {
                sidebar[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|row| {
                        let id = notification_thread_id(&row["id"])?;
                        (requested.contains(id) && row["hasUnreadTurn"].is_null())
                            .then(|| id.to_owned())
                    })
            })
            .collect::<HashSet<_>>();
        if targets.is_empty() {
            return;
        }

        // 用户要求优先事项清除必须依据实际已读状态；原生 thread/list 对 notLoaded 会话不返回已读字段，
        // 这里只补读本次目标，读取失败继续保留 null，不能把未知状态直接当成已读。
        let started = Instant::now();
        let target_count = targets.len();
        let mut resolved = HashMap::new();
        let mut failure_count = 0usize;
        let mut failure_samples = Vec::new();
        let record_failure = |samples: &mut Vec<Value>,
                              count: &mut usize,
                              thread_id: &str,
                              stage: &str,
                              detail: Value| {
            *count += 1;
            if samples.len() < 32 {
                samples.push(json!({"threadId":thread_id,"stage":stage,"detail":detail}));
            }
        };
        for id in targets {
            let value = match self
                .call_backend("thread", json!({"threadId":id}), authorization)
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    let message = error.to_string().chars().take(512).collect::<String>();
                    record_failure(
                        &mut failure_samples,
                        &mut failure_count,
                        &id,
                        "thread_read_error",
                        json!({"message":message}),
                    );
                    continue;
                }
            };
            let thread = &value["thread"];
            if thread["id"].as_str() != Some(id.as_str()) {
                record_failure(
                    &mut failure_samples,
                    &mut failure_count,
                    &id,
                    "thread_identity_mismatch",
                    json!({"returnedId":thread["id"]}),
                );
                continue;
            }
            let Some(unread) = thread["hasUnreadTurn"].as_bool() else {
                record_failure(
                    &mut failure_samples,
                    &mut failure_count,
                    &id,
                    "unread_state_missing",
                    json!({"hasUnreadTurn":thread["hasUnreadTurn"],"readState":thread["readState"]}),
                );
                continue;
            };
            resolved.insert(id, (unread, thread["readState"].clone()));
        }

        for key in ["recent", "pinnedThreads"] {
            for row in sidebar[key]
                .as_array_mut()
                .into_iter()
                .flatten()
            {
                let Some(id) = notification_thread_id(&row["id"]) else {
                    continue;
                };
                let Some((unread, read_state)) = resolved.get(id) else {
                    continue;
                };
                row["hasUnreadTurn"] = json!(*unread);
                if !read_state.is_null() {
                    row["readState"] = read_state.clone();
                }
            }
        }
        let mut requested_sample = requested.iter().copied().take(32).collect::<Vec<_>>();
        requested_sample.sort_unstable();
        let _ = crate::diagnostic_log::append_diagnostic_log(
            "remote_access.priority_unread_hydration",
            json!({
                "requestId":authorization.request_id,
                "deviceId":authorization.device.id,
                "requestedCount":requested.len(),
                "requestedIdsSample":requested_sample,
                "targetCount":target_count,
                "resolvedCount":resolved.len(),
                "failureCount":failure_count,
                "failureSamples":failure_samples,
                "durationMs":started.elapsed().as_millis(),
            }),
        );
    }

    fn publish_backend_event(&self, source: &str, mut event: Value) {
        // Codex 在原生投影接受事件时维护名单；不能在轮询转发历史事件时再次加入已清除项。
        if source == "claude"
            && let Some(id) = event["params"]["threadId"].as_str()
            && claude::Claude::owns_id(id) && !claude::Claude::is_subagent_id(id)
        {
            let priority = &self.harness.recent_priority;
            match event["method"].as_str() {
                Some("thread/deleted") => priority.forget(id),
                Some("thread/started" | "turn/started") => priority.promote(id),
                Some("remote/requestsChanged") if event["params"]["requests"].as_array().is_some_and(|requests| !requests.is_empty()) =>
                    priority.promote(id),
                Some("turn/completed" | "remote/requestsChanged" | "remote/sidebarChanged") =>
                    priority.runtime_changed(),
                _ => {}
            }
        }
        event["_runtimeSource"] = json!(source);
        self.publish_event(event);
    }

    async fn event_for_device(&self, device_id: &str, mut event: Value) -> Value {
        let suppressed = event["_targetDevice"]
            .as_str()
            .is_some_and(|id| id != device_id);
        if suppressed {
            // 设备定向事件仍保留连续序号；屏蔽其他设备的内容时用空心跳占位。
            return json!({"method":"remote/heartbeat","channelEpoch":event["channelEpoch"],
                "channelSequence":event["channelSequence"]});
        }
        if let Some(object) = event.as_object_mut() {
            object.remove("_runtimeSource");
            object.remove("_targetDevice");
        }
        event
    }

    async fn replay_for_device(&self, device_id: &str, mut replay: Value) -> Value {
        if let Some(events) = replay["events"].as_array_mut() {
            for event in events {
                *event = self.event_for_device(device_id, event.clone()).await;
            }
        }
        replay
    }

    async fn execute(
        self: &Arc<Self>,
        device: Device,
        request_id: String,
        action: String,
        payload: Value,
    ) -> Result<Value, ApiError> {
        let mut authorization = self.authorization(device.clone(), Some(&action));
        authorization.request_id = Some(request_id.clone());
        authorization.check().await.map_err(authorization_error)?;
        if request_id.is_empty()
            || request_id.len() > 100
            || !request_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(ApiError::new(400, "需要稳定的客户端请求 ID"));
        }
        let key = format!("{}:{request_id}", device.id);
        let digest = hash(&serde_json::to_string(&json!([action, payload])).unwrap());
        // 未完成删除清理仍依赖原成功回执；换设备或原授权过期不能先淘汰唯一的持久确认。
        let protected_deletions = self.harness.protected_delete_receipts().await;
        {
            let desktop = self.desktop_device.lock().await;
            let mut auth = self.auth.lock().await;
            if !auth
                .resolve_device(desktop.as_ref(), &device.id, Some(&device.key_id))
                .is_some_and(|current| constant_time_eq(&current.token_hash, &device.token_hash))
            {
                return Err(ApiError::new(401, "设备授权已撤销或过期"));
            }
            let mut active_devices = auth
                .devices
                .iter()
                .filter(|device| device.expires_at_ms > now_ms())
                .map(|device| device.id.clone())
                .collect::<std::collections::HashSet<_>>();
            // 本机回执按持久身份保留；远程关闭、换密钥或设备清理不能导致重复执行桌面操作。
            if let Some(desktop) = desktop.as_ref() {
                active_devices.insert(desktop.id.clone());
            }
            drop(desktop);
            auth.receipts.retain(|key, receipt| {
                receipt.state == "pending"
                    || active_devices.contains(&receipt.device_id)
                    || protected_deletions.contains(key)
            });
            if let Some(receipt) = auth.receipts.get(&key) {
                if receipt.payload_hash != digest {
                    return Err(ApiError::new(409, "请求 ID 已用于不同操作"));
                }
                let receipt = receipt.clone();
                let value = receipt_value(&receipt);
                drop(auth);
                let value = self.harness.recover_delete_receipt(&receipt, value).await;
                self.authorization(device, None)
                    .check()
                    .await
                    .map_err(|error| authorization_error(error).after_admission())?;
                return Ok(value);
            }
            if auth.receipts.len() >= MAX_RECEIPTS {
                return Err(ApiError::new(429, "消息回执已达上限"));
            }
            auth.receipts.insert(
                key.clone(),
                Receipt {
                    device_id: device.id.clone(),
                    request_id: request_id.clone(),
                    payload_hash: digest,
                    created_at_ms: now_ms(),
                    state: "pending".into(),
                    result: None,
                },
            );
            if self.save_receipts(&auth).await.is_err() {
                auth.receipts.remove(&key);
                return Err(ApiError::new(503, "无法保存回执，操作未发送"));
            }
        }
        let controller = self.clone();
        let dispatch_authorization = authorization.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let operation_id = request_id.clone();
        tokio::spawn(async move {
            let result = controller
                .call_authorized(&action, payload, &dispatch_authorization)
                .await;
            let mut auth = controller.auth.lock().await;
            let receipt = auth.receipts.get_mut(&key).unwrap();
            match result {
                Ok(mut value) => {
                    controller.attach_response_images(&mut value);
                    receipt.state = if value["outcomeUnknown"] == true {
                        "unknown"
                    } else if value["ok"] == true {
                        "completed"
                    } else {
                        "failed"
                    }
                    .into();
                    receipt.result = Some(value);
                }
                Err(error)
                    if error
                        .downcast_ref::<ApiError>()
                        .is_some_and(|error| !error.outcome_unknown) =>
                {
                    let error = error.downcast_ref::<ApiError>().unwrap();
                    receipt.state = "failed".into();
                    receipt.result = Some(
                        json!({"ok":false,"notSent":true,"code":error.code,"message":error.message}),
                    );
                }
                Err(error) => {
                    // 服务引擎可能已接受操作；连接中断不能据此宣告失败或自动重发。
                    receipt.state = "unknown".into();
                    // 用户要求删除失败能定位原因；删除回执不能套用发送提示，类型化原生错误也不能被吞掉。
                    receipt.result = Some(if action == "deleteThread" {
                        let message = error.downcast_ref::<ApiError>().map_or_else(
                            || "删除结果尚未确认，请在原生客户端核对，勿重复删除。".to_owned(),
                            |error| {
                                format!(
                                    "删除结果尚未确认：{}。请在原生客户端核对，勿重复删除。",
                                    error.message
                                )
                            },
                        );
                        json!({"ok":false,"outcomeUnknown":true,"message":message})
                    } else {
                        json!({"message":"送达状态未知，请查看原生会话，不要重复发送"})
                    });
                }
            }
            let response = receipt_value(receipt);
            if controller.save_receipts(&auth).await.is_err() {
                let _ = sender.send(json!({"requestId":operation_id,"state":"unknown","result":{"message":"回执保存失败，请查看会话确认结果"}}));
            } else {
                let _ = sender.send(response);
            }
        });
        let value = match tokio::time::timeout(Duration::from_secs(40), receiver).await {
            Ok(Ok(value)) => value,
            _ => json!({"requestId":request_id,"state":"pending"}),
        };
        self.authorization(device, None)
            .check()
            .await
            .map_err(|error| authorization_error(error).after_admission())?;
        Ok(value)
    }
}

fn receipt_value(receipt: &Receipt) -> Value {
    let mut result = receipt.result.clone().unwrap_or(Value::Null);
    // 用户要求修复旧版服务模式删除误报；用操作摘要确认这确实是删除，不能把其它成功操作当成删除。
    // 只补齐已成功的 stdio 删除回执，不重放删除，也不放宽 pending/unknown/failed 或显式未确认的结果。
    if receipt.state == "completed"
        && result["ok"] == true
        && result["executionTransport"] == "app-server-stdio"
        && result.get("deleted").is_none()
        && result.get("status").is_none()
        && result["outcomeUnknown"] != true
    {
        if let Some(id) = result["threadId"].as_str() {
            let payload = json!({"threadId":id});
            if validate_delete_thread_payload(&payload).is_ok()
                && receipt.payload_hash == hash(&json!(["deleteThread", payload]).to_string())
            {
                result["deleted"] = json!(true);
                result["status"] = json!("local_deleted");
            }
        }
    }
    json!({"requestId":receipt.request_id,"state":receipt.state,"result":result})
}

pub struct HarnessRuntime {
    shutdown: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl HarnessRuntime {
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = self.task.await;
    }
}

fn shared_controller() -> anyhow::Result<Arc<HarnessController>> {
    let controller = match CONTROLLER.get() {
        Some(controller) => controller.clone(),
        None => {
            let controller = HarnessController::new(crate::paths::default_app_state_dir())
                .map_err(|error| {
                    if let Ok(mut message) = STARTUP_ERROR
                        .get_or_init(|| std::sync::Mutex::new(None))
                        .lock()
                    {
                        *message = Some(error.to_string());
                    }
                    error
                })?;
            let _ = CONTROLLER.set(controller);
            CONTROLLER.get().unwrap().clone()
        }
    };
    Ok(controller)
}

pub fn start_monitor() -> anyhow::Result<HarnessRuntime> {
    let controller = shared_controller()?;
    let (shutdown, mut receiver) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        loop {
            if !controller.closing.load(Ordering::Acquire)
                && let Ok(settings) = SettingsStore::default().load()
            {
                controller.reconcile(settings.remote_access).await;
            }
            controller.scheduled_tasks.tick(&controller).await;
            tokio::select! {
                _ = &mut receiver => break,
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
        // 关停判定已在 prepare_controller_shutdown 中完成（含是否保留 Elves 私有 Codex 服务）；
        // 这里只回收尚未走受控关停的情况，已完成关停时 reconcile 直接返回。
        controller.reconcile(RemoteAccessSettings::default()).await;
    });
    Ok(HarnessRuntime { shutdown, task })
}

pub async fn admin_request(action: &str, payload: Value) -> Value {
    let result = match CONTROLLER.get() {
        Some(controller) => controller.admin(action, payload).await,
        None => Err(anyhow::anyhow!(
            STARTUP_ERROR
                .get()
                .and_then(|error| error.lock().ok()?.clone())
                .unwrap_or_else(|| "请重新启动 Elves 以准备本机 Harness".into())
        )),
    };
    result.unwrap_or_else(|error| json!({"status":"failed","message":error.to_string()}))
}

async fn prepare_controller_shutdown(reason: &str) -> Value {
    let Some(controller) = CONTROLLER.get() else {
        return json!({"status":"stopped","cleanup":{"complete":true}});
    };
    let _serial = controller.shutdown_serial.lock().await;
    if controller.shutdown_complete.load(Ordering::Acquire) {
        return json!({"status":"stopped","cleanup":controller.claude.cleanup_report()});
    }

    // 卸载只影响提示文字：任务同样留在私有服务里继续，但卸载后不会再有 Elves 接上它们。
    let uninstall_requested = reason == "uninstall";
    // 先停止派发和重连，再核对 Codex 状态。
    controller.closing.store(true, Ordering::Release);
    controller.harness.enabled.store(false, Ordering::Release);
    // 用户要求（2026-11-21，第二次确认）：托盘退出和安装升级一律放行，不能因 Codex 状态拦住；Codex 在
    // 执行就留在 Elves 私有服务里继续，下次启动自动接上。release_blockers 最多等 RELEASE_DRAIN_WAIT
    // 让在途请求返回，之后每条原因只决定处理方式和提示，分类见 codex::ShutdownPlan；修改口径时同步核对
    // describe_release_blocker、apps/elves 的安装日志输出和产品规范“Elves 安装、启动与 Harness”。
    let mut blockers = controller
        .harness
        .release_blockers(codex::RELEASE_DRAIN_WAIT)
        .await;
    let transport = controller.harness.handoff_status().await;
    // 用户要求（2026-11-21）：没有执行中的会话时退出要连同 Codex 一起关闭。私有服务当前没有可用连接
    // （连接断开，或上次留下、本次尚未连上）时直接只读核对它：确认空闲就停止，读取失败才按状态未知保留。
    let unknown_server = |blocker: &Value| {
        blocker["kind"] == "server_not_connected"
            || blocker["kind"] == "engine_disconnected" && blocker["transport"] == "private_server"
    };
    let runtime_server = controller
        .harness
        .private_server()
        .await
        .filter(crate::codex_server::alive);
    let mut unconnected = None;
    if transport != codex::HandoffStatus::PrivateServer {
        // 断开连接对应的服务进程已经不在时，它里面不可能再有任务，不再按状态未知保留。
        if runtime_server.is_none() {
            blockers.retain(|blocker| {
                !(blocker["kind"] == "engine_disconnected"
                    && blocker["transport"] == "private_server")
            });
        }
        if let Some(server) = runtime_server
            .clone()
            .or_else(crate::codex_server::running_for_settings)
        {
            match controller
                .harness
                .unconnected_server_blockers(&server)
                .await
            {
                Ok(found) => {
                    blockers.retain(|blocker| !unknown_server(blocker));
                    blockers.extend(found);
                }
                Err(error) => {
                    let _ = crate::diagnostic_log::append_diagnostic_log(
                        "harness.shutdown_server_check_failed",
                        json!({"processId":server.process_id,"error":format!("{error:#}")}),
                    );
                    if !blockers.iter().any(unknown_server) {
                        blockers.push(json!({"kind":"server_not_connected"}));
                    }
                }
            }
            unconnected = Some(server);
        }
    }
    let plan = codex::ShutdownPlan::new(blockers, transport.preserves_execution());
    // 诊断日志只记录会话 ID 和状态，不写入会话标题、命令等用户内容；它们只随响应展示给本机用户。
    let redact = |items: &[Value]| -> Vec<Value> {
        items
            .iter()
            .map(|blocker| {
                let mut blocker = blocker.clone();
                if let Some(object) = blocker.as_object_mut() {
                    object.remove("name");
                    object.remove("command");
                }
                blocker
            })
            .collect()
    };
    if plan.keeps_server()
        && let Some(server) = runtime_server.as_ref().or(unconnected.as_ref())
    {
        controller
            .harness
            .hand_over_background_commands(&server.generation)
            .await;
    }
    let _ = crate::diagnostic_log::append_diagnostic_log(
        "harness.shutdown_confirmed",
        json!({
            "reason":if plan.keeps_server() {
                "codex_continues"
            } else if plan.is_empty() {
                "safe_idle"
            } else {
                "codex_released"
            },
            "request":reason,
            "transport":transport.as_str(),
            "continuing":redact(&plan.continuing),
            "waiting":redact(&plan.waiting),
            "interrupted":redact(&plan.interrupted),
            "saved":redact(&plan.saved)
        }),
    );

    controller.stop_desktop().await;
    controller.publish_access_policy(&RemoteAccessSettings::default());
    controller.claude.set_reconnect_enabled(false);
    let settings = controller
        .live
        .lock()
        .await
        .settings
        .clone()
        .unwrap_or_default();
    controller.reconcile(settings).await;
    if !plan.keeps_server() {
        // 私有服务里没有要继续的任务：释放引擎并停止私有服务（reconcile 内的 stop_preparation 还会因
        // 打开中的临时侧边保留引擎，退出时不需要保留）。stdio 兜底时执行中的任务在这里随引擎结束。
        controller.harness.release().await;
        // 上次留下、本次没连上的服务不在引擎连接里，release 停不到它，核对确认空闲后在这里单独停止。
        if let Some(server) = unconnected.filter(crate::codex_server::alive)
            && let Err(error) = crate::codex_server::stop(&server)
        {
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "remote_access.codex_server.stop_failed",
                json!({"processId":server.process_id,"error":format!("{error:#}")}),
            );
        }
    }
    controller.shutdown_complete.store(true, Ordering::Release);
    let mut response = json!({"status":"stopped","cleanup":controller.claude.cleanup_report()});
    if plan.keeps_server() {
        response["codex"] = json!("continuing");
    }
    for (key, items) in [
        ("continuing", &plan.continuing),
        ("waiting", &plan.waiting),
        ("interrupted", &plan.interrupted),
        ("saved", &plan.saved),
    ] {
        if !items.is_empty() {
            response[key] = json!(items);
        }
    }
    if let Some(message) = plan.message(uninstall_requested) {
        response["message"] = json!(message);
    }
    response
}

pub async fn service_admin_request(action: &str, payload: Value) -> Value {
    service::request_or_start(action, payload)
        .await
        .unwrap_or_else(|error| json!({"status":"failed","message":format!("{error:#}")}))
}

pub async fn local_settings_command(action: &str, payload: Value) -> anyhow::Result<Value> {
    let action = action.to_string();
    tokio::task::spawn_blocking(move || {
        let directory = crate::paths::default_app_state_dir();
        match action.as_str() {
            "credentials" => {
                let mut credentials = access_credentials_status(&directory)?;
                let (configured, token) = ngrok_tunnel::local_credentials(&directory);
                credentials["ngrokTokenConfigured"] = json!(configured);
                if let Some(token) = token {
                    credentials["ngrokToken"] = json!(token);
                }
                let frp = frp_tunnel::Saved::load(&directory, false).unwrap_or_default();
                credentials["frpToken"] = json!(frp.token);
                Ok(credentials)
            }
            "setKey" => {
                configure_access_key(&directory, payload["key"].as_str().context("缺少访问密钥")?)?;
                access_credentials_status(&directory)
            }
            "generateKey" => {
                let key = random_token();
                Ok(json!({"status":"ok","key":key}))
            }
            _ => anyhow::bail!("不支持的本机远程设置操作"),
        }
    })
    .await
    .context("无法保存本机远程访问设置")?
}
#[derive(Clone)]
struct WebState {
    desktop: bool,
    controller: Arc<HarnessController>,
    settings: RemoteAccessSettings,
}

#[derive(Clone, Copy, Default)]
struct RequestTrustContext {
    external_public: bool,
    external_https: bool,
}

struct StaticWebAsset {
    content_type: &'static str,
    identity: &'static [u8],
    identity_etag: OnceLock<HeaderValue>,
    gzip: OnceLock<CompressedWebAsset>,
}

struct CompressedWebAsset {
    bytes: Bytes,
    etag: HeaderValue,
}

impl StaticWebAsset {
    fn new(content_type: &'static str, identity: &'static [u8]) -> Self {
        Self {
            content_type,
            identity,
            identity_etag: OnceLock::new(),
            gzip: OnceLock::new(),
        }
    }

    fn identity_etag(&self) -> &HeaderValue {
        self.identity_etag
            .get_or_init(|| strong_etag(self.identity))
    }

    fn compressed(&self) -> &CompressedWebAsset {
        self.gzip.get_or_init(|| {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder
                .write_all(self.identity)
                .expect("in-memory gzip compression should not fail");
            let bytes = encoder
                .finish()
                .expect("in-memory gzip compression should not fail");
            CompressedWebAsset {
                etag: strong_etag(&bytes),
                bytes: Bytes::from(bytes),
            }
        })
    }
}

fn web_assets() -> &'static HashMap<&'static str, StaticWebAsset> {
    WEB_ASSETS.get_or_init(|| {
        elves_assets::web_assets()
            .iter()
            .map(|&(path, content_type, bytes)| (path, StaticWebAsset::new(content_type, bytes)))
            .collect()
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppResourcesManifest {
    schema_version: u32,
    min_shell_version: u32,
    revision: u64,
    version: String,
    files: Vec<AppResourceFile>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppResourceFile {
    path: String,
    content_type: String,
    size: u64,
    sha256: String,
}

fn build_app_resources_manifest(
    assets: &HashMap<&'static str, StaticWebAsset>,
    desktop: bool,
) -> AppResourcesManifest {
    let mut files: Vec<_> = assets
        .iter()
        .filter(|(path, _)| desktop || !desktop_only_web_page(path))
        .map(|(path, asset)| AppResourceFile {
            path: (*path).to_string(),
            content_type: asset.content_type.to_string(),
            size: asset.identity.len() as u64,
            sha256: format!("{:x}", Sha256::digest(asset.identity)),
        })
        .collect();
    files.sort_by(|left, right| left.path.cmp(&right.path));

    let mut version = Sha256::new();
    for file in &files {
        update_manifest_version_field(&mut version, file.path.as_bytes());
        update_manifest_version_field(&mut version, file.content_type.as_bytes());
        version.update(file.size.to_be_bytes());
        update_manifest_version_field(&mut version, file.sha256.as_bytes());
    }

    AppResourcesManifest {
        schema_version: 1,
        min_shell_version: 1,
        revision: web_resource_revision(),
        version: format!("{:x}", version.finalize()),
        files,
    }
}

fn web_resource_revision() -> u64 {
    // 网页资源有改动时递增编号；Android 会拒绝不高于缓存的版本，APK 与服务共用此文件。
    elves_assets::resource_revision()
}

fn update_manifest_version_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn app_resources_json(desktop: bool) -> &'static [u8] {
    static DESKTOP: OnceLock<Box<[u8]>> = OnceLock::new();
    static REMOTE: OnceLock<Box<[u8]>> = OnceLock::new();
    let json = if desktop { &DESKTOP } else { &REMOTE };
    json.get_or_init(|| {
        serde_json::to_vec(&build_app_resources_manifest(web_assets(), desktop))
            .expect("app resource manifest should serialize")
            .into_boxed_slice()
    })
}

fn app_resources_asset(desktop: bool) -> &'static StaticWebAsset {
    static DESKTOP: OnceLock<StaticWebAsset> = OnceLock::new();
    static REMOTE: OnceLock<StaticWebAsset> = OnceLock::new();
    let asset = if desktop { &DESKTOP } else { &REMOTE };
    asset.get_or_init(|| {
        StaticWebAsset::new(
            "application/json; charset=utf-8",
            app_resources_json(desktop),
        )
    })
}

// 独立文件窗口只由本机客户端打开；远程资源清单与 PWA、Android 缓存拦截要同时排除此页面。
const PREVIEW_WINDOW_PATH: &str = "/file-preview.html";

fn desktop_only_web_page(path: &str) -> bool {
    path == PREVIEW_WINDOW_PATH
}

fn strong_etag(bytes: &[u8]) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{:x}\"", Sha256::digest(bytes)))
        .expect("SHA-256 ETag should be a valid header value")
}

fn is_static_web_asset_path(path: &str) -> bool {
    // 版本壳资源统一按 no-cache 提供并以 ETag 复验；/side-thread 等别名路由不在此列。
    path == "/app-resources.json" || web_assets().contains_key(path)
}

fn accepts_gzip(headers: &HeaderMap) -> bool {
    let mut gzip_quality = None;
    let mut wildcard_quality = None;
    for value in headers.get_all(header::ACCEPT_ENCODING) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for coding in value.split(',') {
            let mut parts = coding.trim().split(';');
            let name = parts.next().unwrap_or("").trim();
            let mut quality = 1000u16;
            for parameter in parts {
                let Some((key, value)) = parameter.trim().split_once('=') else {
                    continue;
                };
                if key.trim().eq_ignore_ascii_case("q") {
                    quality = value
                        .trim()
                        .parse::<f32>()
                        .ok()
                        .filter(|value| (0.0..=1.0).contains(value))
                        .map_or(0, |value| (value * 1000.0).round() as u16);
                }
            }
            let target = if name.eq_ignore_ascii_case("gzip") {
                &mut gzip_quality
            } else if name == "*" {
                &mut wildcard_quality
            } else {
                continue;
            };
            *target = Some(target.map_or(quality, |current: u16| current.max(quality)));
        }
    }
    gzip_quality
        .or(wildcard_quality)
        .is_some_and(|quality| quality > 0)
}

fn if_none_match_matches(headers: &HeaderMap, etag: &HeaderValue) -> bool {
    let Ok(etag) = etag.to_str() else {
        return false;
    };
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| {
            candidate == "*"
                || candidate.strip_prefix("W/").unwrap_or(candidate)
                    == etag.strip_prefix("W/").unwrap_or(etag)
        })
}

fn apply_static_asset_headers(response: &mut Response, asset: &StaticWebAsset, gzip: bool) {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    headers.insert(
        header::ETAG,
        if gzip {
            asset.compressed().etag.clone()
        } else {
            asset.identity_etag().clone()
        },
    );
    headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    if gzip {
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
}

async fn static_web_asset(State(state): State<WebState>, uri: Uri, headers: HeaderMap) -> Response {
    // 摘要和压缩按资源首次访问生成，不让 Mermaid 等非首屏资源阻塞入口。
    // 首次计算或同资源并发等待均在阻塞线程池中完成，避免占住事件/API 工作线程。
    tokio::task::spawn_blocking(move || {
        let asset = if uri.path() == "/app-resources.json" {
            app_resources_asset(state.desktop)
        } else if let Some(asset) = web_assets().get(if uri.path() == "/side-thread" {
            "/"
        } else {
            uri.path()
        }) {
            asset
        } else {
            return StatusCode::NOT_FOUND.into_response();
        };
        web_asset_response(asset, &headers, true)
    })
    .await
    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn cache_recovery_asset(uri: Uri, headers: HeaderMap) -> Response {
    static HTML: OnceLock<StaticWebAsset> = OnceLock::new();
    static SCRIPT: OnceLock<StaticWebAsset> = OnceLock::new();
    let asset = match uri.path() {
        "/cache-recovery.html" => HTML.get_or_init(|| {
            StaticWebAsset::new(
                "text/html; charset=utf-8",
                elves_assets::cache_recovery_html(),
            )
        }),
        "/cache-recovery.js" => SCRIPT.get_or_init(|| {
            StaticWebAsset::new(
                "application/javascript; charset=utf-8",
                elves_assets::cache_recovery_js(),
            )
        }),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    // 恢复入口不属于任一版本壳，损坏的缓存也不能挡住它。
    web_asset_response(asset, &headers, false)
}

fn web_asset_response(asset: &StaticWebAsset, headers: &HeaderMap, conditional: bool) -> Response {
    let gzip = accepts_gzip(headers);
    let etag = if gzip {
        &asset.compressed().etag
    } else {
        asset.identity_etag()
    };
    if conditional && if_none_match_matches(headers, etag) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        apply_static_asset_headers(&mut response, asset, gzip);
        return response;
    }
    let (body, length) = if gzip {
        let compressed = asset.compressed();
        (Body::from(compressed.bytes.clone()), compressed.bytes.len())
    } else {
        (Body::from(asset.identity), asset.identity.len())
    };
    let mut response = Response::new(body);
    apply_static_asset_headers(&mut response, asset, gzip);
    if !conditional {
        response.headers_mut().remove(header::ETAG);
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string())
            .expect("static asset length should be a valid header value"),
    );
    response
}

pub fn web_router(controller: Arc<HarnessController>, settings: RemoteAccessSettings) -> Router {
    scoped_router(controller, settings, false)
}
fn scoped_router(
    controller: Arc<HarnessController>,
    settings: RemoteAccessSettings,
    desktop: bool,
) -> Router {
    let state = WebState {
        controller,
        settings,
        desktop,
    };
    // 版本壳资源按生成表逐项注册（含 "/"），新增网页模块无需在此登记。
    let static_routes = elves_assets::web_assets()
        .iter()
        .fold(Router::new(), |router, &(path, _, _)| {
            router.route(path, get(static_web_asset))
        });
    let static_routes = if desktop {
        static_routes.route(
            "/api/desktop/input-files",
            // 用户要求只在本机取消文件容量限制：流式附件独立于通用 JSON 操作，远程入口不注册。
            post(input_files::desktop_upload).layer(DefaultBodyLimit::disable()),
        )
        .route("/api/desktop/input-files/{batch}/{index}/image", get(desktop_images::preview))
        .route("/api/desktop/input-files/{batch}/{index}/image/thumbnail", get(desktop_images::thumbnail))
    } else {
        static_routes
    };
    static_routes
        .route("/side-thread", get(static_web_asset))
        .route("/app-resources.json", get(static_web_asset))
        .route("/cache-recovery.html", get(cache_recovery_asset))
        .route("/cache-recovery.js", get(cache_recovery_asset))
        .route("/api/login-status", get(login_status))
        .route("/api/entry-check", get(|| async { StatusCode::NOT_FOUND }))
        .route("/api/connect", post(connect))
        .route("/api/session", get(session))
        .route("/api/channels", get(channels::discover))
        .route("/api/channel-events", get(channels::catch_up))
        .route("/api/logout", post(logout))
        .route(
            "/api/diagnostics",
            post(diagnostics::upload).layer(DefaultBodyLimit::max(diagnostics::MAX_BODY_BYTES)),
        )
        .route("/api/read/{action}", post(read))
        .route(
            "/api/settings/environments",
            post(preferences::environments),
        )
        .route(
            "/api/settings/instructions",
            post(preferences::instructions),
        )
        .route("/api/settings/permissions", post(permissions::settings))
        .route("/api/settings/instance", post(instance_settings::settings))
        .route(
            "/api/settings/conversation-naming",
            get(conversation_naming::status),
        )
        .route(
            "/api/settings/shared",
            get(shared_settings::read).post(shared_settings::write),
        )
        .route(
            "/api/settings/claude-recap",
            post(permissions::claude_recap_settings),
        )
        .route(
            "/api/settings/prompt-optimization",
            post(prompt_optimization::settings),
        )
        .route("/api/workspace/context", post(workspace::context))
        .route("/api/workspace/changes", post(workspace::changes))
        .route("/api/workspace/change", post(workspace::change))
        .route("/api/workspace/files", post(workspace::files))
        .route("/api/workspace/file", post(workspace::file))
        .route("/api/workspace/open-browser", post(workspace::open_browser))
        .route("/api/workspace/open-in-targets", post(open_in::targets))
        .route("/api/workspace/open-in", post(open_in::open))
        .route(
            "/api/workspace/view/{thread_id}/{*path}",
            get(workspace::view),
        )
        .route("/api/browser/tunnel", get(browser::tunnel))
        .route(
            "/api/action/{action}",
            post(action).layer(DefaultBodyLimit::max(MAX_ACTION_BODY_BYTES)),
        )
        .route("/api/receipts/{id}", get(receipt))
        .route("/api/images/{id}", get(image))
        .route("/api/images/{id}/thumbnail", get(image_thumbnail))
        .route("/api/videos/{id}", get(video::content))
        .route("/api/videos/{id}/poster", get(video::poster))
        .route("/api/events", get(events))
        .route("/api/events/ws", get(events_ws))
        .layer(DefaultBodyLimit::max(256 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            access_boundary,
        ))
        .with_state(state)
}

#[derive(Debug, Clone)]
struct ApiError {
    code: u16,
    message: String,
    outcome_unknown: bool,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

fn authorization_error(error: anyhow::Error) -> ApiError {
    error
        .downcast_ref::<ApiError>()
        .cloned()
        .unwrap_or_else(|| ApiError::new(503, "无法确认当前访问权限，请重新连接"))
}

impl ApiError {
    fn new(code: u16, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            outcome_unknown: false,
        }
    }

    fn after_admission(mut self) -> Self {
        self.outcome_unknown = true;
        self.message =
            "访问授权已失效，无法确认原操作结果。请勿重复发送，重新连接后请查看会话确认。".into();
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut payload = json!({"ok":false,"message":self.message});
        if self.outcome_unknown {
            payload["outcomeUnknown"] = json!(true);
        }
        (
            StatusCode::from_u16(self.code).unwrap_or(StatusCode::BAD_REQUEST),
            Json(payload),
        )
            .into_response()
    }
}

async fn access_boundary(state: State<WebState>, mut request: Request, next: Next) -> Response {
    // 用户要求将手机恢复日志与电脑接收记录对应；仅观测带合法 diag 的连接路由，鉴权和返回保持原样。
    let mut diagnostic = ConnectionRequestDiagnostic::start(request.method(), request.uri());
    if let Some(diagnostic) = &diagnostic {
        request.extensions_mut().insert(diagnostic.context.clone());
    }
    let response = access_boundary_response(state, request, next).await;
    if let Some(diagnostic) = &mut diagnostic {
        // 响应头已生成不代表手机已经收全；诊断不得把这一步写成客户端成功。
        diagnostic.response_headers_ready(response.status());
    }
    response
}

#[derive(Clone)]
struct ConnectionDiagnosticContext {
    request_id: String,
    path: &'static str,
}

impl ConnectionDiagnosticContext {
    fn from_request(method: &axum::http::Method, uri: &Uri) -> Option<Self> {
        if method != axum::http::Method::GET {
            return None;
        }
        let path = match uri.path() {
            "/api/session" => "/api/session",
            "/api/channels" => "/api/channels",
            "/api/events/ws" => "/api/events/ws",
            _ => return None,
        };
        let mut values = uri
            .query()?
            .split('&')
            .map(|item| item.split_once('=').unwrap_or((item, "")))
            .filter_map(|(name, value)| (name == "diag").then_some(value));
        let request_id = values.next()?;
        if values.next().is_some()
            || !(1..=80).contains(&request_id.len())
            || !request_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return None;
        }
        Some(Self {
            request_id: request_id.to_owned(),
            path,
        })
    }

    fn record(
        &self,
        phase: &'static str,
        started: Instant,
        status: Option<u16>,
        close: Option<(&'static str, Option<u16>)>,
    ) {
        let mut detail = json!({
            "requestId": self.request_id,
            "path": self.path,
            "method": "GET",
            "phase": phase,
            "status": status,
            "durationMs": started.elapsed().as_millis() as u64
        });
        if let Some((reason, code)) = close {
            detail["closeReason"] = json!(reason);
            detail["closeCode"] = json!(code);
        }
        let _ = crate::diagnostic_log::append_diagnostic_log(
            "remote_access.connection_request",
            detail,
        );
    }
}

struct ConnectionRequestDiagnostic {
    context: ConnectionDiagnosticContext,
    started: Instant,
    response_ready: bool,
}

impl ConnectionRequestDiagnostic {
    fn start(method: &axum::http::Method, uri: &Uri) -> Option<Self> {
        let context = ConnectionDiagnosticContext::from_request(method, uri)?;
        let diagnostic = Self {
            context,
            started: Instant::now(),
            response_ready: false,
        };
        diagnostic
            .context
            .record("received", diagnostic.started, None, None);
        Some(diagnostic)
    }

    fn response_headers_ready(&mut self, status: StatusCode) {
        self.response_ready = true;
        self.context.record(
            "response_headers_ready",
            self.started,
            Some(status.as_u16()),
            None,
        );
    }
}

impl Drop for ConnectionRequestDiagnostic {
    fn drop(&mut self) {
        if !self.response_ready {
            self.context
                .record("request_cancelled", self.started, None, None);
        }
    }
}

struct WebSocketConnectionDiagnostic {
    context: ConnectionDiagnosticContext,
    started: Instant,
    close_reason: &'static str,
    close_code: Option<u16>,
}

impl WebSocketConnectionDiagnostic {
    fn start(context: ConnectionDiagnosticContext) -> Self {
        let diagnostic = Self {
            context,
            started: Instant::now(),
            close_reason: "task_cancelled",
            close_code: None,
        };
        diagnostic
            .context
            .record("websocket_opened", diagnostic.started, Some(101), None);
        diagnostic
    }
}

impl Drop for WebSocketConnectionDiagnostic {
    fn drop(&mut self) {
        self.context.record(
            "websocket_handler_ended",
            self.started,
            Some(101),
            Some((self.close_reason, self.close_code)),
        );
    }
}

async fn access_boundary_response(
    State(state): State<WebState>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let desktop_only_message = if path == PREVIEW_WINDOW_PATH {
        Some("独立预览仅可在 Elves 本机客户端使用")
    } else if path == "/api/desktop/input-files" || path.starts_with("/api/desktop/input-files/") {
        Some("本机附件暂存仅可在 Elves 独立客户端使用")
    } else {
        None
    };
    // 用户要求远程不打开独立预览。按实际服务入口在路由、缓存复验和鉴权前拒绝，不影响普通文件及变更读取。
    if !state.desktop && let Some(message) = desktop_only_message {
        let mut response = ApiError::new(403, message).into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return response;
    }
    let static_web_asset = is_static_web_asset_path(request.uri().path());
    let side_thread_panel = request.uri().path() == "/side-thread";
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    // 用户要求本机工作区独立于公网入口；只能使用自己的回环 Host，不能借公网 Host 或通道凭证进入。
    if state.desktop && host != format!("127.0.0.1:{}", state.settings.port) {
        return ApiError::new(403, "Elves 本机入口只接受本机工作区地址").into_response();
    }
    let external_origin = if state.desktop {
        None
    } else {
        state.controller.public_origin()
    };
    let frp_ingress = request
        .extensions()
        .get::<frp_tunnel::Ingress>()
        .map(|v| v.0.clone());
    let active_host = external_origin
        .as_ref()
        .is_some_and(|origin| origin_matches_host(origin, host));
    let active_frp =
        !state.desktop && state.controller.primary.provider() == primary_route::Provider::Frp;
    let accepted_frp_ingress = state.controller.primary.permits_frp(frp_ingress.as_deref());
    if request.uri().path() == "/api/entry-check"
        || !active_host
        || (active_frp && !accepted_frp_ingress)
    {
        if let Some(response) =
            state
                .controller
                .primary
                .probe_response(host, frp_ingress.as_deref(), &request)
        {
            return response;
        }
    }
    // Only the verified FRP ingress may serve its configured public Host.
    if (frp_ingress.is_some() && !(active_host && accepted_frp_ingress))
        || (active_frp && active_host && !accepted_frp_ingress)
    {
        return ApiError::new(403, "FRP 入口尚未生效或请求未经过已验证的入口").into_response();
    }
    let secondary_origins = if state.desktop {
        Vec::new()
    } else {
        state.controller.channels.origins()
    };
    let mut origins = allowed_origins(&state.settings);
    origins.extend(external_origin.iter().cloned());
    origins.extend(secondary_origins.iter().cloned());
    let matched_origin = origins
        .iter()
        .find(|origin| origin_matches_host(origin, host))
        .cloned();
    let valid_host = matched_origin.is_some();
    let external_public = external_origin
        .as_ref()
        .is_some_and(|origin| origin_matches_host(origin, host))
        || secondary_origins
            .iter()
            .any(|origin| origin_matches_host(origin, host));
    let external_https = external_public
        && matched_origin
            .as_ref()
            .is_some_and(|origin| origin.starts_with("https://"));
    let request_origin = if external_public {
        matched_origin.clone().unwrap_or_default()
    } else {
        matched_origin.unwrap_or_else(|| format!("http://{host}"))
    };
    let origin_header = request.headers().get(header::ORIGIN).cloned();
    let origin = origin_header.as_ref().and_then(|value| value.to_str().ok());
    let cors_origin = origin.map(str::to_owned);
    let path = request.uri().path().to_owned();
    if channels::media_path(request.method(), &path)
        && !request.headers().contains_key(header::AUTHORIZATION)
        && let Some(token) = channels::media_token(request.uri())
    {
        request.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Media {token}")).unwrap(),
        );
    }
    let channel_request = channels::bearer(request.headers()).is_some()
        && channels::request_path(request.method(), &path)
        && (!channels::media_authorization(request.headers())
            || channels::media_path(request.method(), &path))
        && external_public;
    if request.method() == axum::http::Method::OPTIONS {
        let method = request
            .headers()
            .get("access-control-request-method")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok());
        let headers_valid = request
            .headers()
            .get("access-control-request-headers")
            .and_then(|value| value.to_str().ok())
            .is_none_or(|value| {
                value.split(',').all(|name| {
                    matches!(
                        name.trim().to_ascii_lowercase().as_str(),
                        "content-type"
                            | "authorization"
                            | "idempotency-key"
                            | "ngrok-skip-browser-warning"
                            | "skip_zrok_interstitial"
                            | "range"
                            | "if-range"
                            | "if-none-match"
                    )
                })
            });
        if !valid_host
            || !external_public
            || !headers_valid
            || !origin.is_some_and(|value| origins.iter().any(|allowed| allowed == value))
            || !method.is_some_and(|method| channels::request_path(&method, &path))
        {
            return ApiError::new(403, "双通道来源未授权").into_response();
        }
        return (StatusCode::NO_CONTENT, [
            ("access-control-allow-origin", cors_origin.unwrap()),
            ("access-control-allow-methods", "GET, HEAD, POST, OPTIONS".into()),
            ("access-control-allow-headers", "Content-Type, Authorization, Idempotency-Key, ngrok-skip-browser-warning, skip_zrok_interstitial, Range, If-Range, If-None-Match".into()),
            ("access-control-max-age", "300".into()),
            ("vary", "Origin".into()),
            ("cache-control", "no-store".into()),
        ]).into_response();
    }
    if channels::bearer(request.headers()).is_some() && !channel_request {
        return ApiError::new(403, "双通道凭证不支持此操作").into_response();
    }
    let bad_origin = origin_header.as_ref().is_some_and(|value| {
        value.to_str().map_or(true, |origin| {
            !origins.iter().any(|allowed| allowed == origin)
        })
    });
    let missing_origin = request.method() == axum::http::Method::POST && origin_header.is_none();
    let cross_site = request
        .headers()
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        == Some("cross-site");
    let root_document_navigation = matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    ) && request.uri().path() == "/"
        && origin_header.is_none()
        && request
            .headers()
            .get("sec-fetch-mode")
            .and_then(|value| value.to_str().ok())
            == Some("navigate")
        && request
            .headers()
            .get("sec-fetch-dest")
            .and_then(|value| value.to_str().ok())
            == Some("document");
    if !valid_host
        || bad_origin
        || missing_origin
        || (cross_site && !root_document_navigation && !channel_request)
    {
        return ApiError::new(403, "只允许从已配置的访问地址操作").into_response();
    }
    if !channel_request && origin.is_some_and(|origin| origin != request_origin) {
        return ApiError::new(403, "请求来源与访问地址不一致").into_response();
    }
    request.extensions_mut().insert(RequestTrustContext {
        external_public,
        external_https,
    });
    let private_api = (path == "/api" || path.starts_with("/api/"))
        && !matches!(path.as_str(), "/api/connect" | "/api/login-status");
    let private_image = path.starts_with("/api/images/")
        || path.starts_with("/api/videos/") && path.ends_with("/poster");
    // Reject unauthorized uploads before JSON/body extraction and before routing.
    let mut response = if private_api {
        match authorized(&state, request.headers()).await {
            Ok(_) => next.run(request).await,
            Err(error) => error.into_response(),
        }
    } else {
        next.run(request).await
    };
    let revalidated_image = private_image
        && matches!(response.status(), StatusCode::OK | StatusCode::NOT_MODIFIED)
        && response.headers().contains_key(header::ETAG);
    let headers = response.headers_mut();
    headers.insert(
        "x-codex-channel-server",
        HeaderValue::from_str(&state.controller.channels.epoch).unwrap(),
    );
    headers.insert(
        "access-control-expose-headers",
        HeaderValue::from_static("x-codex-channel-server, ETag, Accept-Ranges, Content-Range"),
    );
    if channel_request
        && let Some(origin) = cors_origin.and_then(|origin| HeaderValue::from_str(&origin).ok())
    {
        headers.insert("access-control-allow-origin", origin);
        headers.append(header::VARY, HeaderValue::from_static("Origin"));
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if revalidated_image {
            "private, no-cache"
        } else if static_web_asset {
            "no-cache"
        } else {
            "no-store"
        }),
    );
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    // 用户要求 PC 侧边会话并排编辑：只有专用页面可被同源页面嵌入，首页和 API 仍禁止嵌入，外部站点不能嵌入。
    headers.insert(
        "x-frame-options",
        HeaderValue::from_static(if side_thread_panel {
            "SAMEORIGIN"
        } else {
            "DENY"
        }),
    );
    let frame_ancestors = if side_thread_panel {
        "'self'"
    } else {
        "'none'"
    };
    let policy = format!(
        "{}frame-ancestors {frame_ancestors}; form-action 'self'",
        concat!(
            "default-src 'self'; script-src 'self'; worker-src 'self'; style-src 'self'; ",
            "img-src 'self' data: https://*.ngrok-free.app https://*.ngrok-free.dev https://*.ngrok.app https://*.ngrok.dev https://*.ngrok.io https://*.shares.zrok.io; ",
            "media-src 'self' https://*.ngrok-free.app https://*.ngrok-free.dev https://*.ngrok.app https://*.ngrok.dev https://*.ngrok.io https://*.shares.zrok.io; ",
            "connect-src 'self' https://*.ngrok-free.app wss://*.ngrok-free.app https://*.ngrok-free.dev wss://*.ngrok-free.dev https://*.ngrok.app wss://*.ngrok.app https://*.ngrok.dev wss://*.ngrok.dev https://*.ngrok.io wss://*.ngrok.io https://*.shares.zrok.io wss://*.shares.zrok.io; object-src 'none'; base-uri 'none'; "
        )
    );
    headers.insert(
        "content-security-policy",
        HeaderValue::from_str(&policy).expect("static frame policy must be a valid header"),
    );
    if external_https {
        headers.insert(
            "strict-transport-security",
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    response
}

async fn authorized(state: &WebState, headers: &HeaderMap) -> Result<Device, ApiError> {
    if state.desktop {
        return desktop::authorize(state, headers).await;
    }
    if !state.controller.running.load(Ordering::SeqCst)
        || !state
            .controller
            .access_policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .enabled
    {
        return Err(ApiError::new(503, "远程会话网页已关闭"));
    }
    if !state.controller.accepting_requests() {
        return Err(ApiError::new(503, "远程访问服务已停止"));
    }
    state
        .controller
        .authorize(headers)
        .await
        .ok_or_else(|| ApiError::new(401, "设备未授权或授权已过期"))
}

#[derive(Default)]
struct LoginClient {
    id_hash: Option<String>,
    android_app: bool,
}

async fn connect(
    State(state): State<WebState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Extension(trust): Extension<RequestTrustContext>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Response, ApiError> {
    if !state.controller.accepting_requests() {
        return Err(ApiError::new(503, "服务已关闭"));
    }
    let local_owner = local_proof::consume_local_proof(&state, &headers, trust, peer, &payload)?;
    if state.desktop {
        if !local_owner {
            return Err(ApiError::new(403, "Elves 本机入口只接受 Harness 启动证明"));
        }
        return desktop::connect(state).await;
    }
    let key_value = payload["key"].as_str().unwrap_or("").trim();
    if !(8..=256).contains(&key_value.len())
        || !key_value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(ApiError::new(
            400,
            "请填写 8–256 个非空格 ASCII 字符的访问密钥",
        ));
    }
    let client = LoginClient {
        id_hash: match payload.get("clientId") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) if id.is_empty() => None,
            Some(Value::String(id)) => Some(hash(
                &uuid::Uuid::parse_str(id)
                    .map_err(|_| ApiError::new(400, "客户端标识无效，请刷新页面后重试"))?
                    .to_string(),
            )),
            _ => return Err(ApiError::new(400, "客户端标识格式无效")),
        },
        android_app: payload["clientKind"] == "android-app",
    };
    let key = {
        let mut auth = state.controller.auth.lock().await;
        let (_store, key, locked) =
            state.controller.access_key_snapshot().await.map_err(|_| {
                ApiError::new(503, "访问密钥无法读取，请在本机客户端的远程会话页检查")
            })?;
        state.controller.sync_access_key(&mut auth, key, locked);
        if auth.login_locked {
            return Err(login_locked_error());
        }
        auth.access_key
            .clone()
            .ok_or_else(|| ApiError::new(503, "本机尚未设置访问密钥"))?
    };
    let candidate = key_value.to_string();
    let checked_key = key.clone();
    let permit = state
        .controller
        .key_checks
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::new(429, "连接验证繁忙，请稍后再试"))?;
    // Dropping an HTTP request must not discard an already running verification.
    tokio::spawn(async move {
        let valid = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            checked_key.verify(&candidate)
        })
        .await
        .map_err(|_| ApiError::new(503, "无法验证访问密钥，请稍后再试"))?;
        complete_login(state, key, valid, headers, trust, client).await
    })
    .await
    .map_err(|_| ApiError::new(503, "无法完成访问密钥验证，请稍后再试"))?
}

fn login_locked_error() -> ApiError {
    ApiError::new(
        423,
        "访问密钥连续输错超过 3 次，登录已锁定，请在本机客户端的远程会话页重新生成并保存新密钥",
    )
}

async fn login_status(State(state): State<WebState>) -> Result<Json<Value>, ApiError> {
    let mut auth = state.controller.auth.lock().await;
    let (_store, key, locked) = state
        .controller
        .access_key_snapshot()
        .await
        .map_err(|_| ApiError::new(503, "登录状态无法读取，请在本机客户端的远程会话页检查"))?;
    state.controller.sync_access_key(&mut auth, key, locked);
    Ok(Json(json!({"ok":true,"loginLocked":auth.login_locked})))
}

async fn persist_login_result(
    mut store: AccessKeyStore,
    mut key: AccessKey,
    valid: bool,
) -> anyhow::Result<(AccessKeyStore, AccessKey)> {
    tokio::task::spawn_blocking(move || {
        store.record_login_result(&mut key, valid)?;
        Ok((store, key))
    })
    .await?
}

async fn complete_login(
    state: WebState,
    verified_key: AccessKey,
    valid: bool,
    headers: HeaderMap,
    trust: RequestTrustContext,
    client: LoginClient,
) -> Result<Response, ApiError> {
    // Every controller path takes auth before the file lock. Keep both until the
    // outcome and any device authorization are committed, including key rotation.
    let mut auth = state.controller.auth.lock().await;
    let (store, key, locked) = state
        .controller
        .access_key_snapshot()
        .await
        .map_err(|_| ApiError::new(503, "访问密钥无法读取，请在本机客户端的远程会话页检查"))?;
    state.controller.sync_access_key(&mut auth, key, locked);
    let key = auth
        .access_key
        .clone()
        .filter(|key| key.id == verified_key.id)
        .ok_or_else(|| ApiError::new(401, "访问密钥刚被更换，请使用新密钥"))?;
    if !state.controller.accepting_requests() {
        return Err(ApiError::new(503, "服务已关闭"));
    }
    if auth.login_locked {
        return Err(login_locked_error());
    }
    if !valid {
        let key_id = key.id.clone();
        let (_store, key) = persist_login_result(store, key, false).await.map_err(|_| {
            auth.login_locked = true;
            auth.login_storage_failed_for = Some(key_id);
            ApiError::new(
                503,
                "登录状态保存失败，已拒绝新登录，请在本机客户端的远程会话页重新生成密钥",
            )
        })?;
        auth.login_locked = key.login_locked();
        auth.access_key = Some(key);
        return Err(if auth.login_locked {
            login_locked_error()
        } else {
            ApiError::new(401, "访问密钥不正确")
        });
    }
    let previous_devices = auth.devices.clone();
    auth.devices
        .retain(|device| device.expires_at_ms > now_ms() && device.key_id == key.id);
    let public_origin = if trust.external_public {
        state.controller.public_origin()
    } else {
        None
    };
    let cookie_token = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let (name, value) = cookie.trim().split_once('=')?;
                (name == COOKIE_NAME && !value.is_empty() && value.len() <= 256).then_some(value)
            })
        });
    let cookie_hash = cookie_token.map(hash);
    let cookie_index = cookie_hash.as_ref().and_then(|digest| {
        auth.devices.iter().position(|device| {
            device.public_origin == public_origin && constant_time_eq(&device.token_hash, digest)
        })
    });
    // 用户要求旧客户端凭有效 Cookie 迁移时保留设备 ID、草稿和队列；丢失 Cookie 后仍须验证密钥才可复用有效客户端。
    // 仅新授权检查既有 32 条上限；过期、换密钥和已撤销记录不参与匹配，不能恢复其旧 ID 或凭据。
    let existing = cookie_index.or_else(|| {
        client.id_hash.as_ref().and_then(|digest| {
            auth.devices.iter().position(|device| {
                device.public_origin == public_origin
                    && constant_time_eq(&device.client_id_hash, digest)
            })
        })
    });
    if existing.is_none() && auth.devices.len() >= MAX_DEVICES {
        return Err(ApiError::new(409, "授权设备已达上限，请撤销旧设备"));
    }
    let token = if cookie_index.is_some() {
        cookie_token.unwrap().to_owned()
    } else {
        random_token()
    };
    let (expires_at_ms, max_age_seconds) = if state.settings.device_session_hours == 0 {
        (u64::MAX, 315_360_000u64)
    } else {
        (
            now_ms() + u64::from(state.settings.device_session_hours) * 3_600_000,
            u64::from(state.settings.device_session_hours) * 3600,
        )
    };
    let mut device = existing
        .map(|index| auth.devices[index].clone())
        .unwrap_or_else(|| Device {
            id: uuid::Uuid::new_v4().to_string(),
            name: device_name(&headers, client.android_app).into(),
            token_hash: hash(&token),
            created_at_ms: now_ms(),
            expires_at_ms,
            last_seen_at_ms: now_ms(),
            key_id: key.id.clone(),
            public_origin,
            client_id_hash: String::new(),
        });
    device.token_hash = hash(&token);
    device.expires_at_ms = expires_at_ms;
    device.last_seen_at_ms = now_ms();
    if let Some(digest) = client.id_hash {
        device.client_id_hash = digest;
    }
    let name = device_name(&headers, client.android_app);
    if name != "网页设备" || device.name.is_empty() {
        device.name = name.into();
    }
    if let Some(index) = existing {
        auth.devices[index] = device.clone();
    } else {
        auth.devices.push(device.clone());
    }
    if state.controller.save_devices(&auth).await.is_err() {
        auth.devices = previous_devices;
        return Err(ApiError::new(503, "设备授权保存失败"));
    }
    let key_id = key.id.clone();
    let (_store, key) = match persist_login_result(store, key, true).await {
        Ok(result) => result,
        Err(_) => {
            auth.devices = previous_devices;
            let _ = state.controller.save_devices(&auth).await;
            auth.login_locked = true;
            auth.login_storage_failed_for = Some(key_id);
            return Err(ApiError::new(
                503,
                "登录状态保存失败，已拒绝新登录，请在本机客户端的远程会话页重新生成密钥",
            ));
        }
    };
    auth.access_key = Some(key);
    auth.login_locked = false;
    let secure = trust.external_https;
    let cookie = format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_seconds}{}",
        if secure { "; Secure" } else { "" }
    );
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(json!({"ok":true,"device":device.public()})),
    )
        .into_response())
}

async fn session(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let device = authorized(&state, &headers).await?;
    let policy = if state.desktop {
        AccessPolicy {
            enabled: true,
            read_only: false,
            allow_approvals: true,
        }
    } else {
        *state
            .controller
            .access_policy
            .read()
            .unwrap_or_else(|error| error.into_inner())
    };
    let diagnostics_request = state.controller.diagnostics.pending_for(&device.id).await;
    let service_connected = state.controller.harness_connected.load(Ordering::SeqCst);
    let native_write_available = service_connected;
    let shared_settings = state.controller.shared_settings.snapshot().await?;
    let mut response = json!({"ok":true,"device":device.public(),
        "sharedSettings":shared_settings,
        "serviceConnected":service_connected,
        // 用户要求服务模式自带 rg 并可诊断实际来源；状态仅向已授权会话返回，不修改用户 PATH。
        "toolRuntime":crate::tool_runtime::current().diagnostic_value(),
        "readOnly":policy.read_only,"allowApprovals":policy.allow_approvals && native_write_available,
        "claudeAllowApprovals":policy.allow_approvals,
        "claudeWslDistribution":state.controller.claude.wsl_distribution(),
        "claudeInstanceId":state.controller.claude.config().ok().map(|config| config.instance_id()),
        "diagnosticsRequest":diagnostics_request,
        "capabilities":{"imageInput":native_write_available,"localFileInput":true,"queuedImageRemoval":native_write_available,
        "queuedImageEditing":native_write_available,"queueEditRecovery":native_write_available,
        "queueSendRecovery":native_write_available,"projectlessCreation":native_write_available,
        "promptOptimization":true,"promptOptimizationSettings":true,"messageEditModelSettings":native_write_available,
        "fastMode":native_write_available,
        "sessionPermissions":true,"instancePermissions":true,"codexSessionDefaults":true,"sharedSettings":true,"conversationNaming":true,
        "serviceFastMode":native_write_available,
        "goalComposer":native_write_available,"compactionQueueSend":native_write_available,
        "sendText":native_write_available || service_connected,
        "sideThreads":true,"serviceSideThreads":true,"subagents":true,"serviceSubagents":true,
        // 用户要求仅本机开放宿主机应用启动；远程 PC、手机与 Android 不声明此能力。
        "sessionMetrics":true,"workspaceOpenIn":cfg!(windows) && open_in::available(&state, &device),
        "claudeSessions":true,"claudeProjectTrust":true,"claudeSubscriptionUsage":true}});
    response["capabilities"]["desktopCodexImageInput"] =
        json!(state.desktop && device.key_id == desktop::DEVICE_KEY_ID && native_write_available);
    response["capabilities"]["sideIdleRelease"] = json!(true);
    // 用户要求各端都提供“关闭其他占用”，不再区分设备；仍声明为能力，是为了让内置旧网页包（如 Android 先于服务端升级）
    // 连到不认识该动作的旧后台时不显示按钮。单独赋值：上面的 json! 已接近宏递归上限，再加一项无法编译。
    response["capabilities"]["releaseExternalHolders"] = json!(true);
    response["transport"] = json!(if state.desktop { "desktop" } else { "remote" });
    // 代理设置和会话管理已迁移到独立管理器；Harness 不再声明管理页面或管理 API 能力。
    response["capabilities"]["management"] = json!(false);
    Ok(Json(response))
}

async fn logout(
    State(state): State<WebState>,
    Extension(trust): Extension<RequestTrustContext>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let device = authorized(&state, &headers).await?;
    state
        .controller
        .admin("revoke", json!({"deviceId":device.id}))
        .await
        .map_err(|_| ApiError::new(503, "撤销授权失败"))?;
    Ok((
        [(
            header::SET_COOKIE,
            format!(
                "{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
                if trust.external_https { "; Secure" } else { "" }
            ),
        )],
        Json(json!({"ok":true})),
    )
        .into_response())
}

fn workspace_identity(payload: &Value) -> Result<Value, ApiError> {
    let object = payload
        .as_object()
        .ok_or_else(|| ApiError::new(400, "工作目录请求无效"))?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "threadId"
                | "projectId"
                | "path"
                | "section"
                | "ifRevision"
                | "query"
                | "allRepositories"
                | "repository"
        )
    }) {
        return Err(ApiError::new(400, "只能通过会话或项目标识读取变更"));
    }
    if let Some(revision) = object.get("ifRevision")
        && !revision.as_str().is_some_and(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    {
        return Err(ApiError::new(400, "变更缓存版本无效"));
    }
    fn valid_id(value: &Value) -> Option<&str> {
        value.as_str().filter(|id| {
            !id.is_empty()
                && id.len() <= 120
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
    }
    match (object.get("threadId"), object.get("projectId")) {
        (Some(value), None) => valid_id(value)
            .map(|id| json!({"threadId": id}))
            .ok_or_else(|| ApiError::new(400, "会话 ID 无效")),
        (None, Some(value)) => valid_id(value)
            .map(|id| json!({"projectId": id}))
            .ok_or_else(|| ApiError::new(400, "项目 ID 无效")),
        _ => Err(ApiError::new(400, "请指定一个会话或本机项目")),
    }
}

async fn workspace_context(
    state: &WebState,
    payload: &Value,
    authorization: &HarnessAuthorization,
) -> Result<PathBuf, ApiError> {
    workspace_file_context(state, payload, authorization)
        .await
        .map(|(cwd, _)| cwd)
}

async fn workspace_file_context(
    state: &WebState,
    payload: &Value,
    authorization: &HarnessAuthorization,
) -> Result<(PathBuf, Value), ApiError> {
    let identity = workspace_identity(payload)?;
    let guidance = HARNESS_READ_GUIDANCE;
    let result = state
        .controller
        .call_backend("workspaceContext", identity.clone(), authorization)
        .await
        .map_err(|error| {
            error
                .downcast_ref::<ApiError>()
                .cloned()
                .unwrap_or_else(|| ApiError::new(503, guidance))
        })?;
    let result = native_result(result)?;
    if result["hostId"] != "local"
        || identity
            .as_object()
            .unwrap()
            .iter()
            .any(|(key, value)| result.get(key) != Some(value))
    {
        return Err(ApiError::new(409, "会话工作目录身份已变化，请重新打开会话"));
    }
    let cwd = result["cwd"]
        .as_str()
        .filter(|cwd| !cwd.is_empty() && cwd.len() <= 32_768 && !cwd.contains('\0'))
        .ok_or_else(|| ApiError::new(404, "当前会话没有可读取的工作目录"))?;
    // 用户要求 WSL Claude 文件可查看；只将原生会话确认的 Linux 目录映射到其自身发行版，不能放开任意 UNC。
    let distribution = result["wslDistribution"]
        .as_str()
        .filter(|_| result["backend"] == "claude");
    let path = if cfg!(windows)
        && let Some(distribution) = distribution
    {
        preferences::wsl_path(distribution, result["linuxCwd"].as_str().unwrap_or(cwd))?
    } else {
        PathBuf::from(cwd)
    };
    if !path.is_absolute() {
        return Err(ApiError::new(400, "会话工作目录必须是本机绝对路径"));
    }
    #[cfg(windows)]
    if preferences::wsl_distribution(&path).is_none()
        && !matches!(
            path.components().next(),
            Some(Component::Prefix(prefix))
                if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
        )
    {
        return Err(ApiError::new(400, "仅支持本机磁盘上的项目变更"));
    }
    let path = tokio::task::spawn_blocking(move || {
        let path = std::fs::canonicalize(path)
            .map_err(|_| ApiError::new(404, "会话工作目录不存在或无法读取"))?;
        if !path.is_dir()
            || !(canonical_path_is_local(&path) || preferences::wsl_distribution(&path).is_some())
        {
            return Err(ApiError::new(400, "仅支持本机磁盘上的项目目录"));
        }
        Ok(path)
    })
    .await
    .map_err(|_| ApiError::new(503, "读取会话工作目录失败"))??;
    authorization.check().await.map_err(authorization_error)?;
    Ok((path, result))
}

async fn read(
    State(state): State<WebState>,
    Path(action): Path<String>,
    headers: HeaderMap,
    Json(mut payload): Json<Value>,
) -> Result<Response, ApiError> {
    let device = authorized(&state, &headers).await?;
    if !matches!(
        action.as_str(),
        "sidebar"
            | "history"
            | "thread"
            | "models"
            | "newSessionDefaults"
            | "fast"
            | "claudeUsage"
            | "sessionMetrics"
            | "projectTrust"
            | "skills"
            | "queue"
            | "queueEdit"
            | "sendState"
            | "promptOptimization"
            | "notifications"
            | "relatedThreads"
            | "sidePresence"
            | "harnessDiagnostics"
            | "scheduledTasks"
    ) {
        return Err(ApiError::new(404, "不支持的读取操作"));
    }
    if action == "scheduledTasks" && !state.desktop {
        return Err(ApiError::new(403, "定时任务仅可在 Elves 本机工作区使用"));
    }
    if action == "harnessDiagnostics" && !state.desktop {
        return Err(ApiError::new(403, "Harness 诊断仅可在 Elves 独立客户端使用"));
    }
    if action == "harnessDiagnostics"
        && payload.get("threadId").is_some()
        && notification_thread_id(&payload["threadId"]).is_none()
    {
        return Err(ApiError::new(400, "诊断目标会话 ID 无效"));
    }
    if action == "sessionMetrics" && notification_thread_id(&payload["threadId"]).is_none() {
        return Err(ApiError::new(400, "会话统计标识无效"));
    }
    if matches!(action.as_str(), "queue" | "queueEdit" | "sendState") {
        validate_queued_action_payload(&action, &payload)?;
    }
    if action == "notifications" {
        validate_notifications_payload(&payload)?;
    }
    if action == "relatedThreads" {
        validate_related_threads_payload(&action, &payload)?;
    }
    if action == "fast" {
        validate_fast_payload(&action, &payload)?;
    }
    if action == "promptOptimization" {
        payload = json!({"requestId": prompt_optimization_request_id(&device.id, payload["requestId"].as_str().unwrap_or(""))?});
    }
    let guidance = HARNESS_READ_GUIDANCE;
    let authorization = state.controller.authorization(device, None);
    if action == "scheduledTasks" {
        let value = state.controller.scheduled_tasks.snapshot().await?;
        let gzip = accepts_gzip(&headers);
        let response = tokio::task::spawn_blocking(move || read_json_response(value, gzip))
            .await
            .map_err(|_| ApiError::new(503, "定时任务数据处理暂时失败，请重试读取"))??;
        authorization.check().await.map_err(authorization_error)?;
        return Ok(response);
    }
    let value = state
        .controller
        .call_authorized(&action, payload.clone(), &authorization)
        .await
        .map_err(|error| {
            if let Some(error) = error.downcast_ref::<ApiError>() {
                return error.clone();
            }
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "remote_access.native_read_failed",
                json!({
                    "action": action,
                    "error": if action == "notifications" {
                        "通知元数据读取失败".to_owned()
                    } else {
                        format!("{error:#}")
                    }
                }),
            );
            ApiError::new(503, guidance)
        })?;
    let mut value = native_result(value).map_err(|error| {
        let error_message = error.message.to_ascii_lowercase();
        if error.code == 410
            || error_message.contains("thread not loaded:")
            || error_message.contains("unknown thread:")
            || error_message.contains("no rollout found for thread id ")
        {
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "remote_access.side_thread_expiration_detected",
                json!({
                    "action": action,
                    "threadId": payload.get("threadId").and_then(Value::as_str),
                    "code": error.code,
                    "message": error.message.clone()
                }),
            );
        }
        if action == "notifications" {
            ApiError::new(503, "通知元数据暂时不可用，请重试连接")
        } else {
            error
        }
    })?;
    if action == "notifications" {
        validate_notifications_response(&value, &payload)?;
    }
    if action == "history" {
        state.controller.attach_history_images(&mut value);
    }
    if matches!(
        action.as_str(),
        "thread" | "queue" | "queueEdit" | "sendState"
    ) {
        state.controller.attach_queue_images(&mut value);
    }
    let gzip = accepts_gzip(&headers);
    let response = tokio::task::spawn_blocking(move || read_json_response(value, gzip))
        .await
        .map_err(|_| ApiError::new(503, "会话数据处理暂时失败，请重试读取"))??;
    authorization.check().await.map_err(authorization_error)?;
    Ok(response)
}

const MAX_NOTIFICATION_SEQUENCE: u64 = 9_007_199_254_740_991;

fn notification_epoch(value: &Value) -> Option<&str> {
    value.as_str().filter(|epoch| {
        !epoch.is_empty()
            && epoch.len() <= 120
            && epoch
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    })
}

fn notification_sequence(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .filter(|sequence| *sequence <= MAX_NOTIFICATION_SEQUENCE)
}

fn notification_thread_id(value: &Value) -> Option<&str> {
    value.as_str().filter(|id| {
        !id.is_empty()
            && id.len() <= 120
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    })
}

fn validate_related_threads_payload(action: &str, payload: &Value) -> Result<(), ApiError> {
    let object = payload
        .as_object()
        .ok_or_else(|| ApiError::new(400, "关联会话参数无效"))?;
    let close = action == "closeSideThread";
    let create = action == "createSideThread";
    if close && object.len() != 2
        || object.keys().any(|key| {
            key != "threadId"
                && !(close && key == "parentThreadId"
                    || create && matches!(key.as_str(), "model" | "reasoningEffort"))
        })
        || notification_thread_id(&payload["threadId"]).is_none()
        || close
            && (notification_thread_id(&payload["parentThreadId"]).is_none()
                || payload["threadId"] == payload["parentThreadId"])
    {
        return Err(ApiError::new(400, "关联会话标识无效"));
    }
    // 用户要求继承模型与深度；创建入口只新增这两项选择，权限仍从父会话读取。
    // 模型名限制为 256 个 UTF-8 字节，避免误把大段文本当作模型 ID；与 app.js 的创建请求字段同步核对。
    if create
        && (object.get("model").is_some_and(|model| {
            model.as_str().is_none_or(|value| {
                value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control)
            })
        }) || object.get("reasoningEffort").is_some_and(|effort| {
            !effort.is_null()
                && !matches!(
                    effort.as_str(),
                    Some("none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max")
                )
        }))
    {
        return Err(ApiError::new(400, "侧边会话的模型或思考深度选择无效"));
    }
    Ok(())
}

fn validate_notifications_payload(payload: &Value) -> Result<(), ApiError> {
    let valid = payload.as_object().is_some_and(|object| {
        object.is_empty()
            || object.len() == 2
                && notification_epoch(&payload["epoch"]).is_some()
                && notification_sequence(&payload["cursor"]).is_some()
    });
    if !valid {
        return Err(ApiError::new(400, "通知游标无效"));
    }
    Ok(())
}

fn valid_notification_event(event: &Value, epoch: Option<&str>, cursor: u64) -> bool {
    let Some(object) = event.as_object() else {
        return false;
    };
    let Some(event_epoch) = notification_epoch(&event["epoch"]) else {
        return false;
    };
    let Some(sequence) = notification_sequence(&event["sequence"])
        .filter(|sequence| *sequence > 0 && *sequence <= cursor)
    else {
        return false;
    };
    let identity = format!("{event_epoch}:{sequence}");
    if object.len() != 9
        || epoch.is_some_and(|epoch| epoch != event_epoch)
        || event["id"].as_str() != Some(identity.as_str())
        || notification_thread_id(&event["threadId"]).is_none()
        || !matches!(
            event["kind"].as_str(),
            Some("approval" | "question" | "completed" | "failed")
        )
        || !event["title"].as_str().is_some_and(|title| {
            !title.is_empty()
                && title.chars().count() <= 160
                && !title.chars().any(char::is_control)
        })
        || notification_sequence(&event["createdAtMs"]).is_none()
    {
        return false;
    }
    match event["action"].as_str() {
        Some("notify") => event["notificationId"].as_str() == Some(identity.as_str()),
        Some("dismiss") => {
            matches!(event["kind"].as_str(), Some("approval" | "question"))
                && event["notificationId"].as_str().is_some_and(|id| {
                    id.strip_prefix(&format!("{event_epoch}:"))
                        .and_then(|number| number.parse::<u64>().ok())
                        .is_some_and(|number| {
                            number > 0
                                && number < sequence
                                && id == format!("{event_epoch}:{number}")
                        })
                })
        }
        _ => false,
    }
}

fn validate_notifications_response(value: &Value, request: &Value) -> Result<(), ApiError> {
    let invalid = || ApiError::new(503, "通知元数据暂时不可用，请重试连接");
    let object = value.as_object().ok_or_else(invalid)?;
    let epoch = notification_epoch(&value["notificationEpoch"]).ok_or_else(invalid)?;
    let cursor = notification_sequence(&value["notificationCursor"]).ok_or_else(invalid)?;
    let reset = value["reset"].as_bool().ok_or_else(invalid)?;
    let truncated = value["truncated"].as_bool().ok_or_else(invalid)?;
    let pending = value["pending"].as_array().ok_or_else(invalid)?;
    let events = value["events"].as_array().ok_or_else(invalid)?;
    if object.len() != 8
        || value["ok"] != true
        || value["schemaVersion"] != 1
        || events.len() > 256
        || reset && (!events.is_empty() || truncated)
        || request["epoch"].as_str() != Some(epoch) && !reset
        || !reset
            && request["cursor"]
                .as_u64()
                .is_some_and(|previous| previous > cursor)
    {
        return Err(invalid());
    }
    let mut pending_ids = HashSet::new();
    for event in pending {
        if !valid_notification_event(event, Some(epoch), cursor)
            || event["action"] != "notify"
            || !matches!(event["kind"].as_str(), Some("approval" | "question"))
            || !pending_ids.insert(event["notificationId"].as_str().unwrap_or(""))
        {
            return Err(invalid());
        }
    }
    let mut previous = if reset {
        0
    } else {
        request["cursor"].as_u64().unwrap_or(0)
    };
    for event in events {
        if !valid_notification_event(event, Some(epoch), cursor)
            || event["sequence"]
                .as_u64()
                .is_none_or(|sequence| sequence <= previous)
        {
            return Err(invalid());
        }
        previous = event["sequence"].as_u64().unwrap();
    }
    Ok(())
}

fn read_json_response(value: Value, accept_gzip: bool) -> Result<Response, ApiError> {
    let mut bytes =
        serde_json::to_vec(&value).map_err(|_| ApiError::new(500, "无法编码会话数据"))?;
    let mut compressed = false;
    if accept_gzip && bytes.len() >= 1024 {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        if encoder.write_all(&bytes).is_ok() {
            if let Ok(encoded) = encoder.finish() {
                if encoded.len() < bytes.len() {
                    bytes = encoded;
                    compressed = true;
                }
            }
        }
    }
    let length = bytes.len();
    let mut response = Body::from(bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string())
            .expect("serialized JSON length should be a valid header value"),
    );
    if compressed {
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    Ok(response)
}

fn prompt_optimization_request_id(device_id: &str, request_id: &str) -> Result<String, ApiError> {
    if request_id.is_empty()
        || request_id.len() > 100
        || !request_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(ApiError::new(400, "提示词优化请求 ID 无效"));
    }
    Ok(hash(&format!("prompt-optimize:{device_id}:{request_id}")))
}

fn validate_prompt_optimization_payload(payload: &Value) -> Result<(), ApiError> {
    let object = payload
        .as_object()
        .ok_or_else(|| ApiError::new(400, "提示词优化参数无效"))?;
    if object
        .keys()
        .any(|key| !["input", "model", "recentContext"].contains(&key.as_str()))
    {
        return Err(ApiError::new(400, "远程网页不能修改提示词优化设置"));
    }
    if payload["input"]
        .as_str()
        .is_none_or(|input| input.trim().is_empty() || input.chars().count() > 100_000)
        || payload.get("model").is_some_and(|model| {
            model
                .as_str()
                .is_none_or(|model| model.chars().count() > 256)
        })
    {
        return Err(ApiError::new(400, "提示词为空、过长或模型无效"));
    }
    if let Some(context) = payload.get("recentContext") {
        let context = context
            .as_object()
            .ok_or_else(|| ApiError::new(400, "最近一轮上下文无效"))?;
        if context
            .keys()
            .any(|key| !["user", "assistant"].contains(&key.as_str()))
            || ["user", "assistant"]
                .iter()
                .any(|key| context.get(*key).is_none_or(|value| !value.is_string()))
        {
            return Err(ApiError::new(400, "最近一轮上下文无效"));
        }
    }
    Ok(())
}

fn validate_compaction_queue_send_payload(action: &str, payload: &Value) -> Result<(), ApiError> {
    let Some(queue_only) = payload.get("queueOnly") else {
        return Ok(());
    };
    if !queue_only.is_boolean() {
        return Err(ApiError::new(400, "排队发送参数无效"));
    }
    if action != "send" || payload["goalMode"] == true {
        return Err(ApiError::new(400, "排队发送参数仅适用于普通消息"));
    }
    Ok(())
}

fn validate_fast_payload(action: &str, payload: &Value) -> Result<(), ApiError> {
    let object = payload
        .as_object()
        .ok_or_else(|| ApiError::new(400, "Fast 参数无效"))?;
    if matches!(action, "send" | "create") {
        // 用户要求 Fast 只改变当前会话的发送档位；拒绝字符串等隐式真值，不能误开启。
        if object
            .get("fastMode")
            .is_some_and(|enabled| !enabled.is_boolean())
        {
            return Err(ApiError::new(400, "Fast 开关参数无效"));
        }
        return Ok(());
    }
    if object
        .get("model")
        .and_then(Value::as_str)
        .is_none_or(|model| model.trim().is_empty() || model.len() > 256)
    {
        return Err(ApiError::new(400, "Fast 模型参数无效"));
    }
    // 新建页只读取模型能力，不伪造会话 ID；写入必须使用原生会话身份，和 native requiredId 保持一致。
    if object.get("threadId").is_some_and(|id| {
        id.as_str().is_none_or(|id| {
            id.is_empty()
                || id.len() > 120
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
    }) || action == "setFast" && !object.contains_key("threadId")
    {
        return Err(ApiError::new(400, "Fast 会话 ID 无效"));
    }
    if action == "setFast"
        && object
            .get("enabled")
            .is_none_or(|enabled| !enabled.is_boolean())
    {
        return Err(ApiError::new(400, "Fast 开关参数无效"));
    }
    Ok(())
}

fn validate_goal_composer_payload(action: &str, payload: &Value) -> Result<(), ApiError> {
    let object = payload
        .as_object()
        .ok_or_else(|| ApiError::new(400, "目标提交参数无效"))?;
    if object
        .get("goalMode")
        .is_some_and(|mode| !mode.is_boolean())
    {
        return Err(ApiError::new(400, "目标模式参数无效"));
    }
    let goal_mode = object.get("goalMode") == Some(&Value::Bool(true));
    if action == "send" && goal_mode {
        return Err(ApiError::new(400, "目标必须通过目标提交入口发送"));
    }
    if action != "setGoal" && !(action == "create" && goal_mode) {
        return Ok(());
    }
    if !goal_mode {
        return Err(ApiError::new(400, "目标提交需要启用目标模式"));
    }
    let target_field = if action == "setGoal" {
        "threadId"
    } else {
        "projectId"
    };
    if object.keys().any(|key| {
        ![
            target_field,
            "prompt",
            "goalMode",
            "skills",
            "images",
            "files",
            "model",
            "reasoningEffort",
            "backend",
            "instanceId",
        ]
        .contains(&key.as_str())
            && !(action == "create" && key == "fastMode")
            && key != "permissionMode"
            && !(action == "create"
                && payload["backend"] == "claude"
                && matches!(key.as_str(), "trustProject" | "expectedProjectPath"))
    }) {
        return Err(ApiError::new(400, "目标提交包含不支持的字段"));
    }
    let valid_id = |value: &str| {
        !value.is_empty()
            && value.len() <= 120
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    };
    if action == "setGoal" {
        if object
            .get("threadId")
            .and_then(Value::as_str)
            .is_none_or(|id| !valid_id(id))
        {
            return Err(ApiError::new(400, "会话 ID 无效"));
        }
    } else if object.get("projectId").is_some_and(|value| {
        !value.is_null()
            && value
                .as_str()
                .is_none_or(|id| !id.is_empty() && !valid_id(id))
    }) {
        return Err(ApiError::new(400, "项目 ID 无效"));
    }
    validate_input_images(payload)?;
    let has_images = object
        .get("images")
        .and_then(Value::as_array)
        .is_some_and(|images| !images.is_empty())
        || object
            .get("files")
            .and_then(Value::as_array)
            .is_some_and(|files| !files.is_empty());
    if object
        .get("prompt")
        .and_then(Value::as_str)
        .is_none_or(|prompt| prompt.len() > 128 * 1024 || prompt.trim().is_empty() && !has_images)
    {
        return Err(ApiError::new(400, "目标不能为空或超过 128 KiB"));
    }
    if object.get("skills").is_some_and(|skills| {
        skills.as_array().is_none_or(|skills| {
            skills.len() > 16
                || skills.iter().any(|skill| {
                    skill.as_object().is_none_or(|object| {
                        object
                            .keys()
                            .any(|key| !["name", "path"].contains(&key.as_str()))
                            || object
                                .get("name")
                                .and_then(Value::as_str)
                                .is_none_or(|name| name.is_empty() || name.len() > 256)
                            || object
                                .get("path")
                                .and_then(Value::as_str)
                                .is_none_or(|path| path.is_empty() || path.len() > 4096)
                    })
                })
        })
    }) {
        return Err(ApiError::new(400, "Skill 选择无效，最多选择 16 项"));
    }
    for (field, limit) in [("model", 256), ("reasoningEffort", 32)] {
        if object.get(field).is_some_and(|value| {
            value
                .as_str()
                .is_none_or(|text| text.trim().is_empty() || text.len() > limit)
        }) {
            return Err(ApiError::new(400, "目标模型或思考深度无效"));
        }
    }
    Ok(())
}

fn validate_goal_update_payload(payload: &Value) -> Result<(), ApiError> {
    let object = payload
        .as_object()
        .ok_or_else(|| ApiError::new(400, "目标编辑参数无效"))?;
    const ALLOWED_FIELDS: [&str; 6] = [
        "threadId",
        "objective",
        "expectedCreatedAt",
        "expectedObjective",
        "expectedStatus",
        "operation",
    ];
    if object
        .keys()
        .any(|key| !ALLOWED_FIELDS.contains(&key.as_str()))
    {
        return Err(ApiError::new(400, "目标编辑包含不支持的字段"));
    }
    let operation = match object.get("operation") {
        None => "edit",
        Some(Value::String(operation))
            if matches!(operation.as_str(), "edit" | "pause" | "resume" | "delete") =>
        {
            operation.as_str()
        }
        _ => return Err(ApiError::new(400, "目标操作无效")),
    };
    let thread_id = object
        .get("threadId")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::new(400, "会话 ID 无效"))?;
    if thread_id.is_empty()
        || thread_id.len() > 120
        || !thread_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(ApiError::new(400, "会话 ID 无效"));
    }
    let objective = object
        .get("objective")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::new(400, "目标内容不能为空或超过 16 KiB"))?;
    if objective.trim().is_empty() || objective.trim().len() > 16 * 1024 {
        return Err(ApiError::new(400, "目标内容不能为空或超过 16 KiB"));
    }
    let expected_objective = object
        .get("expectedObjective")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::new(400, "目标编辑基线无效"))?;
    if expected_objective.trim().is_empty() || expected_objective.trim().len() > 16 * 1024 {
        return Err(ApiError::new(400, "目标编辑基线无效"));
    }
    if object
        .get("expectedCreatedAt")
        .and_then(Value::as_u64)
        .is_none_or(|value| value > 9_007_199_254_740_991)
    {
        return Err(ApiError::new(400, "目标编辑基线无效"));
    }
    if !matches!(
        object.get("expectedStatus").and_then(Value::as_str),
        Some("active" | "paused" | "blocked" | "budgetLimited" | "usageLimited")
    ) {
        return Err(ApiError::new(400, "目标编辑基线无效"));
    }
    if operation != "edit" && objective != expected_objective {
        return Err(ApiError::new(400, "目标控制操作不能修改目标内容"));
    }
    let expected_status = object.get("expectedStatus").and_then(Value::as_str);
    if operation == "pause" && expected_status != Some("active")
        || operation == "resume" && !matches!(expected_status, Some("paused" | "blocked"))
    {
        return Err(ApiError::new(400, "当前目标状态不支持此操作"));
    }
    Ok(())
}

fn validate_delete_thread_payload(payload: &Value) -> Result<(), ApiError> {
    let object = payload
        .as_object()
        .filter(|object| object.len() == 1 && object.contains_key("threadId"))
        .ok_or_else(|| ApiError::new(400, "永久删除参数只允许包含 threadId"))?;
    let thread_id = object
        .get("threadId")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::new(400, "永久删除需要已创建完成的本机会话 ID"))?;
    if !claude::Claude::owns_id(thread_id)
        && (thread_id.len() != 36 || uuid::Uuid::parse_str(thread_id).is_err())
    {
        return Err(ApiError::new(400, "永久删除需要已创建完成的本机会话 ID"));
    }
    Ok(())
}

fn validate_input_images(payload: &Value) -> Result<(), ApiError> {
    let Some(images) = payload.get("images") else {
        return Ok(());
    };
    let images = images
        .as_array()
        .ok_or_else(|| ApiError::new(400, "图片附件参数无效"))?;
    if images.len() > MAX_INPUT_IMAGE_COUNT {
        return Err(ApiError::new(
            400,
            format!("每条消息最多添加 {MAX_INPUT_IMAGE_COUNT} 张图片"),
        ));
    }
    let mut total = 0usize;
    for image in images {
        let object = image
            .as_object()
            .ok_or_else(|| ApiError::new(400, "图片附件参数无效"))?;
        if object.contains_key("staged") {
            desktop_images::validate_ref(image)?;
            continue;
        }
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "name" | "mime" | "dataUrl" | "size"))
        {
            return Err(ApiError::new(400, "图片附件包含不支持的字段"));
        }
        let name = image["name"]
            .as_str()
            .ok_or_else(|| ApiError::new(400, "图片名称无效"))?;
        if name.is_empty()
            || name.len() > 255
            || name
                .chars()
                .any(|character| character.is_control() || character == '\u{7f}')
        {
            return Err(ApiError::new(400, "图片名称无效"));
        }
        let mime = image["mime"]
            .as_str()
            .ok_or_else(|| ApiError::new(400, "图片类型无效"))?;
        let source = image["dataUrl"]
            .as_str()
            .ok_or_else(|| ApiError::new(400, "图片内容无效"))?;
        let loaded = decode_data_image(source, MAX_INPUT_IMAGE_BYTES)?;
        if loaded.mime != mime {
            return Err(ApiError::new(400, "图片类型与实际内容不一致"));
        }
        if image["size"].as_u64() != Some(loaded.bytes.len() as u64) {
            return Err(ApiError::new(400, "图片大小与实际内容不一致"));
        }
        total = total.saturating_add(loaded.bytes.len());
        if total > MAX_INPUT_IMAGES_TOTAL_BYTES {
            return Err(ApiError::new(413, "图片总大小不能超过 12 MiB"));
        }
    }
    Ok(())
}

fn validate_queued_action_payload(action: &str, payload: &Value) -> Result<(), ApiError> {
    let object = payload
        .as_object()
        .ok_or_else(|| ApiError::new(400, "队列消息参数无效"))?;
    let reading = action == "queue";
    let checking = action == "queueEdit";
    let confirming_send = action == "sendState";
    let reordering = action == "reorderQueued";
    let beginning = action == "beginQueuedEdit";
    let editing = action == "editQueued";
    let canceling = action == "cancelQueuedEdit";
    let dismissing = action == "dismissQueuedEdit";
    let session_editing = editing && object.contains_key("editId");
    if editing && !session_editing && object.contains_key("images") {
        return Err(ApiError::new(
            409,
            "旧版队列编辑不支持新增图片，请刷新后重试",
        ));
    }
    let fields: &[&str] = if confirming_send {
        &["threadId", "clientUserMessageId"]
    } else if checking {
        &["threadId", "editId"]
    } else if dismissing {
        &["threadId", "editId", "confirmed"]
    } else if reading {
        &["threadId"]
    } else if reordering {
        &["threadId", "messageIds", "expectedMessageIds"]
    } else if beginning {
        &[
            "threadId",
            "messageId",
            "originalText",
            "clientUserMessageId",
            "createdAt",
            "editId",
            "expectedImageSources",
        ]
    } else if session_editing {
        &[
            "threadId",
            "editId",
            "prompt",
            "retainedImageSources",
            "images",
            "files",
        ]
    } else if editing {
        &[
            "threadId",
            "messageId",
            "prompt",
            "originalText",
            "clientUserMessageId",
            "createdAt",
        ]
    } else if canceling {
        &["threadId", "editId"]
    } else {
        &["threadId", "messageId"]
    };
    if object.keys().any(|key| !fields.contains(&key.as_str())) {
        return Err(ApiError::new(400, "队列消息包含不支持的字段"));
    }
    let id_fields: &[&str] = if confirming_send {
        &["threadId", "clientUserMessageId"]
    } else if checking || dismissing {
        &["threadId", "editId"]
    } else if reading {
        &["threadId"]
    } else if reordering {
        &["threadId"]
    } else if beginning {
        &["threadId", "messageId", "clientUserMessageId", "editId"]
    } else if session_editing || canceling {
        &["threadId", "editId"]
    } else if editing {
        &["threadId", "messageId", "clientUserMessageId"]
    } else {
        &["threadId", "messageId"]
    };
    for field in id_fields {
        let id = object
            .get(*field)
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::new(400, "队列消息身份无效"))?;
        if id.is_empty()
            || id.len() > 120
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ApiError::new(400, "队列消息身份无效"));
        }
    }
    if dismissing && object.get("confirmed").and_then(Value::as_bool) != Some(true) {
        return Err(ApiError::new(400, "请先核对队列和会话，再结束待确认编辑"));
    }
    if reordering {
        let message_ids = object
            .get("messageIds")
            .and_then(Value::as_array)
            .ok_or_else(|| ApiError::new(400, "队列排序参数无效"))?;
        let expected_message_ids = object
            .get("expectedMessageIds")
            .and_then(Value::as_array)
            .ok_or_else(|| ApiError::new(400, "队列排序参数无效"))?;
        if message_ids.is_empty() || message_ids.len() != expected_message_ids.len() {
            return Err(ApiError::new(400, "队列排序必须包含相同的消息"));
        }
        let validate_ids = |ids: &[Value]| -> Result<HashSet<String>, ApiError> {
            let mut seen = HashSet::with_capacity(ids.len());
            for value in ids {
                let id = value
                    .as_str()
                    .ok_or_else(|| ApiError::new(400, "队列消息身份无效"))?;
                if id.is_empty()
                    || id.len() > 120
                    || !id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                    || !seen.insert(id.to_string())
                {
                    return Err(ApiError::new(400, "队列消息身份无效"));
                }
            }
            Ok(seen)
        };
        let message_ids = validate_ids(message_ids)?;
        let expected_message_ids = validate_ids(expected_message_ids)?;
        if message_ids != expected_message_ids {
            return Err(ApiError::new(400, "队列排序必须包含相同的消息"));
        }
    }
    if beginning || editing && !session_editing {
        if object.get("createdAt").and_then(Value::as_u64).is_none()
            || object
                .get("originalText")
                .and_then(Value::as_str)
                .is_none_or(|text| text.len() > 128 * 1024)
        {
            return Err(ApiError::new(400, "队列消息编辑基线无效"));
        }
    }
    for field in ["expectedImageSources", "retainedImageSources"] {
        let Some(sources) = object.get(field) else {
            continue;
        };
        let sources = sources
            .as_array()
            .ok_or_else(|| ApiError::new(400, "队列图片 source 参数无效"))?;
        if sources.len() > MAX_INPUT_IMAGE_COUNT {
            return Err(ApiError::new(400, "队列图片 source 参数无效"));
        }
        let mut seen = HashSet::with_capacity(sources.len());
        for source in sources {
            let source = source
                .as_str()
                .ok_or_else(|| ApiError::new(400, "队列图片 source 参数无效"))?;
            if source.is_empty()
                || source.len() > 32_767
                || source
                    .chars()
                    .any(|character| character.is_control() || character == '\u{7f}')
                || !seen.insert(source)
            {
                return Err(ApiError::new(400, "队列图片 source 参数无效"));
            }
        }
    }
    if editing {
        let prompt = object
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::new(400, "消息不能为空或超过 128 KiB"))?;
        if prompt.len() > 128 * 1024 {
            return Err(ApiError::new(400, "消息不能为空或超过 128 KiB"));
        }
        if session_editing {
            validate_input_images(payload)?;
            let retained_count = object
                .get("retainedImageSources")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let added_count = object
                .get("images")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            if retained_count + added_count > MAX_INPUT_IMAGE_COUNT {
                return Err(ApiError::new(
                    400,
                    format!("每条消息最多保留或新增 {MAX_INPUT_IMAGE_COUNT} 张图片"),
                ));
            }
            if prompt.trim().is_empty()
                && retained_count + added_count == 0
                && !payload["files"]
                    .as_array()
                    .is_some_and(|files| !files.is_empty())
            {
                return Err(ApiError::new(400, "消息不能为空或超过 128 KiB"));
            }
        } else if prompt.trim().is_empty() {
            return Err(ApiError::new(400, "消息不能为空或超过 128 KiB"));
        }
    }
    Ok(())
}

async fn action(
    State(state): State<WebState>,
    Path(action): Path<String>,
    headers: HeaderMap,
    Json(mut payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let device = authorized(&state, &headers).await?;
    if state.settings.read_only {
        return Err(ApiError::new(403, "当前为只读访问"));
    }
    if !matches!(
        action.as_str(),
        "send"
            | "setFast"
            | "setPermissions"
            | "stop"
            | "compact"
            | "optimizePrompt"
            | "cancelPromptOptimize"
            | "markRead"
            | "clearRecentPriority"
            | "removeQueued"
            | "reorderQueued"
            | "beginQueuedEdit"
            | "editQueued"
            | "cancelQueuedEdit"
            | "dismissQueuedEdit"
            | "steerQueued"
            | "editMessage"
            | "updateGoal"
            | "setGoal"
            | "create"
            | "createSideThread"
            | "forkThread"
            | "closeSideThread"
            | "pinProject"
            | "pinThread"
            | "deleteThread"
            | "releaseExternalHolders"
            | "sort"
            | "rename"
            | "approve"
            | "answer"
            | "createScheduledTask"
            | "updateScheduledTask"
            | "deleteScheduledTask"
            | "toggleScheduledTask"
            | "runScheduledTask"
    ) {
        return Err(ApiError::new(404, "不支持的会话操作"));
    }
    if action.ends_with("ScheduledTask") && !state.desktop {
        return Err(ApiError::new(403, "定时任务仅可在 Elves 本机工作区使用"));
    }
    if action == "releaseExternalHolders" {
        // 用户要求“关闭其他占用”在 Elves 本机客户端、网页、PWA、手机和 Android 上一样可用，不做设备特殊处理；
        // 只读访问已在上方统一拒绝。只接受会话标识；要结束哪些进程由服务端重新核对，页面不能指定。
        if payload.as_object().is_none_or(|object| {
            object.len() != 1
                || object
                    .get("threadId")
                    .and_then(notification_thread_id)
                    .is_none()
        }) {
            return Err(ApiError::new(400, "关闭其他占用只接受会话标识"));
        }
    }
    if matches!(action.as_str(), "approve" | "answer") && !state.settings.allow_approvals {
        return Err(ApiError::new(403, "远程审批与询问回答已关闭，请在桌面处理"));
    }
    if matches!(action.as_str(), "createSideThread" | "closeSideThread") {
        validate_related_threads_payload(&action, &payload)?;
    }
    validate_compaction_queue_send_payload(&action, &payload)?;
    if matches!(action.as_str(), "fast" | "setFast" | "send" | "create") {
        validate_fast_payload(&action, &payload)?;
    }
    if matches!(action.as_str(), "send" | "create" | "setGoal") {
        validate_goal_composer_payload(&action, &payload)?;
    }
    if action == "updateGoal" {
        validate_goal_update_payload(&payload)?;
    }
    if action == "deleteThread" {
        validate_delete_thread_payload(&payload)?;
    }
    if matches!(
        action.as_str(),
        "reorderQueued"
            | "beginQueuedEdit"
            | "editQueued"
            | "cancelQueuedEdit"
            | "dismissQueuedEdit"
            | "steerQueued"
    ) {
        validate_queued_action_payload(&action, &payload)?;
    }
    let has_input_images = matches!(action.as_str(), "send" | "create")
        && (payload["images"]
            .as_array()
            .is_some_and(|images| !images.is_empty())
            || payload["files"]
                .as_array()
                .is_some_and(|files| !files.is_empty()));
    if matches!(action.as_str(), "send" | "create" | "editMessage")
        && payload["prompt"].as_str().is_none_or(|text| {
            text.len() > 128 * 1024 || text.trim().is_empty() && !has_input_images
        })
    {
        return Err(ApiError::new(400, "消息不能为空或超过 128 KiB"));
    }
    if matches!(action.as_str(), "send" | "create")
        && payload["goalMode"] != true
        && payload.get("skills").is_some_and(|skills| {
            skills.as_array().is_none_or(|skills| {
                skills.len() > 16
                    || skills.iter().any(|skill| {
                        skill["name"]
                            .as_str()
                            .is_none_or(|name| name.is_empty() || name.len() > 256)
                            || skill["path"]
                                .as_str()
                                .is_none_or(|path| path.is_empty() || path.len() > 4096)
                    })
            })
        })
    {
        return Err(ApiError::new(400, "Skill 选择无效，最多选择 16 项"));
    }
    if matches!(action.as_str(), "send" | "create") && payload["goalMode"] != true {
        validate_input_images(&payload)?;
    }
    if matches!(
        action.as_str(),
        "send" | "create" | "setGoal" | "editQueued"
    ) {
        input_files::validate_for_device(
            &payload,
            state.desktop && device.key_id == desktop::DEVICE_KEY_ID,
        )?;
    } else if payload.get("files").is_some() {
        return Err(ApiError::new(400, "当前操作不接受文件和文件夹附件"));
    }
    let id = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if desktop_images::has_refs(&payload) && (!state.desktop || device.key_id != desktop::DEVICE_KEY_ID
        || !matches!(action.as_str(), "send" | "create" | "setGoal" | "editQueued")) {
        return Err(ApiError::new(403, "本机原图仅支持 Elves 的本机 Codex 会话"));
    }
    if action == "optimizePrompt" {
        validate_prompt_optimization_payload(&payload)?;
        payload["requestId"] = json!(prompt_optimization_request_id(&device.id, &id)?);
    } else if action == "cancelPromptOptimize" {
        payload = json!({"requestId": prompt_optimization_request_id(&device.id, payload["requestId"].as_str().unwrap_or(""))?});
    }
    state
        .controller
        .execute(device, id, action, payload)
        .await
        .map(Json)
}

async fn receipt(
    State(state): State<WebState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let device = authorized(&state, &headers).await?;
    let auth = state.controller.auth.lock().await;
    let receipt = auth
        .receipts
        .get(&format!("{}:{id}", device.id))
        .ok_or_else(|| ApiError::new(404, "未找到此请求回执"))?;
    let receipt = receipt.clone();
    let value = receipt_value(&receipt);
    drop(auth);
    let value = state
        .controller
        .harness
        .recover_delete_receipt(&receipt, value)
        .await;
    state
        .controller
        .authorization(device, None)
        .check()
        .await
        .map_err(authorization_error)?;
    Ok(Json(value))
}

async fn image(
    State(state): State<WebState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    image_response(state, id, headers, false).await
}

async fn image_thumbnail(
    State(state): State<WebState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    image_response(state, id, headers, true).await
}

async fn image_response(
    state: WebState,
    id: String,
    headers: HeaderMap,
    thumbnail: bool,
) -> Result<Response, ApiError> {
    let device = authorized(&state, &headers).await?;
    if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ApiError::new(404, "图片不存在或已过期"));
    }
    let image = state
        .controller
        .registered_image(&id)
        .ok_or_else(|| ApiError::new(404, "图片不存在或已过期"))?;
    if state.desktop && let RegisteredImageSource::Path(path) = &image.source
        && let Some((batch, index)) = input_files::staged_file_identity(&state.controller.state_dir, &device.id, path)
    {
        // 正文和队列继续用原有图片地址，仅本机本人可走暂存原图的流式读取。
        return desktop_images::response(&state, device, batch.to_owned(), index, Some(path.clone()), thumbnail).await;
    }
    let permit = if thumbnail {
        Some(
            state
                .controller
                .image_preview_slots
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ApiError::new(503, "图片预览暂不可用"))?,
        )
    } else {
        None
    };
    let controller = state.controller.clone();
    let image = tokio::task::spawn_blocking(move || {
        // 请求被取消后，阻塞解码仍可能运行；许可必须由实际工作持有到结束。
        let _permit = permit;
        let source = read_registered_image(&image)?;
        if !thumbnail {
            return Ok(source);
        }
        let key: [u8; 32] = Sha256::digest(&source.bytes).into();
        let cached = controller
            .image_previews
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key);
        if let Some(cached) = cached {
            return Ok(cached.unwrap_or(source));
        }
        let preview = image_preview::thumbnail(&source);
        controller
            .image_previews
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(key, preview.clone());
        Ok::<_, ApiError>(preview.unwrap_or(source))
    })
    .await
    .map_err(|_| ApiError::new(503, "无法读取本机图片"))??;
    state
        .controller
        .authorization(device, None)
        .check()
        .await
        .map_err(authorization_error)?;
    let etag = strong_etag(&image.bytes);
    if if_none_match_matches(&headers, &etag) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response.headers_mut().insert(header::ETAG, etag);
        response
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("Cookie"));
        return Ok(response);
    }
    let content_length = HeaderValue::from_str(&image.bytes.len().to_string())
        .map_err(|_| ApiError::new(500, "无法生成图片响应"))?;
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(image.mime)),
            (header::CONTENT_LENGTH, content_length),
            (header::ETAG, etag),
            (header::VARY, HeaderValue::from_static("Cookie")),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("inline"),
            ),
            (
                header::HeaderName::from_static("cross-origin-resource-policy"),
                HeaderValue::from_static("same-origin"),
            ),
        ],
        image.bytes,
    )
        .into_response())
}

async fn events(
    State(state): State<WebState>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<impl IntoResponse, ApiError> {
    let device = authorized(&state, &headers).await?;
    let notifications_only = uri.query().is_some_and(|query| {
        query
            .split('&')
            .any(|parameter| parameter == "notifications=true")
    });
    let receiver = state.controller.events.subscribe();
    let controller = state.controller.clone();
    let id = device.id.clone();
    let stream = stream::unfold(
        (
            receiver,
            controller,
            id,
            false,
            tokio::time::Instant::now() + Duration::from_secs(15),
        ),
        move |(mut receiver, controller, id, done, mut heartbeat)| async move {
            if done {
                return None;
            }
            let active = controller.device_active(&id).await;
            if !active {
                return Some((
                    Ok::<_, Infallible>(
                        Event::default().data(r#"{"method":"remote/unauthorized"}"#),
                    ),
                    (receiver, controller, id, true, heartbeat),
                ));
            }
            if !controller.accepting_requests() {
                return Some((
                    Ok(Event::default().data(r#"{"method":"remote/disconnected"}"#)),
                    (receiver, controller, id, true, heartbeat),
                ));
            }
            if !notifications_only {
                heartbeat = tokio::time::Instant::now() + Duration::from_secs(15);
            }
            let mut event = loop {
                let received = if tokio::time::Instant::now() >= heartbeat {
                    None
                } else {
                    tokio::time::timeout_at(heartbeat, receiver.recv())
                        .await
                        .ok()
                };
                let event = match received {
                    Some(Ok(event)) => event,
                    Some(Err(broadcast::error::RecvError::Lagged(_))) => {
                        json!({"method":"remote/resync"})
                    }
                    Some(Err(_)) => return None,
                    None => {
                        heartbeat = tokio::time::Instant::now() + Duration::from_secs(15);
                        json!({"method":"remote/heartbeat"})
                    }
                };
                let event = controller.event_for_device(&id, event).await;
                if notifications_only {
                    let Some(event) = notification_stream_event(&event) else {
                        continue;
                    };
                    break event;
                }
                heartbeat = tokio::time::Instant::now() + Duration::from_secs(15);
                break event;
            };
            let authorized = controller.device_active(&id).await;
            let done = !authorized || !controller.accepting_requests();
            if !authorized {
                event = json!({"method":"remote/unauthorized"});
            } else if done {
                event = json!({"method":"remote/disconnected"});
            }
            Some((
                Ok(Event::default().data(serde_json::to_string(&event).unwrap())),
                (receiver, controller, id, done, heartbeat),
            ))
        },
    );
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

fn notification_stream_event(event: &Value) -> Option<Value> {
    let method = event["method"].as_str()?;
    match method {
        "remote/notification" => {
            if valid_notification_event(&event["params"], None, MAX_NOTIFICATION_SEQUENCE) {
                Some(json!({"method":method,"params":event["params"]}))
            } else {
                Some(json!({"method":"remote/resync"}))
            }
        }
        "remote/heartbeat"
        | "remote/resync"
        | "remote/reconnected"
        | "remote/disconnected"
        | "remote/authorizationChanged"
        | "remote/unauthorized" => Some(json!({"method":method})),
        "remote/settingsChanged" => Some(json!({"method":method})),
        "thread/deleted" => notification_thread_id(&event["params"]["threadId"])
            .map(|id| json!({"method":method,"params":{"threadId":id}})),
        _ => None,
    }
}

async fn events_ws(
    ws: WebSocketUpgrade,
    State(state): State<WebState>,
    diagnostic: Option<Extension<ConnectionDiagnosticContext>>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Response, ApiError> {
    let device = authorized(&state, &headers).await?;
    let controller = state.controller.clone();
    let id = device.id;
    Ok(ws
        .protocols(["codex-elves-channel-v1"])
        .on_upgrade(move |socket| {
            stream_websocket_events(
                socket,
                controller,
                id,
                uri,
                diagnostic.map(|Extension(context)| context),
            )
        })
        .into_response())
}

async fn stream_websocket_events(
    mut socket: WebSocket,
    controller: Arc<HarnessController>,
    id: String,
    uri: Uri,
    diagnostic: Option<ConnectionDiagnosticContext>,
) {
    let mut diagnostic = diagnostic.map(WebSocketConnectionDiagnostic::start);
    let mut close = |reason, code| {
        if let Some(diagnostic) = &mut diagnostic {
            diagnostic.close_reason = reason;
            diagnostic.close_code = code;
        }
    };
    let mut receiver = controller.events.subscribe();
    let ready = controller
        .replay_for_device(&id, channels::replay(&controller, &uri))
        .await;
    if !controller.device_active(&id).await || !controller.accepting_requests() {
        close("inactive_at_upgrade", None);
        let _ = socket.close().await;
        return;
    }
    if socket
        .send(WebSocketMessage::Text(ready.to_string().into()))
        .await
        .is_err()
    {
        close("initial_send_failed", None);
        return;
    }
    let heartbeat = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(heartbeat);
    loop {
        let mut event = if !controller.device_active(&id).await {
            json!({"method":"remote/unauthorized"})
        } else if !controller.accepting_requests() {
            json!({"method":"remote/disconnected"})
        } else {
            tokio::select! {
                incoming = socket.recv() => {
                    match incoming {
                        Some(Ok(WebSocketMessage::Close(frame))) => {
                            close("peer_close", frame.map(|frame| frame.code));
                            let _ = socket.close().await;
                            break;
                        }
                        Some(Ok(WebSocketMessage::Ping(_))) => {
                            if socket.flush().await.is_err() {
                                close("flush_failed", None);
                                break;
                            }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(_)) => {
                            close("receive_failed", None);
                            break;
                        }
                        None => {
                            close("peer_eof", None);
                            break;
                        }
                    }
                    continue;
                }
                received = receiver.recv() => match received {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        json!({"method":"remote/resync"})
                    }
                    Err(_) => {
                        close("event_channel_closed", None);
                        break;
                    }
                },
                _ = &mut heartbeat => {
                    heartbeat.as_mut().reset(
                        tokio::time::Instant::now() + Duration::from_secs(15)
                    );
                    json!({"method":"remote/heartbeat"})
                }
            }
        };
        event = controller.event_for_device(&id, event).await;
        let authorized = controller.device_active(&id).await;
        let done = !authorized || !controller.accepting_requests();
        if !authorized {
            event = json!({"method":"remote/unauthorized"});
        } else if done {
            event = json!({"method":"remote/disconnected"});
        }
        let Ok(text) = serde_json::to_string(&event) else {
            close("serialization_failed", None);
            break;
        };
        if socket
            .send(WebSocketMessage::Text(text.into()))
            .await
            .is_err()
        {
            close("send_failed", None);
            break;
        }
        if done {
            close(
                if authorized {
                    "server_stopped"
                } else {
                    "authorization_inactive"
                },
                None,
            );
            let _ = socket.close().await;
            break;
        }
    }
}

struct RegisteredRemoteImage {
    id: String,
    source: String,
    mime: &'static str,
}

fn register_remote_image(
    registry: &mut ImageRegistry,
    source: &str,
) -> Option<RegisteredRemoteImage> {
    if let Some((path, mime)) = eligible_image_path(source) {
        let id = registry.register(path);
        return Some(RegisteredRemoteImage {
            id,
            source: source.to_string(),
            mime,
        });
    }
    let image = decode_data_image(source, MAX_REMOTE_IMAGE_BYTES as usize).ok()?;
    let id = registry.register_inline(image.bytes, image.mime);
    Some(RegisteredRemoteImage {
        source: format!("inline:{id}"),
        id,
        mime: image.mime,
    })
}

fn canonical_item_image_sources(item: &Value) -> Vec<String> {
    let mut sources = Vec::new();
    match item["type"].as_str() {
        Some("agentMessage") => {
            if let Some(text) = item["text"].as_str() {
                sources.extend(markdown_image_sources(text));
            }
        }
        Some("userMessage") => {
            for content in item["content"].as_array().into_iter().flatten() {
                match content["type"].as_str() {
                    Some("localImage") => {
                        if let Some(path) = content["path"].as_str() {
                            sources.push(path.to_string());
                        }
                    }
                    Some("image") => {
                        if let Some(url) = content["url"].as_str() {
                            sources.push(url.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
        Some("imageView") => {
            if let Some(path) = item["path"].as_str() {
                sources.push(path.to_string());
            }
        }
        _ => {
            for image in claude_question_answer_images(item) {
                if let Some(url) = image["url"].as_str() {
                    sources.push(url.to_string());
                }
            }
        }
    }
    let mut seen = HashSet::new();
    sources.retain(|source| seen.insert(source.clone()));
    sources
}

/// 用户要求经远程会话发出的回复在正文中可见：Claude 提问回答里的截图由连接端读成 data URL，
/// 与用户消息图片一样登记为受鉴权的 /api/images 地址，避免每次读取历史都把原图内联发给手机。
fn claude_question_answer_images(item: &Value) -> impl Iterator<Item = &Value> {
    let question = item["type"] == "dynamicToolCall"
        && item["source"] == "claude"
        && item["tool"] == "AskUserQuestion";
    item["questionAnswers"]
        .as_array()
        .filter(|_| question)
        .into_iter()
        .flatten()
        .flat_map(|reply| reply["images"].as_array().into_iter().flatten())
}

fn canonical_queue_image_sources(message: &Value) -> Vec<String> {
    let mut sources = message
        .pointer("/context/imageAttachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|image| image["src"].as_str().map(ToOwned::to_owned))
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();
    sources.retain(|source| seen.insert(source.clone()));
    sources
}

fn replace_item_inline_image_sources(item: &mut Value, replacements: &HashMap<String, String>) {
    if claude_question_answer_images(item).next().is_some() {
        // 提问回答的截图与用户消息相同：data URL 换成 inline:<id>，网页按 remoteImages 取受鉴权地址。
        for reply in item["questionAnswers"].as_array_mut().into_iter().flatten() {
            for image in reply["images"].as_array_mut().into_iter().flatten() {
                if let Some(replacement) =
                    image["url"].as_str().and_then(|url| replacements.get(url))
                {
                    image["url"] = Value::String(replacement.clone());
                }
            }
        }
        return;
    }
    if item["type"] != "userMessage" {
        return;
    }
    for content in item["content"].as_array_mut().into_iter().flatten() {
        if content["type"] != "image" {
            continue;
        }
        let Some(source) = content["url"].as_str() else {
            continue;
        };
        if let Some(replacement) = replacements.get(source) {
            content["url"] = Value::String(replacement.clone());
        }
    }
}

fn replace_queue_inline_image_sources(message: &mut Value, replacements: &HashMap<String, String>) {
    for image in message
        .pointer_mut("/context/imageAttachments")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        let Some(source) = image["src"].as_str() else {
            continue;
        };
        if let Some(replacement) = replacements.get(source) {
            image["src"] = Value::String(replacement.clone());
            image.as_object_mut().map(|object| {
                object.remove("localPath");
            });
        }
    }
}

fn markdown_image_sources(text: &str) -> Vec<String> {
    markdown_attachment_sources(text, false)
}

fn markdown_attachment_sources(text: &str, include_links: bool) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut sources = Vec::new();
    let mut index = 0;
    let mut line_start = true;
    let mut fence: Option<(u8, usize)> = None;
    let mut inline_code_ticks = 0;
    while index < bytes.len() {
        if line_start {
            let mut delimiter = index;
            while delimiter < bytes.len()
                && delimiter - index < 4
                && matches!(bytes[delimiter], b' ' | b'\t')
            {
                delimiter += 1;
            }
            if delimiter < bytes.len() && matches!(bytes[delimiter], b'`' | b'~') {
                let marker = bytes[delimiter];
                let run = marker_run(bytes, delimiter, marker);
                if run >= 3 {
                    match fence {
                        None => fence = Some((marker, run)),
                        Some((open_marker, open_run))
                            if open_marker == marker && run >= open_run =>
                        {
                            fence = None
                        }
                        _ => {}
                    }
                    index = delimiter + run;
                    line_start = false;
                    continue;
                }
            }
        }
        if bytes[index] == b'\n' {
            index += 1;
            line_start = true;
            continue;
        }
        if bytes[index] == b'\r' {
            index += 1;
            continue;
        }
        if fence.is_some() {
            index += 1;
            line_start = false;
            continue;
        }
        line_start = false;
        if bytes[index] == b'`' {
            let run = marker_run(bytes, index, b'`');
            if inline_code_ticks == 0 {
                inline_code_ticks = run;
            } else if inline_code_ticks == run {
                inline_code_ticks = 0;
            }
            index += run;
            continue;
        }
        if inline_code_ticks > 0 {
            index += 1;
            continue;
        }
        if bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
            continue;
        }
        let open = if bytes[index] == b'!' && bytes.get(index + 1) == Some(&b'[') {
            index + 1
        } else if include_links && bytes[index] == b'[' {
            index
        } else {
            index += 1;
            continue;
        };
        let Some(alt_end) = matching_square_bracket(bytes, open) else {
            index = open + 1;
            continue;
        };
        if bytes.get(alt_end + 1) != Some(&b'(') {
            index = alt_end + 1;
            continue;
        }
        let Some((raw_source, end)) = markdown_destination(text, alt_end + 1) else {
            index = alt_end + 2;
            continue;
        };
        let source = unescape_markdown_destination(raw_source);
        if !source.is_empty() {
            sources.push(source);
        }
        index = end;
    }
    sources
}

fn marker_run(bytes: &[u8], start: usize, marker: u8) -> usize {
    bytes[start..]
        .iter()
        .take_while(|byte| **byte == marker)
        .count()
}

fn matching_square_bracket(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut index = open + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'[' => {
                depth += 1;
                index += 1;
            }
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    None
}

fn markdown_destination(text: &str, open: usize) -> Option<(&str, usize)> {
    let bytes = text.as_bytes();
    let mut start = open + 1;
    while start < bytes.len() && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    if bytes.get(start) == Some(&b'<') {
        let mut index = start + 1;
        while index < bytes.len() {
            match bytes[index] {
                b'\\' => index = (index + 2).min(bytes.len()),
                b'>' => {
                    let source = &text[start + 1..index];
                    let end = matching_parenthesis_end(bytes, index + 1)?;
                    return Some((source, end));
                }
                b'\r' | b'\n' => return None,
                _ => index += 1,
            }
        }
        return None;
    }
    let mut depth = 1usize;
    let mut index = start;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'(' => {
                depth += 1;
                index += 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    let source = strip_markdown_title(text[start..index].trim());
                    return Some((source, index + 1));
                }
                index += 1;
            }
            b'\r' | b'\n' => return None,
            _ => index += 1,
        }
    }
    None
}

fn matching_parenthesis_end(bytes: &[u8], mut index: usize) -> Option<usize> {
    let mut quote = None;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'\'' | b'"' if quote == Some(bytes[index]) => {
                quote = None;
                index += 1;
            }
            b'\'' | b'"' if quote.is_none() => {
                quote = Some(bytes[index]);
                index += 1;
            }
            b')' if quote.is_none() => return Some(index + 1),
            b'\r' | b'\n' => return None,
            _ => index += 1,
        }
    }
    None
}

fn strip_markdown_title(value: &str) -> &str {
    for (index, byte) in value.bytes().enumerate().rev() {
        if !byte.is_ascii_whitespace() {
            continue;
        }
        let title = value[index..].trim();
        let quoted = title.len() >= 2
            && matches!(
                (title.as_bytes()[0], title.as_bytes()[title.len() - 1]),
                (b'"', b'"') | (b'\'', b'\'')
            );
        if quoted {
            return value[..index].trim_end();
        }
    }
    value
}

fn unescape_markdown_destination(value: &str) -> String {
    let windows_backslash_path =
        value.as_bytes().get(1) == Some(&b':') && value.as_bytes().get(2) == Some(&b'\\');
    let mut result = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' || windows_backslash_path {
            result.push(character);
            continue;
        }
        let Some(next) = characters.peek().copied() else {
            result.push(character);
            break;
        };
        if next.is_ascii_punctuation() || next.is_ascii_whitespace() {
            result.push(characters.next().unwrap());
        } else {
            result.push(character);
        }
    }
    result
}

fn eligible_image_path(source: &str) -> Option<(PathBuf, &'static str)> {
    let source = source.trim();
    if source.is_empty()
        || source.len() > 32_767
        || source.contains('\0')
        || source.contains("://")
        || source.to_ascii_lowercase().starts_with("file:")
        || source.starts_with(r"\\")
        || source.starts_with("//")
    {
        return None;
    }
    #[cfg(windows)]
    let source = match source.as_bytes() {
        [b'/', drive, b':', b'/' | b'\\', ..] if drive.is_ascii_alphabetic() => &source[1..],
        _ => source,
    };
    let path = PathBuf::from(source);
    if !path.is_absolute() || !input_path_has_local_prefix(&path) {
        return None;
    }
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    };
    Some((path, mime))
}

#[cfg(windows)]
fn input_path_has_local_prefix(path: &FsPath) -> bool {
    matches!(
        path.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))
    )
}

#[cfg(not(windows))]
fn input_path_has_local_prefix(path: &FsPath) -> bool {
    matches!(path.components().next(), Some(Component::RootDir))
}

fn read_registered_image(image: &RegisteredImage) -> Result<LoadedImage, ApiError> {
    let RegisteredImageSource::Path(path) = &image.source else {
        let RegisteredImageSource::Inline { bytes, mime } = &image.source else {
            unreachable!();
        };
        return Ok(LoadedImage {
            bytes: bytes.to_vec(),
            mime,
        });
    };
    let path =
        std::fs::canonicalize(path).map_err(|_| ApiError::new(404, "图片文件不存在或无法读取"))?;
    if !canonical_path_is_local(&path) {
        return Err(ApiError::new(403, "图片不在本机固定磁盘"));
    }
    let metadata =
        std::fs::metadata(&path).map_err(|_| ApiError::new(404, "图片文件不存在或无法读取"))?;
    if !metadata.is_file() {
        return Err(ApiError::new(404, "图片文件不存在或无法读取"));
    }
    if metadata.len() > MAX_REMOTE_IMAGE_BYTES {
        return Err(ApiError::new(413, "图片文件过大"));
    }
    let file =
        std::fs::File::open(&path).map_err(|_| ApiError::new(404, "图片文件不存在或无法读取"))?;
    let mut bytes = Vec::with_capacity(metadata.len().min(MAX_REMOTE_IMAGE_BYTES) as usize);
    file.take(MAX_REMOTE_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ApiError::new(503, "无法读取本机图片"))?;
    if bytes.len() as u64 > MAX_REMOTE_IMAGE_BYTES {
        return Err(ApiError::new(413, "图片文件过大"));
    }
    validate_image_bytes(bytes, None, MAX_REMOTE_IMAGE_BYTES as usize)
}

fn decode_data_image(source: &str, max_bytes: usize) -> Result<LoadedImage, ApiError> {
    let (header, encoded) = source
        .split_once(',')
        .ok_or_else(|| ApiError::new(400, "图片内容无效"))?;
    let declared_mime = match header.to_ascii_lowercase().as_str() {
        "data:image/png;base64" => "image/png",
        "data:image/jpeg;base64" => "image/jpeg",
        "data:image/webp;base64" => "image/webp",
        "data:image/gif;base64" => "image/gif",
        _ => return Err(ApiError::new(415, "仅支持 PNG、JPEG、WebP 和 GIF 图片")),
    };
    if encoded.len() > max_bytes.saturating_mul(4).div_ceil(3).saturating_add(4) {
        return Err(ApiError::new(413, "图片文件过大"));
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| ApiError::new(400, "图片 Base64 内容无效"))?;
    validate_image_bytes(bytes, Some(declared_mime), max_bytes)
}

fn validate_image_bytes(
    bytes: Vec<u8>,
    declared_mime: Option<&str>,
    max_bytes: usize,
) -> Result<LoadedImage, ApiError> {
    if bytes.is_empty() {
        return Err(ApiError::new(400, "图片文件为空"));
    }
    if bytes.len() > max_bytes {
        return Err(ApiError::new(413, "图片文件过大"));
    }
    let format =
        image::guess_format(&bytes).map_err(|_| ApiError::new(415, "文件不是支持的图片格式"))?;
    let mime =
        image_format_mime(format).ok_or_else(|| ApiError::new(415, "文件不是支持的图片格式"))?;
    if declared_mime.is_some_and(|declared| declared != mime) {
        return Err(ApiError::new(400, "图片类型与实际内容不一致"));
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_REMOTE_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_REMOTE_IMAGE_DIMENSION);
    limits.max_alloc = Some(128 * 1024 * 1024);
    let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
    reader.limits(limits);
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| ApiError::new(415, "图片内容损坏或尺寸无效"))?;
    if width == 0
        || height == 0
        || width > MAX_REMOTE_IMAGE_DIMENSION
        || height > MAX_REMOTE_IMAGE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_REMOTE_IMAGE_PIXELS
    {
        return Err(ApiError::new(413, "图片尺寸过大"));
    }
    Ok(LoadedImage { bytes, mime })
}

fn image_format_mime(format: ImageFormat) -> Option<&'static str> {
    match format {
        ImageFormat::Png => Some("image/png"),
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::Gif => Some("image/gif"),
        ImageFormat::WebP => Some("image/webp"),
        _ => None,
    }
}

#[cfg(windows)]
fn canonical_path_is_local(path: &FsPath) -> bool {
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows::core::PCWSTR;

    let drive = match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => return false,
        },
        _ => return false,
    };
    let root = [u16::from(drive), u16::from(b':'), u16::from(b'\\'), 0];
    unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) == 3 }
}

#[cfg(not(windows))]
fn canonical_path_is_local(path: &FsPath) -> bool {
    matches!(path.components().next(), Some(Component::RootDir))
}

fn native_result(value: Value) -> Result<Value, ApiError> {
    if value["ok"] == true {
        Ok(value)
    } else {
        Err(ApiError::new(
            value["code"].as_u64().unwrap_or(503) as u16,
            value["message"].as_str().unwrap_or("原生会话操作失败"),
        ))
    }
}

fn device_name(headers: &HeaderMap, android_app: bool) -> &'static str {
    let agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    // 用户要求安卓 App、手机浏览器和电脑浏览器名称可辨识；标记仅用于名称，不能作为授权或合并依据。
    if android_app || agent.contains("ElvesAndroid/") {
        "Android App"
    } else if agent.contains("iPhone") {
        "iPhone 浏览器"
    } else if agent.contains("iPad") {
        "iPad 浏览器"
    } else if agent.contains("Android") {
        "Android 浏览器"
    } else if agent.contains("Windows") {
        "Windows 浏览器"
    } else if agent.contains("Macintosh") {
        "Mac 浏览器"
    } else {
        "网页设备"
    }
}

fn zrok_target(settings: &RemoteAccessSettings) -> String {
    let host = if settings.bind_host.contains(':') {
        format!("[{}]", settings.bind_host)
    } else {
        settings.bind_host.clone()
    };
    format!("http://{host}:{}", settings.port)
}

pub fn access_addresses(settings: &RemoteAccessSettings) -> Vec<String> {
    let mut addresses = Vec::new();
    if settings.bind_host == "0.0.0.0" {
        addresses.extend(
            crate::lan_proxy::lan_ipv4_addresses()
                .iter()
                .map(|ip| format!("http://{ip}:{}", settings.port)),
        );
        addresses.push(format!("http://127.0.0.1:{}", settings.port));
    } else {
        let host = if settings.bind_host.contains(':') {
            format!("[{}]", settings.bind_host)
        } else {
            settings.bind_host.clone()
        };
        addresses.push(format!("http://{host}:{}", settings.port));
    }
    addresses
        .into_iter()
        .filter_map(|address| reqwest::Url::parse(&address).ok())
        .map(|url| url.origin().ascii_serialization())
        .collect()
}

fn allowed_origins(settings: &RemoteAccessSettings) -> Vec<String> {
    let mut addresses = access_addresses(settings);
    if settings.bind_host == "127.0.0.1" || settings.bind_host == "0.0.0.0" {
        addresses.push(format!("http://localhost:{}", settings.port));
    }
    addresses
}

fn origin_matches_host(origin: &str, host: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(origin) else {
        return false;
    };
    let serialized = url.origin().ascii_serialization();
    let Some((_, authority)) = serialized.split_once("://") else {
        return false;
    };
    authority.eq_ignore_ascii_case(host)
}

fn read_json_or_default<T: serde::de::DeserializeOwned + Default>(
    path: &std::path::Path,
) -> anyhow::Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("远程访问授权文件损坏，已拒绝启动"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error.into()),
    }
}

fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn random_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
fn now_ms() -> u64 {
    crate::proxy_log::current_timestamp_ms()
}
fn constant_time_eq(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recent_priority_is_shared_across_authorized_clients_and_preserved_when_read() {
        struct PriorityCatalog(std::sync::Mutex<Value>);
        #[async_trait]
        impl TestRemoteBackend for PriorityCatalog {
            async fn call(&self, action: &str, payload: Value) -> anyhow::Result<Value> {
                let mut catalog = self.0.lock().unwrap();
                match action {
                    "sidebar" => Ok(catalog.clone()),
                    "markRead" => {
                        for row in catalog["recent"].as_array_mut().unwrap() {
                            if row["id"] == payload["threadId"] {
                                row["hasUnreadTurn"] = json!(false);
                            }
                        }
                        Ok(json!({"ok":true}))
                    }
                    _ => anyhow::bail!("优先事项清除不允许调用原生写操作：{action}"),
                }
            }
        }
        let (dir, remote) = harness_authorization_fixture(None).await;
        let controller = &remote.controller;
        let desktop = Device {
            id: "priority-desktop".into(),
            key_id: desktop::DEVICE_KEY_ID.into(),
            ..remote.device.clone()
        };
        *controller.desktop_device.lock().await = Some(desktop.clone());
        controller.desktop_running.store(true, Ordering::Release);
        let local = controller.authorization(desktop, None);
        let catalog = Arc::new(PriorityCatalog(std::sync::Mutex::new(json!({"ok":true,
            "nativeBridge":"harness","complete":true,"sideThreadsComplete":true,"projects":[],"pinnedThreads":[],
            "recent":[
                {"id":"unread","status":{"type":"idle"},"hasUnreadTurn":true},
                {"id":"running","status":{"type":"active"},"hasUnreadTurn":false},
                {"id":"side-owner","status":{"type":"idle"},"hasUnreadTurn":false,"sideThreadAttention":{"waiting":true}},
                {"id":"side-child","status":{"type":"active"},"parentThreadId":"side-owner"},
                {"id":"old","status":{"type":"idle"},"hasUnreadTurn":false}
            ]}))));
        assert!(controller.test_backend.set(catalog.clone()).is_ok());
        let query = json!({"enabledInstanceIds":["codex:local"]});
        let local_list = controller.call_authorized("sidebar", query.clone(), &local).await.unwrap();
        assert_eq!(local_list["recentPriorityIds"], json!(["side-owner", "unread", "running"]));
        controller.call_authorized("markRead", json!({"threadId":"unread"}), &remote).await.unwrap();
        let reopened = controller.call_authorized("sidebar", query.clone(), &remote).await.unwrap();
        assert_eq!(reopened["recentPriorityIds"], local_list["recentPriorityIds"]);
        assert_eq!(reopened["recentPriorityClearableIds"], json!(["unread"]));
        let mut events = controller.events.subscribe();
        let clear = json!({"threadIds":["side-owner","unread","running","side-child","old"],
            "enabledInstanceIds":["codex:local"]});
        controller.publish_access_policy(&RemoteAccessSettings {
            enabled: true, read_only: true, ..Default::default()
        });
        let blocked = controller.execute(remote.device.clone(), "read-only-priority".into(),
            "clearRecentPriority".into(), clear.clone()).await.unwrap_err();
        assert_eq!(blocked.code, 403);
        controller.publish_access_policy(&RemoteAccessSettings { enabled: true, ..Default::default() });
        let receipt = controller.execute(remote.device.clone(), "clear-priority-test".into(),
            "clearRecentPriority".into(), clear).await.unwrap();
        assert_eq!(receipt["state"], "completed");
        assert_eq!(receipt["result"]["removed"], 1);
        let event = events.try_recv().unwrap();
        assert_eq!(event["method"], "remote/sidebarChanged");
        assert_eq!(event["params"]["reason"], "priority");
        let local_list = controller.call_authorized("sidebar", query, &local).await.unwrap();
        assert_eq!(local_list["recentPriorityIds"], json!(["side-owner", "running"]));
        let restored = recent_priority::RecentPriority::new(dir.path());
        let mut idle = catalog.0.lock().unwrap().clone();
        idle["recent"][1]["status"]["type"] = json!("idle");
        idle["recent"][2]["sideThreadAttention"] = Value::Null;
        restored.annotate(&mut idle, restored.revision());
        assert_eq!(idle["recentPriorityIds"], json!(["side-owner", "running"]));
    }

    #[tokio::test]
    async fn recent_priority_hydrates_unknown_codex_unread_state_before_clear() {
        struct HydratingCatalog(std::sync::Mutex<Value>);
        #[async_trait]
        impl TestRemoteBackend for HydratingCatalog {
            async fn call(&self, action: &str, payload: Value) -> anyhow::Result<Value> {
                match action {
                    "sidebar" => Ok(self.0.lock().unwrap().clone()),
                    "thread" => Ok(json!({
                        "ok":true,
                        "thread":{
                            "id":payload["threadId"],
                            "hasUnreadTurn":false,
                            "readState":{"firstUnread":null,"revision":"r1"}
                        }
                    })),
                    _ => anyhow::bail!("unexpected priority hydration action: {action}"),
                }
            }
        }

        let (directory, remote) = harness_authorization_fixture(None).await;
        struct ResetDiagnosticLog;
        impl Drop for ResetDiagnosticLog {
            fn drop(&mut self) {
                crate::diagnostic_log::set_diagnostic_log_path_for_tests(None);
            }
        }
        let log_path = directory.path().join("priority-diagnostics.jsonl");
        crate::diagnostic_log::set_diagnostic_log_path_for_tests(Some(log_path.clone()));
        let _reset_log = ResetDiagnosticLog;
        let controller = &remote.controller;
        let id = "codex-unknown-unread";
        let catalog = Arc::new(HydratingCatalog(std::sync::Mutex::new(json!({
            "ok":true,
            "nativeBridge":"harness",
            "complete":true,
            "sideThreadsComplete":true,
            "projects":[],
            "pinnedThreads":[],
            "recent":[{
                "id":id,
                "backend":"codex",
                "instanceId":"codex:local",
                "status":{"type":"notLoaded"},
                "priority":{"status":{"type":"notLoaded"},"active":false,"eligible":true,"attentionState":"idle"},
                "hasUnreadTurn":null,
                "requestsKnown":true,
                "requests":[],
                "parentThreadId":null,
                "attachedToThreadId":null,
                "ephemeral":false,
                "serviceSide":false,
                "threadSource":"user"
            }]
        }))));
        assert!(controller.test_backend.set(catalog).is_ok());
        controller.harness.recent_priority.promote(id);

        let receipt = controller
            .execute(
                remote.device.clone(),
                "clear-unknown-unread".into(),
                "clearRecentPriority".into(),
                json!({"threadIds":[id],"enabledInstanceIds":["codex:local"]}),
            )
            .await
            .unwrap();
        assert_eq!(receipt["state"], "completed");
        assert_eq!(receipt["result"]["removed"], 1);
        let log = std::fs::read_to_string(log_path).unwrap();
        assert!(log.contains("\"event\":\"remote_access.priority_unread_hydration\""));
        assert!(log.contains("\"resolvedCount\":1"));
        assert!(log.contains("\"event\":\"remote_access.priority_clear\""));
        assert!(log.contains("\"removed\":1"));
    }

    #[tokio::test]
    async fn harness_diagnostics_collects_local_runtime_priority_and_side_relationships() {
        struct DiagnosticsCatalog;
        #[async_trait]
        impl TestRemoteBackend for DiagnosticsCatalog {
            async fn call(&self, action: &str, payload: Value) -> anyhow::Result<Value> {
                match action {
                    "sidebar" => Ok(json!({
                        "ok":true,
                        "nativeBridge":"harness",
                        "complete":true,
                        "sideThreadsComplete":false,
                        "sideThreadsError":"持久侧边读取失败",
                        "sideThreadIds":["persistent-side"],
                        "unreadSideThreads":[{"id":"persistent-side","parentThreadId":"target-parent"}],
                        "projects":[],
                        "pinnedThreads":[],
                        "recent":[{
                            "id":"target",
                            "backend":"codex",
                            "instanceId":"codex:local",
                            "status":{"type":"idle"},
                            "hasUnreadTurn":null,
                            "requestsKnown":true,
                            "requests":[],
                            "parentThreadId":null,
                            "attachedToThreadId":null,
                            "ephemeral":false,
                            "serviceSide":false
                        }]
                    })),
                    "thread" if payload["threadId"] == "target" => Ok(json!({
                        "ok":true,
                        "thread":{
                            "id":"target",
                            "parentThreadId":"target-parent",
                            "attachedToThreadId":"target-parent",
                            "status":{"type":"idle"},
                            "hasUnreadTurn":false
                        }
                    })),
                    "relatedThreads" if payload["threadId"] == "target-parent" => Ok(json!({
                        "ok":true,
                        "threadId":"target-parent",
                        "sideThreads":[{"id":"persistent-side","parentThreadId":"target-parent"}],
                        "sideThreadsComplete":false,
                        "sideThreadsError":"持久侧边读取失败"
                    })),
                    _ => anyhow::bail!("unexpected diagnostics action: {action}"),
                }
            }
        }

        let (_directory, authorization) = harness_fixture_with_store(None).await;
        let authorization = desktop_authorization(authorization).await;
        let controller = &authorization.controller;
        assert!(controller
            .test_backend
            .set(Arc::new(DiagnosticsCatalog))
            .is_ok());

        let snapshot = controller
            .call_authorized(
                "harnessDiagnostics",
                json!({"threadId":"target"}),
                &authorization,
            )
            .await
            .unwrap();

        assert_eq!(snapshot["ok"], true);
        assert_eq!(snapshot["runtime"]["desktop"], true);
        assert_eq!(snapshot["runtime"]["desktopRunning"], true);
        assert_eq!(snapshot["target"]["requestedId"], "target");
        assert_eq!(snapshot["target"]["foundInSidebar"], true);
        assert_eq!(snapshot["target"]["relatedForId"], "target-parent");
        assert_eq!(snapshot["target"]["threadRead"]["id"], "target");
        assert_eq!(
            snapshot["target"]["relatedThreads"]["sideThreads"][0]["id"],
            "persistent-side"
        );
        assert_eq!(snapshot["priority"]["unknownUnreadIds"], json!(["target"]));
        assert_eq!(snapshot["persistentSides"]["ids"], json!(["persistent-side"]));
        assert_eq!(
            snapshot["priority"]["sideThreadsError"],
            "持久侧边读取失败"
        );
    }

    #[tokio::test]
    async fn harness_diagnostics_is_rejected_for_remote_read_entry() {
        let (_directory, authorization) = harness_authorization_fixture(None).await;
        let controller = authorization.controller.clone();
        let state = WebState {
            desktop: false,
            controller,
            settings: RemoteAccessSettings::default(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("codex_elves_device=isolated-service-dispatch-token"),
        );

        let error = read(
            State(state),
            Path("harnessDiagnostics".into()),
            headers,
            Json(json!({})),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, 403);
        assert_eq!(error.message, "Harness 诊断仅可在 Elves 独立客户端使用");
    }

    #[test]
    fn recent_priority_keeps_unknown_or_new_work_and_rejects_late_catalog_promotions() {
        let dir = tempfile::tempdir().unwrap();
        let priority = recent_priority::RecentPriority::new(dir.path());
        priority.promote("active-again");
        priority.promote("done");
        priority.promote("unknown");
        let idle = json!({"sideThreadsComplete":true,"recent":[
            {"id":"active-again","status":{"type":"idle"},"hasUnreadTurn":false},
            {"id":"done","status":{"type":"notLoaded"},"hasUnreadTurn":false},
            {"id":"unknown","status":{"type":"unknown"},"hasUnreadTurn":false}
        ]});
        let observed = priority.revision();
        priority.promote("active-again");
        let ids = std::collections::HashSet::from(["active-again","done","unknown"]);
        let stale = priority.clear(&idle, &ids, observed).unwrap_err();
        assert_eq!(stale.downcast_ref::<ApiError>().unwrap().code, 409);
        let mut now = idle.clone();
        now["recent"][0]["status"]["type"] = json!("active");
        let mut incomplete = now.clone();
        incomplete["sideThreadsComplete"] = json!(false);
        assert_eq!(priority.clear(&incomplete, &ids, priority.revision()).unwrap(), 0);
        let observed = priority.revision();
        assert_eq!(priority.clear(&now, &ids, observed).unwrap(), 1);
        now["recent"][1]["hasUnreadTurn"] = json!(true);
        priority.annotate(&mut now, observed);
        assert_eq!(now["recentPriorityIds"], json!(["unknown","active-again"]));
        priority.promote("done");
        priority.annotate(&mut now, priority.revision());
        assert_eq!(now["recentPriorityIds"], json!(["done","unknown","active-again"]));
        priority.forget("unknown");
        priority.annotate(&mut now, priority.revision());
        assert_eq!(now["recentPriorityIds"], json!(["done","active-again"]));
    }

    #[tokio::test]
    async fn claude_subagent_write_actions_are_rejected_before_any_engine_dispatch() {
        let (_directory, authorization) = service_dispatch_fixture().await;
        let child = "claude-agent-00000000-0000-4000-8000-000000000001-agent_fixture";
        for action in [
            "sendText",
            "stop",
            "setModel",
            "setReasoningEffort",
            "respond",
            "rename",
            "delete",
        ] {
            let failure = authorization
                .controller
                .call_backend(
                    action,
                    json!({"threadId":child,"text":"不能发送","model":"test-model"}),
                    &authorization,
                )
                .await
                .unwrap_err();
            assert_eq!(
                failure.downcast_ref::<ApiError>().unwrap().code,
                403,
                "{action}"
            );
        }
    }

    #[tokio::test]
    async fn claude_read_only_model_metadata_does_not_start_wsl_or_codex() {
        let (_directory, authorization) = service_dispatch_fixture().await;
        let controller = &authorization.controller;
        controller.publish_access_policy(&RemoteAccessSettings {
            enabled: true,
            read_only: true,
            ..Default::default()
        });
        // 故意使用不存在的项目；只读能力查询必须先返回，不得解析项目或启动任何执行引擎。
        for include_available in [false, true] {
            let response = controller
                .call_backend(
                    "models",
                    json!({"backend":"claude","projectId":"does-not-exist",
                        "includeAvailable":include_available,"forceReload":true}),
                    &authorization,
                )
                .await
                .unwrap();
            assert_eq!(response["data"], json!([]));
            assert_eq!(response["readOnly"], true);
        }
    }

    #[tokio::test]
    async fn claude_read_only_usage_does_not_resolve_projects_or_start_cli() {
        let (_directory, authorization) = service_dispatch_fixture().await;
        let controller = &authorization.controller;
        controller.publish_access_policy(&RemoteAccessSettings {
            enabled: true,
            read_only: true,
            ..Default::default()
        });
        let response = controller
            .call_backend(
                "claudeUsage",
                json!({"backend":"claude","projectId":"does-not-exist"}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(response["ok"], true);
        assert_eq!(response["backend"], "claude");
        assert!(response["usage"].is_null());
        let failure = controller
            .call_backend("claudeUsage", json!({"backend":"codex"}), &authorization)
            .await
            .unwrap_err();
        assert_eq!(failure.downcast_ref::<ApiError>().unwrap().code, 400);
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "显式运行：需要独立 codex-elves-claude-test 中已配置的测试 API"]
    async fn claude_wsl_api_end_to_end() {
        struct SharedProjects {
            cwd: String,
        }
        #[async_trait]
        impl TestRemoteBackend for SharedProjects {
            async fn call(&self, action: &str, payload: Value) -> anyhow::Result<Value> {
                match action {
                    "workspaceContext" => Ok(json!({"ok":true,"hostId":"local",
                        "projectId":payload["projectId"],"cwd":self.cwd})),
                    "sidebar" => Ok(json!({"ok":true,"nativeBridge":"harness","complete":true,
                        "recent":[],"pinnedThreads":[],"sideThreadIds":[],
                        "projects":[{"id":"shared-project","name":"Claude 隔离测试项目","cwd":self.cwd,
                            "roots":[self.cwd],"threads":[]}],
                        "preferences":{"mode":"project","chats":"updated_at","projects":"updated_at","pinned":"manual"}})),
                    "models" => Ok(json!({"ok":true,"data":[{"model":"codex-fixture",
                        "displayName":"Codex 隔离样例","supportedReasoningEfforts":[]}]})),
                    "skills" => Ok(json!({"ok":true,"data":[],"errors":[]})),
                    "events" => Ok(json!({"ok":true,"events":[],"writeAvailable":true})),
                    _ => anyhow::bail!("Claude 测试不允许进入 Codex 执行入口：{action}"),
                }
            }
        }
        async fn idle(controller: &HarnessController, id: &str) -> Value {
            tokio::time::timeout(Duration::from_secs(150), async {
                loop {
                    let detail = controller.claude.detail(id).await.unwrap();
                    if detail["thread"]["status"]["type"] == "idle" {
                        return controller
                            .claude
                            .history(&json!({"threadId":id}))
                            .await
                            .unwrap();
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            })
            .await
            .expect("Claude 测试轮次超时")
        }
        async fn pending(controller: &HarnessController, id: &str, method: &str) -> Value {
            tokio::time::timeout(Duration::from_secs(100), async {
                loop {
                    let detail = controller.claude.detail(id).await.unwrap();
                    if let Some(request) = detail["thread"]["requests"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|r| r["method"] == method)
                    {
                        return request.clone();
                    }
                    assert_ne!(
                        detail["thread"]["status"]["type"],
                        "idle",
                        "Claude 没有发出预期请求 {method}：{}",
                        controller
                            .claude
                            .history(&json!({"threadId":id}))
                            .await
                            .unwrap()
                    );
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            })
            .await
            .expect("等待 Claude 原生交互请求超时")
        }
        let (directory, old_state) = login_lock_test_state();
        drop(old_state);
        let fixture = directory.path().join("shared-project");
        std::fs::create_dir_all(fixture.join(".claude")).unwrap();
        std::fs::write(
            fixture.join(".claude/settings.local.json"),
            br#"{"permissions":{"ask":["Bash","Write","Edit"]}}"#,
        )
        .unwrap();
        let native = Arc::new(SharedProjects {
            cwd: fixture.to_string_lossy().into_owned(),
        });
        let controller =
            HarnessController::with_test_backend(native, directory.path().to_path_buf()).unwrap();
        let settings = RemoteAccessSettings {
            enabled: true,
            allow_approvals: true,
            ..RemoteAccessSettings::default()
        };
        controller.publish_access_policy(&settings);
        controller.running.store(true, Ordering::SeqCst);
        let device = {
            let mut auth = controller.auth.lock().await;
            let device = Device {
                id: "claude-fixture-device".into(),
                name: "Claude 隔离联调".into(),
                token_hash: hash("claude-fixture-cookie"),
                client_id_hash: String::new(),
                key_id: auth.access_key.as_ref().unwrap().id.clone(),
                created_at_ms: now_ms(),
                last_seen_at_ms: now_ms(),
                expires_at_ms: now_ms() + 3_600_000,
                public_origin: None,
            };
            auth.devices.push(device.clone());
            device
        };
        let authorization = controller.authorization(device.clone(), Some("create"));
        let catalog = controller
            .call_backend(
                "models",
                json!({"backend":"claude","projectId":"shared-project"}),
                &authorization,
            )
            .await
            .unwrap();
        let models = catalog["data"].as_array().unwrap();
        assert!(!models.is_empty());
        // 用户已添加 GPT 供手动验收；自动工具联调仍显式选择 Haiku，不能依赖选项顺序。
        let model = models
            .iter()
            .find(|m| m["model"] == "claude-haiku-5-5")
            .expect("测试环境必须提供 Haiku")["model"]
            .clone();
        let phrase = format!("elves-{}", uuid::Uuid::new_v4().simple());
        let payload = json!({"backend":"claude","projectId":"shared-project","model":model,
            "reasoningEffort":"low","prompt":format!("记住测试短语 {phrase}，只回复已记住，不使用工具。")});
        let receipt = "claude-fixture-create".to_owned();
        let created = controller
            .execute(
                device.clone(),
                receipt.clone(),
                "create".into(),
                payload.clone(),
            )
            .await
            .unwrap();
        assert_eq!(created["result"]["ok"], true, "{created}");
        let result = created["result"].clone();
        let id = result["threadId"].as_str().unwrap().to_owned();
        let repeated = controller
            .execute(device.clone(), receipt, "create".into(), payload)
            .await
            .unwrap();
        assert_eq!(
            repeated["result"]["threadId"], id,
            "同一回执不能重复创建会话"
        );
        let history = idle(&controller, &id).await;
        assert_eq!(history["data"][0]["status"], "completed", "{history}");
        let sidebar = controller
            .call_backend("sidebar", json!({"includeClaude":true}), &authorization)
            .await
            .unwrap();
        assert!(
            sidebar["recent"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["id"] == id && t["backend"] == "claude")
        );
        assert_eq!(sidebar["projects"][0]["id"], "shared-project");
        assert_eq!(sidebar["projects"][0]["threads"][0]["id"], id);
        assert!(
            controller
                .call_backend(
                    "send",
                    json!({"threadId":id,"backend":"codex","prompt":"禁止切换"}),
                    &authorization
                )
                .await
                .is_err()
        );
        println!("真实 CLI 创建、共享项目、两种列表、类型锁定和幂等创建通过");

        controller.execute(device.clone(), "claude-fixture-write".into(), "send".into(),
            json!({"threadId":id,"model":model,"reasoningEffort":"low",
                "prompt":"请只使用 Write 工具在当前项目目录创建 approval.txt，内容严格为 ELVES_APPROVAL_OK，不运行其他工具，完成后简短回复。"}))
            .await.unwrap();
        let request = pending(&controller, &id, "item/commandExecution/requestApproval").await;
        assert_eq!(request["_claude"]["tool_name"], "Write");
        assert!(
            request["_claude"]["input"]["file_path"]
                .as_str()
                .unwrap()
                .ends_with("/approval.txt")
        );
        assert_eq!(
            request["_claude"]["input"]["content"]
                .as_str()
                .unwrap()
                .trim(),
            "ELVES_APPROVAL_OK"
        );
        let approved = controller
            .execute(
                device.clone(),
                "claude-fixture-approve".into(),
                "approve".into(),
                json!({"threadId":id,"requestId":request["id"],"decision":"accept"}),
            )
            .await
            .unwrap();
        assert_eq!(approved["result"]["ok"], true);
        let history = idle(&controller, &id).await;
        assert_eq!(history["data"][0]["status"], "completed", "{history}");
        assert_eq!(
            std::fs::read_to_string(fixture.join("approval.txt"))
                .unwrap()
                .trim(),
            "ELVES_APPROVAL_OK"
        );
        println!("真实 CLI 工具请求经远程批准后写入隔离文件通过");

        controller.execute(device.clone(), "claude-fixture-question".into(), "send".into(),
            json!({"threadId":id,"model":model,"reasoningEffort":"low",
                "prompt":"请立即调用 AskUserQuestion 工具询问我选择方案一还是方案二，收到我的选择后复述选择。不使用文字代替工具询问。"}))
            .await.unwrap();
        let request = pending(&controller, &id, "item/tool/requestUserInput").await;
        let question = &request["params"]["questions"][0];
        let choice = question["options"][0]["label"].as_str().unwrap();
        let answered = controller
            .execute(
                device.clone(),
                "claude-fixture-answer".into(),
                "answer".into(),
                json!({"threadId":id,"requestId":request["id"],"response":{"answers":{
                    question["id"].as_str().unwrap():{"answers":[choice]}
                }}}),
            )
            .await
            .unwrap();
        assert_eq!(answered["result"]["ok"], true);
        let history = idle(&controller, &id).await;
        assert_eq!(history["data"][0]["status"], "completed", "{history}");
        println!("真实 AskUserQuestion 经远程回答后继续执行通过");

        controller
            .execute(
                device.clone(),
                "claude-fixture-stream".into(),
                "send".into(),
                json!({"threadId":id,"model":model,"reasoningEffort":"low",
                "prompt":"请逐行输出从 1 到 2000 的全部整数，每行一个数字，不省略，不使用工具。"}),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(100), async {
            loop {
                let history = controller
                    .claude
                    .history(&json!({"threadId":id}))
                    .await
                    .unwrap();
                if history["data"][0]["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| {
                        item["type"] == "agentMessage"
                            && item["text"].as_str().is_some_and(|s| !s.is_empty())
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("等待首个真实流式输出超时");
        let stopped = controller
            .execute(
                device.clone(),
                "claude-fixture-stop".into(),
                "stop".into(),
                json!({"threadId":id}),
            )
            .await
            .unwrap();
        assert_eq!(stopped["result"]["ok"], true);
        let history = tokio::time::timeout(Duration::from_secs(20), idle(&controller, &id))
            .await
            .unwrap();
        assert_eq!(history["data"][0]["status"], "interrupted", "{history}");
        println!("真实流式生成被远程停止通过");

        // 关闭本后端启动的进程，重新加载持久记录；下一轮必须由 Claude --resume 恢复上下文。
        controller.claude.release().await;
        let restored = claude::Claude::new(directory.path()).unwrap();
        let sent = restored
            .command(
                "send",
                &json!({"threadId":id,"model":model,"reasoningEffort":"low",
            "prompt":"刚才让你记住的测试短语是什么？只回复该短语，不使用工具。"}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(sent["ok"], true);
        tokio::time::timeout(Duration::from_secs(150), async {
            while restored.detail(&id).await.unwrap()["thread"]["status"]["type"] != "idle" {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .unwrap();
        let history = restored.history(&json!({"threadId":id})).await.unwrap();
        assert_eq!(history["data"][0]["status"], "completed", "{history}");
        assert!(
            history["data"][0]["items"].to_string().contains(&phrase),
            "必须恢复 Claude 原生上下文：{history}"
        );
        restored.release().await;
        println!("重建后端后由原版 Claude --resume 恢复真实上下文通过");

        // 可选本机 UI 验收入口，使用隔离项目和测试访问密钥，不连接当前桌面会话。
        if let Ok(port) = std::env::var("CODEX_ELVES_CLAUDE_UI_PORT") {
            let port = port.parse::<u16>().unwrap();
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .unwrap();
            let router = web_router(
                controller.clone(),
                RemoteAccessSettings { port, ..settings },
            );
            println!("Claude 隔离 UI：http://127.0.0.1:{port}；测试访问密钥：{LOGIN_TEST_KEY}");
            println!(
                "关闭验收：创建 {}",
                directory.path().join("finish-ui").display()
            );
            let service = tokio::spawn(async move {
                axum::serve(
                    listener,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                .unwrap()
            });
            let stop = directory.path().join("finish-ui");
            while !stop.is_file() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            service.abort();
        }
        controller.claude.release().await;
    }

    #[test]
    fn connection_diagnostics_accept_only_scoped_get_requests_and_bounded_ids() {
        let accepted = format!("/api/session?diag={}&ignored=secret", "a".repeat(80));
        let context = ConnectionDiagnosticContext::from_request(
            &axum::http::Method::GET,
            &accepted.parse().unwrap(),
        )
        .unwrap();
        assert_eq!(context.request_id.len(), 80);
        assert_eq!(context.path, "/api/session");
        for path in ["/api/session", "/api/channels", "/api/events/ws"] {
            let uri = format!("{path}?diag=http-Ab_12").parse().unwrap();
            assert!(
                ConnectionDiagnosticContext::from_request(&axum::http::Method::GET, &uri).is_some()
            );
            assert!(
                ConnectionDiagnosticContext::from_request(&axum::http::Method::HEAD, &uri)
                    .is_none()
            );
            assert!(
                ConnectionDiagnosticContext::from_request(&axum::http::Method::POST, &uri)
                    .is_none()
            );
        }
        for uri in [
            "/api/session".to_owned(),
            "/api/session?diag=".to_owned(),
            "/api/session?diag".to_owned(),
            "/api/session?diag=http-ok&diag=http-duplicate".to_owned(),
            "/api/session?diag=http-ok&diag".to_owned(),
            "/api/session?diag=bad%0Aid".to_owned(),
            "/api/session?diag=bad/id".to_owned(),
            "/api/login-status?diag=http-unscoped".to_owned(),
            format!("/api/session?diag={}", "a".repeat(81)),
        ] {
            assert!(
                ConnectionDiagnosticContext::from_request(
                    &axum::http::Method::GET,
                    &uri.parse().unwrap()
                )
                .is_none(),
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn connection_diagnostics_cover_http_boundaries_websocket_and_cancellation() {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        struct ResetDiagnosticLog;
        impl Drop for ResetDiagnosticLog {
            fn drop(&mut self) {
                crate::diagnostic_log::set_diagnostic_log_path_for_tests(None);
            }
        }
        let (directory, state) = login_lock_test_state();
        let log_path = directory.path().join("connection-diagnostics.jsonl");
        crate::diagnostic_log::set_diagnostic_log_path_for_tests(Some(log_path.clone()));
        let _reset_log = ResetDiagnosticLog;
        {
            let mut auth = state.controller.auth.lock().await;
            let key_id = auth.access_key.as_ref().unwrap().id.clone();
            auth.devices.push(Device {
                id: "diagnostic-fixture".into(),
                name: "diagnostic-fixture".into(),
                token_hash: hash("private-fixture-cookie"),
                client_id_hash: String::new(),
                key_id,
                created_at_ms: now_ms(),
                last_seen_at_ms: now_ms(),
                expires_at_ms: now_ms() + 120_000,
                public_origin: None,
            });
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = web_router(
            state.controller,
            RemoteAccessSettings {
                enabled: true,
                port: address.port(),
                ..RemoteAccessSettings::default()
            },
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let cookie = format!("{COOKIE_NAME}=private-fixture-cookie");
        for (path, authenticated, host, expected) in [
            (
                "/api/session?diag=http-ready&ignored=private-query-value",
                true,
                None,
                200,
            ),
            ("/api/channels?diag=http-unauthorized", false, None, 401),
            (
                "/api/session?diag=http-host-rejected",
                true,
                Some("private-rejected-host.example"),
                403,
            ),
            ("/api/events/ws?diag=ws-bad-handshake", true, None, 400),
            ("/api/session", true, None, 200),
            ("/api/session?diag=bad%0Aid", true, None, 200),
            ("/api/login-status?diag=http-unscoped", false, None, 200),
        ] {
            let mut request = client.get(format!("http://{address}{path}"));
            if authenticated {
                request = request.header("Cookie", &cookie);
            }
            if let Some(host) = host {
                request = request.header("Host", host);
            }
            assert_eq!(request.send().await.unwrap().status().as_u16(), expected);
        }
        let mut request = format!("ws://{address}/api/events/ws?diag=ws-ready")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert_eq!(response.status().as_u16(), 101);
        let ready: Value =
            serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(ready["method"], "remote/streamReady");
        socket
            .close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal,
                reason: "private-client-close-reason".into(),
            }))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let text = std::fs::read_to_string(&log_path).unwrap();
                if text.contains("\"phase\":\"websocket_handler_ended\"") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let task_entered = entered.clone();
        let task = tokio::spawn(async move {
            let _diagnostic = ConnectionRequestDiagnostic::start(
                &axum::http::Method::GET,
                &"/api/session?diag=http-cancelled".parse().unwrap(),
            );
            task_entered.notify_one();
            std::future::pending::<()>().await;
        });
        entered.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        let text = std::fs::read_to_string(&log_path).unwrap();
        for secret in [
            "private-fixture-cookie",
            "private-query-value",
            "private-rejected-host",
            "private-client-close-reason",
            "127.0.0.1",
        ] {
            assert!(!text.contains(secret), "diagnostics leaked {secret}");
        }
        let records: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|record| record["event"] == "remote_access.connection_request")
            .map(|record| record["detail"].clone())
            .collect();
        assert_eq!(records.len(), 14);
        for (request_id, status) in [
            ("http-ready", 200),
            ("http-unauthorized", 401),
            ("http-host-rejected", 403),
            ("ws-bad-handshake", 400),
            ("ws-ready", 101),
        ] {
            let phases: Vec<_> = records
                .iter()
                .filter(|record| record["requestId"] == request_id)
                .collect();
            assert_eq!(phases[0]["phase"], "received");
            let response = phases
                .iter()
                .find(|record| record["phase"] == "response_headers_ready")
                .unwrap();
            assert_eq!(response["status"], status);
            assert_eq!(response["method"], "GET");
            assert!(response["durationMs"].as_u64().is_some());
        }
        let closed = records
            .iter()
            .find(|record| record["phase"] == "websocket_handler_ended")
            .unwrap();
        assert_eq!(closed["requestId"], "ws-ready");
        assert_eq!(closed["closeReason"], "peer_close");
        assert_eq!(closed["closeCode"], 1000);
        let cancelled = records
            .iter()
            .find(|record| record["phase"] == "request_cancelled")
            .unwrap();
        assert_eq!(cancelled["requestId"], "http-cancelled");
        assert!(cancelled["status"].is_null());

        // An unusable log destination must not change the HTTP response.
        crate::diagnostic_log::set_diagnostic_log_path_for_tests(Some(directory.path().into()));
        assert_eq!(
            client
                .get(format!("http://{address}/api/session?diag=http-log-failed"))
                .header("Cookie", &cookie)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            200
        );
        server.abort();
        let _ = server.await;
    }

    pub(super) async fn service_dispatch_fixture() -> (tempfile::TempDir, HarnessAuthorization) {
        let (directory, state) = login_lock_test_state();
        let controller = state.controller;
        let device = {
            let mut auth = controller.auth.lock().await;
            let device = Device {
                id: "service-dispatch-device".into(),
                name: "隔离派发测试".into(),
                token_hash: hash("isolated-service-dispatch-token"),
                client_id_hash: String::new(),
                key_id: auth.access_key.as_ref().unwrap().id.clone(),
                created_at_ms: now_ms(),
                // 功能回归在 Windows 并发构建时可能排队超过两分钟；过期行为由专门用例显式设置。
                expires_at_ms: now_ms() + 3_600_000,
                last_seen_at_ms: now_ms(),
                public_origin: None,
            };
            auth.devices.push(device.clone());
            device
        };
        (directory, controller.authorization(device, Some("send")))
    }

    pub(super) async fn harness_fixture() -> (tempfile::TempDir, HarnessAuthorization) {
        harness_fixture_with_store(None).await
    }

    pub(super) async fn desktop_authorization(
        mut authorization: HarnessAuthorization,
    ) -> HarnessAuthorization {
        authorization.device.key_id = desktop::DEVICE_KEY_ID.into();
        authorization.device.expires_at_ms = u64::MAX;
        let controller = &authorization.controller;
        *controller.desktop_device.lock().await = Some(authorization.device.clone());
        controller.desktop_running.store(true, Ordering::Release);
        controller.running.store(false, Ordering::SeqCst);
        controller.publish_access_policy(&RemoteAccessSettings {
            enabled: false,
            read_only: true,
            allow_approvals: false,
            ..Default::default()
        });
        controller.auth.lock().await.devices.clear();
        authorization.check().await.unwrap();
        authorization
    }

    async fn harness_authorization_fixture(
        saved: Option<Value>,
    ) -> (tempfile::TempDir, HarnessAuthorization) {
        let (directory, authorization) = service_dispatch_fixture().await;
        if let Some(saved) = saved {
            std::fs::write(
                directory.path().join("remote-harness-state.json"),
                serde_json::to_vec(&saved).unwrap(),
            )
            .unwrap();
        }
        let mut git = std::process::Command::new("git");
        git.args(["init", "--quiet"]).current_dir(directory.path());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            git.creation_flags(crate::windows_create_hidden_process());
        }
        assert!(git.status().unwrap().success());
        let controller = HarnessController::new(directory.path().to_owned()).unwrap();
        controller.publish_access_policy(&RemoteAccessSettings {
            enabled: true,
            ..Default::default()
        });
        controller.running.store(true, Ordering::SeqCst);
        controller
            .auth
            .lock()
            .await
            .devices
            .push(authorization.device.clone());
        let authorization = controller.authorization(authorization.device.clone(), Some("send"));
        (directory, authorization)
    }

    async fn harness_fixture_with_store(
        saved: Option<Value>,
    ) -> (tempfile::TempDir, HarnessAuthorization) {
        let (directory, authorization) = harness_authorization_fixture(saved).await;
        let script = directory.path().join("engine.cjs");
        std::fs::write(&script,r#"
const fs=require('node:fs'),readline=require('node:readline'),crypto=require('node:crypto');
const persisted=fs.existsSync(__dirname+'/persistent-engine.json')?JSON.parse(fs.readFileSync(__dirname+'/persistent-engine.json','utf8')):{};
const threads=new Map(persisted.threads||[]),goals=new Map(persisted.goals||[]),modelContexts=new Map(persisted.contexts||[]),boundaryReads=new Map();let approval=null,related={rows:[],loaded:[]};
const output=v=>{
  if(fs.existsSync(__dirname+'/persistent-fixture'))fs.writeFileSync(__dirname+'/persistent-engine.json',JSON.stringify({
    threads:[...threads].filter(([,t])=>!t.ephemeral),goals:[...goals],contexts:[...modelContexts].filter(([id])=>!threads.get(id)?.ephemeral)}));
  process.stdout.write(JSON.stringify(v)+'\n');
};
const event=(method,params)=>output({method,params});
const logTurn=(t,type,turn)=>{if(t?.path)fs.appendFileSync(t.path,JSON.stringify({
  timestamp:new Date().toISOString(),type:'event_msg',payload:{type,turn_id:turn.id}})+'\n');};
const pageRows=(rows,p,prefix)=>{
  const ordered=p.sortDirection==='asc'?[...rows]:[...rows].reverse();
  const start=p.cursor?Number(String(p.cursor).slice(prefix.length+1)):0,limit=p.limit||100;
  const end=Math.min(start+limit,ordered.length);
  return {data:ordered.slice(start,end),nextCursor:end<ordered.length?`${prefix}:${end}`:null};
};
const permissionSnapshot=(p,base={})=>{
  const mode=p.sandbox||(p.permissions===':read-only'?'read-only':p.permissions===':danger-full-access'?'danger-full-access':null);
  const config=p.config||{};
  const sandbox=p.sandboxPolicy||(mode==='read-only'?{type:'readOnly'}:mode==='danger-full-access'?{type:'dangerFullAccess'}
    :mode==='workspace-write'?{type:'workspaceWrite',writableRoots:config['sandbox_workspace_write.writable_roots']||[p.cwd||__dirname],
      networkAccess:config['sandbox_workspace_write.network_access']||false,
      excludeTmpdirEnvVar:config['sandbox_workspace_write.exclude_tmpdir_env_var']||false,
      excludeSlashTmp:config['sandbox_workspace_write.exclude_slash_tmp']||false}:base.sandbox||{type:'readOnly'});
  return {approvalPolicy:p.approvalPolicy??base.approvalPolicy??'on-request',
    approvalsReviewer:p.approvalsReviewer??base.approvalsReviewer??'user',sandbox,
    activePermissionProfile:p.permissions?{id:p.permissions}:mode||p.sandboxPolicy?null:base.activePermissionProfile??null,
    runtimeWorkspaceRoots:p.runtimeWorkspaceRoots??base.runtimeWorkspaceRoots??[p.cwd||__dirname]};
};
const finish=(id,status='completed')=>{
  const t=threads.get(id),turn=t?.turns.at(-1);if(!turn)return;
  turn.status=status;t.status={type:'idle'};
  logTurn(t,status==='completed'?'task_complete':'turn_aborted',turn);
  event('turn/completed',{threadId:id,turn});
  event('thread/status/changed',{threadId:id,status:t.status});
};
readline.createInterface({input:process.stdin}).on('line',line=>{
 const q=JSON.parse(line);fs.appendFileSync(__dirname+'/requests.jsonl',JSON.stringify(q)+'\n');
 if(!q.method){if(approval){finish(approval.threadId);approval=null;}return;}
 if(q.method==='initialized')return;
  const p=q.params||{},pinPath=__dirname+'/native-pins.json';
  const fastPolicyPath=__dirname+'/fast-policy.json';
  const fastPolicy=fs.existsSync(fastPolicyPath)?JSON.parse(fs.readFileSync(fastPolicyPath,'utf8')):{};
  const sidePolicyPath=__dirname+'/side-policy.json';
  const sidePolicy=fs.existsSync(sidePolicyPath)?JSON.parse(fs.readFileSync(sidePolicyPath,'utf8')):{};
 const nativePins=fs.existsSync(pinPath)?JSON.parse(fs.readFileSync(pinPath,'utf8')):null;
 const pinnedSection='01984de2-8f74-7c91-a3b2-5c5e937cf318';
 const t=threads.get(p.threadId)||nativePins?.rows?.find(row=>row.id===p.threadId)||related.rows.find(row=>row.id===p.threadId);let result={};
 switch(q.method){
 case 'initialize':result={userAgent:sidePolicy.userAgent||'codex_cli_rs/0.169.0-alpha.11.1 (Windows)'};break;
 case 'project/list':result={data:[{id:'project-a',name:'隔离项目',roots:[{path:__dirname}],position:0,metadata:{}}],nextCursor:null};break;
 case 'threadSection/list':
   if(!nativePins){output({id:q.id,error:{code:-32601,message:'unsupported sections'}});return;}
   if(nativePins.sectionError){output({id:q.id,error:{code:nativePins.sectionError,message:'section lookup failed'}});return;}
   result={data:nativePins.missingSection?[]:[{id:pinnedSection,name:'已改名的置顶分区'}],nextCursor:null};break;
 case 'thread/list':
   if(p.attachedToThreadId){
     if(Object.hasOwn(sidePolicy,'attachedResponse')){result=sidePolicy.attachedResponse;break;}
     const rows=[...threads.values(),...related.rows];
     const data=rows.filter(row=>!row.ephemeral&&!!row.archived===!!p.archived
       &&(row.attachedToThreadId===p.attachedToThreadId||sidePolicy.attachedUnrelated));
     const start=Number(p.cursor||0),end=start+(sidePolicy.attachedPageSize||100);
     result={data:data.slice(start,end),nextCursor:sidePolicy.attachedCursorLoop?'1':end<data.length?String(end):null};
   }else if(p.ancestorThreadId){
     if(related.listFailure){output({id:q.id,error:{code:-32000,message:'related lookup failed'}});return;}
     const all=new Map([...threads.values(),...related.rows].map(row=>[row.id,row]));
     const belongs=row=>{const seen=new Set([row.id]);for(let parent=row.parentThreadId;parent&&!seen.has(parent);parent=all.get(parent)?.parentThreadId){if(parent===p.ancestorThreadId)return true;seen.add(parent);}return false;};
     const data=related.rows.filter(row=>!row.ephemeral&&!!row.archived===!!p.archived&&(belongs(row)||related.injectUnrelated));
     const start=Number(p.cursor||0),end=start+(related.pageSize||100);
     result={data:data.slice(start,end),nextCursor:related.cursorLoop?'1':end<data.length?String(end):null};
   }else if(p.sectionId&&nativePins){
     const data=nativePins.ids.map(id=>threads.get(id)||nativePins.rows.find(row=>row.id===id))
       .map(row=>({...row,section:{id:nativePins.wrongSection?'custom':pinnedSection,name:'Pinned'}}));
     const start=Number(p.cursor||0),end=start+(nativePins.pageSize||100);
     result={data:data.slice(start,end),nextCursor:nativePins.cursorLoop?'1':end<data.length?String(end):null};
   }else result={data:[...threads.values(),...(nativePins?.recentIds||[]).map(id=>nativePins.rows.find(row=>row.id===id))]
     .filter(row=>!row.attachedToThreadId),nextCursor:null};
   break;
 case 'thread/loaded/list':result={data:related.loaded,nextCursor:null};break;
 case 'test/seedSubagents':
   related={rows:[],loaded:[],...p};
   for(const row of related.rows)if(related.emitStarted&&row.parentThreadId)event('thread/started',{thread:row});
   for(const e of related.events||[])output(e);
   break;
 case 'thread/section/move':
   if(!nativePins){output({id:q.id,error:{code:-32601,message:'unsupported sections'}});return;}
   if(!t){output({id:q.id,error:{code:-32600,message:'unknown thread'}});return;}
   nativePins.ids=nativePins.ids.filter(id=>id!==p.threadId);
   if(p.sectionId===pinnedSection)nativePins.ids.push(p.threadId);
   nativePins.sections={...nativePins.sections,[p.threadId]:p.sectionId};
   nativePins.moved=true;fs.writeFileSync(pinPath,JSON.stringify(nativePins));
   event('thread/section/changed',{threadId:p.threadId,sectionId:p.sectionId});break;
 case 'thread/start':{
   const id=crypto.randomUUID();const thread={id,cwd:p.cwd,name:fs.existsSync(__dirname+'/naming-fixture')?null:'隔离会话',ephemeral:false,threadSource:'user',source:'appServer',
     projectId:p.projectId,attachedToThreadId:null,status:{type:'idle'},turns:[],createdAt:1,updatedAt:1,path:__dirname+'/'+id+'.jsonl'};
   modelContexts.set(id,{items:[],developerInstructions:p.developerInstructions||'',model:p.model||'gpt-6.1-sol',
     reasoningEffort:p.config?.model_reasoning_effort||'high',permissions:permissionSnapshot(p)});
   threads.set(id,thread);result={thread,model:'gpt-6.1-sol',reasoningEffort:'high',cwd:p.cwd};
   event('thread/started',{thread});break;
 }
 case 'thread/fork':{
   if(!t||t.ephemeral){output({id:q.id,error:{code:-32600,message:'invalid fork parent'}});return;}
   const cutoff=p.lastTurnId?t.turns.findIndex(turn=>turn.id===p.lastTurnId):t.turns.length-1;
   if(p.lastTurnId&&(cutoff<0||t.turns[cutoff].status==='inProgress')){
     output({id:q.id,error:{code:-32600,message:'invalid fork turn'}});return;
   }
   const id=crypto.randomUUID(),ephemeral=p.ephemeral===true;
   const turns=ephemeral||p.attachToSource?[]:structuredClone(t.turns.slice(0,cutoff+1));
   if(!p.lastTurnId&&turns.at(-1)?.status==='inProgress')turns.at(-1).status='interrupted';
   const thread={...t,id,ephemeral,path:ephemeral?null:__dirname+'/'+id+'.jsonl',forkedFromId:p.threadId,
     parentThreadId:null,attachedToThreadId:p.attachToSource?p.threadId:null,turns,status:{type:'idle'}};
   // fork 的界面轮次可以为空，但模型历史必须保留父上下文，避免测试漏掉主任务被继续执行的问题。
   const inherited=modelContexts.get(p.threadId)||{items:[],developerInstructions:''};
   const permissions={...permissionSnapshot(p,inherited.permissions),...sidePolicy.forkPermissions};
   const model=p.model??inherited.model??'gpt-6.1-sol',reasoningEffort=p.config?.model_reasoning_effort??inherited.reasoningEffort??'high';
   modelContexts.set(id,{...inherited,items:[...inherited.items],developerInstructions:p.developerInstructions??inherited.developerInstructions,
     model,reasoningEffort,permissions});
   threads.set(id,thread);result={thread:{...thread,turns:p.excludeTurns?[]:thread.turns},model,reasoningEffort,cwd:t.cwd,...permissions};
   event('thread/started',{thread});break;
 }
 case 'thread/inject_items':{
   if(!t){output({id:q.id,error:{code:-32600,message:'unknown thread'}});return;}
   if(sidePolicy.boundaryDelayMs){
     setTimeout(()=>{modelContexts.get(p.threadId)?.items.push(...p.items);output({id:q.id,result:{}});},sidePolicy.boundaryDelayMs);return;
   }
   if(!sidePolicy.boundaryError||sidePolicy.acceptBoundaryBeforeError)modelContexts.get(p.threadId).items.push(...p.items);
   if(sidePolicy.boundaryError){output({id:q.id,error:{code:sidePolicy.boundaryError,message:'side boundary fixture failure'}});return;}
   break;
 }
 case 'thread/unsubscribe':
   if(sidePolicy.idleEventBurst)for(let index=0;index<1024;index++)event('thread/status/changed',{threadId:'idle-background',status:{type:'idle'}});
   if(sidePolicy.idleUnsubscribeError){output({id:q.id,error:{code:-32000,message:'idle unsubscribe outcome unknown'}});return;}
   if(t?.ephemeral&&sidePolicy.unsubscribeError){output({id:q.id,error:{code:sidePolicy.unsubscribeError,message:'side unsubscribe fixture failure'}});return;}
   if(!t){result={status:'notLoaded'};break;}
   if(t.ephemeral){threads.delete(p.threadId);modelContexts.delete(p.threadId);}else t.status={type:'notLoaded'};
   event('thread/closed',{threadId:p.threadId});result={status:'unsubscribed'};break;
 case 'thread/read': if(!t){output({id:q.id,error:{code:-32600,message:'unknown thread'}});return;}
   if(sidePolicy.readFailures?.includes(p.threadId)){output({id:q.id,error:{code:-32000,message:'isolated side read failure'}});return;}
   if(fastPolicy.finishCompactionRead){
     const count=(boundaryReads.get(p.threadId)||0)+1;boundaryReads.set(p.threadId,count);
     if(count===2){const snapshot=structuredClone(t);finish(p.threadId);
       setTimeout(()=>output({id:q.id,result:{thread:snapshot,readState:{firstUnread:null,revision:'r1'}}}),200);return;}
   }
   if(nativePins?.confirmFailure&&nativePins.moved){output({id:q.id,error:{code:-32000,message:'confirmation failed'}});return;}
   if(t.ephemeral&&p.includeTurns&&related.snapshotError){output({id:q.id,error:related.snapshotError});return;}
   const sectionId=nativePins?.ids.includes(p.threadId)?pinnedSection:nativePins?.sections?.[p.threadId];
   const readThread=related.rows.includes(t)?{...t,turns:p.includeTurns?t.turns||[]:[]}:t;
   result={thread:nativePins?{...readThread,section:sectionId?{id:sectionId,name:'Section'}:null}
     :{...readThread,...sidePolicy.readOverrides?.[p.threadId]},
     readState:{firstUnread:null,revision:'r1'}};break;
 case 'thread/resume':
   if(t?.name==='external-writer'){output({id:q.id,error:{code:-32600,message:`thread ${p.threadId} already has an active writer`}});return;}
   result={thread:t,model:'gpt-6.1-sol',reasoningEffort:'high'};break;
 case 'thread/turns/list':
   if(t?.ephemeral&&related.ephemeralHistoryUnavailable){output({id:q.id,error:{code:-32600,message:'ephemeral threads do not support thread/turns/list'}});return;}
   result=pageRows(t?.turns||[],p,'turns');break;
 case 'thread/items/list':{
   result=pageRows(t.turns.find(turn=>turn.id===p.turnId)?.items||[],p,'items');
   result.data=result.data.map(item=>({turnId:t.foreignHistoryTurn?'unrelated-turn':p.turnId,item}));
   if(t.historyCursorLoop&&p.cursor==='items:80')result.nextCursor='items:40';
   break;
 }
 case 'test/seedHistory':
   if(p.finish){finish(p.threadId,p.status||'completed');break;}
   if(p.nativeStatus&&t){t.status=p.nativeStatus;event('thread/status/changed',{threadId:p.threadId,status:t.status});break;}
   for(const frame of p.events||[])output(frame);
   for(const context of p.contexts||[])if(t?.path)fs.appendFileSync(t.path,JSON.stringify({type:'turn_context',payload:context})+'\n');
   if(!p.turns)break;
   t.turns=p.turns;t.historyCursorLoop=p.cursorLoop===true;t.foreignHistoryTurn=p.foreignTurn===true;
   t.status={type:t.turns.some(turn=>turn.status==='inProgress')?'active':'idle'};
   event('thread/status/changed',{threadId:p.threadId,status:t.status});break;
 case 'thread/goal/get':result={goal:goals.get(p.threadId)||null};break;
 case 'thread/goal/set':{
   const goal={...(goals.get(p.threadId)||{createdAt:1,tokensUsed:0,timeUsedSeconds:0,tokenBudget:null}),...p,updatedAt:2};
   goals.set(p.threadId,goal);result={goal};event('thread/goal/updated',{threadId:p.threadId,goal});break;
 }
 case 'thread/goal/clear':goals.delete(p.threadId);event('thread/goal/cleared',{threadId:p.threadId});break;
 case 'turn/start':{
   const context=modelContexts.get(p.threadId);
   if(context){
     context.model=p.model??context.model;context.reasoningEffort=p.effort??context.reasoningEffort;
     context.permissions=permissionSnapshot(p,context.permissions);
     context.items.push({type:'message',role:'user',content:p.input.map(item=>item.type==='text'?{type:'input_text',text:item.text}:item)});
     fs.appendFileSync(__dirname+'/model-inputs.jsonl',JSON.stringify({threadId:p.threadId,...context})+'\n');
   }
   const turn={id:crypto.randomUUID(),status:'inProgress',items:[{id:crypto.randomUUID(),type:'userMessage',content:p.input,clientUserMessageId:p.clientUserMessageId}]};
   if(t.path)fs.appendFileSync(t.path,JSON.stringify({type:'turn_context',payload:{turn_id:turn.id,
     collaboration_mode:p.collaborationMode||{mode:'default',settings:{developer_instructions:null}}}})+'\n');
   t.turns.push(turn);t.status={type:'active'};logTurn(t,'task_started',turn);
   event('turn/started',{threadId:p.threadId,turn});result={turn};
   const prompt=p.input.find(i=>i.type==='text')?.text||'';
   if(prompt.includes('uncertain')){
     setTimeout(()=>finish(p.threadId),20);
     output({id:q.id,error:{code:-32603,message:'test failure after acceptance'}});return;
   }
   if(prompt.includes('approval')){
     approval={threadId:p.threadId};output({id:q.id,method:'item/commandExecution/requestApproval',
       params:{threadId:p.threadId,turnId:turn.id,itemId:'command-a',command:'echo test',availableDecisions:['accept','decline']}});
   }else if(!prompt.includes('wait'))setTimeout(()=>finish(p.threadId),20);
   break;
 }
 case 'turn/steer':result={turnId:t.turns.at(-1).id};break;
 case 'turn/interrupt':finish(p.threadId,'interrupted');break;
 case 'thread/compact/start':{
   if(fastPolicy.compactReject){output({id:q.id,error:{code:-32600,message:'compaction rejected before execution'}});return;}
   if(fastPolicy.nativeCompactionTurns){
     const compact=()=>{
       const turn={id:crypto.randomUUID(),status:'inProgress',items:[]},item={id:crypto.randomUUID(),type:'contextCompaction'};
       t.turns.push(turn);t.status={type:'active'};logTurn(t,'task_started',turn);
       event('turn/started',{threadId:p.threadId,turn});
       event('item/started',{threadId:p.threadId,turnId:turn.id,item});
       setTimeout(()=>{
         if(turn.status!=='inProgress')return;
         const status=fastPolicy.compactOutcome||'completed';
         if(status==='completed')event('item/completed',{threadId:p.threadId,turnId:turn.id,item});
         finish(p.threadId,status);
       },fastPolicy.compactDelayMs||20);
     };
     if(fastPolicy.compactReplyFirst){output({id:q.id,result:{}});setImmediate(compact);return;}
     compact();break;
   }
   event('item/started',{threadId:p.threadId,turnId:'compact',item:{id:'compact',type:'contextCompaction'}});
   setTimeout(()=>event('item/completed',{threadId:p.threadId,turnId:'compact',item:{id:'compact',type:'contextCompaction'}}),fastPolicy.compactDelayMs||20);break;
 }
 case 'thread/name/set':
   if(t.forkedFromId&&t.name==='reject-fork-name'){output({id:q.id,error:{code:-32000,message:'name fixture failure'}});return;}
   t.name=p.name;break;
 case 'thread/revert':{
   const index=t.turns.findIndex(turn=>turn.id===p.beforeTurnId);
   if(index<0||t.turns.some(turn=>turn.status==='inProgress')){
     output({id:q.id,error:{code:-32600,message:'invalid history revert'}});return;
   }
   t.turns=t.turns.slice(0,index);result={thread:t};
   event('thread/reverted',{threadId:p.threadId});break;
 }
  case 'thread/delete':
    if(t?.name==='reject-delete'){output({id:q.id,error:{code:-32001,message:'fixture deletion rejected'}});return;}
    if(t?.name==='busy-delete'){output({id:q.id,error:{code:-32600,message:`thread ${p.threadId} already has an active writer`}});return;}
    if(fs.existsSync(__dirname+'/delete-policy.json')){
      const policy=JSON.parse(fs.readFileSync(__dirname+'/delete-policy.json','utf8'));
      const complete=()=>{
        if(policy.blockStore){
          fs.renameSync(__dirname+'/remote-harness-state.json',__dirname+'/harness-before-delete.json');
          fs.mkdirSync(__dirname+'/remote-harness-state.json');
        }
        if(policy.error){output({id:q.id,error:policy.error});return;}
        if(policy.disconnect){process.exit(0);return;}
        if(policy.malformed){output({id:q.id,result:null});return;}
        threads.delete(p.threadId);related.rows=related.rows.filter(row=>row.id!==p.threadId);
        if(policy.notification)event('thread/deleted',{threadId:p.threadId});
        output({id:q.id,result:{}});
      };
      setTimeout(complete,policy.delayMs||0);return;
    }
    threads.delete(p.threadId);related.rows=related.rows.filter(row=>row.id!==p.threadId);break;
 case 'thread/readState/update':result={readState:{firstUnread:null,revision:'r2'}};break;
 case 'skills/list':result={data:p.cwds.map(cwd=>({cwd,skills:[],errors:[]}))};break;
 case 'model/list':result={data:[{id:'gpt-6.1-sol',model:'gpt-6.1-sol',serviceTiers:fastPolicy.noTiers?[]:[{id:'priority',name:'Fast'}],
   ...(fastPolicy.modelFlagsDisabled?{supports_fast:false,fastModeSupported:false}:{}),
   supportedReasoningEfforts:[{reasoningEffort:'high'}]}],nextCursor:null};break;
 case 'account/read':result={account:{type:fastPolicy.accountType||'apiKey'}};break;
 case 'configRequirements/read':result=fastPolicy.requirementsMalformed?{requirements:'unknown'}:fastPolicy.requirementsUnknown?{}:
   {requirements:fastPolicy.requirementsDenied?{featureRequirements:{fast_mode:false}}:null};break;
 case 'config/read':result={config:{model:'gpt-6.1-sol',model_reasoning_effort:'high',model_provider:fastPolicy.provider||'test',
   developer_instructions:sidePolicy.developerInstructions??null,
   approval_policy:fastPolicy.approvalPolicy||'on-request',sandbox_mode:fastPolicy.sandboxMode||'workspace-write',
   sandbox_workspace_write:{writable_roots:[],network_access:false},
   features:{fast_mode:fastPolicy.configFast!==false},service_tier:fastPolicy.defaultTier||null}};break;
 default: output({id:q.id,error:{code:-32601,message:'unsupported: '+q.method}});return;
 }
 if(q.method==='thread/read'&&sidePolicy.idleReadDelayMs&&p.threadId===sidePolicy.idleReadId){
   setTimeout(()=>output({id:q.id,result}),sidePolicy.idleReadDelayMs);return;
 }
 output({id:q.id,result});
});
"#).unwrap();
        let mut command = tokio::process::Command::new("node");
        command
            .arg(&script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(crate::windows_create_hidden_process());
        let mut process =
            crate::codex_service::RunningService::for_test(command.spawn().unwrap()).unwrap();
        process.codex_home = Some(directory.path().join("codex-home"));
        authorization
            .controller
            .harness
            .install_test_process(process)
            .await;
        (directory, authorization)
    }

    #[tokio::test]
    async fn harness_history_running_turn_restores_all_messages() {
        let (directory, authorization, id) =
            harness_history_fixture("inProgress", false, false).await;
        let controller = &authorization.controller;
        let history = controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        let turn = &history["data"][0];
        assert_eq!(turn["items"].as_array().unwrap().len(), 114);
        assert_eq!(turn["items"][0]["content"][0]["text"], "继续");
        assert_eq!(turn["items"][113]["id"], "item-113");
        assert_eq!(turn["itemsView"], "full");
        assert_eq!(turn["itemsComplete"], true);
        assert_eq!(turn["itemsPageComplete"], true);
        assert!(turn["itemsNextCursor"].is_null());
        let requests = harness_history_requests(directory.path());
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1]["params"]["cursor"], "items:40");
        assert_eq!(requests[2]["params"]["cursor"], "items:80");
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_history_finished_turn_keeps_input_and_advances_item_cursor() {
        for status in ["completed", "interrupted"] {
            let (directory, authorization, id) =
                harness_history_fixture(status, false, false).await;
            let controller = &authorization.controller;
            let history = controller
                .call_authorized("history", json!({"threadId":id}), &authorization)
                .await
                .unwrap();
            let turn = &history["data"][0];
            assert_eq!(turn["items"].as_array().unwrap().len(), 42);
            assert_eq!(turn["items"][0]["content"][0]["text"], "继续");
            assert_eq!(turn["items"][0]["remoteHistoryContext"], true);
            assert_eq!(turn["itemsView"], "summary");
            assert_eq!(turn["itemsComplete"], false);
            assert_eq!(turn["itemsPageComplete"], true);
            assert_eq!(turn["itemsNextCursor"], "items:40");
            assert_eq!(turn["newestSnapshotItemId"], "item-113");
            assert_eq!(
                history["itemsPaginationByTurnId"]["history-turn"]["nextCursor"],
                turn["itemsNextCursor"]
            );
            let mut ids: std::collections::HashSet<String> = turn["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["id"].as_str().unwrap().to_owned())
                .collect();
            let mut cursor = turn["itemsNextCursor"].clone();
            for expected in ["items:80", ""] {
                let page = controller.call_authorized("history",
                    json!({"threadId":id,"turnId":"history-turn","itemCursor":cursor,"turnStatus":status}),
                    &authorization).await.unwrap();
                assert_ne!(page["itemsNextCursor"], cursor);
                assert_eq!(page["itemsNextCursor"].as_str().unwrap_or(""), expected);
                ids.extend(
                    page["data"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|item| item["id"].as_str().unwrap().to_owned()),
                );
                cursor = page["itemsNextCursor"].clone();
                assert_eq!(page["itemsComplete"], cursor.is_null());
            }
            assert_eq!(ids.len(), 114);
            let requests = harness_history_requests(directory.path());
            assert_eq!(requests.len(), 4, "终态轮次只补开头，较早过程仍按需翻页");
            assert_eq!(requests[1]["params"]["sortDirection"], "asc");
            assert_eq!(requests[2]["params"]["cursor"], "items:40");
            assert_eq!(requests[3]["params"]["cursor"], "items:80");
            controller.harness.enabled.store(false, Ordering::Release);
            controller.harness.release().await;
        }
    }

    #[tokio::test]
    async fn harness_history_rejects_cursor_cycles_and_invalid_client_cursor() {
        let (directory, authorization, id) =
            harness_history_fixture("inProgress", true, false).await;
        let controller = &authorization.controller;
        let error = controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("游标循环"));
        assert_eq!(harness_history_requests(directory.path()).len(), 3);
        let error = controller
            .call_authorized(
                "history",
                json!({"threadId":id,"turnId":"history-turn","itemCursor":42}),
                &authorization,
            )
            .await
            .unwrap_err();
        assert_eq!(error.downcast_ref::<ApiError>().unwrap().code, 400);
        assert_eq!(harness_history_requests(directory.path()).len(), 3);
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_history_rejects_items_from_another_turn() {
        let (_directory, authorization, id) =
            harness_history_fixture("completed", false, true).await;
        let controller = &authorization.controller;
        let error = controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("归属"));
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    async fn harness_history_fixture(
        status: &str,
        cursor_loop: bool,
        foreign_turn: bool,
    ) -> (tempfile::TempDir, HarnessAuthorization, String) {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let created = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"ready"}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap().to_owned();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let items: Vec<_> = (0..114).map(|index| {
            let item_id = format!("item-{index:03}");
            if index == 0 {
                json!({"id":item_id,"type":"userMessage","content":[{"type":"text","text":"继续"}]})
            } else if index == 1 || index == 113 {
                json!({"id":item_id,"type":"agentMessage","text":if index==1 {"开始处理"} else {"处理完成"}})
            } else {
                json!({"id":item_id,"type":"commandExecution","command":"echo check","status":"completed"})
            }
        }).collect();
        controller.harness.seed_test_history(
            json!({"threadId":id,"turns":[{"id":"history-turn","status":status,"startedAt":1000,"items":items}],
                "cursorLoop":cursor_loop,"foreignTurn":foreign_turn})).await.unwrap();
        (directory, authorization, id)
    }

    fn harness_engine_requests(directory: &FsPath) -> Vec<Value> {
        std::fs::read_to_string(directory.join("requests.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn harness_external_writer_read_preserves_history_and_recovers_without_resume() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let id = uuid::Uuid::new_v4().to_string();
        controller
            .harness
            .seed_test_subagents(json!({"rows":[{
                "id":id,"name":"外部持有的会话","cwd":directory.path(),"ephemeral":false,
                "threadSource":"user","source":"vscode","status":{"type":"idle"},
                "turns":[],"createdAt":1,"updatedAt":1
            }]}))
            .await
            .unwrap();
        let read = || controller.call_authorized("thread", json!({"threadId":id}), &authorization);
        let initial = read().await.unwrap();
        assert_eq!(initial["thread"]["externalWriter"], false);
        let locks = directory.path().join("codex-home/thread-writer-locks");
        assert!(!locks.exists(), "读取不能创建原生锁目录");
        std::fs::create_dir_all(&locks).unwrap();
        let lock_path = locks.join(format!("{id}.lock"));
        let owner = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .unwrap();
        std::fs::write(&lock_path, b"native-lock-marker").unwrap();
        assert_eq!(
            read().await.unwrap()["thread"]["externalWriter"],
            false,
            "未被加锁的残留文件不能显示外部占用"
        );
        owner.lock().unwrap();
        let blocked = read().await.unwrap();
        assert_eq!(blocked["thread"]["externalWriter"], true);
        assert_eq!(blocked["thread"]["readOnly"], true);
        assert_eq!(
            blocked["thread"]["readOnlyReason"],
            "请先在那边关闭会话，才能在这里继续。"
        );
        controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        owner.unlock().unwrap();
        let released = read().await.unwrap();
        assert_eq!(released["thread"]["externalWriter"], false);
        assert_eq!(released["thread"]["readOnly"], false);
        assert_eq!(std::fs::read(&lock_path).unwrap(), b"native-lock-marker");
        assert!(
            harness_engine_requests(directory.path())
                .iter()
                .all(|frame| !matches!(
                    frame["method"].as_str(),
                    Some(
                        "thread/resume"
                            | "turn/start"
                            | "thread/fork"
                            | "thread/delete"
                            | "thread/unsubscribe"
                    )
                )),
            "检查和重试只读取，不产生写操作"
        );
        // 模拟原生连接已由本服务恢复并持有 writer；同代次的锁不能阻止本服务继续输入。
        controller
            .call_authorized(
                "send",
                json!({"threadId":id,"prompt":"测试自身写入者"}),
                &authorization,
            )
            .await
            .unwrap();
        owner.lock().unwrap();
        let own = read().await.unwrap();
        assert_eq!(own["thread"]["externalWriter"], false);
        assert_eq!(own["thread"]["readOnly"], false);
        owner.unlock().unwrap();
        drop(owner);
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    fn harness_history_requests(directory: &FsPath) -> Vec<Value> {
        harness_engine_requests(directory)
            .into_iter()
            .filter(|frame| frame["method"] == "thread/items/list")
            .collect()
    }

    #[tokio::test]
    async fn harness_fork_history_is_persistent_inclusive_paginated_and_idempotent() {
        let (directory, authorization, id) =
            harness_history_fixture("completed", false, false).await;
        let controller = &authorization.controller;
        let turns: Vec<_> = (0..105).map(|index| json!({
            "id":format!("fork-turn-{index}"),"status":"completed",
            "items":[{"id":format!("fork-user-{index}"),"type":"userMessage",
                "content":[{"type":"text","text":format!("问题 {index}")}]},
                {"id":format!("fork-agent-{index}"),"type":"agentMessage","text":format!("回复 {index}")}]
        })).collect();
        controller
            .harness
            .seed_test_history(json!({"threadId":id,"turns":turns}))
            .await
            .unwrap();
        let payload = json!({"threadId":id,"turnId":"fork-turn-1"});
        let mut receipt = controller
            .execute(
                authorization.device.clone(),
                "fork-once".into(),
                "forkThread".into(),
                payload.clone(),
            )
            .await
            .unwrap();
        // HTTP 等待窗口可能先返回 pending；后续只能核对同一请求，不能重新生成分支操作 ID。
        tokio::time::timeout(Duration::from_secs(90), async {
            while receipt["state"] == "pending" {
                tokio::time::sleep(Duration::from_millis(100)).await;
                receipt = controller
                    .execute(
                        authorization.device.clone(),
                        "fork-once".into(),
                        "forkThread".into(),
                        payload.clone(),
                    )
                    .await
                    .unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(receipt["state"], "completed");
        let result = &receipt["result"];
        let branch = result["threadId"].as_str().unwrap();
        assert_ne!(branch, id);
        assert_eq!(result["sourceThreadId"], id);
        let replay = controller
            .execute(
                authorization.device.clone(),
                "fork-once".into(),
                "forkThread".into(),
                payload,
            )
            .await
            .unwrap();
        assert_eq!(replay, receipt);
        let history = controller
            .call_authorized("history", json!({"threadId":branch}), &authorization)
            .await
            .unwrap();
        assert_eq!(history["data"].as_array().unwrap().len(), 2);
        assert_eq!(history["data"][0]["id"], "fork-turn-1");
        assert_eq!(history["data"][1]["id"], "fork-turn-0");
        let detail = controller
            .call_authorized("thread", json!({"threadId":branch}), &authorization)
            .await
            .unwrap();
        assert_eq!(detail["thread"]["ephemeral"], false);
        assert_eq!(detail["thread"]["capabilities"]["forkThread"], true);
        assert_eq!(detail["thread"]["forkedFrom"]["threadId"], id);
        assert_eq!(detail["thread"]["title"], "隔离会话 (2)");
        assert_eq!(
            std::fs::canonicalize(detail["thread"]["cwd"].as_str().unwrap()).unwrap(),
            std::fs::canonicalize(directory.path()).unwrap()
        );
        let original = controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(original["data"][0]["id"], "fork-turn-104");
        let persisted: Value = serde_json::from_slice(
            &std::fs::read(directory.path().join("remote-harness-state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(persisted["forkSources"][branch]["threadId"], id);
        let before_latest = harness_engine_requests(directory.path());
        let write_count = before_latest
            .iter()
            .filter(|frame| frame["method"] == "turn/start")
            .count();
        let latest = controller
            .call_authorized("forkThread", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        let latest_detail = controller
            .call_authorized(
                "thread",
                json!({"threadId":latest["threadId"]}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(latest_detail["thread"]["title"], "隔离会话 (3)");
        let latest_history = controller
            .call_authorized(
                "history",
                json!({"threadId":latest["threadId"]}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(latest_history["data"][0]["id"], "fork-turn-104");
        let requests = harness_engine_requests(directory.path());
        let forks: Vec<_> = requests
            .iter()
            .filter(|frame| frame["method"] == "thread/fork")
            .collect();
        assert_eq!(forks.len(), 2);
        assert_eq!(forks[0]["params"]["lastTurnId"], "fork-turn-1");
        assert_eq!(forks[0]["params"]["ephemeral"], false);
        assert_eq!(forks[0]["params"]["excludeTurns"], true);
        assert!(forks[1]["params"].get("lastTurnId").is_none());
        assert_eq!(
            requests
                .iter()
                .filter(|frame| frame["method"] == "turn/start")
                .count(),
            write_count
        );
        assert!(
            !requests
                .iter()
                .any(|frame| frame["method"] == "thread/revert")
        );
        assert!(
            requests
                .iter()
                .any(|frame| frame["method"] == "thread/turns/list"
                    && frame["params"]["cursor"] == "turns:100")
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_fork_guards_side_running_foreign_turn_and_destination() {
        let (directory, authorization, id) =
            harness_history_fixture("inProgress", false, false).await;
        let controller = &authorization.controller;
        for payload in [
            json!({"threadId":id,"turnId":"history-turn"}),
            json!({"threadId":id,"turnId":"foreign-turn"}),
            json!({"threadId":id,"cwd":"C:/other"}),
            json!({"threadId":id,"worktree":true}),
            json!({"threadId":id,"ephemeral":true}),
        ] {
            assert!(
                controller
                    .call_authorized("forkThread", payload, &authorization)
                    .await
                    .is_err()
            );
        }
        assert!(
            !harness_engine_requests(directory.path())
                .iter()
                .any(|frame| frame["method"] == "thread/fork")
        );
        // 最新分支不停止源会话；只有新分支中的执行轮次变成中断快照。
        let latest = controller
            .call_authorized("forkThread", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        let original = controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(original["data"][0]["status"], "inProgress");
        let branch = controller
            .call_authorized(
                "history",
                json!({"threadId":latest["threadId"]}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(branch["data"][0]["status"], "interrupted");
        let side = controller
            .call_authorized("createSideThread", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        let side_id = side["threadId"].as_str().unwrap();
        assert!(
            controller
                .call_authorized("forkThread", json!({"threadId":side_id}), &authorization)
                .await
                .is_err()
        );
        let requests = harness_engine_requests(directory.path());
        assert!(!requests.iter().any(
            |frame| frame["method"] == "thread/fork" && frame["params"]["threadId"] == side_id
        ));
        assert!(
            !requests
                .iter()
                .any(|frame| frame["method"] == "turn/interrupt")
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_fork_name_failure_keeps_created_thread_and_inherits_permissions() {
        let (directory, authorization, id) =
            harness_history_fixture("completed", false, false).await;
        let controller = &authorization.controller;
        controller
            .call_authorized(
                "rename",
                json!({"threadId":id,"title":"reject-fork-name"}),
                &authorization,
            )
            .await
            .unwrap();
        controller
            .call_authorized(
                "setPermissions",
                json!({"threadId":id,"permissionMode":"full-access"}),
                &authorization,
            )
            .await
            .unwrap();
        let result = controller
            .call_authorized("forkThread", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(result["ok"], true);
        assert!(
            result["warning"]
                .as_str()
                .unwrap()
                .contains("标题设置未确认")
        );
        let branch = result["threadId"].as_str().unwrap();
        let detail = controller
            .call_authorized("thread", json!({"threadId":branch}), &authorization)
            .await
            .unwrap();
        assert_eq!(detail["thread"]["permissionMode"], "full-access");
        let requests = harness_engine_requests(directory.path());
        let fork = requests
            .iter()
            .find(|frame| frame["method"] == "thread/fork")
            .unwrap();
        assert_eq!(fork["params"]["sandbox"], "danger-full-access");
        assert_eq!(
            requests
                .iter()
                .filter(|frame| frame["method"] == "thread/fork")
                .count(),
            1
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_history_edit_reverts_paginated_suffix_and_preserves_original_inputs() {
        let (directory, authorization, id) =
            harness_history_fixture("completed", false, false).await;
        let controller = &authorization.controller;
        let turns: Vec<_> = (0..105).map(|index| json!({
            "id":format!("round-{index}"),"status":"completed","startedAt":1000+index,
            "items":[{"id":format!("user-{index}"),"type":"userMessage","content":[
                {"type":"text","text":format!("问题 {index}"),"text_elements":[{"start":0,"end":1}]},
                {"type":"image","url":"data:image/png;base64,fixture"},
                {"type":"skill","name":"review","path":"C:/fixture/SKILL.md"}
            ]}]
        })).collect();
        controller
            .harness
            .seed_test_history(json!({"threadId":id,"turns":turns}))
            .await
            .unwrap();
        let detail = controller
            .call_authorized("thread", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(detail["thread"]["capabilities"]["messageEditHistory"], true);
        assert_eq!(detail["thread"]["messageEditReady"], true);
        let first_page = controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(
            first_page["data"][0]["items"][0]["messageMetadata"]["editable"],
            true
        );
        let older = controller
            .call_authorized(
                "history",
                json!({"threadId":id,"cursor":"turns:102"}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(older["data"][1]["id"], "round-1");
        assert_eq!(
            older["data"][1]["items"][0]["messageMetadata"]["editable"],
            true
        );
        let payload = json!({"threadId":id,"turnId":"round-1","messageId":"user-1",
            "historyHead":detail["thread"]["editHistoryHead"],"clientUserMessageId":"historical-edit",
            "prompt":"修改较早的问题","model":"gpt-6.1-sol","reasoningEffort":"low"});
        let result = controller
            .call_authorized("editMessage", payload.clone(), &authorization)
            .await
            .unwrap();
        assert_eq!(result["deliveryState"], "accepted");
        assert_eq!(result["clientUserMessageId"], "historical-edit");
        let replay = controller
            .call_authorized("editMessage", payload.clone(), &authorization)
            .await
            .unwrap();
        assert_eq!(replay, result);
        let history = controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(history["data"].as_array().unwrap().len(), 2);
        assert_eq!(history["data"][1]["id"], "round-0");
        assert!(!history["data"].to_string().contains("user-1\""));
        let requests = harness_engine_requests(directory.path());
        let edits: Vec<_> = requests
            .iter()
            .filter(|frame| frame["method"] == "thread/revert")
            .collect();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0]["params"]["beforeTurnId"], "round-1");
        assert!(
            requests
                .iter()
                .any(|frame| frame["method"] == "thread/turns/list"
                    && frame["params"]["cursor"] == "turns:100")
        );
        let start = requests
            .iter()
            .rfind(|frame| frame["method"] == "turn/start")
            .unwrap();
        assert_eq!(start["params"]["input"][0]["text"], "修改较早的问题");
        assert_eq!(start["params"]["input"][0]["text_elements"], json!([]));
        assert_eq!(
            start["params"]["input"].as_array().unwrap()[1..],
            turns[1]["items"][0]["content"].as_array().unwrap()[1..]
        );
        assert_eq!(start["params"]["effort"], "low");
        assert!(!requests.iter().any(
            |frame| frame["method"] == "thread/read" && frame["params"]["includeTurns"] == true
        ));
        let mut collision = payload;
        collision["prompt"] = json!("同 ID 不同内容");
        assert!(
            controller
                .call_authorized("editMessage", collision, &authorization)
                .await
                .is_err()
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_history_edit_rejects_stale_foreign_and_running_targets_before_revert() {
        let (directory, authorization, id) =
            harness_history_fixture("completed", false, false).await;
        let controller = &authorization.controller;
        let detail = controller
            .call_authorized("thread", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        let baseline = json!({"threadId":id,"turnId":"history-turn","messageId":"item-000",
            "historyHead":detail["thread"]["editHistoryHead"],"clientUserMessageId":"stale-edit","prompt":"新内容"});
        let mut stale = baseline.clone();
        stale["historyHead"] = json!("old-history-head");
        assert!(
            controller
                .call_authorized("editMessage", stale, &authorization)
                .await
                .is_err()
        );
        let mut foreign = baseline.clone();
        foreign["messageId"] = json!("another-message");
        assert!(
            controller
                .call_authorized("editMessage", foreign, &authorization)
                .await
                .is_err()
        );
        controller.harness.seed_test_history(json!({"threadId":id,"turns":[{
            "id":"history-turn","status":"inProgress","items":[{"id":"item-000","type":"userMessage",
                "content":[{"type":"text","text":"继续"}]}]
        }]})).await.unwrap();
        assert!(
            controller
                .call_authorized("editMessage", baseline, &authorization)
                .await
                .is_err()
        );
        assert!(
            !harness_engine_requests(directory.path())
                .iter()
                .any(|frame| frame["method"] == "thread/revert")
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_history_edit_unknown_send_confirms_only_the_same_new_message_without_replay() {
        let (directory, authorization, id) =
            harness_history_fixture("completed", false, false).await;
        let controller = &authorization.controller;
        let detail = controller
            .call_authorized("thread", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        let payload = json!({"threadId":id,"turnId":"history-turn","messageId":"item-000",
            "historyHead":detail["thread"]["editHistoryHead"],"clientUserMessageId":"uncertain-edit",
            "prompt":"uncertain 历史编辑"});
        assert!(
            controller
                .call_authorized("editMessage", payload.clone(), &authorization)
                .await
                .is_err()
        );
        let replay = controller
            .call_authorized("editMessage", payload.clone(), &authorization)
            .await
            .unwrap();
        assert_eq!(replay["outcomeUnknown"], true);
        let state = controller
            .call_authorized(
                "sendState",
                json!({"threadId":id,"clientUserMessageId":"uncertain-edit"}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(state["deliveryState"], "accepted");
        assert_eq!(state["clientUserMessageId"], "uncertain-edit");
        let replay = controller
            .call_authorized("editMessage", payload, &authorization)
            .await
            .unwrap();
        assert_eq!(replay, state);
        assert_eq!(
            harness_engine_requests(directory.path())
                .iter()
                .filter(|frame| frame["method"] == "thread/revert")
                .count(),
            1
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_delete_receipt_confirms_success_and_replays_without_deleting_again() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let created = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"ready"}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let delete = || {
            controller.execute(
                authorization.device.clone(),
                "delete-service".into(),
                "deleteThread".into(),
                json!({"threadId":id}),
            )
        };
        let first = delete().await.unwrap();
        assert_eq!(first["state"], "completed");
        assert_eq!(first["result"]["ok"], true);
        assert_eq!(first["result"]["threadId"], id);
        assert_eq!(first["result"]["deleted"], true);
        assert_eq!(first["result"]["status"], "local_deleted");
        assert_eq!(delete().await.unwrap(), first);
        let persisted: Value = serde_json::from_slice(
            &std::fs::read(directory.path().join("remote-access-receipts.json")).unwrap(),
        )
        .unwrap();
        let key = format!("{}:delete-service", authorization.device.id);
        assert_eq!(persisted[&key]["result"]["deleted"], true);
        let requests = std::fs::read_to_string(directory.path().join("requests.jsonl")).unwrap();
        assert_eq!(
            requests
                .lines()
                .filter(
                    |line| serde_json::from_str::<Value>(line).unwrap()["method"]
                        == "thread/delete"
                )
                .count(),
            1
        );
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_delete_failure_is_not_upgraded_to_success() {
        let (_directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let created = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"ready"}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        controller
            .call_authorized(
                "rename",
                json!({"threadId":id,"title":"reject-delete"}),
                &authorization,
            )
            .await
            .unwrap();
        let receipt = controller
            .execute(
                authorization.device.clone(),
                "delete-rejected".into(),
                "deleteThread".into(),
                json!({"threadId":id}),
            )
            .await
            .unwrap();
        assert_eq!(receipt["state"], "unknown");
        assert_ne!(receipt["result"]["deleted"], true);
        assert_eq!(receipt["result"]["outcomeUnknown"], true);
        let message = receipt["result"]["message"].as_str().unwrap();
        assert!(message.contains("删除结果尚未确认"));
        assert!(message.contains("fixture deletion rejected"));
        assert!(!message.contains("发送"));
        let thread = controller
            .call_authorized("thread", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(thread["thread"]["id"], id);
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_delete_unloaded_thread_uses_only_app_server_and_keeps_rejection_receipt() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let id = uuid::Uuid::new_v4().to_string();
        // 导入一条由其他客户端创建、从未在本服务 resume 的主会话，覆盖旧 owner IPC 误路由。
        controller
            .harness
            .seed_test_subagents(json!({"rows":[{
                "id":id,"name":"busy-delete","cwd":directory.path(),
                "ephemeral":false,"forkedFromId":null,"threadSource":"user","source":"vscode",
                "status":{"type":"idle"},"turns":[],"createdAt":1,"updatedAt":1
            }]}))
            .await
            .unwrap();
        let delete = |request_id: &str| {
            controller.execute(
                authorization.device.clone(),
                request_id.to_owned(),
                "deleteThread".into(),
                json!({"threadId":id}),
            )
        };
        let rejected = delete("delete-owned").await.unwrap();
        assert_eq!(rejected["state"], "failed");
        assert_eq!(rejected["result"]["code"], 409);
        assert_eq!(rejected["result"]["notSent"], true);
        assert_eq!(
            rejected["result"]["message"],
            "该会话由其他 Codex 持有，当前无法删除。"
        );
        assert_eq!(delete("delete-owned").await.unwrap(), rejected);
        assert_eq!(
            harness_engine_requests(directory.path())
                .iter()
                .filter(|frame| frame["method"] == "thread/delete")
                .count(),
            1,
            "核对原拒绝回执不能重复派发删除"
        );
        // 模拟持有者释放；只允许用户使用新的请求标识明确再次删除，旧失败回执保持原样。
        controller
            .call_authorized(
                "rename",
                json!({"threadId":id,"title":"released"}),
                &authorization,
            )
            .await
            .unwrap();
        let completed = delete("delete-released").await.unwrap();
        assert_eq!(completed["state"], "completed");
        assert_eq!(completed["result"]["deleted"], true);
        assert_eq!(delete("delete-owned").await.unwrap(), rejected);
        let audit = controller.harness.audit.snapshot();
        let records: Vec<_> = audit["records"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|record| record["action"] == "deleteThread")
            .collect();
        assert_eq!(records.len(), 2);
        for record in records {
            assert_eq!(record["transports"], json!(["app-server-stdio"]));
        }
        assert_eq!(
            harness_engine_requests(directory.path())
                .iter()
                .filter(|frame| frame["method"] == "thread/delete")
                .count(),
            2
        );
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    async fn deletion_fixture() -> (tempfile::TempDir, HarnessAuthorization, String) {
        let (directory, authorization) = harness_fixture().await;
        let created = authorization
            .controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"ready"}),
                &authorization,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        (
            directory,
            authorization,
            created["threadId"].as_str().unwrap().to_owned(),
        )
    }

    #[tokio::test]
    async fn harness_delete_parent_closes_idle_service_sides() {
        let (directory, authorization, parent) = deletion_fixture().await;
        let controller = &authorization.controller;
        let mut sides = Vec::new();
        for _ in 0..2 {
            let created = controller
                .call_authorized(
                    "createSideThread",
                    json!({"threadId":parent}),
                    &authorization,
                )
                .await
                .unwrap();
            sides.push(created["threadId"].as_str().unwrap().to_owned());
        }
        let receipt = controller
            .execute(
                authorization.device.clone(),
                "delete-parent-with-sides".into(),
                "deleteThread".into(),
                json!({"threadId":parent}),
            )
            .await
            .unwrap();
        assert_eq!(receipt["state"], "completed");
        assert_eq!(receipt["result"]["deleted"], true);
        assert_eq!(receipt["result"]["threadId"], parent);
        for side in &sides {
            let error = controller
                .call_authorized("thread", json!({"threadId":side}), &authorization)
                .await
                .unwrap_err();
            assert_eq!(error.downcast_ref::<ApiError>().unwrap().code, 410);
        }

        let requests = harness_engine_requests(directory.path());
        let deleted = requests
            .iter()
            .position(|frame| frame["method"] == "thread/delete")
            .unwrap();
        for side in &sides {
            let closed = requests
                .iter()
                .position(|frame| {
                    frame["method"] == "thread/unsubscribe" && frame["params"]["threadId"] == *side
                })
                .unwrap();
            assert!(closed < deleted, "侧边必须先于主会话完成原生关闭");
        }
        assert_eq!(
            requests
                .iter()
                .filter(|frame| frame["method"] == "thread/delete")
                .count(),
            1
        );
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_delete_running_side_is_rejected_until_explicitly_stopped() {
        let (directory, authorization, parent) = deletion_fixture().await;
        let controller = &authorization.controller;
        let created = controller
            .call_authorized(
                "createSideThread",
                json!({"threadId":parent}),
                &authorization,
            )
            .await
            .unwrap();
        let side = created["threadId"].as_str().unwrap().to_owned();
        controller
            .call_authorized(
                "send",
                json!({"threadId":side,"prompt":"wait running side",
                    "clientUserMessageId":"running-side-delete-block"}),
                &authorization,
            )
            .await
            .unwrap();
        let delete = |request_id: &str| {
            controller.execute(
                authorization.device.clone(),
                request_id.into(),
                "deleteThread".into(),
                json!({"threadId":parent}),
            )
        };

        let rejected = delete("delete-running-side-rejected").await.unwrap();
        assert_eq!(rejected["state"], "failed");
        assert_eq!(rejected["result"]["code"], 409);
        assert_eq!(rejected["result"]["notSent"], true);
        assert_eq!(rejected["result"]["message"], "侧边会话执行中，无法删除");
        assert!(
            harness_engine_requests(directory.path())
                .iter()
                .all(|frame| {
                    !matches!(
                        frame["method"].as_str(),
                        Some("thread/delete" | "thread/unsubscribe" | "turn/interrupt")
                    )
                })
        );
        let retained = controller
            .call_authorized("thread", json!({"threadId":side}), &authorization)
            .await
            .unwrap();
        assert_eq!(retained["thread"]["sideThreadClosed"], false);

        controller
            .call_authorized("stop", json!({"threadId":side}), &authorization)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let completed = delete("delete-running-side-after-stop").await.unwrap();
        assert_eq!(completed["state"], "completed");
        assert_eq!(completed["result"]["deleted"], true);
        let requests = harness_engine_requests(directory.path());
        assert_eq!(
            requests
                .iter()
                .filter(|frame| {
                    frame["method"] == "turn/interrupt" && frame["params"]["threadId"] == side
                })
                .count(),
            1,
            "只有用户显式停止侧边才允许发送中断"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|frame| {
                    frame["method"] == "thread/unsubscribe" && frame["params"]["threadId"] == side
                })
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|frame| frame["method"] == "thread/delete")
                .count(),
            1
        );
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_delete_side_close_failure_keeps_parent_and_allows_retry() {
        let (directory, authorization, parent) = deletion_fixture().await;
        let controller = &authorization.controller;
        let created = controller
            .call_authorized(
                "createSideThread",
                json!({"threadId":parent}),
                &authorization,
            )
            .await
            .unwrap();
        let side = created["threadId"].as_str().unwrap().to_owned();
        let policy = directory.path().join("side-policy.json");
        std::fs::write(&policy, json!({"unsubscribeError":-32600}).to_string()).unwrap();
        let failed = controller
            .execute(
                authorization.device.clone(),
                "delete-side-close-failure".into(),
                "deleteThread".into(),
                json!({"threadId":parent}),
            )
            .await
            .unwrap();
        assert_eq!(failed["state"], "failed");
        assert_eq!(failed["result"]["notSent"], true);
        assert!(
            failed["result"]["message"]
                .as_str()
                .unwrap()
                .contains("主会话未删除")
        );
        assert!(
            !harness_engine_requests(directory.path())
                .iter()
                .any(|frame| frame["method"] == "thread/delete")
        );
        let retained = controller
            .call_authorized("thread", json!({"threadId":parent}), &authorization)
            .await
            .unwrap();
        assert_eq!(retained["thread"]["id"], parent);

        std::fs::remove_file(&policy).unwrap();
        let completed = controller
            .execute(
                authorization.device.clone(),
                "delete-side-close-retry".into(),
                "deleteThread".into(),
                json!({"threadId":parent}),
            )
            .await
            .unwrap();
        assert_eq!(completed["state"], "completed");
        assert_eq!(completed["result"]["deleted"], true);
        let requests = harness_engine_requests(directory.path());
        assert_eq!(
            requests
                .iter()
                .filter(|frame| {
                    frame["method"] == "thread/unsubscribe" && frame["params"]["threadId"] == side
                })
                .count(),
            2
        );
        assert_eq!(
            requests
                .iter()
                .filter(|frame| frame["method"] == "thread/delete")
                .count(),
            1
        );
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_delete_concurrent_new_requests_delete_once_and_never_resume() {
        let (directory, authorization, id) = deletion_fixture().await;
        std::fs::write(
            directory.path().join("delete-policy.json"),
            r#"{"delayMs":150,"notification":true}"#,
        )
        .unwrap();
        let controller = &authorization.controller;
        let remove = |request: &str| {
            controller.execute(
                authorization.device.clone(),
                request.into(),
                "deleteThread".into(),
                json!({"threadId":id}),
            )
        };
        let (first, second) = tokio::join!(remove("delete-first"), remove("delete-second"));
        assert_eq!(first.unwrap()["result"]["deleted"], true);
        assert_eq!(second.unwrap()["result"]["deleted"], true);
        let writes = harness_engine_requests(directory.path());
        assert_eq!(
            writes
                .iter()
                .filter(|frame| frame["method"] == "thread/delete")
                .count(),
            1
        );
        assert!(!writes.iter().any(|frame| matches!(
            frame["method"].as_str(),
            Some("thread/resume" | "thread/unsubscribe")
        )));
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_delete_unknown_blocks_new_ids_writes_and_survives_restart() {
        let (directory, authorization, id) = deletion_fixture().await;
        let controller = &authorization.controller;
        std::fs::write(
            directory.path().join("delete-policy.json"),
            r#"{"error":{"code":-32603,"message":"lost deletion result"}}"#,
        )
        .unwrap();
        let first = controller
            .execute(
                authorization.device.clone(),
                "delete-unknown".into(),
                "deleteThread".into(),
                json!({"threadId":id}),
            )
            .await
            .unwrap();
        assert_eq!(first["state"], "unknown");
        let mut other_device = authorization.device.clone();
        other_device.id = "delete-other-device".into();
        controller
            .auth
            .lock()
            .await
            .devices
            .push(other_device.clone());
        let second = controller
            .execute(
                other_device,
                "delete-new".into(),
                "deleteThread".into(),
                json!({"threadId":id}),
            )
            .await
            .unwrap();
        assert_eq!(second["state"], "failed");
        assert_eq!(second["result"]["notSent"], true);
        let before = harness_engine_requests(directory.path()).len();
        for action in [
            "send",
            "rename",
            "compact",
            "pinThread",
            "setFast",
            "createSideThread",
            "setPermissions",
        ] {
            assert!(controller.call_authorized(action,
                json!({"threadId":id,"title":"changed","prompt":"do not send","enabled":true}),
                &authorization).await.is_err(), "{action}");
        }
        assert_eq!(harness_engine_requests(directory.path()).len(), before);
        let reopened = codex::CodexHarness::new(directory.path()).unwrap();
        let rejected = reopened
            .call("deleteThread", json!({"threadId":id}), Some(&authorization))
            .await
            .unwrap_err();
        assert!(rejected.to_string().contains("尚未确认"));
        assert_eq!(
            harness_engine_requests(directory.path())
                .iter()
                .filter(|frame| frame["method"] == "thread/delete")
                .count(),
            1
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_delete_busy_and_preflight_failures_never_dispatch_delete() {
        let (directory, authorization, id) = deletion_fixture().await;
        let controller = &authorization.controller;
        controller
            .call_authorized(
                "send",
                json!({"threadId":id,"prompt":"wait","clientUserMessageId":"wait-delete"}),
                &authorization,
            )
            .await
            .unwrap();
        let busy = controller
            .execute(
                authorization.device.clone(),
                "delete-running".into(),
                "deleteThread".into(),
                json!({"threadId":id}),
            )
            .await
            .unwrap();
        assert_eq!(busy["state"], "failed");
        assert_eq!(busy["result"]["notSent"], true);
        assert!(
            busy["result"]["message"]
                .as_str()
                .unwrap()
                .contains("本次未删除")
        );
        let missing = controller
            .execute(
                authorization.device.clone(),
                "delete-missing".into(),
                "deleteThread".into(),
                json!({"threadId":uuid::Uuid::new_v4().to_string()}),
            )
            .await
            .unwrap();
        assert_eq!(missing["state"], "failed");
        assert_eq!(missing["result"]["notSent"], true);
        assert!(
            missing["result"]["message"]
                .as_str()
                .unwrap()
                .contains("删除前校验失败")
        );
        assert!(
            !harness_engine_requests(directory.path())
                .iter()
                .any(|frame| matches!(
                    frame["method"].as_str(),
                    Some("thread/delete" | "thread/unsubscribe" | "turn/interrupt")
                ))
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_delete_accepts_thread_left_in_system_error() {
        // 用户要求（2026-11-21）：上游或引擎报错后原生状态为 systemError 的会话已不在执行，必须能删除，
        // 不能一直提示“执行状态未确认”。
        let (_directory, authorization, id) = deletion_fixture().await;
        let controller = &authorization.controller;
        controller
            .harness
            .seed_test_history(json!({"threadId":id,"nativeStatus":{"type":"systemError"}}))
            .await
            .unwrap();
        let deleted = controller
            .execute(
                authorization.device.clone(),
                "delete-system-error".into(),
                "deleteThread".into(),
                json!({"threadId":id}),
            )
            .await
            .unwrap();
        assert_eq!(deleted["state"], "completed", "{deleted}");
        assert_eq!(deleted["result"]["deleted"], true, "{deleted}");
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_delete_native_success_survives_cleanup_failure_and_clears_only_target() {
        let (directory, authorization, id) = deletion_fixture().await;
        let controller = &authorization.controller;
        let control = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"control"}),
                &authorization,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        std::fs::write(
            directory.path().join("delete-policy.json"),
            r#"{"blockStore":true}"#,
        )
        .unwrap();
        let first = controller
            .execute(
                authorization.device.clone(),
                "delete-cleanup".into(),
                "deleteThread".into(),
                json!({"threadId":id}),
            )
            .await
            .unwrap();
        assert_eq!(first["state"], "completed");
        assert_eq!(first["result"]["deleted"], true);
        assert_eq!(first["result"]["status"], "partial");
        // 只恢复隔离引擎故意阻塞的空目录；随后重复请求仅重做本地清理，不重放原生删除。
        std::fs::remove_dir(directory.path().join("remote-harness-state.json")).unwrap();
        std::fs::rename(
            directory.path().join("harness-before-delete.json"),
            directory.path().join("remote-harness-state.json"),
        )
        .unwrap();
        // 模拟清理失败后重启：磁盘仍是 pending，但独立成功回执可证明原生删除已完成。
        let original = controller.auth.lock().await.receipts
            [&format!("{}:delete-cleanup", authorization.device.id)]
            .clone();
        let receipt_directory = tempfile::tempdir().unwrap();
        std::fs::copy(
            directory.path().join("remote-harness-state.json"),
            receipt_directory.path().join("remote-harness-state.json"),
        )
        .unwrap();
        let query_reopened = codex::CodexHarness::new(receipt_directory.path()).unwrap();
        assert!(
            query_reopened
                .protected_delete_receipts()
                .await
                .contains(&format!("{}:delete-cleanup", authorization.device.id))
        );
        let recovered = query_reopened
            .recover_delete_receipt(&original, receipt_value(&original))
            .await;
        assert_eq!(recovered["state"], "completed");
        assert_eq!(recovered["result"]["status"], "local_deleted");
        let reopened = codex::CodexHarness::new(directory.path()).unwrap();
        reopened.attach(Arc::downgrade(controller));
        assert_eq!(
            reopened
                .call("deleteThread", json!({"threadId":id}), Some(&authorization))
                .await
                .unwrap()["deleted"],
            true
        );
        // 查询原成功回执同样只能补本地清理，不启动原生引擎。
        let restored = reopened
            .recover_delete_receipt(&original, receipt_value(&original))
            .await;
        assert_eq!(restored["state"], "completed");
        assert_eq!(restored["result"]["deleted"], true);
        let again = controller
            .execute(
                authorization.device.clone(),
                "delete-cleanup-new".into(),
                "deleteThread".into(),
                json!({"threadId":id}),
            )
            .await
            .unwrap();
        assert_eq!(again["result"]["status"], "local_deleted");
        let state: Value = serde_json::from_slice(
            &std::fs::read(directory.path().join("remote-harness-state.json")).unwrap(),
        )
        .unwrap();
        assert!(state["settings"].get(&id).is_none());
        assert!(state["threadDevices"].get(&id).is_none());
        assert!(
            !state["sent"]
                .as_object()
                .unwrap()
                .keys()
                .any(|key| key.starts_with(&format!("{id}:")))
        );
        assert_eq!(state["deletions"][&id]["state"], "deleted");
        assert!(
            state["settings"]
                .get(control["threadId"].as_str().unwrap())
                .is_some()
        );
        assert_eq!(
            harness_engine_requests(directory.path())
                .iter()
                .filter(|frame| frame["method"] == "thread/delete")
                .count(),
            1
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_delete_late_native_confirmation_recovers_only_original_receipt() {
        let (directory, authorization, id) = deletion_fixture().await;
        let controller = &authorization.controller;
        std::fs::write(
            directory.path().join("delete-policy.json"),
            r#"{"malformed":true}"#,
        )
        .unwrap();
        let remove = || {
            controller.execute(
                authorization.device.clone(),
                "delete-late".into(),
                "deleteThread".into(),
                json!({"threadId":id}),
            )
        };
        assert_eq!(remove().await.unwrap()["state"], "unknown");
        controller
            .harness
            .seed_test_history(json!({"threadId":id,"events":[
                {"method":"thread/deleted","params":{"threadId":id}}
            ]}))
            .await
            .unwrap();
        let recovered = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let receipt = remove().await.unwrap();
                if receipt["state"] == "completed" {
                    break receipt;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(recovered["result"]["deleted"], true);
        let original = controller.auth.lock().await.receipts
            [&format!("{}:delete-late", authorization.device.id)]
            .clone();
        for changed_field in ["device", "request", "operation"] {
            let mut altered = original.clone();
            match changed_field {
                "device" => altered.device_id = "unrelated-device".into(),
                "request" => altered.request_id = "unrelated-request".into(),
                _ => altered.payload_hash = hash(&json!(["rename", {"threadId":id}]).to_string()),
            }
            let value = receipt_value(&altered);
            assert_eq!(
                controller
                    .harness
                    .recover_delete_receipt(&altered, value)
                    .await["state"],
                "unknown"
            );
        }
        controller.harness.seed_test_history(json!({"threadId":id,"events":[
            {"method":"thread/started","params":{"thread":{"id":id,"ephemeral":false,"source":"appServer","threadSource":"user","cwd":directory.path()}}},
            {"method":"thread/status/changed","params":{"threadId":id,"status":{"type":"active"}}}
        ]})).await.unwrap();
        let sidebar = controller
            .call_authorized("sidebar", json!({}), &authorization)
            .await
            .unwrap();
        assert!(
            !sidebar["recent"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["id"] == id)
        );
        assert_eq!(
            harness_engine_requests(directory.path())
                .iter()
                .filter(|frame| frame["method"] == "thread/delete")
                .count(),
            1
        );
        let reopened = codex::CodexHarness::new(directory.path()).unwrap();
        assert_eq!(
            reopened
                .call("deleteThread", json!({"threadId":id}), Some(&authorization))
                .await
                .unwrap()["deleted"],
            true
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_delete_unsupported_is_known_failure_but_descendant_errors_are_unknown() {
        for (engine_code, message, expected_state) in [
            (-32601, "unknown method thread/delete", "failed"),
            (
                -32600,
                "thread descendant already has an active writer",
                "unknown",
            ),
        ] {
            let (directory, authorization, id) = deletion_fixture().await;
            std::fs::write(
                directory.path().join("delete-policy.json"),
                json!({"error":{"code":engine_code,"message":message}}).to_string(),
            )
            .unwrap();
            let controller = &authorization.controller;
            let result = controller
                .execute(
                    authorization.device.clone(),
                    "delete-error".into(),
                    "deleteThread".into(),
                    json!({"threadId":id}),
                )
                .await
                .unwrap();
            assert_eq!(result["state"], expected_state);
            assert_ne!(result["result"]["deleted"], true);
            controller.harness.release().await;
        }
    }

    #[tokio::test]
    async fn harness_delete_intent_persistence_failure_does_not_dispatch() {
        let (directory, authorization, id) = deletion_fixture().await;
        let controller = &authorization.controller;
        let path = directory.path().join("remote-harness-state.json");
        let backup = directory.path().join("harness-before-delete.json");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        let result = controller
            .execute(
                authorization.device.clone(),
                "delete-no-storage".into(),
                "deleteThread".into(),
                json!({"threadId":id}),
            )
            .await
            .unwrap();
        assert_eq!(result["state"], "failed");
        assert_eq!(result["result"]["notSent"], true);
        assert!(
            !harness_engine_requests(directory.path())
                .iter()
                .any(|frame| frame["method"] == "thread/delete")
        );
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&backup, &path).unwrap();
        assert_eq!(
            controller
                .execute(
                    authorization.device.clone(),
                    "delete-storage-restored".into(),
                    "deleteThread".into(),
                    json!({"threadId":id})
                )
                .await
                .unwrap()["result"]["deleted"],
            true
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_delete_disconnect_retains_protection_without_reconnecting_to_delete() {
        let (directory, authorization, id) = deletion_fixture().await;
        let controller = &authorization.controller;
        std::fs::write(
            directory.path().join("delete-policy.json"),
            r#"{"disconnect":true}"#,
        )
        .unwrap();
        let result = controller
            .execute(
                authorization.device.clone(),
                "delete-disconnect".into(),
                "deleteThread".into(),
                json!({"threadId":id}),
            )
            .await
            .unwrap();
        assert_eq!(result["state"], "unknown");
        let repeated = controller
            .execute(
                authorization.device.clone(),
                "delete-after-disconnect".into(),
                "deleteThread".into(),
                json!({"threadId":id}),
            )
            .await
            .unwrap();
        assert_eq!(repeated["state"], "failed");
        assert_eq!(repeated["result"]["notSent"], true);
        assert_eq!(
            harness_engine_requests(directory.path())
                .iter()
                .filter(|frame| frame["method"] == "thread/delete")
                .count(),
            1
        );
        controller.harness.release().await;
    }

    #[test]
    fn service_legacy_delete_receipt_recovery_requires_exact_success_and_operation_identity() {
        let id = "01234567-89ab-4cde-8123-456789abcdef";
        let receipt = Receipt {
            device_id: "device".into(),
            request_id: "legacy-delete".into(),
            payload_hash: hash(&json!(["deleteThread", {"threadId":id}]).to_string()),
            created_at_ms: 1,
            state: "completed".into(),
            result: Some(json!({"ok":true,"threadId":id,"executionTransport":"app-server-stdio"})),
        };
        assert_eq!(receipt_value(&receipt)["result"]["deleted"], true);
        assert_eq!(receipt_value(&receipt)["result"]["status"], "local_deleted");
        assert!(
            receipt.result.as_ref().unwrap().get("deleted").is_none(),
            "保留原始持久化证据"
        );
        for state in ["pending", "unknown", "failed"] {
            let mut changed = receipt.clone();
            changed.state = state.into();
            assert!(receipt_value(&changed)["result"].get("deleted").is_none());
        }
        for result in [
            json!({"ok":false,"threadId":id,"executionTransport":"app-server-stdio"}),
            json!({"ok":true,"threadId":id,"executionTransport":"app-server-stdio","outcomeUnknown":true}),
            json!({"ok":true,"threadId":id,"executionTransport":"app-server-stdio","deleted":false}),
            json!({"ok":true,"threadId":id,"executionTransport":"app-server-stdio","status":"partial"}),
            json!({"ok":true,"threadId":id,"executionTransport":"desktop-ipc"}),
            json!({"ok":true,"threadId":"fedcba98-7654-4321-8abc-0123456789ab","executionTransport":"app-server-stdio"}),
        ] {
            let mut changed = receipt.clone();
            changed.result = Some(result);
            assert_ne!(receipt_value(&changed)["result"]["deleted"], true);
        }
        let mut renamed = receipt;
        renamed.payload_hash = hash(&json!(["rename", {"threadId":id}]).to_string());
        assert!(receipt_value(&renamed)["result"].get("deleted").is_none());
    }

    #[tokio::test]
    async fn harness_compaction_history_reopens_without_sending_another_message() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let created = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"ready"}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        controller
            .call_authorized("compact", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        let history = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                // 每次读取都模拟重新进入会话；fixture 的持久化历史没有压缩轮，必须从真实事件恢复。
                let history = controller
                    .call_authorized("history", json!({"threadId":id}), &authorization)
                    .await
                    .unwrap();
                if history["transcriptEvents"][0]["item"]["completed"] == true {
                    break history;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            history["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|turn| turn["id"] == "compact")
        );
        let thread = controller
            .call_authorized("thread", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(thread["thread"]["compaction"]["state"], "idle");
        assert_eq!(
            thread["thread"]["transcriptEvents"][0]["item"]["completed"],
            true
        );
        let requests = std::fs::read_to_string(directory.path().join("requests.jsonl")).unwrap();
        let methods: Vec<_> = requests
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect();
        assert_eq!(
            methods
                .iter()
                .filter(|frame| frame["method"] == "turn/start")
                .count(),
            1
        );
        assert_eq!(
            methods
                .iter()
                .filter(|frame| frame["method"] == "thread/compact/start")
                .count(),
            1
        );
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_session_permissions_persist_and_override_only_the_selected_thread() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let first = controller
            .call_authorized(
                "create",
                json!({
                    "projectId":"project-a","prompt":"wait first","permissionMode":"read-only"
                }),
                &authorization,
            )
            .await
            .unwrap();
        let id = first["threadId"].as_str().unwrap();
        let created = harness_engine_requests(directory.path());
        let start = created
            .iter()
            .find(|frame| frame["method"] == "thread/start")
            .unwrap();
        let turn = created
            .iter()
            .find(|frame| frame["method"] == "turn/start")
            .unwrap();
        assert_eq!(start["params"]["sandbox"], "read-only");
        assert_eq!(turn["params"]["sandboxPolicy"]["type"], "readOnly");
        assert_eq!(turn["params"]["approvalPolicy"], "on-request");
        let detail = controller
            .call_authorized("thread", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(detail["thread"]["permissionMode"], "read-only");
        controller
            .call_authorized(
                "stop",
                json!({"threadId":id,"turnId":first["turnId"]}),
                &authorization,
            )
            .await
            .unwrap();
        let permission_auth =
            controller.authorization(authorization.device.clone(), Some("setPermissions"));
        let saved = controller
            .call_authorized(
                "setPermissions",
                json!({
                    "threadId":id,"instanceId":"codex:local","permissionMode":"full-access"
                }),
                &permission_auth,
            )
            .await
            .unwrap();
        assert_eq!(saved["permissionRevision"], 2);
        let reopened = permissions::SessionPermissions::open(directory.path()).unwrap();
        assert_eq!(reopened.get(id).await.unwrap().mode, "full-access");
        controller
            .call_authorized(
                "send",
                json!({
                    "threadId":id,"prompt":"wait next","clientUserMessageId":"permission-next"
                }),
                &authorization,
            )
            .await
            .unwrap();
        let frames = harness_engine_requests(directory.path());
        let turn = frames
            .iter()
            .rev()
            .find(|frame| frame["method"] == "turn/start")
            .unwrap();
        assert_eq!(turn["params"]["sandboxPolicy"]["type"], "dangerFullAccess");
        assert_eq!(turn["params"]["approvalPolicy"], "never");
        let other = controller
            .call_authorized(
                "create",
                json!({
                    "projectId":"project-a","prompt":"wait other"
                }),
                &authorization,
            )
            .await
            .unwrap();
        let frames = harness_engine_requests(directory.path());
        let turn = frames
            .iter()
            .rev()
            .find(|frame| frame["method"] == "turn/start")
            .unwrap();
        assert_eq!(turn["params"]["threadId"], other["threadId"]);
        assert_eq!(turn["params"]["sandboxPolicy"]["type"], "workspaceWrite");
        assert_eq!(turn["params"]["approvalPolicy"], "on-request");
        let detail = controller
            .call_authorized(
                "thread",
                json!({"threadId":other["threadId"]}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(detail["thread"]["permissionMode"], "auto");
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_new_session_defaults_use_independent_config_and_ignore_app_preferences() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        std::fs::write(
            directory.path().join(".codex-global-state.json"),
            serde_json::to_vec(&json!({
                "electron-renderer-content-owner":"fixture",
                "electron-owner-atoms:fixture":{"permission-selection-by-host-id:local":
                    {"kind":"agent-mode","agentMode":"full-access"}}
            }))
            .unwrap(),
        )
        .unwrap();
        let defaults = controller
            .call_authorized(
                "newSessionDefaults",
                json!({"instanceId":"codex:local","projectId":"project-a"}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(defaults["model"], "gpt-6.1-sol");
        assert_eq!(defaults["reasoningEffort"], "high");
        assert_eq!(defaults["permissionMode"], "auto");
        assert!(
            !harness_engine_requests(directory.path())
                .iter()
                .any(|frame| frame["method"] == "thread/start")
        );
        // App 私有文件即使损坏也不能改变或阻断独立服务的新建配置。
        std::fs::write(
            directory.path().join(".codex-global-state.json"),
            "{invalid app state",
        )
        .unwrap();
        let created = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"wait defaults"}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(created["permissionMode"], "auto");
        let frames = harness_engine_requests(directory.path());
        let start = frames
            .iter()
            .find(|frame| frame["method"] == "thread/start")
            .unwrap();
        let turn = frames
            .iter()
            .find(|frame| frame["method"] == "turn/start")
            .unwrap();
        assert_eq!(start["params"]["model"], defaults["model"]);
        assert_eq!(
            start["params"]["config"]["model_reasoning_effort"],
            defaults["reasoningEffort"]
        );
        assert_eq!(start["params"]["sandbox"], "workspace-write");
        assert_eq!(turn["params"]["effort"], defaults["reasoningEffort"]);
        assert_eq!(turn["params"]["approvalPolicy"], "on-request");
        assert_eq!(turn["params"]["sandboxPolicy"]["type"], "workspaceWrite");
        assert!(
            frames
                .iter()
                .filter(|frame| frame["method"] == "config/read"
                    && frame["params"]["cwd"] == start["params"]["cwd"])
                .count()
                >= 2,
            "新建页读取及实际创建都必须核对目标项目配置；Fast 目录的全局读取独立保留"
        );
        let detail = controller
            .call_authorized(
                "thread",
                json!({"threadId":created["threadId"]}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(detail["thread"]["permissionMode"], "auto");
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_external_writer_rejection_never_delegates_or_reports_a_false_stop() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let id = uuid::Uuid::new_v4().to_string();
        controller
            .harness
            .seed_test_subagents(json!({"rows":[{
                "id":id,"name":"external-writer","cwd":directory.path(),"ephemeral":false,
                "forkedFromId":null,"threadSource":"user","source":"vscode",
                "status":{"type":"notLoaded"},"turns":[],"createdAt":1,"updatedAt":1
            }]}))
            .await
            .unwrap();
        let send = || {
            controller.execute(authorization.device.clone(), "standalone-send".into(),
            "send".into(), json!({"threadId":id,"prompt":"must not be dispatched","clientUserMessageId":"standalone-user"}))
        };
        let rejected = send().await.unwrap();
        assert_eq!(rejected["state"], "failed");
        assert_eq!(rejected["result"]["notSent"], true);
        assert_eq!(send().await.unwrap(), rejected);
        let stopped = controller
            .execute(
                authorization.device.clone(),
                "standalone-stop".into(),
                "stop".into(),
                json!({"threadId":id}),
            )
            .await
            .unwrap();
        assert_eq!(stopped["state"], "failed");
        assert_eq!(stopped["result"]["notSent"], true);
        let frames = harness_engine_requests(directory.path());
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame["method"] == "thread/resume")
                .count(),
            1
        );
        assert!(!frames.iter().any(|frame| matches!(
            frame["method"].as_str(),
            Some("turn/start" | "turn/interrupt" | "thread/compact/start")
        )));
        let queue = controller
            .call_authorized("queue", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(queue["queuedMessages"], json!([]));
        let audit = controller.harness.audit.snapshot();
        for record in audit["records"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|record| record["action"] == "send")
        {
            assert_eq!(record["transports"], json!(["app-server-stdio"]));
        }
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_configured_permissions_are_resolved_on_server_and_preserve_approvals() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        std::fs::write(
            directory.path().join("fast-policy.json"),
            br#"{"approvalPolicy":"on-request","sandboxMode":"danger-full-access"}"#,
        )
        .unwrap();
        controller.call_authorized("create", json!({
            "projectId":"project-a","prompt":"wait configured","permissionMode":"configured",
            "permissionConfig":{"approvalPolicy":"never","sandboxPolicy":{"type":"dangerFullAccess"}}
        }), &authorization).await.unwrap();
        let frames = harness_engine_requests(directory.path());
        let turn = frames
            .iter()
            .find(|frame| frame["method"] == "turn/start")
            .unwrap();
        assert_eq!(turn["params"]["sandboxPolicy"]["type"], "dangerFullAccess");
        assert_eq!(
            turn["params"]["approvalPolicy"], "on-request",
            "不能接受客户端伪造的免审批配置"
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_fast_running_toggle_applies_only_to_new_turns_and_queued_dispatch() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        std::fs::write(
            directory.path().join("fast-policy.json"),
            br#"{"noTiers":true,"modelFlagsDisabled":true}"#,
        )
        .unwrap();
        let created = controller.call_authorized("create",
            json!({"projectId":"project-a","prompt":"wait approval current","model":"gpt-6.1-sol"}),
            &authorization).await.unwrap();
        assert_eq!(created["ok"], true, "{created}");
        let id = created["threadId"].as_str().unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let starts = |frames: Vec<Value>| {
            frames
                .into_iter()
                .filter(|frame| frame["method"] == "turn/start")
                .collect::<Vec<_>>()
        };
        let first = starts(harness_engine_requests(directory.path()));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["params"]["serviceTier"], "default");
        let queued = controller.call_authorized("send",
            json!({"threadId":id,"prompt":"wait approval queued fast","clientUserMessageId":"fast-queued-a"}),
            &authorization).await.unwrap();
        assert_eq!(queued["deliveryState"], "queued");
        let fast_auth = controller.authorization(authorization.device.clone(), Some("setFast"));
        let enabled = controller
            .call_authorized(
                "setFast",
                json!({"threadId":id,"model":"gpt-6.1-sol","enabled":true}),
                &fast_auth,
            )
            .await
            .unwrap();
        assert_eq!(enabled["effectiveFrom"], "nextTurn");
        assert_eq!(
            starts(harness_engine_requests(directory.path())),
            first,
            "执行中开关只保存偏好，不启动、引导、重发或改写当前轮次"
        );
        let detail = controller
            .call_authorized("thread", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(detail["thread"]["fastMode"], true);
        assert_eq!(detail["thread"]["serviceTier"], "default");
        let saved: Value = serde_json::from_slice(
            &std::fs::read(directory.path().join("remote-harness-state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["settings"][id]["fastMode"], true);
        let approve = controller.authorization(authorization.device.clone(), Some("approve"));
        let request = &detail["thread"]["requests"][0];
        controller
            .call_authorized(
                "approve",
                json!({"threadId":id,"requestId":request["id"],
            "method":request["method"],"decision":"accept"}),
                &approve,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let result = controller
                    .call_authorized(
                        "sendState",
                        json!({"threadId":id,"clientUserMessageId":"fast-queued-a"}),
                        &authorization,
                    )
                    .await
                    .unwrap();
                if result["deliveryState"] == "accepted" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        let second = starts(harness_engine_requests(directory.path()));
        assert_eq!(second.len(), 2);
        assert_eq!(
            second[1]["params"]["serviceTier"], "priority",
            "入队发生在开启之前，派发仍须读取最新 Fast 设置"
        );
        controller.call_authorized("send",
            json!({"threadId":id,"prompt":"wait queued standard","clientUserMessageId":"fast-queued-b"}),
            &authorization).await.unwrap();
        controller
            .call_authorized(
                "setFast",
                json!({"threadId":id,"model":"gpt-6.1-sol","enabled":false}),
                &fast_auth,
            )
            .await
            .unwrap();
        assert_eq!(starts(harness_engine_requests(directory.path())), second);
        let detail = controller
            .call_authorized("thread", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        let request = &detail["thread"]["requests"][0];
        controller
            .call_authorized(
                "approve",
                json!({"threadId":id,"requestId":request["id"],
            "method":request["method"],"decision":"accept"}),
                &approve,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let result = controller
                    .call_authorized(
                        "sendState",
                        json!({"threadId":id,"clientUserMessageId":"fast-queued-b"}),
                        &authorization,
                    )
                    .await
                    .unwrap();
                if result["deliveryState"] == "accepted" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        let final_starts = starts(harness_engine_requests(directory.path()));
        assert_eq!(final_starts.len(), 3);
        assert_eq!(final_starts[0]["params"]["serviceTier"], "default");
        assert_eq!(final_starts[1]["params"]["serviceTier"], "priority");
        assert_eq!(final_starts[2]["params"]["serviceTier"], "default");
        assert_eq!(
            final_starts[1]["params"]["clientUserMessageId"],
            "fast-queued-a"
        );
        assert_eq!(
            final_starts[2]["params"]["clientUserMessageId"],
            "fast-queued-b"
        );
        assert!(
            !harness_engine_requests(directory.path())
                .iter()
                .any(|frame| matches!(
                    frame["method"].as_str(),
                    Some("turn/steer" | "turn/interrupt" | "thread/resume")
                ))
        );
        let explicit = controller.call_authorized("create",
            json!({"projectId":"project-a","prompt":"wait explicit fast","model":"gpt-6.1-sol","fastMode":true}),
            &authorization).await.unwrap();
        let normal = controller.call_authorized("create",
            json!({"projectId":"project-a","prompt":"wait default standard","model":"gpt-6.1-sol"}),
            &authorization).await.unwrap();
        let final_starts = starts(harness_engine_requests(directory.path()));
        assert_eq!(final_starts[3]["params"]["serviceTier"], "priority");
        assert_eq!(final_starts[4]["params"]["serviceTier"], "default");
        assert_ne!(explicit["threadId"], normal["threadId"]);
    }

    #[tokio::test]
    async fn harness_fast_respects_account_admin_model_and_readonly_boundaries() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let created = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"wait","model":"gpt-6.1-sol"}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        let fast_auth = controller.authorization(authorization.device.clone(), Some("setFast"));
        let saved_fast = || {
            let saved: Value = serde_json::from_slice(
                &std::fs::read(directory.path().join("remote-harness-state.json")).unwrap(),
            )
            .unwrap();
            saved["settings"][id]["fastMode"] == true
        };
        for policy in [
            json!({"accountType":"unknown"}),
            json!({"requirementsDenied":true}),
            json!({"requirementsUnknown":true}),
            json!({"requirementsMalformed":true}),
            json!({"configFast":false}),
            json!({"provider":"copilot"}),
        ] {
            std::fs::write(
                directory.path().join("fast-policy.json"),
                serde_json::to_vec(&policy).unwrap(),
            )
            .unwrap();
            let result = controller
                .call_authorized(
                    "setFast",
                    json!({"threadId":id,"model":"gpt-6.1-sol","enabled":true}),
                    &fast_auth,
                )
                .await;
            assert!(result.is_err(), "{policy}");
            assert!(!saved_fast());
        }
        std::fs::write(directory.path().join("fast-policy.json"), b"{}").unwrap();
        controller
            .call_authorized(
                "setFast",
                json!({"threadId":id,"model":"gpt-6.1-sol","enabled":true}),
                &fast_auth,
            )
            .await
            .unwrap();
        std::fs::write(
            directory.path().join("fast-policy.json"),
            br#"{"requirementsDenied":true}"#,
        )
        .unwrap();
        controller
            .call_authorized("models", json!({"forceReload":true}), &authorization)
            .await
            .unwrap();
        controller
            .call_authorized(
                "setFast",
                json!({"threadId":id,"model":"gpt-6.1-sol","enabled":false}),
                &fast_auth,
            )
            .await
            .unwrap();
        assert!(!saved_fast());
        controller.access_policy.write().unwrap().read_only = true;
        let result = controller
            .call_authorized(
                "setFast",
                json!({"threadId":id,"model":"gpt-6.1-sol","enabled":false}),
                &fast_auth,
            )
            .await;
        assert!(result.is_err());
        assert_eq!(
            harness_engine_requests(directory.path())
                .iter()
                .filter(|frame| frame["method"] == "turn/start")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn harness_fast_side_toggle_keeps_current_steer_and_ephemeral_preferences_separate() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let main = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"wait","model":"gpt-6.1-sol"}),
                &authorization,
            )
            .await
            .unwrap();
        let created = controller
            .call_authorized(
                "createSideThread",
                json!({"threadId":main["threadId"]}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        let fast_auth = controller.authorization(authorization.device.clone(), Some("setFast"));
        let send =
            |client: &str| json!({"threadId":id,"prompt":"wait","clientUserMessageId":client});
        controller
            .call_authorized("send", send("side-first"), &authorization)
            .await
            .unwrap();
        controller
            .call_authorized(
                "setFast",
                json!({"threadId":id,"model":"gpt-6.1-sol","enabled":true}),
                &fast_auth,
            )
            .await
            .unwrap();
        let queued = controller
            .call_authorized("send", send("side-steer"), &authorization)
            .await
            .unwrap();
        assert_eq!(queued["deliveryState"], "queued");
        assert!(
            !harness_engine_requests(directory.path())
                .iter()
                .any(|frame| frame["method"] == "turn/steer"),
            "普通发送不能自动引导侧边任务"
        );
        controller
            .call_authorized(
                "steerQueued",
                json!({"threadId":id,"messageId":queued["messageId"]}),
                &authorization,
            )
            .await
            .unwrap();
        let frames = harness_engine_requests(directory.path());
        let first: Vec<_> = frames
            .iter()
            .filter(|frame| frame["method"] == "turn/start" && frame["params"]["threadId"] == id)
            .collect();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["params"]["serviceTier"], "default");
        let steer = frames
            .iter()
            .find(|frame| frame["method"] == "turn/steer")
            .unwrap();
        assert!(steer["params"].get("serviceTier").is_none());
        let saved: Value = serde_json::from_slice(
            &std::fs::read(directory.path().join("remote-harness-state.json")).unwrap(),
        )
        .unwrap();
        assert!(saved["settings"].get(id).is_none(), "临时侧边偏好不能落盘");
        controller
            .call_authorized("stop", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        controller
            .call_authorized("send", send("side-next-fast"), &authorization)
            .await
            .unwrap();
        controller
            .call_authorized(
                "setFast",
                json!({"threadId":id,"model":"gpt-6.1-sol","enabled":false}),
                &fast_auth,
            )
            .await
            .unwrap();
        controller
            .call_authorized("stop", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        controller
            .call_authorized("send", send("side-next-standard"), &authorization)
            .await
            .unwrap();
        let frames = harness_engine_requests(directory.path());
        let starts: Vec<_> = frames
            .iter()
            .filter(|frame| frame["method"] == "turn/start" && frame["params"]["threadId"] == id)
            .collect();
        assert_eq!(starts.len(), 3);
        assert_eq!(starts[1]["params"]["serviceTier"], "priority");
        assert_eq!(starts[2]["params"]["serviceTier"], "default");
    }

    #[tokio::test]
    async fn harness_session_queue_and_approval_share_one_runtime() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let created = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"wait approval"}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        assert_eq!(created["ok"], true);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let thread = controller
            .call_authorized("thread", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        let request = thread["thread"]["requests"][0].clone();
        assert_eq!(request["method"], "item/commandExecution/requestApproval");
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        let data_url = format!("data:image/png;base64,{}", STANDARD.encode(png.get_ref()));
        let mut queued = controller
            .call_authorized(
                "send",
                json!({"threadId":id,"prompt":"queued text","clientUserMessageId":"client-a",
                    "images":[{"name":"test.png","mime":"image/png","dataUrl":data_url,"size":png.get_ref().len()}]}),
                &authorization,
            )
            .await
            .unwrap();
        controller.attach_response_images(&mut queued);
        let message = queued["queuedMessages"][0].clone();
        let source = message["remoteImages"][0]["source"].clone();
        assert!(source.as_str().unwrap().starts_with("inline:"));
        let edit = json!({"threadId":id,"messageId":message["id"],"editId":"edit-a","originalText":"queued text",
            "clientUserMessageId":"client-a","createdAt":message["createdAt"],"expectedImageSources":[source]});
        assert_eq!(
            controller
                .call_authorized("beginQueuedEdit", edit, &authorization)
                .await
                .unwrap()["editState"],
            "editing"
        );
        assert_eq!(controller.call_authorized("editQueued",json!({"threadId":id,"editId":"edit-a","prompt":"edited text","retainedImageSources":[source],"images":[]}),&authorization).await.unwrap()["editState"],"committed");
        assert!(controller.call_authorized("editQueued",json!({"threadId":id,"editId":"edit-a","prompt":"different text","retainedImageSources":[source],"images":[]}),&authorization).await.is_err());
        let approve = controller.authorization(authorization.device.clone(), Some("approve"));
        assert_eq!(
            controller
                .call_authorized(
                    "approve",
                    json!({"threadId":id,"requestId":request["id"],
            "method":request["method"],"decision":"accept"}),
                    &approve
                )
                .await
                .unwrap()["ok"],
            true
        );
        let delivered = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let sent = controller
                    .call_authorized(
                        "sendState",
                        json!({"threadId":id,"clientUserMessageId":"client-a"}),
                        &authorization,
                    )
                    .await
                    .unwrap();
                if sent["deliveryState"] == "accepted" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(
            delivered.is_ok(),
            "queue did not complete; state={}, methods={}",
            std::fs::read_to_string(directory.path().join("remote-harness-state.json")).unwrap(),
            std::fs::read_to_string(directory.path().join("requests.jsonl")).unwrap()
        );
        let history = controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(history["data"].as_array().unwrap().len(), 2);
        let requests = std::fs::read_to_string(directory.path().join("requests.jsonl")).unwrap();
        let frames: Vec<Value> = requests
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            frames
                .iter()
                .filter(|f| f["method"] == "initialize")
                .count(),
            1
        );
        assert_eq!(
            frames
                .iter()
                .filter(|f| f["method"] == "turn/start")
                .count(),
            2
        );
        assert!(frames.iter().any(
            |f| f["method"] == "turn/start" && f["params"]["input"][0]["text"] == "edited text"
        ));
        assert!(
            frames
                .iter()
                .any(|f| f["method"] == "turn/start" && f["params"]["input"][1]["url"] == data_url)
        );
        assert!(
            frames
                .iter()
                .any(|f| f.get("method").is_none() && f["result"]["decision"] == "accept")
        );
    }

    #[tokio::test]
    async fn harness_desktop_queue_dispatches_with_remote_access_disabled() {
        let (directory, authorization) = harness_fixture().await;
        let authorization = desktop_authorization(authorization).await;
        let controller = &authorization.controller;
        let created = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"wait desktop first"}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        let queued = controller
            .call_authorized(
                "send",
                json!({"threadId":id,"prompt":"desktop followup","clientUserMessageId":"desktop-queued"}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(queued["deliveryState"], "queued");
        controller
            .harness
            .seed_test_history(json!({"threadId":id,"finish":true}))
            .await
            .unwrap();
        harness_wait_side_starts(directory.path(), id, 2).await;
        assert!(
            harness_engine_requests(directory.path())
                .iter()
                .any(|frame| frame["method"] == "turn/start"
                    && frame["params"]["clientUserMessageId"] == "desktop-queued")
        );
        assert!(controller.auth.lock().await.devices.is_empty());
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_queue_edit_is_unsent_and_resend_preserves_remaining_order() {
        for resend in [false, true] {
            let (directory, authorization) = harness_fixture().await;
            let controller = &authorization.controller;
            let created = controller
                .call_authorized(
                    "create",
                    json!({"projectId":"project-a","prompt":"wait approval active"}),
                    &authorization,
                )
                .await
                .unwrap();
            let id = created["threadId"].as_str().unwrap();
            let mut first = Value::Null;
            for client in ["B", "C", "D"] {
                let queued = controller
                    .call_authorized(
                        "send",
                        json!({"threadId":id,"prompt":format!("wait approval {client}"),
                            "clientUserMessageId":client}),
                        &authorization,
                    )
                    .await
                    .unwrap();
                if client == "B" {
                    first = queued["queuedMessages"][0].clone();
                }
            }
            let taken = controller
                .call_authorized(
                    "beginQueuedEdit",
                    json!({"threadId":id,"messageId":first["id"],"editId":"draft-B",
                        "originalText":first["text"],"clientUserMessageId":"B","createdAt":first["createdAt"]}),
                    &authorization,
                )
                .await
                .unwrap();
            assert_eq!(
                taken["queuedMessages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|message| message["clientUserMessageId"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["C", "D"]
            );
            let approve_current = || async {
                let approve =
                    controller.authorization(authorization.device.clone(), Some("approve"));
                let request = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let detail = controller
                            .call_authorized("thread", json!({"threadId":id}), &authorization)
                            .await
                            .unwrap();
                        if let Some(request) = detail["thread"]["requests"]
                            .as_array()
                            .and_then(|requests| requests.first())
                        {
                            break request.clone();
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
                controller
                    .call_authorized(
                        "approve",
                        json!({"threadId":id,"requestId":request["id"],
                            "method":request["method"],"decision":"accept"}),
                        &approve,
                    )
                    .await
                    .unwrap();
            };
            let wait_authorization = &authorization;
            let delivered = |client: &'static str| async move {
                tokio::time::timeout(Duration::from_secs(15), async {
                    loop {
                        let result = controller
                            .call_authorized(
                                "sendState",
                                json!({"threadId":id,"clientUserMessageId":client}),
                                wait_authorization,
                            )
                            .await
                            .unwrap();
                        if result["deliveryState"] == "accepted" {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                })
                .await
                .expect("取回编辑的草稿不应阻塞后续队列");
            };
            approve_current().await;
            delivered("C").await;
            let saved: Value = serde_json::from_slice(
                &std::fs::read(directory.path().join("remote-harness-state.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(saved["edits"]["draft-B"]["state"], "editing");
            assert!(
                !harness_engine_requests(directory.path())
                    .iter()
                    .any(|frame| frame["method"] == "turn/start"
                        && frame["params"]["clientUserMessageId"] == "B"),
                "取回后未点击发送，不能自动恢复或派发原消息"
            );
            if resend {
                let restored = controller
                    .call_authorized(
                        "editQueued",
                        json!({"threadId":id,"editId":"draft-B","prompt":"wait approval B edited"}),
                        &authorization,
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    restored["queuedMessages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|message| message["clientUserMessageId"].as_str().unwrap())
                        .collect::<Vec<_>>(),
                    ["B", "D"],
                    "已执行的 C 不回退，重新发送的 B 仍位于尚未执行的 D 之前"
                );
                approve_current().await;
                delivered("B").await;
            }
            approve_current().await;
            delivered("D").await;
            let starts = harness_engine_requests(directory.path())
                .into_iter()
                .filter(|frame| frame["method"] == "turn/start")
                .map(|frame| {
                    frame["params"]["input"][0]["text"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect::<Vec<_>>();
            let expected = if resend {
                vec![
                    "wait approval active",
                    "wait approval C",
                    "wait approval B edited",
                    "wait approval D",
                ]
            } else {
                vec!["wait approval active", "wait approval C", "wait approval D"]
            };
            assert_eq!(starts, expected);
            controller.harness.release().await;
        }
    }

    #[tokio::test]
    async fn harness_queue_recovery_requires_normal_completion_and_keeps_manual_retry_available() {
        for mode in ["missing", "before-restart", "after-reconnect"] {
            let completed = mode == "before-restart";
            let (original_dir, original_auth) = harness_fixture().await;
            let original = &original_auth.controller;
            let created = original
                .call_authorized(
                    "create",
                    json!({"projectId":"project-a","prompt":"wait original"}),
                    &original_auth,
                )
                .await
                .unwrap();
            let id = created["threadId"].as_str().unwrap();
            let old_turn = created["turnId"].as_str().unwrap();
            let mut old_messages = Value::Null;
            for client in ["B", "C"] {
                let queued = original.call_authorized("send", json!({
                    "threadId":id,"prompt":format!("wait {client}"),"clientUserMessageId":client
                }), &original_auth).await.unwrap();
                old_messages = queued["queuedMessages"].clone();
            }
            let saved: Value = serde_json::from_slice(
                &std::fs::read(original_dir.path().join("remote-harness-state.json")).unwrap(),
            )
            .unwrap();
            let device = original_auth.device.clone();
            let key = original.auth.lock().await.access_key.clone();
            original.harness.release().await;
            let rollout = original_dir.path().join(format!("{id}.jsonl"));
            if completed {
                use std::io::Write;
                let record = json!({"timestamp":chrono::DateTime::from_timestamp_millis(now_ms() as i64)
                    .unwrap().to_rfc3339(),"type":"event_msg",
                    "payload":{"type":"task_complete","turn_id":old_turn}});
                writeln!(
                    std::fs::OpenOptions::new()
                        .append(true)
                        .open(&rollout)
                        .unwrap(),
                    "{record}"
                )
                .unwrap();
            }
            let (recovered_dir, recovered_auth) = harness_fixture_with_store(Some(saved)).await;
            let controller = &recovered_auth.controller;
            {
                // 恢复沿用同一设备和密钥，模拟真实重启；仅复制隔离测试目录中的密钥材料。
                let _key_store = lock_access_key_store(recovered_dir.path()).unwrap();
                crate::settings::atomic_write(
                    &recovered_dir.path().join("remote-access-key.json"),
                    &serde_json::to_vec(key.as_ref().unwrap()).unwrap(),
                )
                .unwrap();
            }
            {
                let mut auth = controller.auth.lock().await;
                auth.access_key = key;
                auth.devices = vec![device.clone()];
            }
            let authorization = controller.authorization(device, Some("send"));
            controller.harness.seed_test_subagents(json!({"rows":[{
                "id":id,"cwd":recovered_dir.path(),"path":rollout,"ephemeral":false,
                "threadSource":"user","source":"appServer","status":{"type":"idle"},
                "turns":[{"id":old_turn,"status":"completed","items":[{
                    "id":"original-input","type":"userMessage","content":[{"type":"text","text":"wait original"}]
                }]}]
            }],"loaded":[]})).await.unwrap();
            // 恢复读取故意返回 completed；没有原生结束记录时，不能据此认定异常前任务已完成。
            for action in ["thread", "history", "queue"] {
                controller
                    .call_authorized(action, json!({"threadId":id}), &authorization)
                    .await
                    .unwrap();
            }
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let state: Value = serde_json::from_slice(
                        &std::fs::read(recovered_dir.path().join("remote-harness-state.json"))
                            .unwrap(),
                    )
                    .unwrap();
                    if completed {
                        if state["sent"]
                            .as_object()
                            .unwrap()
                            .values()
                            .any(|result| result["clientUserMessageId"] == "B")
                        {
                            break;
                        }
                    } else if state["queueGates"][id]["phase"] == "recovering"
                        && state["queueGates"][id]["reason"]
                            .as_str()
                            .is_some_and(|reason| !reason.is_empty())
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .unwrap();
            let frames = harness_engine_requests(recovered_dir.path());
            let starts = frames
                .iter()
                .filter(|frame| frame["method"] == "turn/start")
                .collect::<Vec<_>>();
            if completed {
                assert_eq!(starts.len(), 1);
                assert_eq!(starts[0]["params"]["clientUserMessageId"], "B");
            } else {
                assert!(starts.is_empty(), "崩溃恢复读取不能启动排队消息");
                assert!(!frames.iter().any(|frame| matches!(
                    frame["method"].as_str(),
                    Some("thread/resume" | "turn/steer")
                )));
                let queue = controller
                    .call_authorized("queue", json!({"threadId":id}), &authorization)
                    .await
                    .unwrap();
                assert_eq!(
                    queue["queuedMessages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|message| message["id"].clone())
                        .collect::<Vec<_>>(),
                    old_messages
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|message| message["id"].clone())
                        .collect::<Vec<_>>()
                );
                assert!(
                    queue["queuedMessages"][0]["pausedReason"]
                        .as_str()
                        .unwrap()
                        .contains("未确认正常结束")
                );
                if mode == "after-reconnect" {
                    // 临时连接丢失后，迟到的真实完成记录仍应自然释放原队列。
                    use std::io::Write;
                    let record = json!({"timestamp":chrono::DateTime::from_timestamp_millis(now_ms() as i64)
                        .unwrap().to_rfc3339(),"type":"event_msg",
                        "payload":{"type":"task_complete","turn_id":old_turn}});
                    writeln!(
                        std::fs::OpenOptions::new()
                            .append(true)
                            .open(&rollout)
                            .unwrap(),
                        "{record}"
                    )
                    .unwrap();
                    tokio::time::timeout(Duration::from_secs(30), async {
                        loop {
                            let state = controller
                                .call_authorized(
                                    "sendState",
                                    json!({"threadId":id,"clientUserMessageId":"B"}),
                                    &authorization,
                                )
                                .await
                                .unwrap();
                            if state["deliveryState"] == "accepted" {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                    })
                    .await
                    .unwrap();
                } else {
                    controller
                        .call_authorized(
                            "steerQueued",
                            json!({
                                "threadId":id,"messageId":queue["queuedMessages"][0]["id"]
                            }),
                            &authorization,
                        )
                        .await
                        .unwrap();
                }
                let queue = controller
                    .call_authorized("queue", json!({"threadId":id}), &authorization)
                    .await
                    .unwrap();
                assert_eq!(queue["queuedMessages"].as_array().unwrap().len(), 1);
                assert_eq!(queue["queuedMessages"][0]["clientUserMessageId"], "C");
                assert_eq!(
                    queue["queuedMessages"][0]["pausedReason"].is_string(),
                    mode == "missing"
                );
            }
            controller.harness.release().await;
        }
    }

    #[tokio::test]
    async fn harness_queue_waits_for_upstream_retry_then_respects_the_real_terminal_status() {
        for outcome in ["completed", "failed", "interrupted", "completed-with-error"] {
            let status = if outcome == "completed-with-error" {
                "completed"
            } else {
                outcome
            };
            let terminal_error = if outcome == "completed-with-error" {
                json!({"message":"upstream failure"})
            } else {
                Value::Null
            };
            let (directory, authorization) = harness_fixture().await;
            let controller = &authorization.controller;
            let created = controller
                .call_authorized(
                    "create",
                    json!({"projectId":"project-a","prompt":"wait upstream"}),
                    &authorization,
                )
                .await
                .unwrap();
            let id = created["threadId"].as_str().unwrap();
            let turn = created["turnId"].as_str().unwrap();
            controller
                .call_authorized(
                    "send",
                    json!({
                        "threadId":id,"prompt":"wait queued","clientUserMessageId":"retry-queue"
                    }),
                    &authorization,
                )
                .await
                .unwrap();
            controller
                .harness
                .seed_test_history(json!({"threadId":id,"events":[{
                    "method":"error","params":{"threadId":id,"turnId":turn,"willRetry":true,
                        "error":{"message":"temporary upstream failure"}}
                }]}))
                .await
                .unwrap();
            // 队列每 750 毫秒检查一次，额外留 100 毫秒观察重试期间是否误派发。
            tokio::time::sleep(Duration::from_millis(850)).await;
            let queue = controller
                .call_authorized("queue", json!({"threadId":id}), &authorization)
                .await
                .unwrap();
            assert!(
                queue["queuedMessages"][0]["pausedReason"].is_null(),
                "模型重试中不能暂停队列"
            );
            assert_eq!(
                harness_engine_requests(directory.path())
                    .iter()
                    .filter(|frame| frame["method"] == "turn/start")
                    .count(),
                1,
                "重试期间不重放外层执行请求"
            );
            controller
                .harness
                .seed_test_history(json!({"threadId":id,"turns":[{
                    "id":turn,"status":status,"items":[],"error":terminal_error
                }],"events":[{"method":"turn/completed","params":{
                    "threadId":id,"turn":{"id":turn,"status":status,"error":terminal_error}
                }}]}))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let queue = controller
                        .call_authorized("queue", json!({"threadId":id}), &authorization)
                        .await
                        .unwrap();
                    if outcome == "completed"
                        && queue["queuedMessages"].as_array().unwrap().is_empty()
                        || outcome != "completed"
                            && queue["queuedMessages"][0]["pausedReason"].is_string()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .unwrap();
            let starts = harness_engine_requests(directory.path())
                .into_iter()
                .filter(|frame| frame["method"] == "turn/start")
                .collect::<Vec<_>>();
            assert_eq!(starts.len(), if outcome == "completed" { 2 } else { 1 });
            controller.harness.release().await;
        }
    }

    #[tokio::test]
    async fn harness_compaction_boundary_rejected_head_keeps_followers_paused() {
        for side in [false, true] {
            let (directory, authorization, id) = if side {
                harness_active_side_fixture().await
            } else {
                let (directory, authorization) = harness_fixture().await;
                let created = authorization
                    .controller
                    .call_authorized(
                        "create",
                        json!({"projectId":"project-a","prompt":"wait reject compact"}),
                        &authorization,
                    )
                    .await
                    .unwrap();
                (
                    directory,
                    authorization,
                    created["threadId"].as_str().unwrap().to_owned(),
                )
            };
            let controller = &authorization.controller;
            controller.harness.enabled.store(false, Ordering::Release);
            let compact = controller
                .call_authorized("compact", json!({"threadId":id}), &authorization)
                .await
                .unwrap();
            controller
                .call_authorized(
                    "send",
                    json!({"threadId":id,"prompt":"wait follower",
                "clientUserMessageId":"reject-follower"}),
                    &authorization,
                )
                .await
                .unwrap();
            std::fs::write(
                directory.path().join("fast-policy.json"),
                br#"{"compactReject":true}"#,
            )
            .unwrap();
            controller
                .harness
                .seed_test_history(json!({"threadId":id,"finish":true}))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                controller
                    .call_authorized(
                        "steerQueued",
                        json!({"threadId":id,"messageId":compact["messageId"]}),
                        &authorization
                    )
                    .await
                    .is_err()
            );
            controller
                .call_authorized(
                    "removeQueued",
                    json!({"threadId":id,"messageId":compact["messageId"]}),
                    &authorization,
                )
                .await
                .unwrap();
            controller.harness.enabled.store(true, Ordering::Release);
            tokio::time::sleep(Duration::from_millis(850)).await;
            let queue = controller
                .call_authorized("queue", json!({"threadId":id}), &authorization)
                .await
                .unwrap();
            assert_eq!(
                queue["queuedMessages"].as_array().unwrap().len(),
                1,
                "side={side}: 删除失败压缩不能自动续发后续消息"
            );
            assert!(queue["queuedMessages"][0]["pausedReason"].is_string());
            controller.harness.release().await;
        }
    }

    #[tokio::test]
    async fn harness_compaction_boundary_completion_during_admission_does_not_strand_the_request() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let created = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"wait admission boundary"}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        std::fs::write(
            directory.path().join("fast-policy.json"),
            br#"{"finishCompactionRead":true}"#,
        )
        .unwrap();
        controller
            .call_authorized("compact", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if harness_engine_requests(directory.path())
                    .iter()
                    .any(|frame| frame["method"] == "thread/compact/start")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("任务在准入期间正常结束后，压缩仍必须执行");
        assert_eq!(
            harness_engine_requests(directory.path())
                .iter()
                .filter(|frame| frame["method"] == "thread/compact/start")
                .count(),
            1
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_compaction_boundary_native_turn_lifecycle_preserves_two_compactions_and_fifo()
    {
        for side in [false, true] {
            for reply_first in [false, true] {
                let (directory, authorization, id) = if side {
                    harness_active_side_fixture().await
                } else {
                    let (directory, authorization) = harness_fixture().await;
                    let created = authorization
                        .controller
                        .call_authorized(
                            "create",
                            json!({"projectId":"project-a","prompt":"wait native compactions"}),
                            &authorization,
                        )
                        .await
                        .unwrap();
                    (
                        directory,
                        authorization,
                        created["threadId"].as_str().unwrap().to_owned(),
                    )
                };
                let controller = &authorization.controller;
                std::fs::write(directory.path().join("fast-policy.json"), serde_json::to_vec(&json!({
                    "nativeCompactionTurns":true,"compactReplyFirst":reply_first,"compactDelayMs":200
                })).unwrap()).unwrap();
                for _ in 0..2 {
                    controller
                        .call_authorized("compact", json!({"threadId":id}), &authorization)
                        .await
                        .unwrap();
                }
                controller
                    .call_authorized(
                        "send",
                        json!({"threadId":id,"prompt":"wait native follower",
                    "clientUserMessageId":"native-follower"}),
                        &authorization,
                    )
                    .await
                    .unwrap();
                controller
                    .harness
                    .seed_test_history(json!({"threadId":id,"finish":true}))
                    .await
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(20), async {
                    loop {
                        let queue = controller
                            .call_authorized("queue", json!({"threadId":id}), &authorization)
                            .await
                            .unwrap();
                        assert!(
                            queue["queuedMessages"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .all(|message| message["pausedReason"].is_null()),
                            "side={side}, first={reply_first}: {queue}"
                        );
                        if queue["queuedMessages"].as_array().unwrap().is_empty() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                })
                .await
                .unwrap();
                let requests = harness_engine_requests(directory.path());
                let compacts: Vec<_> = requests
                    .iter()
                    .enumerate()
                    .filter(|(_, frame)| frame["method"] == "thread/compact/start")
                    .collect();
                assert_eq!(compacts.len(), 2);
                assert!(
                    requests
                        .iter()
                        .position(|frame| frame["method"] == "turn/start"
                            && frame["params"]["clientUserMessageId"] == "native-follower")
                        .unwrap()
                        > compacts[1].0
                );
                controller.harness.release().await;
            }
        }
    }

    #[tokio::test]
    async fn harness_compaction_boundary_native_failure_and_stop_keep_followers_paused() {
        for side in [false, true] {
            for stop in [false, true] {
                let (directory, authorization, id) = if side {
                    harness_active_side_fixture().await
                } else {
                    let (directory, authorization) = harness_fixture().await;
                    let created=authorization.controller.call_authorized("create",
                        json!({"projectId":"project-a","prompt":"wait native terminal boundary"}), &authorization).await.unwrap();
                    (
                        directory,
                        authorization,
                        created["threadId"].as_str().unwrap().to_owned(),
                    )
                };
                let controller = &authorization.controller;
                std::fs::write(
                    directory.path().join("fast-policy.json"),
                    serde_json::to_vec(&json!({
                        "nativeCompactionTurns":true,"compactReplyFirst":true,
                        "compactDelayMs":if stop {5000} else {200},
                        "compactOutcome":if stop {"completed"} else {"failed"}
                    }))
                    .unwrap(),
                )
                .unwrap();
                controller
                    .call_authorized("compact", json!({"threadId":id}), &authorization)
                    .await
                    .unwrap();
                controller
                    .call_authorized(
                        "send",
                        json!({"threadId":id,"prompt":"wait terminal follower",
                    "clientUserMessageId":"terminal-follower"}),
                        &authorization,
                    )
                    .await
                    .unwrap();
                controller
                    .harness
                    .seed_test_history(json!({"threadId":id,"finish":true}))
                    .await
                    .unwrap();
                if stop {
                    let compact = tokio::time::timeout(Duration::from_secs(5), async {
                        loop {
                            let view = controller
                                .call_authorized("thread", json!({"threadId":id}), &authorization)
                                .await
                                .unwrap();
                            if view["thread"]["compaction"]["state"] == "running" {
                                break view["thread"]["compaction"].clone();
                            }
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    })
                    .await
                    .unwrap();
                    controller
                        .call_authorized("stop", json!({"threadId":id}), &authorization)
                        .await
                        .unwrap();
                    assert!(
                        harness_engine_requests(directory.path())
                            .iter()
                            .any(|request| request["method"] == "turn/interrupt"
                                && request["params"]["turnId"] == compact["turnId"])
                    );
                }
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let queue = controller
                            .call_authorized("queue", json!({"threadId":id}), &authorization)
                            .await
                            .unwrap();
                        if queue["queuedMessages"].as_array().unwrap().len() == 1
                            && queue["queuedMessages"][0]["pausedReason"].is_string()
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap();
                assert!(
                    !harness_engine_requests(directory.path())
                        .iter()
                        .any(|request| request["method"] == "turn/start"
                            && request["params"]["clientUserMessageId"] == "terminal-follower")
                );
                let view = controller
                    .call_authorized("thread", json!({"threadId":id}), &authorization)
                    .await
                    .unwrap();
                assert_eq!(view["thread"]["compaction"]["state"], "idle");
                controller.harness.release().await;
            }
        }
    }

    #[tokio::test]
    async fn harness_manual_compaction_queues_in_order_for_main_and_side_sessions() {
        for side in [false, true] {
            let (directory, authorization, id) = if side {
                harness_active_side_fixture().await
            } else {
                let (directory, authorization) = harness_fixture().await;
                let created = authorization
                    .controller
                    .call_authorized(
                        "create",
                        json!({"projectId":"project-a","prompt":"wait manual compact"}),
                        &authorization,
                    )
                    .await
                    .unwrap();
                (
                    directory,
                    authorization,
                    created["threadId"].as_str().unwrap().to_owned(),
                )
            };
            let controller = &authorization.controller;
            std::fs::write(
                directory.path().join("fast-policy.json"),
                br#"{"compactDelayMs":1500}"#,
            )
            .unwrap();
            controller
                .call_authorized(
                    "send",
                    json!({"threadId":id,"prompt":"wait before manual compact",
                "clientUserMessageId":"before-manual-compact"}),
                    &authorization,
                )
                .await
                .unwrap();
            let queued = controller
                .call_authorized("compact", json!({"threadId":id}), &authorization)
                .await
                .unwrap();
            assert_eq!(queued["deliveryState"], "queued");
            assert_eq!(queued["queuedMessages"][1]["action"], "compact");
            let compact = queued["queuedMessages"][1].clone();
            assert!(
                controller
                    .call_authorized(
                        "steerQueued",
                        json!({"threadId":id,"messageId":compact["id"]}),
                        &authorization
                    )
                    .await
                    .is_err()
            );
            assert!(controller.call_authorized("beginQueuedEdit", json!({"threadId":id,
                "messageId":compact["id"],"editId":"compact-edit","originalText":compact["text"],
                "clientUserMessageId":compact["clientUserMessageId"],"createdAt":compact["createdAt"]}),
                &authorization).await.is_err());
            controller
                .call_authorized(
                    "send",
                    json!({"threadId":id,"prompt":"wait after manual compact",
                "clientUserMessageId":"after-manual-compact"}),
                    &authorization,
                )
                .await
                .unwrap();
            assert!(
                !harness_engine_requests(directory.path())
                    .iter()
                    .any(|frame| frame["method"] == "thread/compact/start")
            );
            controller
                .harness
                .seed_test_history(json!({"threadId":id,"finish":true}))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    if harness_engine_requests(directory.path())
                        .iter()
                        .any(|frame| {
                            frame["method"] == "turn/start"
                                && frame["params"]["clientUserMessageId"] == "before-manual-compact"
                        })
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .unwrap();
            assert!(
                !harness_engine_requests(directory.path())
                    .iter()
                    .any(|frame| frame["method"] == "thread/compact/start"),
                "压缩不能越过前面的排队消息"
            );
            controller
                .harness
                .seed_test_history(json!({"threadId":id,"finish":true}))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let queue = controller
                        .call_authorized("queue", json!({"threadId":id}), &authorization)
                        .await
                        .unwrap();
                    if queue["queuedMessages"].as_array().unwrap().is_empty() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .unwrap();
            let requests = harness_engine_requests(directory.path());
            let compact_calls: Vec<_> = requests
                .iter()
                .enumerate()
                .filter(|(_, frame)| frame["method"] == "thread/compact/start")
                .collect();
            assert_eq!(compact_calls.len(), 1);
            let after = requests
                .iter()
                .position(|frame| {
                    frame["method"] == "turn/start"
                        && frame["params"]["clientUserMessageId"] == "after-manual-compact"
                })
                .unwrap();
            assert!(compact_calls[0].0 < after, "后续消息不能越过压缩");
            let view = controller
                .call_authorized("thread", json!({"threadId":id}), &authorization)
                .await
                .unwrap();
            assert_eq!(view["thread"]["compaction"]["state"], "idle");
            let history = controller
                .call_authorized("history", json!({"threadId":id}), &authorization)
                .await
                .unwrap();
            assert!(
                history["transcriptEvents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|event| event["item"]["type"] == "contextCompaction"),
                "压缩执行后的真实事件在重读主会话和侧边历史时均保留"
            );
            controller.harness.release().await;
        }
    }

    #[tokio::test]
    async fn harness_manual_compaction_keeps_stop_pause_and_allows_idle_retry_of_the_same_item() {
        for side in [false, true] {
            let (directory, authorization, id) = if side {
                harness_active_side_fixture().await
            } else {
                let (directory, authorization) = harness_fixture().await;
                let created = authorization
                    .controller
                    .call_authorized(
                        "create",
                        json!({"projectId":"project-a","prompt":"wait stopped compact"}),
                        &authorization,
                    )
                    .await
                    .unwrap();
                (
                    directory,
                    authorization,
                    created["threadId"].as_str().unwrap().to_owned(),
                )
            };
            let controller = &authorization.controller;
            let compact = controller
                .call_authorized("compact", json!({"threadId":id}), &authorization)
                .await
                .unwrap();
            controller
                .call_authorized("stop", json!({"threadId":id}), &authorization)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(850)).await;
            let queue = controller
                .call_authorized("queue", json!({"threadId":id}), &authorization)
                .await
                .unwrap();
            assert!(queue["queuedMessages"][0]["pausedReason"].is_string());
            assert!(
                !harness_engine_requests(directory.path())
                    .iter()
                    .any(|frame| frame["method"] == "thread/compact/start")
            );
            controller
                .call_authorized(
                    "steerQueued",
                    json!({"threadId":id,"messageId":compact["messageId"]}),
                    &authorization,
                )
                .await
                .unwrap();
            assert_eq!(
                harness_engine_requests(directory.path())
                    .iter()
                    .filter(|frame| frame["method"] == "thread/compact/start")
                    .count(),
                1
            );
            assert!(
                controller
                    .call_authorized("queue", json!({"threadId":id}), &authorization)
                    .await
                    .unwrap()["queuedMessages"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            controller.harness.release().await;
        }
    }

    #[tokio::test]
    async fn harness_queue_normal_compaction_completion_still_dispatches_in_order() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        // 隔离引擎保留 2 秒压缩窗口，供真实只读及入队 RPC 完成，不依赖事件竞速。
        std::fs::write(
            directory.path().join("fast-policy.json"),
            br#"{"compactDelayMs":2000}"#,
        )
        .unwrap();
        let created = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"initial"}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let detail = controller
                    .call_authorized("thread", json!({"threadId":id}), &authorization)
                    .await
                    .unwrap();
                if detail["thread"]["priority"]["active"] == false {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        controller
            .call_authorized("compact", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        let queued = controller.call_authorized("send", json!({
            "threadId":id,"prompt":"wait after compact","clientUserMessageId":"after-compact"
        }), &authorization).await.unwrap();
        assert_eq!(queued["deliveryState"], "queued");
        let state: Value = serde_json::from_slice(
            &std::fs::read(directory.path().join("remote-harness-state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(state["queueGates"][id]["kind"], "compaction");
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let sent = controller
                    .call_authorized(
                        "sendState",
                        json!({"threadId":id,"clientUserMessageId":"after-compact"}),
                        &authorization,
                    )
                    .await
                    .unwrap();
                if sent["deliveryState"] == "accepted" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            harness_engine_requests(directory.path())
                .iter()
                .filter(|frame| frame["method"] == "turn/start")
                .count(),
            2
        );
        controller.harness.release().await;
    }

    async fn subagent_fixture() -> (tempfile::TempDir, HarnessAuthorization, String, Value) {
        let (directory, authorization) = harness_fixture().await;
        let created = authorization.controller.call_authorized("create", json!({
            "projectId":"project-a","prompt":"wait parent","model":"gpt-6.1-sol","reasoningEffort":"high"
        }), &authorization).await.unwrap();
        let root = created["threadId"].as_str().unwrap().to_owned();
        let row = |id: &str, parent: &str, ephemeral: bool, status: &str| {
            json!({
                "id":id,"parentThreadId":parent,"cwd":directory.path(),"ephemeral":ephemeral,
                "source":"appServer","threadSource":"subagent","agentNickname":id,
                "createdAt":1,"updatedAt":1,"status":{"type":status},"turns":[]
            })
        };
        let mut done = row("agent-done", &root, false, "notLoaded");
        let items: Vec<_> = (0..95).map(|i| if i == 0 {
            json!({"id":"child-user","type":"userMessage","content":[{"type":"text","text":"子任务"}]})
        } else { json!({"id":format!("child-item-{i}"),"type":"agentMessage","text":format!("结果 {i}")}) }).collect();
        done["turns"] =
            json!([{"id":"child-turn","status":"completed","startedAt":1,"items":items}]);
        let mut nested = row("agent-nested", "agent-done", false, "idle");
        nested["archived"] = json!(true);
        nested["turns"] = json!([{"id":"nested-turn","status":"failed","startedAt":2,"items":[]}]);
        let mut live = row("agent-live", &root, true, "active");
        live["turns"] = json!([{"id":"live-turn","status":"inProgress","startedAt":3,
            "items":[{"id":"live-message","type":"agentMessage","text":"运行中的输出"}]}]);
        let unrelated = row("agent-other", "other-parent", false, "active");
        let other = json!({"id":"other-parent","cwd":directory.path(),"ephemeral":false,
            "source":"appServer","threadSource":"user","status":{"type":"idle"},"turns":[]});
        let seed = json!({"rows":[done,nested,live,unrelated,other],"loaded":["agent-live","agent-other"],
            "pageSize":1,"ephemeralHistoryUnavailable":true,"emitStarted":true,
            "events":[{"method":"turn/started","params":{"threadId":"agent-live",
                "turn":{"id":"live-turn","status":"inProgress","startedAt":3}}}]});
        authorization
            .controller
            .harness
            .seed_test_subagents(seed.clone())
            .await
            .unwrap();
        (directory, authorization, root, seed)
    }

    #[tokio::test]
    async fn harness_subagents_restore_nested_archived_and_ephemeral_children_read_only() {
        let (directory, authorization, root, _) = subagent_fixture().await;
        let controller = &authorization.controller;
        let list = controller
            .call_authorized("relatedThreads", json!({"threadId":root}), &authorization)
            .await
            .unwrap();
        assert_eq!(list["subagents"].as_array().unwrap().len(), 1);
        assert_eq!(list["subagents"][0]["id"], "agent-live");
        assert_eq!(list["subagents"][0]["status"], "active");
        assert_eq!(list["completedSubagentCount"], 2);
        assert_eq!(list["subagentsComplete"], true);
        assert_eq!(list["sideThreads"], json!([]));
        assert_eq!(list["canCreateSideThread"], true);
        assert!(!list.to_string().contains("agent-other"));
        for id in ["agent-done", "agent-live"] {
            let detail = controller
                .call_authorized("thread", json!({"threadId":id}), &authorization)
                .await
                .unwrap();
            assert_eq!(detail["thread"]["readOnly"], true);
            assert_eq!(detail["thread"]["viewKind"], "subagent");
            assert_eq!(detail["thread"]["parentThreadId"], root);
            assert_eq!(detail["thread"]["requests"], json!([]));
            assert_eq!(detail["thread"]["queuedMessages"], json!([]));
            assert!(detail["thread"]["editableMessage"].is_null());
        }
        let main = controller
            .call_authorized("thread", json!({"threadId":root}), &authorization)
            .await
            .unwrap();
        assert_eq!(main["thread"]["viewKind"], "main");
        assert_eq!(main["thread"]["readOnly"], false);
        let log = std::fs::read_to_string(directory.path().join("requests.jsonl")).unwrap();
        assert!(
            log.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|q| q["method"] == "thread/list" && q["params"]["ancestorThreadId"] == root)
                .count()
                >= 2
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_subagent_history_pages_and_all_write_guards_remain_separate() {
        let (directory, authorization, root, _) = subagent_fixture().await;
        let controller = &authorization.controller;
        let first = controller
            .call_authorized("history", json!({"threadId":"agent-done"}), &authorization)
            .await
            .unwrap();
        assert!(
            first["data"][0]["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["id"] == "child-user")
        );
        let cursor = &first["data"][0]["itemsNextCursor"];
        assert!(cursor.is_string());
        let next = controller.call_authorized("history", json!({
            "threadId":"agent-done","turnId":"child-turn","turnStatus":"completed","itemCursor":cursor
        }), &authorization).await.unwrap();
        assert_eq!(next["scope"], "items");
        assert_eq!(next["data"].as_array().unwrap().len(), 40);
        let ephemeral = controller
            .call_authorized("history", json!({"threadId":"agent-live"}), &authorization)
            .await
            .unwrap();
        assert_eq!(ephemeral["data"][0]["items"][0]["text"], "运行中的输出");
        assert_eq!(ephemeral["turnsComplete"], true);
        assert_eq!(ephemeral["data"][0]["itemsComplete"], true);
        for action in [
            "send",
            "stop",
            "rename",
            "compact",
            "pinThread",
            "approve",
            "answer",
            "editMessage",
            "deleteThread",
            "setGoal",
            "updateGoal",
            "markRead",
            "removeQueued",
            "reorderQueued",
            "beginQueuedEdit",
            "editQueued",
            "cancelQueuedEdit",
            "dismissQueuedEdit",
            "steerQueued",
            "createSideThread",
            "closeSideThread",
        ] {
            assert!(controller.call_authorized(action, json!({
                "threadId":"agent-live","prompt":"禁止发送","name":"禁止改名","pinned":true
            }), &authorization).await.is_err(), "{action} 不能对只读子智能体执行");
        }
        let requests = std::fs::read_to_string(directory.path().join("requests.jsonl")).unwrap();
        let child_writes: Vec<_> = requests
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|q| {
                q["params"]["threadId"] != root
                    && matches!(
                        q["method"].as_str(),
                        Some(
                            "thread/resume"
                                | "turn/start"
                                | "turn/interrupt"
                                | "thread/name/set"
                                | "thread/compact/start"
                                | "thread/section/move"
                                | "thread/delete"
                                | "thread/goal/set"
                                | "thread/goal/update"
                                | "thread/readState/update"
                        )
                    )
            })
            .collect();
        assert!(child_writes.is_empty(), "{child_writes:?}");
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_subagent_snapshot_unavailable_does_not_expire_an_active_child() {
        let (_directory, authorization, _root, mut seed) = subagent_fixture().await;
        let controller = &authorization.controller;
        for (code, message) in [
            (-32600, "ephemeral threads do not support includeTurns"),
            (-32000, "snapshot read temporarily failed"),
        ] {
            seed["snapshotError"] = json!({"code":code,"message":message});
            controller
                .harness
                .seed_test_subagents(seed.clone())
                .await
                .unwrap();
            let failure = controller
                .call_authorized("history", json!({"threadId":"agent-live"}), &authorization)
                .await
                .unwrap_err();
            assert_eq!(failure.downcast_ref::<ApiError>().unwrap().code, 503);
            let detail = controller
                .call_authorized("thread", json!({"threadId":"agent-live"}), &authorization)
                .await
                .unwrap();
            assert_eq!(detail["thread"]["readOnly"], true);
            assert_eq!(detail["thread"]["status"]["type"], "active");
        }
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_subagents_expiry_and_lookup_failures_do_not_restart_or_fake_empty() {
        let (_directory, authorization, root, mut seed) = subagent_fixture().await;
        let controller = &authorization.controller;
        controller
            .call_authorized("relatedThreads", json!({"threadId":root}), &authorization)
            .await
            .unwrap();
        seed["rows"]
            .as_array_mut()
            .unwrap()
            .retain(|row| row["id"] != "agent-live");
        seed["loaded"] = json!([]);
        seed["events"] = json!([{"method":"thread/closed","params":{"threadId":"agent-live"}}]);
        controller
            .harness
            .seed_test_subagents(seed.clone())
            .await
            .unwrap();
        let failure = controller
            .call_authorized("thread", json!({"threadId":"agent-live"}), &authorization)
            .await
            .unwrap_err();
        assert_eq!(failure.downcast_ref::<ApiError>().unwrap().code, 410);
        seed["listFailure"] = json!(true);
        controller.harness.seed_test_subagents(seed).await.unwrap();
        let partial = controller
            .call_authorized("relatedThreads", json!({"threadId":root}), &authorization)
            .await
            .unwrap();
        assert_eq!(partial["subagentsComplete"], false);
        assert!(partial["subagentsError"].is_string());
        assert_eq!(partial["canCreateSideThread"], true);
        assert!(
            controller
                .call_authorized("thread", json!({"threadId":root}), &authorization)
                .await
                .is_ok()
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_boundary_separates_active_parent_history_from_new_instructions() {
        let (directory, authorization) = harness_fixture().await;
        std::fs::write(
            directory.path().join("side-policy.json"),
            serde_json::to_vec(&json!({
                "developerInstructions":"Keep the configured project instruction."
            }))
            .unwrap(),
        )
        .unwrap();
        let controller = &authorization.controller;
        let main = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"wait parent-only active task"}),
                &authorization,
            )
            .await
            .unwrap();
        let parent = main["threadId"].as_str().unwrap();
        let created = controller
            .call_authorized(
                "createSideThread",
                json!({"threadId":parent}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        let requests = harness_engine_requests(directory.path());
        let fork_index = requests
            .iter()
            .position(|request| request["method"] == "thread/fork")
            .unwrap();
        let instructions = requests[fork_index]["params"]["developerInstructions"]
            .as_str()
            .unwrap();
        assert!(instructions.starts_with("Keep the configured project instruction."));
        assert!(instructions.contains("reference context only"));
        assert!(instructions.contains("Do not continue, execute, or complete"));
        assert!(instructions.contains("Do not interact with existing or new sub-agents"));
        assert!(
            instructions
                .contains("unless the user explicitly requests that mutation after the boundary")
        );
        let inject_index = requests
            .iter()
            .position(|request| {
                request["method"] == "thread/inject_items" && request["params"]["threadId"] == id
            })
            .unwrap();
        assert!(fork_index < inject_index);
        let boundary = &requests[inject_index]["params"]["items"][0];
        assert_eq!(boundary["role"], "user");
        assert_eq!(boundary["content"][0]["type"], "input_text");
        let boundary_text = boundary["content"][0]["text"].as_str().unwrap();
        assert!(boundary_text.starts_with("Side conversation boundary."));
        assert!(boundary_text.contains(&json!({"threadId":parent}).to_string()));
        assert!(
            !requests.iter().any(|request| {
                request["params"]["threadId"] == id && request["method"] == "turn/start"
            }),
            "边界注入不能自行启动侧边任务"
        );
        assert_eq!(
            controller
                .call_authorized("history", json!({"threadId":id}), &authorization)
                .await
                .unwrap()["data"],
            json!([]),
            "边界消息只进入模型上下文，不进入聊天正文"
        );
        controller
            .call_authorized(
                "send",
                json!({"threadId":id,"prompt":"wait side-only new question","clientUserMessageId":"side-boundary-message"}),
                &authorization,
            )
            .await
            .unwrap();
        let contexts: Vec<Value> =
            std::fs::read_to_string(directory.path().join("model-inputs.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        let context = contexts
            .iter()
            .find(|value| value["threadId"] == id)
            .unwrap();
        assert_eq!(context["developerInstructions"], instructions);
        let texts: Vec<_> = context["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["content"][0]["text"].as_str())
            .collect();
        assert_eq!(
            texts,
            vec![
                "wait parent-only active task",
                boundary_text,
                "wait side-only new question"
            ],
            "父任务必须仍在边界前，侧边新要求必须位于边界后"
        );
        let requests = harness_engine_requests(directory.path());
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "thread/inject_items")
                .count(),
            1,
            "发送不会再次插入边界，避免把已有侧边要求变成历史参考"
        );
        assert!(!requests.iter().any(|request| {
            request["params"]["threadId"] == parent
                && matches!(
                    request["method"].as_str(),
                    Some("turn/steer" | "turn/interrupt")
                )
        }));
        let history = controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap()
            .to_string();
        assert!(history.contains("side-only new question"));
        assert!(!history.contains("parent-only active task"));
        assert!(!history.contains("Side conversation boundary."));
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_inherits_selected_model_effort_and_parent_permissions() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let main = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"wait parent","model":"parent-model",
                    "reasoningEffort":"high","permissionMode":"guardian-approvals"}),
                &authorization,
            )
            .await
            .unwrap();
        let parent = main["threadId"].as_str().unwrap();
        controller
            .harness
            .seed_test_history(json!({"threadId":parent,"contexts":[{
                "turn_id":main["turnId"],"collaboration_mode":{"mode":"plan",
                    "settings":{"developer_instructions":null}}}]}))
            .await
            .unwrap();
        let created = controller
            .call_authorized(
                "createSideThread",
                json!({"threadId":parent,"model":"parent-selected","reasoningEffort":"xhigh"}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        assert_eq!(created["thread"]["model"], "parent-selected");
        assert_eq!(created["thread"]["reasoningEffort"], "xhigh");
        assert_eq!(created["thread"]["permissionMode"], "guardian-approvals");
        assert_eq!(
            created["thread"]["capabilities"]["sessionPermissions"],
            false
        );
        let calls = harness_engine_requests(directory.path());
        let fork = calls
            .iter()
            .find(|call| call["method"] == "thread/fork")
            .unwrap();
        assert_eq!(fork["params"]["model"], "parent-selected");
        assert_eq!(fork["params"]["config"]["model_reasoning_effort"], "xhigh");
        assert_eq!(fork["params"]["config"]["thread_unload_delay_secs"], 0);
        assert_eq!(fork["params"]["config"]["features.goals"], false);
        assert!(
            calls
                .iter()
                .filter(|call| call["method"] == "thread/start")
                .all(|call| call["params"]["config"].get("features.goals").is_none()),
            "临时侧边的目标覆盖不能写入主会话"
        );
        assert_eq!(fork["params"]["approvalPolicy"], "on-request");
        assert_eq!(fork["params"]["approvalsReviewer"], "auto_review");
        assert_eq!(fork["params"]["sandbox"], "workspace-write");
        assert_eq!(
            fork["params"]["config"]["sandbox_workspace_write.writable_roots"],
            json!([created["thread"]["cwd"]])
        );
        controller
            .call_authorized(
                "setPermissions",
                json!({"threadId":parent,"permissionMode":"full-access"}),
                &authorization,
            )
            .await
            .unwrap();
        controller
            .harness
            .seed_test_history(json!({"threadId":parent,"contexts":[{
                "turn_id":main["turnId"],"collaboration_mode":{"mode":"default",
                    "settings":{"developer_instructions":null}}}]}))
            .await
            .unwrap();
        controller
            .call_authorized(
                "send",
                json!({"threadId":id,"prompt":"wait first side","clientUserMessageId":"inherited-permissions",
                    "permissionMode":"full-access","collaborationMode":{"mode":"default"}}),
                &authorization,
            )
            .await
            .unwrap();
        let second = controller
            .call_authorized(
                "createSideThread",
                json!({"threadId":parent,"model":"second-selected","reasoningEffort":"low"}),
                &authorization,
            )
            .await
            .unwrap();
        let second_id = second["threadId"].as_str().unwrap();
        controller
            .call_authorized(
                "send",
                json!({"threadId":second_id,"prompt":"wait second side","clientUserMessageId":"second-permissions",
                    "permissionMode":"read-only"}),
                &authorization,
            )
            .await
            .unwrap();
        let calls = harness_engine_requests(directory.path());
        let first_turn = calls
            .iter()
            .find(|call| call["method"] == "turn/start" && call["params"]["threadId"] == id)
            .unwrap();
        assert_eq!(first_turn["params"]["approvalPolicy"], "on-request");
        assert_eq!(first_turn["params"]["approvalsReviewer"], "auto_review");
        assert_eq!(
            first_turn["params"]["sandboxPolicy"]["type"],
            "workspaceWrite"
        );
        assert_eq!(
            first_turn["params"]["collaborationMode"],
            json!({"mode":"plan","settings":{"model":"parent-selected",
                "reasoning_effort":"xhigh","developer_instructions":null}}),
            "父模式后续变化及客户端覆盖不能改变已有侧边的 Plan 模式"
        );
        let second_turn = calls
            .iter()
            .find(|call| call["method"] == "turn/start" && call["params"]["threadId"] == second_id)
            .unwrap();
        assert_eq!(second["thread"]["model"], "second-selected");
        assert_eq!(second["thread"]["reasoningEffort"], "low");
        assert_eq!(second_turn["params"]["approvalPolicy"], "never");
        assert_eq!(
            second_turn["params"]["collaborationMode"],
            json!({"mode":"default","settings":{"model":"second-selected",
                "reasoning_effort":"low","developer_instructions":null}})
        );
        assert_eq!(
            second_turn["params"]["sandboxPolicy"]["type"],
            "dangerFullAccess"
        );
        let failure = controller
            .call_authorized(
                "setPermissions",
                json!({"threadId":id,"permissionMode":"full-access"}),
                &authorization,
            )
            .await
            .unwrap_err();
        assert_eq!(failure.downcast_ref::<ApiError>().unwrap().code, 409);
        assert!(controller.permissions.get(id).await.is_none());
        assert_eq!(
            controller
                .call_authorized("thread", json!({"threadId":id}), &authorization)
                .await
                .unwrap()["thread"]["permissionMode"],
            "guardian-approvals",
            "父会话后续改变权限不能覆盖已有侧边"
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_mode_uses_retained_history_and_rejects_missing_context() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let main = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"wait mode source"}),
                &authorization,
            )
            .await
            .unwrap();
        let parent = main["threadId"].as_str().unwrap();
        let turn_id = main["turnId"].as_str().unwrap();
        controller
            .harness
            .seed_test_history(json!({"threadId":parent,"contexts":[
                {"turn_id":turn_id,"collaboration_mode":{"mode":"plan",
                    "settings":{"developer_instructions":"mode-fixture"}}},
                {"turn_id":"reverted-turn","collaboration_mode":{"mode":"default"}}
            ],"turns":[
                {"id":turn_id,"status":"completed","items":[]},
                {"id":"standalone-compaction","status":"completed",
                    "items":[{"id":"compact","type":"contextCompaction"}]}
            ]}))
            .await
            .unwrap();
        let side = controller
            .call_authorized(
                "createSideThread",
                json!({"threadId":parent}),
                &authorization,
            )
            .await
            .unwrap();
        let id = side["threadId"].as_str().unwrap();
        controller
            .call_authorized(
                "send",
                json!({"threadId":id,"prompt":"wait inherited mode",
                    "clientUserMessageId":"retained-mode","model":"changed-side-model",
                    "reasoningEffort":"low"}),
                &authorization,
            )
            .await
            .unwrap();
        let calls = harness_engine_requests(directory.path());
        let start = calls
            .iter()
            .find(|call| call["method"] == "turn/start" && call["params"]["threadId"] == id)
            .unwrap();
        assert_eq!(
            start["params"]["collaborationMode"],
            json!({"mode":"plan","settings":{"model":"changed-side-model",
                "reasoning_effort":"low","developer_instructions":"mode-fixture"}}),
            "跳过独立压缩和已回退轮次，保留模式指令并使用侧边当前模型"
        );
        for mode in [Value::Null, json!({"mode":"unsupported"})] {
            let contexts = if mode.is_null() {
                json!([])
            } else {
                json!([{"turn_id":"missing-context","collaboration_mode":mode}])
            };
            controller
                .harness
                .seed_test_history(json!({"threadId":parent,"contexts":contexts,
                    "turns":[{"id":"missing-context","status":"completed","items":[]}]}))
                .await
                .unwrap();
            let failure = controller
                .call_authorized(
                    "createSideThread",
                    json!({"threadId":parent}),
                    &authorization,
                )
                .await
                .unwrap_err();
            assert_eq!(failure.downcast_ref::<ApiError>().unwrap().code, 503);
        }
        assert_eq!(
            harness_engine_requests(directory.path())
                .iter()
                .filter(|call| call["method"] == "thread/fork")
                .count(),
            1,
            "无法确认模式时必须在创建临时分支前失败"
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_preserves_native_profiles_and_custom_permission_boundaries() {
        for named in [false, true] {
            let (directory, authorization) = harness_fixture().await;
            let sandbox = json!({"type":"workspaceWrite","writableRoots":[directory.path().join("scoped")],
                "networkAccess":false,"excludeTmpdirEnvVar":true,"excludeSlashTmp":true});
            let approval = json!({"reject":{"sandbox_approval":true,"rules":true}});
            std::fs::write(
                directory.path().join("side-policy.json"),
                serde_json::to_vec(&json!({"forkPermissions":{
                    "approvalPolicy":approval,"approvalsReviewer":"user","sandbox":sandbox,
                    "activePermissionProfile":named.then(|| json!({"id":"fixture-restricted-profile"})),
                    "runtimeWorkspaceRoots":[directory.path().join("scoped")]
                }}))
                .unwrap(),
            )
            .unwrap();
            let controller = &authorization.controller;
            let main = controller
                .call_authorized(
                    "create",
                    json!({"projectId":"project-a","prompt":"wait parent"}),
                    &authorization,
                )
                .await
                .unwrap();
            let created = controller
                .call_authorized(
                    "createSideThread",
                    json!({"threadId":main["threadId"]}),
                    &authorization,
                )
                .await
                .unwrap();
            let id = created["threadId"].as_str().unwrap();
            controller
                .call_authorized("send", json!({"threadId":id,"prompt":"wait side","clientUserMessageId":"profile-side"}), &authorization)
                .await
                .unwrap();
            let calls = harness_engine_requests(directory.path());
            let turn = &calls
                .iter()
                .find(|call| call["method"] == "turn/start" && call["params"]["threadId"] == id)
                .unwrap()["params"];
            assert_eq!(turn["approvalPolicy"], approval);
            assert_eq!(
                turn["runtimeWorkspaceRoots"],
                json!([directory.path().join("scoped")])
            );
            if named {
                assert_eq!(turn["permissions"], "fixture-restricted-profile");
                assert!(
                    turn.get("sandboxPolicy").is_none(),
                    "配置 ID 与沙盒覆盖不能同时发送"
                );
            } else {
                assert_eq!(
                    turn["sandboxPolicy"], sandbox,
                    "不能扩大目录、网络或临时目录权限"
                );
                assert!(turn.get("permissions").is_none());
            }
            controller.harness.release().await;
        }
    }

    #[tokio::test]
    async fn harness_service_side_boundary_failure_never_opens_or_replays_an_unisolated_fork() {
        for (policy, outcome_unknown, inject) in [
            (json!({"boundaryError":-32601}), false, true),
            (
                json!({"boundaryError":-32603,"acceptBoundaryBeforeError":true}),
                false,
                true,
            ),
            (
                json!({"boundaryError":-32603,"acceptBoundaryBeforeError":true,"unsubscribeError":-32603}),
                true,
                true,
            ),
            (json!({"forkPermissions":{"sandbox":null}}), false, false),
        ] {
            let (directory, authorization) = harness_fixture().await;
            std::fs::write(
                directory.path().join("side-policy.json"),
                serde_json::to_vec(&policy).unwrap(),
            )
            .unwrap();
            let controller = &authorization.controller;
            let main = controller
                .call_authorized(
                    "create",
                    json!({"projectId":"project-a","prompt":"wait parent-only active task"}),
                    &authorization,
                )
                .await
                .unwrap();
            let parent = main["threadId"].as_str().unwrap();
            let failure = controller
                .call_authorized(
                    "createSideThread",
                    json!({"threadId":parent}),
                    &authorization,
                )
                .await
                .unwrap_err();
            let failure = failure.downcast_ref::<ApiError>().unwrap();
            assert_eq!(failure.code, 503);
            assert_eq!(failure.outcome_unknown, outcome_unknown);
            assert!(failure.message.contains("侧边会话初始化失败"));
            let requests = harness_engine_requests(directory.path());
            let writes: Vec<_> = requests
                .iter()
                .filter(|request| {
                    matches!(
                        request["method"].as_str(),
                        Some("thread/fork" | "thread/inject_items" | "thread/unsubscribe")
                    )
                })
                .collect();
            assert_eq!(
                writes.len(),
                if inject { 3 } else { 2 },
                "创建、注入和释放均只能派发一次"
            );
            assert_eq!(writes[0]["method"], "thread/fork");
            assert_eq!(
                writes[1]["method"],
                if inject {
                    "thread/inject_items"
                } else {
                    "thread/unsubscribe"
                }
            );
            assert_eq!(writes.last().unwrap()["method"], "thread/unsubscribe");
            let id = writes[1]["params"]["threadId"].as_str().unwrap();
            assert_ne!(id, parent);
            assert_eq!(writes.last().unwrap()["params"]["threadId"], id);
            assert_eq!(
                controller
                    .call_authorized("relatedThreads", json!({"threadId":parent}), &authorization)
                    .await
                    .unwrap()["sideThreads"],
                json!([])
            );
            assert!(
                controller
                    .call_authorized(
                        "send",
                        json!({"threadId":id,"prompt":"must not run","clientUserMessageId":"unisolated-side"}),
                        &authorization,
                    )
                    .await
                    .is_err(),
                "即使关闭结果未知，也不能绕过登记向未初始化侧边发送"
            );
            assert!(
                !harness_engine_requests(directory.path())
                    .iter()
                    .any(|request| {
                        request["params"]["threadId"] == id && request["method"] == "turn/start"
                    })
            );
            assert!(
                controller
                    .call_authorized("thread", json!({"threadId":parent}), &authorization)
                    .await
                    .is_ok(),
                "边界失败不能关闭父会话"
            );
            controller.harness.release().await;
        }
    }

    #[tokio::test]
    async fn harness_service_side_idle_presence_expiry_and_persistent_resume() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let parent = controller.call_authorized("create", json!({"projectId":"project-a","prompt":"父上下文"}),
            &authorization).await.unwrap()["threadId"].as_str().unwrap().to_owned();
        std::fs::write(directory.path().join("remote-instance-preferences.json"),
            r#"{"instances":{"codex:local":{"persistentSideThreads":true}}}"#).unwrap();
        let id = controller.call_authorized("createSideThread", json!({"threadId":parent}), &authorization)
            .await.unwrap()["threadId"].as_str().unwrap().to_owned();
        controller.call_authorized("send", json!({"threadId":id,"prompt":"持久侧边保留历史",
            "clientUserMessageId":"idle-first"}), &authorization).await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        controller.harness.release_idle_sides_for_test().await;
        assert!(!harness_engine_requests(directory.path()).iter().any(|frame| frame["method"] == "thread/unsubscribe"));
        // 同一设备两个页面分别租约，关闭一页不能使另一页仍在看的侧边被退订。
        for page in ["page-a", "page-b"] {
            controller.call_authorized("sidePresence", json!({"threadId":id,"viewId":page,"sequence":1,"visible":true}),
                &authorization).await.unwrap();
        }
        controller.call_authorized("sidePresence", json!({"threadId":id,"viewId":"page-a","sequence":2,"visible":false}),
            &authorization).await.unwrap();
        controller.harness.age_side_for_test(&id, 10801).await;
        controller.harness.release_idle_sides_for_test().await;
        assert!(!harness_engine_requests(directory.path()).iter().any(|frame| frame["method"] == "thread/unsubscribe"));
        controller.call_authorized("sidePresence", json!({"threadId":id,"viewId":"page-b","sequence":2,"visible":false}),
            &authorization).await.unwrap();
        controller.call_authorized("sidePresence", json!({"threadId":id,"viewId":"page-b","sequence":1,"visible":true}),
            &authorization).await.unwrap();
        controller.harness.age_side_for_test(&id, 10801).await;
        controller.harness.release_idle_sides_for_test().await;
        let detail = controller.call_authorized("thread", json!({"threadId":id}), &authorization).await.unwrap();
        assert_eq!(detail["thread"]["ephemeral"], false);
        assert_ne!(detail["thread"]["sideThreadExpired"], true);
        let history = controller.call_authorized("history", json!({"threadId":id}), &authorization).await.unwrap();
        assert!(history.to_string().contains("持久侧边保留历史"));
        assert!(!harness_engine_requests(directory.path()).iter().any(|frame| frame["method"] == "thread/resume"),
            "闲置后读取不应恢复执行");
        let sent = controller.call_authorized("send", json!({"threadId":id,"prompt":"退订后继续",
            "clientUserMessageId":"idle-second"}), &authorization).await.unwrap();
        assert_eq!(sent["deliveryState"], "accepted");
        let requests = harness_engine_requests(directory.path());
        assert_eq!(requests.iter().filter(|frame| frame["method"] == "thread/unsubscribe").count(), 1);
        assert_eq!(requests.iter().filter(|frame| frame["method"] == "thread/resume").count(), 1);
        assert!(!requests.iter().any(|frame| matches!(frame["method"].as_str(), Some("thread/delete" | "turn/interrupt"))));
        tokio::time::sleep(Duration::from_millis(80)).await;
        controller.call_authorized("sidePresence", json!({"threadId":id,"viewId":"lost-page","sequence":1,"visible":true}),
            &authorization).await.unwrap();
        controller.harness.expire_side_views_for_test().await;
        controller.harness.age_side_for_test(&id, 10801).await;
        controller.harness.release_idle_sides_for_test().await;
        assert_eq!(harness_engine_requests(directory.path()).iter().filter(|frame| frame["method"] == "thread/unsubscribe").count(), 2,
            "页面直接离开且没有隐藏回执时，过期租约不能永久阻止闲置回收");
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_idle_keeps_running_queue_questions_and_unknown_results() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let parent = controller.call_authorized("create", json!({"projectId":"project-a","prompt":"父上下文"}),
            &authorization).await.unwrap()["threadId"].as_str().unwrap().to_owned();
        let id = controller.call_authorized("createSideThread", json!({"threadId":parent}), &authorization)
            .await.unwrap()["threadId"].as_str().unwrap().to_owned();
        controller.call_authorized("send", json!({"threadId":id,"prompt":"wait 执行中",
            "clientUserMessageId":"idle-active"}), &authorization).await.unwrap();
        controller.harness.age_side_for_test(&id, 10801).await;
        controller.harness.release_idle_sides_for_test().await;
        controller.call_authorized("send", json!({"threadId":id,"prompt":"队列保留",
            "clientUserMessageId":"idle-queued"}), &authorization).await.unwrap();
        controller.call_authorized("stop", json!({"threadId":id}), &authorization).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        controller.harness.age_side_for_test(&id, 10801).await;
        controller.harness.release_idle_sides_for_test().await;
        assert!(!harness_engine_requests(directory.path()).iter().any(|frame| frame["method"] == "thread/unsubscribe"),
            "暂停队列也不能被闲置回收清理");
        let queue = controller.call_authorized("queue", json!({"threadId":id}), &authorization).await.unwrap();
        controller.call_authorized("removeQueued", json!({"threadId":id,"messageId":queue["queuedMessages"][0]["id"]}),
            &authorization).await.unwrap();
        // 原生未知状态、待处理问答仍需保留，不能仅依赖计时器判断为空闲。
        controller.harness.seed_test_history(json!({"threadId":id,"events":[
            {"method":"thread/status/changed","params":{"threadId":id,"status":{"type":"unknown"}}}
        ]})).await.unwrap();
        let mut policy = json!({"readOverrides":{}});
        policy["readOverrides"][&id] = json!({"status":{"type":"unknown"}});
        std::fs::write(directory.path().join("side-policy.json"), policy.to_string()).unwrap();
        controller.harness.age_side_for_test(&id, 10801).await;
        controller.harness.release_idle_sides_for_test().await;
        assert!(!harness_engine_requests(directory.path()).iter().any(|frame| frame["method"] == "thread/unsubscribe"));
        std::fs::write(directory.path().join("side-policy.json"), "{}").unwrap();
        controller.harness.seed_test_history(json!({"threadId":id,"events":[
            {"method":"thread/status/changed","params":{"threadId":id,"status":{"type":"idle"}}},
            {"id":991,"method":"item/tool/requestUserInput","params":{"threadId":id,"questions":[
                {"id":"idle-question","header":"确认","question":"保留待处理问答","options":[]}
            ]}}
        ]})).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        controller.harness.age_side_for_test(&id, 10801).await;
        controller.harness.release_idle_sides_for_test().await;
        assert!(!harness_engine_requests(directory.path()).iter().any(|frame| frame["method"] == "thread/unsubscribe"));
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_idle_persistent_overage_and_unknown_unsubscribe() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let parent = controller.call_authorized("create", json!({"projectId":"project-a","prompt":"父上下文"}),
            &authorization).await.unwrap()["threadId"].as_str().unwrap().to_owned();
        std::fs::write(directory.path().join("remote-instance-preferences.json"),
            r#"{"instances":{"codex:local":{"persistentSideThreads":true}}}"#).unwrap();
        let mut ids = Vec::new();
        for _ in 0..11 {
            ids.push(controller.call_authorized("createSideThread", json!({"threadId":parent}), &authorization)
                .await.unwrap()["threadId"].as_str().unwrap().to_owned());
        }
        std::fs::write(directory.path().join("side-policy.json"), r#"{"idleEventBurst":true}"#).unwrap();
        tokio::time::timeout(Duration::from_secs(10), controller.harness.release_idle_sides_for_test()).await
            .expect("回收不能持租约锁等待 RPC，阻断其他会话的流式事件");
        let requests = harness_engine_requests(directory.path());
        let unsubscribes: Vec<_> = requests.iter().filter(|frame| frame["method"] == "thread/unsubscribe").collect();
        assert_eq!(unsubscribes.len(), 1, "超过 10 个未活动持久加载项时只回收最早的超出项");
        assert_eq!(unsubscribes[0]["params"]["threadId"], ids[0]);
        std::fs::write(directory.path().join("side-policy.json"), r#"{"idleUnsubscribeError":true}"#).unwrap();
        controller.harness.age_side_for_test(&ids[1], 10801).await;
        controller.harness.release_idle_sides_for_test().await;
        controller.harness.release_idle_sides_for_test().await;
        assert_eq!(harness_engine_requests(directory.path()).iter()
            .filter(|frame| frame["method"] == "thread/unsubscribe" && frame["params"]["threadId"] == ids[1]).count(), 1,
            "退订结果未知不能自动重试或误报成功");
        let detail = controller.call_authorized("thread", json!({"threadId":ids[1]}), &authorization).await.unwrap();
        assert_ne!(detail["thread"]["sideThreadExpired"], true);
        assert_eq!(detail["thread"]["sideIdleReleaseUnknown"], true);
        assert!(controller.call_authorized("send", json!({"threadId":ids[1],"prompt":"不能越过未知退订",
            "clientUserMessageId":"idle-unknown-barrier"}), &authorization).await.is_err());
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_idle_new_view_cancels_before_unsubscribe() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let parent = controller.call_authorized("create", json!({"projectId":"project-a","prompt":"父上下文"}),
            &authorization).await.unwrap()["threadId"].as_str().unwrap().to_owned();
        std::fs::write(directory.path().join("remote-instance-preferences.json"),
            r#"{"instances":{"codex:local":{"persistentSideThreads":true}}}"#).unwrap();
        let id = controller.call_authorized("createSideThread", json!({"threadId":parent}), &authorization)
            .await.unwrap()["threadId"].as_str().unwrap().to_owned();
        std::fs::write(directory.path().join("side-policy.json"),
            json!({"idleReadDelayMs":300,"idleReadId":id}).to_string()).unwrap();
        let previous = harness_engine_requests(directory.path()).len();
        controller.harness.age_side_for_test(&id, 10801).await;
        let harness = controller.harness.clone();
        let release = tokio::spawn(async move { harness.release_idle_sides_for_test().await; });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !harness_engine_requests(directory.path()).iter().skip(previous).any(|frame|
                frame["method"] == "thread/read" && frame["params"]["threadId"] == id)
            { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.unwrap();
        controller.call_authorized("sidePresence", json!({"threadId":id,"viewId":"reopened-page",
            "sequence":1,"visible":true}), &authorization).await.unwrap();
        release.await.unwrap();
        assert!(!harness_engine_requests(directory.path()).iter().any(|frame| frame["method"] == "thread/unsubscribe"),
            "只读预检期间重新打开侧边时，最终派发必须取消旧闲置候选");
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_idle_temporary_history_and_capacity_removal() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let parent = controller.call_authorized("create", json!({"projectId":"project-a","prompt":"父上下文"}),
            &authorization).await.unwrap()["threadId"].as_str().unwrap().to_owned();
        let mut ids = Vec::new();
        for _ in 0..17 {
            ids.push(controller.call_authorized("createSideThread", json!({"threadId":parent}), &authorization)
                .await.unwrap()["threadId"].as_str().unwrap().to_owned());
        }
        let id = &ids[0];
        controller.call_authorized("send", json!({"threadId":id,"prompt":"闲置前已取得的正文",
            "clientUserMessageId":"idle-temporary"}), &authorization).await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        controller.harness.age_side_for_test(id, 10801).await;
        controller.harness.release_idle_sides_for_test().await;
        let detail = controller.call_authorized("thread", json!({"threadId":id}), &authorization).await.unwrap();
        assert_eq!(detail["thread"]["sideIdleReleased"], true);
        assert_eq!(detail["thread"]["readOnly"], true);
        let history = controller.call_authorized("history", json!({"threadId":id}), &authorization).await.unwrap();
        assert!(history.to_string().contains("闲置前已取得的正文"));
        assert!(controller.call_authorized("send", json!({"threadId":id,"prompt":"不自动复活",
            "clientUserMessageId":"idle-expired"}), &authorization).await.is_err());
        let requests = harness_engine_requests(directory.path());
        assert_eq!(requests.iter().filter(|frame| frame["method"] == "thread/unsubscribe").count(), 1);
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_boundary_initialization_holds_child_lock() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let parent = controller.call_authorized("create", json!({"projectId":"project-a","prompt":"父上下文"}),
            &authorization).await.unwrap()["threadId"].as_str().unwrap().to_owned();
        std::fs::write(directory.path().join("remote-instance-preferences.json"),
            r#"{"instances":{"codex:local":{"persistentSideThreads":true}}}"#).unwrap();
        std::fs::write(directory.path().join("side-policy.json"), r#"{"boundaryDelayMs":500}"#).unwrap();
        let create_auth = authorization.clone();
        let creating = tokio::spawn(async move {
            create_auth.controller.call_authorized("createSideThread", json!({"threadId":parent}), &create_auth).await
        });
        let id = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(frame) = harness_engine_requests(directory.path()).iter()
                    .find(|frame| frame["method"] == "thread/inject_items")
                { return frame["params"]["threadId"].as_str().unwrap().to_owned(); }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        let delete_auth = authorization.clone();
        let deleted_id = id.clone();
        let deleting = tokio::spawn(async move {
            delete_auth.controller.call_authorized("deleteThread", json!({"threadId":deleted_id}), &delete_auth).await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!deleting.is_finished(), "边界注入尚未结束，删除必须等待该侧边初始化完成");
        assert!(!harness_engine_requests(directory.path()).iter().any(|frame| frame["method"] == "thread/delete"));
        let created = creating.await.unwrap().unwrap();
        assert_eq!(created["threadId"], id);
        assert_eq!(deleting.await.unwrap().unwrap()["deleted"], true);
        let saved: Value = serde_json::from_slice(&std::fs::read(directory.path().join("remote-harness-state.json")).unwrap()).unwrap();
        assert!(saved["persistentSides"].get(&id).is_none());
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_boundary_external_unknown_delete_keeps_parent_relation() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let parent = json!({"id":"external-parent","cwd":directory.path(),"ephemeral":false,
            "status":{"type":"idle"},"turns":[]});
        controller.harness.seed_test_subagents(json!({"rows":[parent,{
            "id":"external-child","cwd":directory.path(),"ephemeral":false,"attachedToThreadId":"external-parent",
            "status":{"type":"idle"},"turns":[]
        }]})).await.unwrap();
        std::fs::write(directory.path().join("delete-policy.json"),
            r#"{"error":{"code":-32000,"message":"unknown child deletion"}}"#).unwrap();
        assert!(controller.call_authorized("deleteThread", json!({"threadId":"external-parent"}), &authorization).await.is_err());
        let saved: Value = serde_json::from_slice(&std::fs::read(directory.path().join("remote-harness-state.json")).unwrap()).unwrap();
        assert_eq!(saved["deletions"]["external-child"]["attachedToThreadId"], "external-parent");
        assert_eq!(saved["deletions"]["external-child"]["state"], "unknown");
        assert!(saved["persistentSides"].get("external-child").is_none(), "外部侧边不能伪造创建权限快照");
        controller.harness.seed_test_subagents(json!({"rows":[parent]})).await.unwrap();
        let failure = controller.call_authorized("deleteThread", json!({"threadId":"external-parent"}), &authorization).await.unwrap_err();
        assert!(failure.to_string().contains("尚未确认的删除"), "{failure}");
        assert!(!harness_engine_requests(directory.path()).iter().any(|frame|
            frame["method"] == "thread/delete" && frame["params"]["threadId"] == "external-parent"));
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_boundary_invalid_snapshot_stays_read_only() {
        let snapshot = json!({"parent":"snapshot-parent","permissions":{"approvalPolicy":"never",
            "approvalsReviewer":"user","sandboxPolicy":{"type":"readOnly"}},
            "collaborationMode":{"mode":"default","settings":{"developer_instructions":null}},
            "permissionMode":"read-only","initialized":true});
        for invalid in [json!({"permissions":{}}), json!({"collaborationMode":"bad"}),
            json!({"collaborationMode":{"mode":"default","settings":[]}})]
        {
            let mut side = snapshot.clone();
            side.as_object_mut().unwrap().extend(invalid.as_object().unwrap().clone());
            let (directory, authorization) = harness_fixture_with_store(Some(json!({"persistentSides":{"snapshot-side":side}}))).await;
            let controller = &authorization.controller;
            controller.harness.seed_test_subagents(json!({"rows":[{
                "id":"snapshot-side","cwd":directory.path(),"ephemeral":false,"attachedToThreadId":"snapshot-parent",
                "status":{"type":"idle"},"turns":[]
            }]})).await.unwrap();
            let view = controller.call_authorized("thread", json!({"threadId":"snapshot-side"}), &authorization).await.unwrap();
            assert_eq!(view["thread"]["readOnly"], true);
            assert!(controller.call_authorized("send", json!({"threadId":"snapshot-side","prompt":"不能扩大权限",
                "clientUserMessageId":"invalid-snapshot"}), &authorization).await.is_err());
            assert!(!harness_engine_requests(directory.path()).iter().any(|frame|
                matches!(frame["method"].as_str(), Some("turn/start" | "thread/resume"))));
            controller.harness.enabled.store(false, Ordering::Release);
            controller.harness.release().await;
        }
    }

    #[tokio::test]
    async fn harness_service_side_boundary_malformed_attached_pages_never_delete() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        for response in [json!({}), json!({"data":null,"nextCursor":null}),
            json!({"data":[],"nextCursor":7}), json!({"data":[]})]
        {
            controller.harness.seed_test_subagents(json!({"rows":[{
                "id":"page-parent","cwd":directory.path(),"ephemeral":false,"status":{"type":"idle"},
                "attachedToThreadId":null,"turns":[]
            }]})).await.unwrap();
            std::fs::write(directory.path().join("side-policy.json"), json!({"attachedResponse":response}).to_string()).unwrap();
            let result = controller.call_authorized("deleteThread", json!({"threadId":"page-parent"}), &authorization).await;
            assert!(result.is_err(), "不完整的原生关联分页不能作为无侧边的证据：{response}");
            assert!(!harness_engine_requests(directory.path()).iter().any(|frame| frame["method"] == "thread/delete"));
        }
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_boundary_delete_rechecks_relation_and_unknown_status() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        for changed in [json!({"status":null}), json!({"status":{"type":"future-status"}}),
            json!({"attachedToThreadId":"different-parent"}), json!({"id":"different-id"})]
        {
            controller.harness.seed_test_subagents(json!({"rows":[
                {"id":"relation-parent","cwd":directory.path(),"ephemeral":false,"status":{"type":"idle"},"turns":[]},
                {"id":"relation-child","cwd":directory.path(),"ephemeral":false,"attachedToThreadId":"relation-parent",
                    "status":{"type":"idle"},"turns":[]}
            ]})).await.unwrap();
            std::fs::write(directory.path().join("side-policy.json"),
                json!({"readOverrides":{"relation-child":changed}}).to_string()).unwrap();
            let result = controller.call_authorized("deleteThread", json!({"threadId":"relation-parent"}), &authorization).await;
            assert!(result.is_err(), "删除前必须核对准确关联及明确安全状态：{changed}");
            assert!(!harness_engine_requests(directory.path()).iter().any(|frame| frame["method"] == "thread/delete"));
        }
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_boundary_read_failure_preserves_other_sessions() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let parent = controller.call_authorized("create", json!({"projectId":"project-a","prompt":"父会话"}),
            &authorization).await.unwrap()["threadId"].as_str().unwrap().to_owned();
        let temporary = controller.call_authorized("createSideThread", json!({"threadId":parent}),
            &authorization).await.unwrap()["threadId"].clone();
        std::fs::write(directory.path().join("remote-instance-preferences.json"),
            r#"{"instances":{"codex:local":{"persistentSideThreads":true}}}"#).unwrap();
        let persistent = controller.call_authorized("createSideThread", json!({"threadId":parent}),
            &authorization).await.unwrap()["threadId"].as_str().unwrap().to_owned();
        std::fs::write(directory.path().join("side-policy.json"),
            json!({"readFailures":[persistent]}).to_string()).unwrap();
        // 让已有的 200ms 只读缓存过期，避免把故障前的成功快照当成故障验证。
        tokio::time::sleep(Duration::from_millis(250)).await;
        let related = controller.call_authorized("relatedThreads", json!({"threadId":parent}), &authorization).await.unwrap();
        assert_eq!(related["sideThreadsComplete"], false);
        assert!(related["sideThreadsError"].as_str().is_some_and(|text| text.contains("read failure")));
        assert!(related["sideThreads"].as_array().unwrap().iter().any(|row| row["id"] == temporary));
        let sidebar = controller.call_authorized("sidebar", json!({}), &authorization).await.unwrap();
        assert!(sidebar["recent"].as_array().unwrap().iter().any(|row| row["id"] == parent));
        assert!(sidebar["sideThreadsError"].is_string());
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_boundary_overlapping_deletes_do_not_deadlock() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        controller.harness.seed_test_subagents(json!({"rows":[
            {"id":"root","cwd":directory.path(),"ephemeral":false,"status":{"type":"idle"},"turns":[]},
            {"id":"z-child","cwd":directory.path(),"ephemeral":false,"attachedToThreadId":"root",
                "status":{"type":"idle"},"turns":[]},
            {"id":"a-grandchild","cwd":directory.path(),"ephemeral":false,"attachedToThreadId":"z-child",
                "status":{"type":"idle"},"turns":[]}
        ]})).await.unwrap();
        let blocked = controller.harness.thread_lock("a-grandchild").await.lock_owned().await;
        let root_auth = authorization.clone();
        let mut root = tokio::spawn(async move {
            root_auth.controller.call_authorized("deleteThread", json!({"threadId":"root"}), &root_auth).await
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !harness_engine_requests(directory.path()).iter().any(|frame|
                frame["method"] == "thread/read" && frame["params"]["threadId"] == "a-grandchild")
            { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.unwrap();
        let child_auth = authorization.clone();
        let mut child = tokio::spawn(async move {
            child_auth.controller.call_authorized("deleteThread", json!({"threadId":"z-child"}), &child_auth).await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(blocked);
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            let root_result = (&mut root).await.unwrap();
            let child_result = (&mut child).await.unwrap();
            assert!(root_result.is_ok(), "{root_result:?}");
            assert!(child_result.is_ok(), "{child_result:?}");
        }).await;
        if result.is_err() { root.abort(); child.abort(); }
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
        assert!(result.is_ok(), "祖先与后代删除不能互相持锁等待");
    }

    #[tokio::test]
    async fn harness_service_side_persistent_setting_restart_queue_and_deletion() {
        let (directory, authorization) = harness_fixture().await;
        std::fs::write(directory.path().join("persistent-fixture"), "").unwrap();
        let controller = &authorization.controller;
        let main = controller.call_authorized("create",
            json!({"projectId":"project-a","prompt":"父会话参考历史","permissionMode":"read-only"}), &authorization).await.unwrap();
        let parent = main["threadId"].as_str().unwrap();
        let temporary = controller.call_authorized("createSideThread",
            json!({"threadId":parent}), &authorization).await.unwrap();
        assert_eq!(temporary["thread"]["ephemeral"], true, "缺少偏好时必须默认临时侧边");
        controller.call_authorized("closeSideThread",
            json!({"threadId":temporary["threadId"],"parentThreadId":parent}), &authorization).await.unwrap();
        let preference_path = directory.path().join("remote-instance-preferences.json");
        std::fs::write(&preference_path, r#"{"instances":{"codex:local":{"persistentSideThreads":true}}}"#).unwrap();
        let side = controller.call_authorized("createSideThread",
            json!({"threadId":parent,"permissionMode":"read-only"}), &authorization).await.unwrap();
        let id = side["threadId"].as_str().unwrap();
        assert_eq!(side["thread"]["ephemeral"], false);
        assert_eq!(side["thread"]["parentThreadId"], parent);
        assert_eq!(side["thread"]["capabilities"]["sessionPermissions"], false);
        assert_eq!(controller.call_authorized("history", json!({"threadId":id}), &authorization).await.unwrap()["data"], json!([]));
        let sent = controller.call_authorized("send",
            json!({"threadId":id,"prompt":"wait 持久侧边正文","clientUserMessageId":"persistent-first",
                "permissionMode":"full-access","collaborationMode":{"mode":"plan"}}), &authorization).await.unwrap();
        assert_eq!(sent["deliveryState"], "accepted");
        let queued = controller.call_authorized("send",
            json!({"threadId":id,"prompt":"保留的持久队列","clientUserMessageId":"persistent-queued"}), &authorization).await.unwrap();
        assert_eq!(queued["deliveryState"], "queued");
        let closed = controller.call_authorized("closeSideThread",
            json!({"threadId":id,"parentThreadId":parent}), &authorization).await.unwrap();
        assert_eq!(closed["historyRetained"], true);
        assert_eq!(controller.call_authorized("relatedThreads", json!({"threadId":parent}), &authorization).await.unwrap()["sideThreads"][0]["id"], id);
        let blocked = controller.call_authorized("deleteThread", json!({"threadId":parent}), &authorization).await.unwrap_err();
        assert!(blocked.to_string().contains("侧边会话执行中"), "{blocked}");
        controller.call_authorized("stop", json!({"threadId":id}), &authorization).await.unwrap();
        std::fs::write(&preference_path, r#"{"instances":{"codex:local":{"persistentSideThreads":false}}}"#).unwrap();
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;

        let recovered = HarnessController::new(directory.path().to_owned()).unwrap();
        recovered.publish_access_policy(&RemoteAccessSettings { enabled:true, ..Default::default() });
        recovered.running.store(true, Ordering::SeqCst);
        recovered.auth.lock().await.devices.push(authorization.device.clone());
        let recovered_auth = recovered.authorization(authorization.device.clone(), Some("recovered"));
        let mut command = tokio::process::Command::new("node");
        command.arg(directory.path().join("engine.cjs")).stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(crate::windows_create_hidden_process());
        recovered.harness.install_test_process(crate::codex_service::RunningService::for_test(command.spawn().unwrap()).unwrap()).await;
        let related = recovered.call_authorized("relatedThreads", json!({"threadId":parent}), &recovered_auth).await.unwrap();
        assert_eq!(related["sideThreads"][0]["id"], id, "关闭开关和重启不能隐藏已有持久侧边");
        let history = recovered.call_authorized("history", json!({"threadId":id}), &recovered_auth).await.unwrap();
        assert!(history.to_string().contains("持久侧边正文"));
        assert!(!history.to_string().contains("父会话参考历史"));
        assert!(recovered.call_authorized("queue", json!({"threadId":id}), &recovered_auth).await.unwrap().to_string().contains("保留的持久队列"));
        let detail = recovered.call_authorized("thread", json!({"threadId":id}), &recovered_auth).await.unwrap();
        assert_eq!(detail["thread"]["capabilities"]["forkThread"], false);
        for action in ["forkThread", "createSideThread", "pinThread", "editMessage"] {
            assert!(recovered.call_authorized(action, json!({"threadId":id,"pinned":true}), &recovered_auth).await.is_err(), "{action}");
        }
        recovered.call_authorized("send", json!({"threadId":id,"prompt":"重启后继续",
            "clientUserMessageId":"persistent-recovered","model":"gpt-6.1-sol","permissionMode":"full-access"}), &recovered_auth).await.unwrap();
        let requests: Vec<Value> = std::fs::read_to_string(directory.path().join("requests.jsonl")).unwrap()
            .lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        let forks: Vec<_> = requests.iter().filter(|request| request["method"] == "thread/fork").collect();
        assert_eq!(forks[1]["params"]["attachToSource"], true);
        assert!(forks[1]["params"]["config"].get("features.goals").is_none());
        let resumed = requests.iter().find(|request| request["method"] == "thread/resume" && request["params"]["threadId"] == id).unwrap();
        assert_eq!(resumed["params"]["sandbox"], "read-only");
        let last = requests.iter().rev().find(|request| request["method"] == "turn/start" && request["params"]["threadId"] == id).unwrap();
        assert_eq!(last["params"]["sandboxPolicy"]["type"], "readOnly");
        assert_eq!(last["params"]["collaborationMode"]["mode"], "default");
        assert_eq!(last["params"]["collaborationMode"]["settings"]["model"], "gpt-6.1-sol");
        let sidebar = recovered.call_authorized("sidebar", json!({}), &recovered_auth).await.unwrap();
        assert!(!sidebar["recent"].as_array().unwrap().iter().any(|row| row["id"] == id));
        recovered.harness.seed_test_history(json!({"threadId":id,"finish":true})).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let goal = recovered.call_authorized("setGoal", json!({"threadId":id,"prompt":"持久侧边独立目标"}),
            &recovered_auth).await.unwrap();
        assert_eq!(goal["goal"]["objective"], "持久侧边独立目标");
        recovered.call_authorized("updateGoal", json!({"threadId":id,"operation":"pause",
            "objective":goal["goal"]["objective"],"expectedCreatedAt":goal["goal"]["createdAt"],
            "expectedObjective":goal["goal"]["objective"],"expectedStatus":goal["goal"]["status"]}),
            &recovered_auth).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        recovered.harness.seed_test_subagents(json!({"rows":[
            {"id":"archived-side","attachedToThreadId":parent,"ephemeral":false,"archived":true,
                "cwd":directory.path(),"status":{"type":"idle"},"turns":[]},
            {"id":"nested-side","attachedToThreadId":"archived-side","ephemeral":false,
                "cwd":directory.path(),"status":{"type":"idle"},"turns":[]}
        ]})).await.unwrap();
        recovered.call_authorized("deleteThread", json!({"threadId":parent}), &recovered_auth).await.unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(directory.path().join("remote-harness-state.json")).unwrap()).unwrap();
        assert!(saved["persistentSides"].get(id).is_none());
        assert!(saved["queues"].get(id).is_none());
        let requests: Vec<Value> = std::fs::read_to_string(directory.path().join("requests.jsonl")).unwrap()
            .lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        let deletions: Vec<_> = requests.iter().filter(|request| request["method"] == "thread/delete")
            .map(|request| request["params"]["threadId"].clone()).collect();
        assert_eq!(deletions, vec![json!("nested-side"),json!("archived-side"),json!(id),json!(parent)]);
        recovered.harness.enabled.store(false, Ordering::Release);
        recovered.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_persistent_missing_native_child_keeps_unknown_delete_barrier() {
        let saved = json!({"persistentSides":{"lost-side":{"parent":"delete-parent",
            "permissions":{},"collaborationMode":null,"permissionMode":null,"initialized":true}},
            "deletions":{"lost-side":{"state":"unknown","deviceId":"device","requestId":"original-delete"}}});
        let (directory, authorization) = harness_fixture_with_store(Some(saved)).await;
        let controller = &authorization.controller;
        controller.harness.seed_test_subagents(json!({"rows":[{
            "id":"delete-parent","cwd":directory.path(),"ephemeral":false,"threadSource":"user",
            "attachedToThreadId":null,"status":{"type":"idle"},"turns":[]
        }]})).await.unwrap();
        let failure = controller.call_authorized("deleteThread", json!({"threadId":"delete-parent"}),
            &authorization).await.unwrap_err();
        assert!(failure.to_string().contains("尚未确认的删除"), "{failure}");
        assert!(!std::fs::read_to_string(directory.path().join("requests.jsonl")).unwrap().contains("\"thread/delete\""));
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_persistent_rejects_old_engine_and_uninitialized_history() {
        for policy in [json!({"userAgent":"codex_cli_rs/0.165.0-alpha.9"}), json!({"boundaryError":-32000})] {
            let (directory, authorization) = harness_fixture().await;
            let controller = &authorization.controller;
            std::fs::write(directory.path().join("side-policy.json"), policy.to_string()).unwrap();
            let parent = controller.call_authorized("create", json!({"projectId":"project-a","prompt":"父任务"}),
                &authorization).await.unwrap()["threadId"].as_str().unwrap().to_owned();
            std::fs::write(directory.path().join("remote-instance-preferences.json"),
                r#"{"instances":{"codex:local":{"persistentSideThreads":true}}}"#).unwrap();
            let failure = controller.call_authorized("createSideThread", json!({"threadId":parent}), &authorization).await.unwrap_err();
            if policy.get("userAgent").is_some() {
                assert!(failure.to_string().contains("升级"), "{failure}");
                assert!(!std::fs::read_to_string(directory.path().join("requests.jsonl")).unwrap().contains("\"thread/fork\""));
            } else {
                assert!(failure.downcast_ref::<ApiError>().unwrap().outcome_unknown);
                let related = controller.call_authorized("relatedThreads", json!({"threadId":parent}), &authorization).await.unwrap();
                let id = related["sideThreads"][0]["id"].as_str().unwrap();
                let detail = controller.call_authorized("thread", json!({"threadId":id}), &authorization).await.unwrap();
                assert_eq!(detail["thread"]["readOnly"], true);
                assert!(controller.call_authorized("send", json!({"threadId":id,"prompt":"不能发送","clientUserMessageId":"uninitialized"}),
                    &authorization).await.is_err());
            }
            controller.harness.enabled.store(false, Ordering::Release);
            controller.harness.release().await;
        }
    }

    #[tokio::test]
    async fn harness_service_side_create_send_queue_stop_close_stays_in_memory() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let main = controller
            .call_authorized(
                "create",
                json!({
                    "projectId":"project-a","prompt":"wait parent","model":"gpt-6.1-sol"
                }),
                &authorization,
            )
            .await
            .unwrap();
        let parent = main["threadId"].as_str().unwrap();
        let created = controller
            .call_authorized(
                "createSideThread",
                json!({"threadId":parent}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        assert_ne!(id, parent);
        assert_eq!(created["thread"]["viewKind"], "side");
        assert_eq!(created["thread"]["readOnly"], false);
        assert_eq!(created["thread"]["ephemeral"], true);
        let empty = controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(empty["data"], json!([]));
        let payload = json!({"threadId":id,"prompt":"wait 侧边专属内容","clientUserMessageId":"side-message-1"});
        let first = controller
            .call_authorized("send", payload.clone(), &authorization)
            .await
            .unwrap();
        let retry = controller
            .call_authorized("send", payload.clone(), &authorization)
            .await
            .unwrap();
        assert_eq!(first, retry);
        assert_eq!(first["deliveryState"], "accepted");
        assert!(controller.call_authorized("send",json!({
            "threadId":id,"prompt":"执行中改模型","model":"different-model","clientUserMessageId":"side-model-change"
        }),&authorization).await.is_err());
        let history = controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert!(history.to_string().contains("侧边专属内容"));
        assert!(!history.to_string().contains("wait parent"));
        assert_eq!(history["sideHistoryVersion"], 1);
        let queued = controller
            .call_authorized(
                "send",
                json!({
                    "threadId":id,"prompt":"追加要求","clientUserMessageId":"side-message-2"
                }),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(queued["deliveryState"], "queued");
        assert_eq!(queued["confirmed"], true);
        assert_eq!(queued["queuedMessages"].as_array().unwrap().len(), 1);
        let queued_retry = controller
            .call_authorized(
                "send",
                json!({"threadId":id,"prompt":"追加要求","clientUserMessageId":"side-message-2"}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(queued_retry, queued, "重复提交不能产生第二条队列消息");
        let detail = controller
            .call_authorized("thread", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(detail["thread"]["queuedMessages"], queued["queuedMessages"]);
        let history = controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert!(
            !history.to_string().contains("追加要求"),
            "入队确认不能提前插入正文"
        );
        for action in [
            "createSideThread",
            "deleteThread",
            "pinThread",
            "setGoal",
            "editMessage",
        ] {
            assert!(
                controller
                    .call_authorized(action, json!({"threadId":id,"pinned":true}), &authorization)
                    .await
                    .is_err(),
                "{action}"
            );
        }
        assert!(
            controller
                .call_authorized(
                    "closeSideThread",
                    json!({
                        "threadId":id,"parentThreadId":"wrong-parent"
                    }),
                    &authorization
                )
                .await
                .is_err()
        );
        assert!(
            controller
                .call_authorized(
                    "closeSideThread",
                    json!({
                        "threadId":parent,"parentThreadId":"wrong-parent"
                    }),
                    &authorization
                )
                .await
                .is_err()
        );
        controller
            .call_authorized("stop", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        let paused = controller
            .call_authorized("queue", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(paused["queuedMessages"][0]["pausedReason"], "消息已暂停");
        let list = controller
            .call_authorized("relatedThreads", json!({"threadId":parent}), &authorization)
            .await
            .unwrap();
        assert_eq!(list["sideThreads"][0]["id"], id);
        assert_eq!(list["sideThreads"][0]["title"], "wait 侧边专属内容");
        let closed = controller
            .call_authorized(
                "closeSideThread",
                json!({
                    "threadId":id,"parentThreadId":parent
                }),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(closed["closed"], true);
        assert_eq!(
            controller
                .call_authorized("history", json!({"threadId":id}), &authorization)
                .await
                .unwrap_err()
                .downcast_ref::<ApiError>()
                .unwrap()
                .code,
            410
        );
        assert!(
            controller
                .call_authorized("thread", json!({"threadId":parent}), &authorization)
                .await
                .is_ok()
        );
        let state =
            std::fs::read_to_string(directory.path().join("remote-harness-state.json")).unwrap();
        assert!(!state.contains(id));
        assert!(!state.contains("侧边专属内容"));
        let calls: Vec<Value> = std::fs::read_to_string(directory.path().join("requests.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call["method"] == "thread/fork")
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call["method"] == "turn/start" && call["params"]["threadId"] == id)
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call["method"] == "turn/steer" && call["params"]["threadId"] == id)
                .count(),
            0
        );
        assert!(!calls.iter().any(|call| call["params"]["threadId"] == id
            && matches!(
                call["method"].as_str(),
                Some("thread/resume" | "thread/turns/list" | "thread/items/list")
            )));
        controller.harness.release().await;
    }

    async fn harness_active_side_fixture() -> (tempfile::TempDir, HarnessAuthorization, String) {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let main = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"wait parent"}),
                &authorization,
            )
            .await
            .unwrap();
        let created = controller
            .call_authorized(
                "createSideThread",
                json!({"threadId":main["threadId"]}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap().to_owned();
        controller
            .call_authorized(
                "send",
                json!({"threadId":id,"prompt":"wait first","clientUserMessageId":"side-first"}),
                &authorization,
            )
            .await
            .unwrap();
        (directory, authorization, id)
    }

    async fn harness_wait_side_starts(directory: &std::path::Path, id: &str, expected: usize) {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if harness_engine_requests(directory)
                    .iter()
                    .filter(|frame| {
                        frame["method"] == "turn/start" && frame["params"]["threadId"] == id
                    })
                    .count()
                    >= expected
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("侧边队列没有按正常完成事件继续派发");
    }

    #[tokio::test]
    async fn harness_service_side_queue_waits_for_completion_and_dispatches_fifo_without_model_options()
     {
        let (directory, authorization, id) = harness_active_side_fixture().await;
        let controller = &authorization.controller;
        let cursor = controller
            .call_authorized("events", json!({}), &authorization)
            .await
            .unwrap();
        for index in 0..2 {
            let payload = json!({"threadId":id,"prompt":format!("wait queued {index}"),
                "clientUserMessageId":format!("queued-{index}"),"model":"gpt-6.1-sol","reasoningEffort":"high"});
            let queued = controller
                .call_authorized("send", payload.clone(), &authorization)
                .await
                .unwrap();
            assert_eq!(queued["deliveryState"], "queued");
            assert_eq!(
                queued["queuedMessages"].as_array().unwrap().len(),
                index + 1
            );
            assert!(queued["queuedMessages"][index]["submissionOptions"]["model"].is_null());
            assert!(
                queued["queuedMessages"][index]["submissionOptions"]["reasoningEffort"].is_null()
            );
            assert_eq!(
                controller
                    .call_authorized("send", payload, &authorization)
                    .await
                    .unwrap(),
                queued
            );
        }
        let events = controller
            .call_authorized(
                "events",
                json!({"epoch":cursor["epoch"],"cursor":cursor["cursor"]}),
                &authorization,
            )
            .await
            .unwrap();
        assert!(events["events"].as_array().unwrap().iter().any(|frame| {
            frame["method"] == "thread/queue/changed"
                && frame["params"]["threadId"] == id
                && frame["params"]["queuedMessages"]
                    .as_array()
                    .is_some_and(|queue| queue.len() == 2)
        }));
        let starts = || {
            harness_engine_requests(directory.path())
                .into_iter()
                .filter(|frame| {
                    frame["method"] == "turn/start" && frame["params"]["threadId"] == id
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(starts().len(), 1);
        for expected in 2..=3 {
            controller
                .harness
                .seed_test_history(json!({"threadId":id,"finish":true}))
                .await
                .unwrap();
            harness_wait_side_starts(directory.path(), &id, expected).await;
            let frames = starts();
            assert_eq!(frames.len(), expected);
            assert_eq!(
                frames[expected - 1]["params"]["clientUserMessageId"],
                format!("queued-{}", expected - 2)
            );
            assert!(frames[expected - 1]["params"].get("model").is_none());
            assert!(frames[expected - 1]["params"].get("effort").is_none());
            assert_eq!(
                frames[expected - 1]["params"]["collaborationMode"],
                json!({"mode":"default","settings":{"model":"gpt-6.1-sol",
                    "reasoning_effort":"high","developer_instructions":null}}),
                "排队续发也必须显式保留侧边模式和自身模型、深度"
            );
        }
        assert!(
            !harness_engine_requests(directory.path())
                .iter()
                .any(|frame| frame["method"] == "turn/steer" && frame["params"]["threadId"] == id)
        );
        assert!(
            !std::fs::read_to_string(directory.path().join("remote-harness-state.json"))
                .unwrap()
                .contains(&id)
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_queue_supports_edit_reorder_remove_and_manual_retry() {
        let (directory, authorization, id) = harness_active_side_fixture().await;
        let controller = &authorization.controller;
        let mut messages = Vec::new();
        for index in 0..3 {
            let queued = controller.call_authorized("send", json!({"threadId":id,
                "prompt":format!("wait queued {index}"),"clientUserMessageId":format!("queued-{index}")}),
                &authorization).await.unwrap();
            messages.push(queued["queuedMessages"][index].clone());
        }
        let ids: Vec<_> = messages
            .iter()
            .map(|message| message["id"].clone())
            .collect();
        controller
            .call_authorized(
                "reorderQueued",
                json!({"threadId":id,
            "messageIds":[ids[2],ids[1],ids[0]],"expectedMessageIds":ids}),
                &authorization,
            )
            .await
            .unwrap();
        let message = &messages[2];
        let editing = controller
            .call_authorized(
                "beginQueuedEdit",
                json!({"threadId":id,
            "editId":"side-edit","messageId":message["id"],"originalText":message["text"],
            "clientUserMessageId":message["clientUserMessageId"],"createdAt":message["createdAt"]}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(editing["editState"], "editing");
        let draft = controller
            .call_authorized(
                "queueEdit",
                json!({"threadId":id,"editId":"side-edit"}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(draft["draftText"], "wait queued 2");
        let edited = controller
            .call_authorized(
                "editQueued",
                json!({"threadId":id,"editId":"side-edit","prompt":"wait edited","images":[]}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(edited["editState"], "committed");
        assert_eq!(edited["queuedMessages"][0]["text"], "wait edited");
        controller
            .call_authorized(
                "removeQueued",
                json!({"threadId":id,"messageId":messages[1]["id"]}),
                &authorization,
            )
            .await
            .unwrap();
        controller
            .call_authorized("stop", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(850)).await;
        let queue = controller
            .call_authorized("queue", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(queue["queuedMessages"].as_array().unwrap().len(), 2);
        assert_eq!(queue["queuedMessages"][0]["pausedReason"], "消息已暂停");
        assert_eq!(queue["queuedMessages"][1]["pausedReason"], "消息已暂停");
        let starts = harness_engine_requests(directory.path())
            .into_iter()
            .filter(|frame| frame["method"] == "turn/start" && frame["params"]["threadId"] == id)
            .count();
        assert_eq!(starts, 1, "停止之后不能自动发送剩余队列");
        let retried = controller
            .call_authorized(
                "steerQueued",
                json!({"threadId":id,"messageId":messages[2]["id"]}),
                &authorization,
            )
            .await
            .unwrap();
        assert!(
            retried["turnId"].is_string(),
            "手动重试回执保留实际接收轮次"
        );
        harness_wait_side_starts(directory.path(), &id, 2).await;
        let queue = controller
            .call_authorized("queue", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(queue["queuedMessages"].as_array().unwrap().len(), 1);
        assert!(
            !queue["queuedMessages"][0]["pausedReason"].is_null(),
            "手动重试一条不能恢复其它暂停消息"
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_compaction_allows_queue_changes_and_resumes_fifo() {
        for active in [false, true] {
            let (directory, authorization, id) = harness_active_side_fixture().await;
            let controller = &authorization.controller;
            let history = controller
                .call_authorized("history", json!({"threadId":id}), &authorization)
                .await
                .unwrap();
            let turn = history["data"][0]["id"].as_str().unwrap();
            if !active {
                controller
                    .harness
                    .seed_test_history(json!({"threadId":id,"finish":true}))
                    .await
                    .unwrap();
            }
            controller
                .harness
                .seed_test_history(json!({"threadId":id,"events":[{
                "method":"item/started","params":{"threadId":id,"turnId":turn,
                    "item":{"id":"compact-side","type":"contextCompaction"}}}]}))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let view = controller
                        .call_authorized("thread", json!({"threadId":id}), &authorization)
                        .await
                        .unwrap();
                    if view["thread"]["compaction"]["state"] == "running" {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let mut messages = Vec::new();
            for index in 0..3 {
                let payload = json!({"threadId":id,"prompt":format!("wait compact queued {index}"),
                    "clientUserMessageId":format!("compact-queued-{index}"),"queueOnly":true});
                let queued = controller
                    .call_authorized("send", payload.clone(), &authorization)
                    .await
                    .unwrap();
                assert_eq!(queued["deliveryState"], "queued");
                assert_eq!(
                    queued["queuedMessages"].as_array().unwrap().len(),
                    index + 1
                );
                assert_eq!(
                    controller
                        .call_authorized("send", payload, &authorization)
                        .await
                        .unwrap(),
                    queued,
                    "压缩期间重复提交不能生成第二份队列消息"
                );
                messages.push(queued["queuedMessages"][index].clone());
            }
            let ids: Vec<_> = messages
                .iter()
                .map(|message| message["id"].clone())
                .collect();
            controller
                .call_authorized(
                    "reorderQueued",
                    json!({"threadId":id,
                "messageIds":[ids[2],ids[1],ids[0]],"expectedMessageIds":ids}),
                    &authorization,
                )
                .await
                .unwrap();
            let message = &messages[2];
            let editing = controller.call_authorized("beginQueuedEdit", json!({"threadId":id,
                "editId":"compact-edit","messageId":message["id"],"originalText":message["text"],
                "clientUserMessageId":message["clientUserMessageId"],"createdAt":message["createdAt"]}),
                &authorization).await.unwrap();
            assert_eq!(editing["editState"], "editing");
            let edited = controller
                .call_authorized(
                    "editQueued",
                    json!({"threadId":id,
                "editId":"compact-edit","prompt":"wait edited during compaction"}),
                    &authorization,
                )
                .await
                .unwrap();
            assert_eq!(
                edited["queuedMessages"][0]["text"],
                "wait edited during compaction"
            );
            let removed = controller
                .call_authorized(
                    "removeQueued",
                    json!({"threadId":id,
                "messageId":messages[1]["id"]}),
                    &authorization,
                )
                .await
                .unwrap();
            assert_eq!(removed["queuedMessages"].as_array().unwrap().len(), 2);
            let failure = controller
                .call_authorized(
                    "steerQueued",
                    json!({"threadId":id,
                "messageId":messages[0]["id"]}),
                    &authorization,
                )
                .await
                .unwrap_err();
            assert!(failure.to_string().contains("压缩期间不能引导"));
            tokio::time::sleep(Duration::from_millis(350)).await;
            let starts = || {
                harness_engine_requests(directory.path())
                    .into_iter()
                    .filter(|frame| {
                        frame["method"] == "turn/start" && frame["params"]["threadId"] == id
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(starts().len(), 1, "压缩期间不能派发排队消息");
            controller.harness.seed_test_history(json!({"threadId":id,"events":[{
                "method":"item/completed","params":{"threadId":id,"turnId":turn,
                    "item":{"id":"compact-side","type":"contextCompaction","status":"completed"}}}]}))
                .await.unwrap();
            if active {
                controller
                    .harness
                    .seed_test_history(json!({"threadId":id,"finish":true}))
                    .await
                    .unwrap();
            }
            harness_wait_side_starts(directory.path(), &id, 2).await;
            assert_eq!(
                starts()[1]["params"]["clientUserMessageId"],
                "compact-queued-2"
            );
            assert_eq!(
                starts()[1]["params"]["input"][0]["text"],
                "wait edited during compaction"
            );
            controller
                .harness
                .seed_test_history(json!({"threadId":id,"finish":true}))
                .await
                .unwrap();
            harness_wait_side_starts(directory.path(), &id, 3).await;
            assert_eq!(
                starts()[2]["params"]["clientUserMessageId"],
                "compact-queued-0"
            );
            assert!(
                !harness_engine_requests(directory.path())
                    .iter()
                    .any(|frame| frame["method"] == "turn/steer"
                        && frame["params"]["threadId"] == id)
            );
            assert!(
                !std::fs::read_to_string(directory.path().join("remote-harness-state.json"))
                    .unwrap()
                    .contains(&id),
                "侧边压缩队列仍只保留在内存"
            );
            controller.harness.release().await;
        }
    }

    #[tokio::test]
    async fn harness_service_side_queue_unknown_delivery_pauses_followers_without_replay() {
        let (directory, authorization, id) = harness_active_side_fixture().await;
        let controller = &authorization.controller;
        for (client, prompt) in [
            ("uncertain-side-queued", "uncertain queued"),
            ("follower", "wait follower"),
        ] {
            controller
                .call_authorized(
                    "send",
                    json!({"threadId":id,
                "prompt":prompt,"clientUserMessageId":client}),
                    &authorization,
                )
                .await
                .unwrap();
        }
        controller
            .harness
            .seed_test_history(json!({"threadId":id,"finish":true}))
            .await
            .unwrap();
        harness_wait_side_starts(directory.path(), &id, 2).await;
        tokio::time::sleep(Duration::from_millis(850)).await;
        let queue = controller
            .call_authorized("queue", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(
            queue["queuedMessages"][0]["submission"]["status"],
            "outcome-unknown"
        );
        assert!(!queue["queuedMessages"][1]["pausedReason"].is_null());
        let calls = harness_engine_requests(directory.path());
        assert_eq!(
            calls
                .iter()
                .filter(|frame| frame["method"] == "turn/start"
                    && frame["params"]["clientUserMessageId"] == "uncertain-side-queued")
                .count(),
            1
        );
        assert!(!calls.iter().any(|frame| frame["method"] == "turn/start"
            && frame["params"]["clientUserMessageId"] == "follower"));
        let state = controller
            .call_authorized(
                "sendState",
                json!({"threadId":id,
            "clientUserMessageId":"uncertain-side-queued"}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(
            state["deliveryState"], "accepted",
            "只读原生证据可以确认已接收的同一消息"
        );
        let queue = controller
            .call_authorized("queue", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(queue["queuedMessages"].as_array().unwrap().len(), 1);
        assert!(!queue["queuedMessages"][0]["pausedReason"].is_null());
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_concurrent_queue_and_fast_operations_keep_sessions_independent() {
        let (_directory, authorization, first_id) = harness_active_side_fixture().await;
        let controller = &authorization.controller;
        let detail = controller
            .call_authorized("thread", json!({"threadId":first_id}), &authorization)
            .await
            .unwrap();
        let created = controller
            .call_authorized(
                "createSideThread",
                json!({"threadId":detail["thread"]["parentThreadId"]}),
                &authorization,
            )
            .await
            .unwrap();
        let second_id = created["threadId"].as_str().unwrap();
        controller
            .call_authorized(
                "send",
                json!({"threadId":second_id,"prompt":"wait second",
            "clientUserMessageId":"second-first"}),
                &authorization,
            )
            .await
            .unwrap();
        let fast_authorization =
            controller.authorization(authorization.device.clone(), Some("setFast"));
        let (first, fast, second) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(
                controller.call_authorized(
                    "send",
                    json!({"threadId":first_id,
                    "prompt":"wait queued one","clientUserMessageId":"concurrent-one"}),
                    &authorization
                ),
                controller.call_authorized(
                    "setFast",
                    json!({"threadId":second_id,
                    "model":"gpt-6.1-sol","enabled":true}),
                    &fast_authorization
                ),
                controller.call_authorized(
                    "send",
                    json!({"threadId":second_id,
                    "prompt":"wait queued two","clientUserMessageId":"concurrent-two"}),
                    &authorization
                ),
            )
        })
        .await
        .expect("不同侧边的入队与 Fast 操作不能交叉持锁等待");
        assert_eq!(first.unwrap()["deliveryState"], "queued");
        assert_eq!(fast.unwrap()["enabled"], true);
        assert_eq!(second.unwrap()["deliveryState"], "queued");
        for (id, client) in [
            (first_id.as_str(), "concurrent-one"),
            (second_id, "concurrent-two"),
        ] {
            let queue = controller
                .call_authorized("queue", json!({"threadId":id}), &authorization)
                .await
                .unwrap();
            assert_eq!(queue["queuedMessages"].as_array().unwrap().len(), 1);
            assert_eq!(queue["queuedMessages"][0]["clientUserMessageId"], client);
        }
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_async_answer_steers_waiting_turn_without_consuming_queue() {
        let (directory, authorization, id) = harness_active_side_fixture().await;
        let controller = &authorization.controller;
        controller
            .call_authorized(
                "send",
                json!({"threadId":id,"prompt":"wait queued",
            "clientUserMessageId":"queued"}),
                &authorization,
            )
            .await
            .unwrap();
        let history = controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        let turn = &history["data"][0]["id"];
        controller
            .harness
            .seed_test_history(json!({"threadId":id,"events":[
                {"method":"item/completed","params":{"threadId":id,"turnId":turn,
                    "item":{"id":"async-question","type":"agentMessage","text":"请选择",
                        "questions":[{"title":"采用哪一种？","options":["第一种","第二种"]}]}}}
            ]}))
            .await
            .unwrap();
        let request = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let detail = controller
                    .call_authorized("thread", json!({"threadId":id}), &authorization)
                    .await
                    .unwrap();
                if let Some(request) = detail["thread"]["requests"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|request| request["method"] == "remote/requestUserInputAsync")
                {
                    break request.clone();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let mut response = json!({"answers":{}});
        response["answers"][request["params"]["questions"][0]["id"].as_str().unwrap()] =
            json!({"answers":["第一种"]});
        controller
            .call_authorized(
                "answer",
                json!({"threadId":id,"method":request["method"],
            "requestId":request["id"],"response":response}),
                &authorization,
            )
            .await
            .unwrap();
        let calls = harness_engine_requests(directory.path());
        assert_eq!(calls.iter().filter(|frame| frame["method"] == "turn/steer"
            && frame["params"]["threadId"] == id).count(), 1, "询问答案需要立即补充到等待中的任务");
        let queue = controller
            .call_authorized("queue", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(queue["queuedMessages"].as_array().unwrap().len(), 1);
        assert_eq!(queue["queuedMessages"][0]["clientUserMessageId"], "queued");
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_queue_failed_turn_and_revoked_device_do_not_dispatch() {
        for revoke in [false, true] {
            let (directory, authorization, id) = harness_active_side_fixture().await;
            let controller = &authorization.controller;
            controller
                .call_authorized(
                    "send",
                    json!({"threadId":id,"prompt":"wait queued",
                "clientUserMessageId":"queued"}),
                    &authorization,
                )
                .await
                .unwrap();
            if revoke {
                controller.auth.lock().await.devices.clear();
            }
            controller
                .harness
                .seed_test_history(json!({"threadId":id,"finish":true,
                "status":if revoke {"completed"} else {"failed"}}))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(1600)).await;
            assert!(
                !harness_engine_requests(directory.path())
                    .iter()
                    .any(|frame| frame["method"] == "turn/start"
                        && frame["params"]["clientUserMessageId"] == "queued")
            );
            if !revoke {
                let queue = controller
                    .call_authorized("queue", json!({"threadId":id}), &authorization)
                    .await
                    .unwrap();
                assert!(!queue["queuedMessages"][0]["pausedReason"].is_null());
            }
            controller.harness.release().await;
        }
    }

    #[tokio::test]
    async fn harness_service_side_expiry_keeps_final_history_and_allows_read_ack_and_close() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let main = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"parent","model":"gpt-6.1-sol"}),
                &authorization,
            )
            .await
            .unwrap();
        let parent = main["threadId"].as_str().unwrap();
        let created = controller
            .call_authorized(
                "createSideThread",
                json!({"threadId":parent}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        controller
            .call_authorized(
                "send",
                json!({"threadId":id,"prompt":"wait side",
                "clientUserMessageId":"expiry-input"}),
                &authorization,
            )
            .await
            .unwrap();
        let pending = controller
            .call_authorized(
                "send",
                json!({"threadId":id,
            "prompt":"wait expired queued","clientUserMessageId":"expiry-queued"}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(pending["deliveryState"], "queued");
        let history = controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        let turn = history["data"][0]["id"].as_str().unwrap();
        controller.harness.seed_test_history(json!({"threadId":id,"events":[
            {"method":"item/completed","params":{"threadId":id,"turnId":turn,
                "item":{"id":"final-answer","type":"agentMessage","text":"过期前的最终回复"}}},
            {"method":"turn/completed","params":{"threadId":id,"turn":{"id":turn,"status":"completed","items":[]}}},
            {"method":"thread/unloaded","params":{"threadId":id}}
        ]})).await.unwrap();
        let mut detail = Value::Null;
        for _ in 0..100 {
            detail = controller
                .call_authorized("thread", json!({"threadId":id}), &authorization)
                .await
                .unwrap();
            if detail["thread"]["sideThreadExpired"] == true {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(detail["thread"]["sideThreadExpired"], true);
        assert_eq!(detail["thread"]["readOnly"], true);
        assert_eq!(detail["thread"]["priority"]["active"], false);
        assert_eq!(
            detail["thread"]["queuedMessages"],
            json!([]),
            "侧边过期时释放待发队列"
        );
        let retained = controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert!(retained.to_string().contains("过期前的最终回复"));
        assert!(retained.to_string().contains("wait side"));
        assert_eq!(detail["thread"]["hasUnreadTurn"], true);
        let list = controller
            .call_authorized("relatedThreads", json!({"threadId":parent}), &authorization)
            .await
            .unwrap();
        assert_eq!(list["sideThreads"][0]["id"], id);
        assert_eq!(list["sideThreads"][0]["hasUnreadTurn"], true);
        assert!(
            controller
                .call_authorized(
                    "send",
                    json!({"threadId":id,"prompt":"不能重启",
            "clientUserMessageId":"expired-send"}),
                    &authorization
                )
                .await
                .is_err()
        );
        controller
            .call_authorized("markRead", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        let list = controller
            .call_authorized("relatedThreads", json!({"threadId":parent}), &authorization)
            .await
            .unwrap();
        assert_eq!(list["sideThreads"][0]["hasUnreadTurn"], false);
        let sidebar = controller
            .call_authorized("sidebar", json!({}), &authorization)
            .await
            .unwrap();
        assert_eq!(sidebar["unreadSideThreads"], json!([]));
        // 引擎已退出时仍须读回同一份最终正文，已读与主动关闭都不能等待或重启引擎。
        controller.harness.release().await;
        let offline_history = controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(offline_history, retained);
        let offline_detail = controller
            .call_authorized("thread", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(offline_detail["thread"]["sideThreadExpired"], true);
        controller
            .call_authorized("markRead", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        let closed = controller
            .call_authorized(
                "closeSideThread",
                json!({"threadId":id,"parentThreadId":parent}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(closed["closed"], true);
        assert!(
            controller
                .call_authorized("history", json!({"threadId":id}), &authorization)
                .await
                .is_err()
        );
        let calls = std::fs::read_to_string(directory.path().join("requests.jsonl")).unwrap();
        let calls: Vec<Value> = calls
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call["method"] == "turn/start" && call["params"]["threadId"] == id)
                .count(),
            1
        );
        assert!(!calls.iter().any(|call| call["params"]["threadId"] == id
            && matches!(
                call["method"].as_str(),
                Some("thread/resume" | "thread/unsubscribe")
            )));
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_service_side_unknown_send_is_not_replayed_and_approval_is_bound() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let main = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"wait parent"}),
                &authorization,
            )
            .await
            .unwrap();
        let created = controller
            .call_authorized(
                "createSideThread",
                json!({"threadId":main["threadId"]}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        let payload =
            json!({"threadId":id,"prompt":"uncertain side","clientUserMessageId":"unknown-side"});
        assert!(
            controller
                .call_authorized("send", payload.clone(), &authorization)
                .await
                .is_err()
        );
        let retry = controller
            .call_authorized("send", payload, &authorization)
            .await
            .unwrap();
        assert_eq!(retry["outcomeUnknown"], true);
        tokio::time::sleep(Duration::from_millis(100)).await;
        controller
            .call_authorized(
                "send",
                json!({
                    "threadId":id,"prompt":"approval side","clientUserMessageId":"approval-side"
                }),
                &authorization,
            )
            .await
            .unwrap();
        let detail = controller
            .call_authorized("thread", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        let request = &detail["thread"]["requests"][0];
        assert!(request["id"].is_number());
        controller.call_authorized("approve",json!({
            "threadId":id,"method":request["method"],"requestId":request["id"],"decision":"accept"
        }),&authorization).await.unwrap();
        let log = std::fs::read_to_string(directory.path().join("requests.jsonl")).unwrap();
        assert_eq!(
            log.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|call| call["method"] == "turn/start"
                    && call["params"]["clientUserMessageId"] == "unknown-side")
                .count(),
            1
        );
        controller.harness.release().await;
    }

    #[test]
    #[ignore = "显式提供隔离原生引擎后验收服务侧边会话"]
    fn harness_service_side_real_app_server_lifecycle() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(check_service_side_real_app_server_lifecycle())
        }));
        // 断言、原生进程及模拟上游关闭均在验收主体完成；管道的阻塞读取不能让测试运行时无限等待。
        runtime.shutdown_timeout(Duration::from_secs(3));
        if let Err(failure) = result {
            std::panic::resume_unwind(failure);
        }
    }

    async fn check_service_side_real_app_server_lifecycle() {
        let executable = std::env::var_os("CODEX_ELVES_SERVICE_CHECK_EXE").unwrap();
        // 原生验收直接安装指定引擎，不先创建随后被覆盖且未消费输出的模拟子进程。
        let (directory, authorization) = harness_authorization_fixture(None).await;
        let controller = &authorization.controller;
        controller.harness.release().await;
        eprintln!("原生侧边验收：准备隔离配置");
        let home = directory.path().join("side-codex-home");
        std::fs::create_dir_all(&home).unwrap();
        struct SettingsGuard(Option<PathBuf>);
        impl Drop for SettingsGuard {
            fn drop(&mut self) {
                crate::paths::set_settings_path_for_tests(self.0.take());
            }
        }
        let settings_path = directory.path().join("settings.json");
        let settings = crate::settings::BackendSettings {
            codex_home_path: home.to_string_lossy().into_owned(),
            ..Default::default()
        };
        std::fs::write(&settings_path, serde_json::to_vec(&settings).unwrap()).unwrap();
        let _settings = SettingsGuard(crate::paths::set_settings_path_for_tests(Some(
            settings_path,
        )));
        let requests = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
        let delay = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let router = Router::new().route("/v1/responses", post(
            |State((requests, delay)):State<(Arc<tokio::sync::Mutex<Vec<Value>>>,Arc<std::sync::atomic::AtomicU64>)>,
             Json(request):Json<Value>| async move {
                let mut rows=requests.lock().await; rows.push(request);
                let id=format!("side-native-{}",rows.len()); drop(rows);
                tokio::time::sleep(Duration::from_millis(delay.load(Ordering::SeqCst))).await;
                let item=json!({"id":id,"type":"message","role":"assistant","phase":"final_answer",
                    "content":[{"type":"output_text","text":"SIDE_NATIVE_REPLY","annotations":[]}]});
                let frames=[
                    json!({"type":"response.created","response":{"id":id,"status":"in_progress","output":[]}}),
                    json!({"type":"response.output_item.added","output_index":0,"item":item}),
                    json!({"type":"response.output_text.delta","output_index":0,"item_id":id,"content_index":0,"delta":"SIDE_NATIVE_REPLY"}),
                    json!({"type":"response.output_item.done","output_index":0,"item":item}),
                    json!({"type":"response.completed","response":{"id":id,"status":"completed","output":[item],
                        "usage":{"input_tokens":10,"output_tokens":10,"total_tokens":20}}}),
                ];
                ([("content-type","text/event-stream")],frames.iter().map(|frame|
                    format!("event: {}\ndata: {frame}\n\n",frame["type"].as_str().unwrap())).collect::<String>())
            }
        )).with_state((requests.clone(),delay.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        std::fs::write(home.join("config.toml"),format!(
            "model=\"gpt-6-astra\"\nmodel_provider=\"side-test\"\napproval_policy=\"never\"\nsandbox_mode=\"read-only\"\n[model_providers.side-test]\nname=\"Isolated side test\"\nbase_url=\"http://127.0.0.1:{port}/v1\"\nwire_api=\"responses\"\nrequires_openai_auth=false\nsupports_websockets=false\n"
        )).unwrap();
        eprintln!("原生侧边验收：启动独立 app-server");
        let mut command = tokio::process::Command::new(executable);
        command
            .args(["app-server"])
            .env("CODEX_HOME", &home)
            .current_dir(directory.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(crate::windows_create_hidden_process());
        controller
            .harness
            .install_test_process(
                crate::codex_service::RunningService::for_test(command.spawn().unwrap()).unwrap(),
            )
            .await;
        controller.harness.prepare().await.unwrap();
        eprintln!("原生侧边验收：开始主会话和侧边轮次");
        async fn completed(auth: &HarnessAuthorization, id: &str) -> Value {
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    if let Ok(history) = auth
                        .controller
                        .call_authorized("history", json!({"threadId":id}), auth)
                        .await
                    {
                        if history["data"][0]["status"] == "completed" {
                            return history;
                        }
                        assert_ne!(history["data"][0]["status"], "failed", "{history}");
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("隔离会话未完成")
        }
        let main = controller
            .call_authorized(
                "create",
                json!({"prompt":"PARENT_NATIVE_CONTEXT","model":"gpt-6-astra"}),
                &authorization,
            )
            .await
            .unwrap();
        let parent = main["threadId"].as_str().unwrap();
        completed(&authorization, parent).await;
        controller
            .harness
            .start_test_turn(
                json!({"threadId":parent,"input":[{"type":"text","text":"PARENT_PLAN_MODE"}],
                    "collaborationMode":{"mode":"plan","settings":{"model":"gpt-6-astra",
                        "reasoning_effort":"low","developer_instructions":null}}}),
                &authorization,
            )
            .await
            .unwrap();
        completed(&authorization, parent).await;
        let created = controller
            .call_authorized(
                "createSideThread",
                json!({"threadId":parent}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        controller.call_authorized("send",json!({"threadId":id,"prompt":"SIDE_NATIVE_INPUT","clientUserMessageId":"native-side-send"}),&authorization).await.unwrap();
        let history = completed(&authorization, id).await;
        assert!(history.to_string().contains("SIDE_NATIVE_INPUT"));
        assert!(history.to_string().contains("SIDE_NATIVE_REPLY"));
        assert!(!history.to_string().contains("PARENT_NATIVE_CONTEXT"));
        let side_usage = controller
            .call_authorized("sessionMetrics", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(side_usage["metrics"]["source"], "codex-side");
        assert_eq!(side_usage["metrics"]["steps"], 1);
        assert_eq!(side_usage["metrics"]["turnTokens"], 20);
        assert_eq!(side_usage["metrics"]["totalTokens"], 20);
        assert!(requests.lock().await.iter().any(|request| {
            let input = request["input"].to_string();
            input.contains("SIDE_NATIVE_INPUT") && input.contains("PARENT_NATIVE_CONTEXT")
        }));
        fn effective_tools(request: &Value) -> String {
            if request["tools"].is_array() {
                return request["tools"].to_string();
            }
            request["input"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find(|item| {
                    item["type"] == "additional_tools"
                        && item["tools"].as_array().is_some_and(|tools| {
                            tools.iter().any(|tool| tool["name"] == "functions")
                        })
                })
                .unwrap()["tools"]
                .to_string()
        }
        fn latest_mode(request: &Value) -> String {
            request["input"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .filter(|item| item["role"] == "developer")
                .find_map(|item| {
                    item["content"].as_array()?.iter().find_map(|content| {
                        content["text"]
                            .as_str()
                            .filter(|text| text.starts_with("<collaboration_mode>"))
                            .map(str::to_owned)
                    })
                })
                .unwrap()
        }
        {
            let captured = requests.lock().await;
            let side_request = captured
                .iter()
                .find(|request| request["input"].to_string().contains("SIDE_NATIVE_INPUT"))
                .unwrap();
            assert!(latest_mode(side_request).contains("# Plan Mode"));
            let tools = effective_tools(side_request);
            for name in ["get_goal", "create_goal", "update_goal"] {
                assert!(!tools.contains(name), "临时侧边仍注册了 {name}");
                assert!(
                    effective_tools(&captured[0]).contains(name),
                    "父会话原生目标工具被侧边配置影响"
                );
            }
        }
        eprintln!("原生侧边验收：Plan 继承及目标工具隔离通过");
        controller
            .call_authorized("compact", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        controller
            .call_authorized("history", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        delay.store(5000, Ordering::SeqCst);
        controller.call_authorized("send",json!({"threadId":id,"prompt":"CLOSE_RUNNING_SIDE","clientUserMessageId":"native-side-stop"}),&authorization).await.unwrap();
        let closed = controller
            .call_authorized(
                "closeSideThread",
                json!({"threadId":id,"parentThreadId":parent}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(closed["closed"], true);
        assert_eq!(
            controller
                .call_authorized("thread", json!({"threadId":id}), &authorization)
                .await
                .unwrap_err()
                .downcast_ref::<ApiError>()
                .unwrap()
                .code,
            410
        );
        assert!(
            controller
                .call_authorized("thread", json!({"threadId":parent}), &authorization)
                .await
                .is_ok()
        );
        delay.store(0, Ordering::SeqCst);
        std::fs::write(directory.path().join("remote-instance-preferences.json"),
            r#"{"instances":{"codex:local":{"persistentSideThreads":true}}}"#).unwrap();
        if controller.harness.require_persistent_sides().await.is_err() {
            let failure = controller.call_authorized("createSideThread",
                json!({"threadId":parent}), &authorization).await.unwrap_err();
            assert!(failure.to_string().contains("升级"), "{failure}");
            controller.harness.enabled.store(false, Ordering::Release);
            controller.harness.release().await;
            server.abort();
            let _ = server.await;
            eprintln!("原生侧边验收：旧引擎临时侧边通过，持久侧边在创建前提示升级");
            return;
        }
        let persistent = controller.call_authorized("createSideThread",
            json!({"threadId":parent}), &authorization).await.unwrap();
        let persistent_id = persistent["threadId"].as_str().unwrap();
        assert_eq!(persistent["thread"]["ephemeral"], false);
        controller.call_authorized("send", json!({"threadId":persistent_id,"prompt":"PERSISTENT_NATIVE_INPUT",
            "clientUserMessageId":"persistent-native-first"}), &authorization).await.unwrap();
        let persistent_history = completed(&authorization, persistent_id).await;
        assert!(persistent_history.to_string().contains("PERSISTENT_NATIVE_INPUT"));
        assert!(!persistent_history.to_string().contains("PARENT_NATIVE_CONTEXT"));
        let unsent = controller.call_authorized("createSideThread",
            json!({"threadId":parent}), &authorization).await.unwrap();
        let unsent_id = unsent["threadId"].as_str().unwrap();
        let closed = controller.call_authorized("closeSideThread",
            json!({"threadId":persistent_id,"parentThreadId":parent}), &authorization).await.unwrap();
        assert_eq!(closed["historyRetained"], true);
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
        eprintln!("原生侧边验收：重启独立服务，恢复已发送和未发送的持久侧边");
        let recovered = HarnessController::new(directory.path().to_owned()).unwrap();
        recovered.publish_access_policy(&RemoteAccessSettings { enabled:true, ..Default::default() });
        recovered.running.store(true, Ordering::SeqCst);
        recovered.auth.lock().await.devices.push(authorization.device.clone());
        let recovered_auth = recovered.authorization(authorization.device.clone(), Some("persistent-restart"));
        recovered.harness.install_test_process(
            crate::codex_service::RunningService::for_test(command.spawn().unwrap()).unwrap()).await;
        let related = recovered.call_authorized("relatedThreads", json!({"threadId":parent}), &recovered_auth).await.unwrap();
        assert!(related["sideThreads"].as_array().unwrap().iter().any(|row| row["id"] == persistent_id));
        assert!(related["sideThreads"].as_array().unwrap().iter().any(|row| row["id"] == unsent_id));
        let history = recovered.call_authorized("history", json!({"threadId":persistent_id}), &recovered_auth).await.unwrap();
        assert!(history.to_string().contains("PERSISTENT_NATIVE_INPUT"));
        assert!(!history.to_string().contains("PARENT_NATIVE_CONTEXT"));
        assert_eq!(recovered.call_authorized("history", json!({"threadId":unsent_id}), &recovered_auth).await.unwrap()["data"], json!([]));
        recovered.call_authorized("send", json!({"threadId":unsent_id,"prompt":"UNSENT_AFTER_RESTART",
            "clientUserMessageId":"unsent-after-restart"}), &recovered_auth).await.unwrap();
        completed(&recovered_auth, unsent_id).await;
        {
            let captured = requests.lock().await;
            for marker in ["PERSISTENT_NATIVE_INPUT", "UNSENT_AFTER_RESTART"] {
                let request = captured.iter().find(|request| request["input"].to_string().contains(marker)).unwrap();
                let input = request["input"].to_string();
                assert!(input.contains("PARENT_NATIVE_CONTEXT"), "持久侧边丢失父参考上下文");
                assert!(input.contains("Side conversation boundary."), "重启后丢失侧边任务边界");
                assert!(latest_mode(request).contains("# Plan Mode"));
                for name in ["get_goal","create_goal","update_goal"] {
                    assert!(effective_tools(request).contains(name), "持久侧边未注册 {name}");
                }
            }
        }
        let metrics = recovered.call_authorized("sessionMetrics", json!({"threadId":persistent_id}), &recovered_auth).await.unwrap();
        assert_eq!(metrics["metrics"]["source"], "codex-side");
        assert_eq!(metrics["metrics"]["totalTokens"], 20, "持久侧边统计不能累加父会话用量");
        // 重启后只读不会加载线程；先明确执行一轮，确保下面验收的确是“加载后闲置退订”，而非原本就 notLoaded。
        recovered.call_authorized("send", json!({"threadId":persistent_id,"prompt":"PERSISTENT_BEFORE_IDLE",
            "clientUserMessageId":"persistent-before-idle"}), &recovered_auth).await.unwrap();
        completed(&recovered_auth, persistent_id).await;
        tokio::time::sleep(Duration::from_millis(80)).await;
        let before_idle = recovered.call_authorized("thread", json!({"threadId":persistent_id}), &recovered_auth).await.unwrap();
        assert_eq!(before_idle["thread"]["status"]["type"], "idle");
        recovered.harness.age_side_for_test(persistent_id, 10801).await;
        recovered.harness.release_idle_sides_for_test().await;
        let unloaded = recovered.call_authorized("thread", json!({"threadId":persistent_id}), &recovered_auth).await.unwrap();
        assert_eq!(unloaded["thread"]["status"]["type"], "notLoaded");
        assert_ne!(unloaded["thread"]["sideThreadExpired"], true);
        recovered.call_authorized("send", json!({"threadId":persistent_id,"prompt":"PERSISTENT_AFTER_IDLE",
            "clientUserMessageId":"persistent-after-idle"}), &recovered_auth).await.unwrap();
        let resumed = completed(&recovered_auth, persistent_id).await;
        assert!(resumed.to_string().contains("PERSISTENT_AFTER_IDLE"));
        assert!(resumed.to_string().contains("PERSISTENT_NATIVE_INPUT"));
        recovered.call_authorized("deleteThread", json!({"threadId":parent}), &recovered_auth).await.unwrap();
        recovered.harness.enabled.store(false, Ordering::Release);
        recovered.harness.release().await;
        server.abort();
        let _ = server.await;
        eprintln!("原生侧边验收：临时及持久侧边、重启恢复和删除均已通过");
    }

    async fn native_pin_fixture(options: Value) -> (tempfile::TempDir, HarnessAuthorization) {
        let (directory, authorization) =
            harness_fixture_with_store(Some(json!({"threadPins":["custom"]}))).await;
        // 先保留旧服务自己的置顶，再提供与它不同的原生分区，验证升级不能复活旧记录。
        let rows: Vec<_> = ["old-a", "old-b", "custom"]
            .into_iter()
            .map(|id| {
                json!({
                    "id":id,"cwd":directory.path(),"name":id,"ephemeral":false,
                    "threadSource":"user","source":"appServer","projectId":"project-a",
                    "status":{"type":"idle"},"turns":[],"createdAt":1,"updatedAt":1
                })
            })
            .collect();
        let mut config = json!({"rows":rows,"ids":["old-b","old-a"],"pageSize":1,
            "recentIds":["custom"],"sections":{"custom":"custom-section"}});
        config
            .as_object_mut()
            .unwrap()
            .extend(options.as_object().unwrap().clone());
        std::fs::write(
            directory.path().join("native-pins.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        (directory, authorization)
    }

    #[tokio::test]
    async fn sidebar_requires_explicit_claude_opt_in_before_wsl_detection() {
        let (directory, authorization) = native_pin_fixture(json!({})).await;
        // 故意放入无效配置；未启用时不得尝试检测，显式启用仅报告配置错误，不连接真实 WSL。
        std::fs::write(directory.path().join("remote-claude-wsl.json"), b"{invalid").unwrap();
        for payload in [
            json!({}),
            json!({"includeClaude":false}),
            json!({"includeClaude":"true"}),
        ] {
            let sidebar = authorization
                .controller
                .call_authorized("sidebar", payload, &authorization)
                .await
                .unwrap();
            assert!(sidebar["claudeDiscoveryError"].is_null(), "{sidebar}");
            assert_eq!(sidebar["recent"].as_array().unwrap().len(), 3);
        }
        let sidebar = authorization
            .controller
            .call_authorized("sidebar", json!({"includeClaude":true}), &authorization)
            .await
            .unwrap();
        assert!(sidebar["claudeDiscoveryError"].is_string());
        authorization.controller.harness.release().await;
    }

    #[tokio::test]
    async fn session_instances_route_rejects_cross_instance_writes_and_filters_disabled_rows() {
        let (_directory, authorization) = native_pin_fixture(json!({})).await;
        let controller = &authorization.controller;
        validate_goal_composer_payload(
            "create",
            &json!({"projectId":"project-a","prompt":"goal",
            "goalMode":true,"backend":"codex","instanceId":"codex:local"}),
        )
        .unwrap();
        for payload in [
            json!({"threadId":"old-a","instanceId":"codex:wsl:other","prompt":"must not dispatch"}),
            json!({"threadId":"old-a","instanceId":"claude:local","prompt":"must not dispatch"}),
            json!({"instanceId":"codex:wsl:other","prompt":"must not dispatch"}),
            json!({"instanceId":"future:local","prompt":"must not dispatch"}),
        ] {
            assert!(
                controller
                    .call_authorized("send", payload, &authorization)
                    .await
                    .is_err()
            );
        }
        let none = controller
            .call_authorized("sidebar", json!({"enabledInstanceIds":[]}), &authorization)
            .await
            .unwrap();
        assert!(none["recent"].as_array().unwrap().is_empty());
        assert!(none["pinnedThreads"].as_array().unwrap().is_empty());
        assert!(none["claudeDiscoveryError"].is_null());
        let local = controller
            .call_authorized(
                "sidebar",
                json!({"enabledInstanceIds":["codex:local","claude:local"]}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(local["recent"].as_array().unwrap().len(), 3);
        assert!(local["claudeDiscoveryError"].is_null());
        assert!(
            local["recent"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["instanceId"] == "codex:local")
        );
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_native_pins_restore_all_pages_and_share_desktop_mutations() {
        let (directory, authorization) = native_pin_fixture(json!({})).await;
        let controller = &authorization.controller;
        let sidebar = controller
            .call_authorized("sidebar", json!({}), &authorization)
            .await
            .unwrap();
        let ids = |sidebar: &Value| {
            sidebar["pinnedThreads"]
                .as_array()
                .unwrap()
                .iter()
                .map(|thread| thread["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&sidebar),
            ["old-b", "old-a"],
            "保留原生顺序并补齐最近目录之外的全部置顶页"
        );
        assert_eq!(sidebar["recent"].as_array().unwrap().len(), 3);
        assert_eq!(
            sidebar["recent"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["id"] == "custom")
                .unwrap()["pinned"],
            false
        );
        let initial_moves = || {
            std::fs::read_to_string(directory.path().join("requests.jsonl"))
                .unwrap()
                .lines()
                .filter(|line| {
                    serde_json::from_str::<Value>(line).unwrap()["method"] == "thread/section/move"
                })
                .count()
        };
        controller
            .call_authorized(
                "pinThread",
                json!({"threadId":"old-a","pinned":true}),
                &authorization,
            )
            .await
            .unwrap();
        controller
            .call_authorized(
                "pinThread",
                json!({"threadId":"custom","pinned":false}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(
            initial_moves(),
            0,
            "重复置顶不重排；取消未置顶项不清除自定义分区"
        );
        controller
            .call_authorized(
                "pinThread",
                json!({"threadId":"old-b","pinned":false}),
                &authorization,
            )
            .await
            .unwrap();
        let sidebar = controller
            .call_authorized("sidebar", json!({}), &authorization)
            .await
            .unwrap();
        assert_eq!(ids(&sidebar), ["old-a"], "取消后不能返回写入前的置顶缓存");
        controller
            .call_authorized(
                "pinThread",
                json!({"threadId":"old-b","pinned":true}),
                &authorization,
            )
            .await
            .unwrap();
        let sidebar = controller
            .call_authorized("sidebar", json!({}), &authorization)
            .await
            .unwrap();
        assert_eq!(ids(&sidebar), ["old-a", "old-b"]);
        let canonical: Value =
            read_json_or_default(&directory.path().join("native-pins.json")).unwrap();
        assert_eq!(canonical["ids"], json!(["old-a", "old-b"]));
        assert_eq!(canonical["sections"]["custom"], "custom-section");
        assert_eq!(initial_moves(), 2);
        let requests = std::fs::read_to_string(directory.path().join("requests.jsonl")).unwrap();
        assert!(!requests.contains("\"thread/resume\"") && !requests.contains("\"turn/start\""));
        controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_native_pins_never_hide_lookup_errors_or_invalid_pages() {
        for options in [
            json!({"sectionError":-32000}),
            json!({"sectionError":-32602}),
            json!({"missingSection":true}),
            json!({"cursorLoop":true}),
            json!({"wrongSection":true}),
            json!({"ids":["old-a","old-a"]}),
        ] {
            let (_directory, authorization) = native_pin_fixture(options.clone()).await;
            assert!(
                authorization
                    .controller
                    .call_authorized("sidebar", json!({}), &authorization)
                    .await
                    .is_err(),
                "原生异常不能退回私有置顶或伪造空列表：{options}"
            );
            authorization.controller.harness.release().await;
        }
    }

    #[tokio::test]
    async fn harness_native_pin_confirmation_failure_preserves_unknown_outcome() {
        let (directory, authorization) = native_pin_fixture(json!({"confirmFailure":true})).await;
        let error = authorization
            .controller
            .call_authorized(
                "pinThread",
                json!({"threadId":"old-b","pinned":false}),
                &authorization,
            )
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<ApiError>().unwrap().outcome_unknown);
        let canonical: Value =
            read_json_or_default(&directory.path().join("native-pins.json")).unwrap();
        assert_eq!(
            canonical["ids"],
            json!(["old-a"]),
            "失败发生在写入后的确认，不能重放操作"
        );
        authorization.controller.harness.release().await;
    }

    #[tokio::test]
    async fn harness_goals_controls_and_workspace_preview_work() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let created = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"wait goal",
            "goalMode":true,"model":"gpt-6.1-sol","reasoningEffort":"high"}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        assert_eq!(created["goalSubmission"]["goalSet"], "succeeded");
        for (operation, before, after) in [
            ("pause", "active", "paused"),
            ("resume", "paused", "active"),
        ] {
            let response=controller.call_authorized("updateGoal",json!({"threadId":id,"objective":"wait goal",
                "expectedCreatedAt":1,"expectedObjective":"wait goal","expectedStatus":before,"operation":operation}),&authorization).await.unwrap();
            assert_eq!(response["goal"]["status"], after);
        }
        assert_eq!(
            controller
                .call_authorized("stop", json!({"threadId":id}), &authorization)
                .await
                .unwrap()["ok"],
            true
        );
        assert_eq!(
            controller
                .call_authorized("compact", json!({"threadId":id}), &authorization)
                .await
                .unwrap()["ok"],
            true
        );
        controller
            .call_authorized(
                "rename",
                json!({"threadId":id,"title":"服务模式重命名"}),
                &authorization,
            )
            .await
            .unwrap();
        controller
            .call_authorized(
                "pinThread",
                json!({"threadId":id,"pinned":true}),
                &authorization,
            )
            .await
            .unwrap();
        controller
            .call_authorized(
                "pinProject",
                json!({"projectId":"project-a","pinned":true}),
                &authorization,
            )
            .await
            .unwrap();
        let sidebar = controller
            .call_authorized("sidebar", json!({}), &authorization)
            .await
            .unwrap();
        assert_eq!(sidebar["pinnedThreads"][0]["title"], "服务模式重命名");
        assert_eq!(sidebar["projects"][0]["pinned"], true);
        let models = controller
            .call_authorized("models", json!({}), &authorization)
            .await
            .unwrap();
        assert_eq!(models["defaultModel"], "gpt-6.1-sol");
        assert_eq!(
            controller
                .call_authorized("skills", json!({"threadId":id}), &authorization)
                .await
                .unwrap()["data"],
            json!([])
        );
        std::fs::write(directory.path().join("preview.txt"), "远程预览正文").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("codex_elves_device=isolated-service-dispatch-token"),
        );
        let state = WebState {
            desktop: false,
            controller: controller.clone(),
            settings: RemoteAccessSettings::default(),
        };
        let response = workspace::file(
            State(state),
            headers,
            Json(json!({"threadId":id,"path":"preview.txt"})),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("远程预览正文"));
        let deleted=controller.call_authorized("updateGoal",json!({"threadId":id,"objective":"wait goal",
            "expectedCreatedAt":1,"expectedObjective":"wait goal","expectedStatus":"active","operation":"delete"}),&authorization).await.unwrap();
        assert!(deleted["goal"].is_null());
    }

    #[tokio::test]
    async fn harness_idle_send_bypasses_old_queue_and_deduplicates_success() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let created = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"wait"}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        let old = controller
            .call_authorized(
                "send",
                json!({"threadId":id,"prompt":"old waiting","clientUserMessageId":"old-waiting"}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(old["deliveryState"], "queued", "明确执行中才入队");
        controller
            .call_authorized("stop", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        let input = json!({"threadId":id,"prompt":"direct now","clientUserMessageId":"new-direct"});
        let sent = controller
            .call_authorized("send", input.clone(), &authorization)
            .await
            .unwrap();
        assert_eq!(
            sent["deliveryState"], "accepted",
            "旧队列不能让空闲普通消息继续入队"
        );
        let queue = controller
            .call_authorized("queue", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert_eq!(queue["queuedMessages"].as_array().unwrap().len(), 1);
        assert_eq!(
            queue["queuedMessages"][0]["clientUserMessageId"],
            "old-waiting"
        );
        assert_eq!(
            controller
                .call_authorized("send", input, &authorization)
                .await
                .unwrap(),
            sent
        );
        let frames = std::fs::read_to_string(directory.path().join("requests.jsonl")).unwrap();
        assert_eq!(
            frames
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .filter(|request| request["method"] == "turn/start"
                    && request["params"]["clientUserMessageId"] == "new-direct")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn harness_idle_direct_unknown_is_hidden_and_recovers_without_replay() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let created = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"initial"}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let input = json!({"threadId":id,"prompt":"uncertain direct","clientUserMessageId":"direct-unknown"});
        assert!(
            controller
                .call_authorized("send", input.clone(), &authorization)
                .await
                .is_err()
        );
        let queue = controller
            .call_authorized("queue", json!({"threadId":id}), &authorization)
            .await
            .unwrap();
        assert!(
            queue["queuedMessages"].as_array().unwrap().is_empty(),
            "直接发送的未知意图不能展示为等待队列"
        );
        let state = controller
            .call_authorized("sendState", input.clone(), &authorization)
            .await
            .unwrap();
        assert_eq!(
            state["deliveryState"], "accepted",
            "只能通过相同原生消息身份核对成功"
        );
        assert_eq!(
            controller
                .call_authorized("send", input, &authorization)
                .await
                .unwrap()["deliveryState"],
            "accepted"
        );
        let frames = std::fs::read_to_string(directory.path().join("requests.jsonl")).unwrap();
        assert_eq!(
            frames
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .filter(|request| request["method"] == "turn/start"
                    && request["params"]["clientUserMessageId"] == "direct-unknown")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn harness_uncertain_send_recovers_from_history_without_duplicate_turn() {
        let (directory, authorization) = harness_fixture().await;
        let controller = &authorization.controller;
        let created = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"initial"}),
                &authorization,
            )
            .await
            .unwrap();
        let id = created["threadId"].as_str().unwrap();
        let payload =
            json!({"threadId":id,"prompt":"uncertain","clientUserMessageId":"uncertain-a"});
        controller
            .call_authorized("send", payload.clone(), &authorization)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let state = controller
                    .call_authorized(
                        "sendState",
                        json!({"threadId":id,"clientUserMessageId":"uncertain-a"}),
                        &authorization,
                    )
                    .await
                    .unwrap();
                if state["deliveryState"] == "accepted" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        let repeated = controller
            .call_authorized("send", payload, &authorization)
            .await
            .unwrap();
        assert_eq!(repeated["deliveryState"], "accepted");
        let requests = std::fs::read_to_string(directory.path().join("requests.jsonl")).unwrap();
        let count = requests
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|request| {
                request["method"] == "turn/start"
                    && request["params"]["clientUserMessageId"] == "uncertain-a"
            })
            .count();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn service_dispatch_rechecks_revocation_and_expiry_after_preflight() {
        for expired in [false, true] {
            let (_directory, authorization) = service_dispatch_fixture().await;
            authorization.check().await.unwrap();
            {
                let mut auth = authorization.controller.auth.lock().await;
                if expired {
                    auth.devices[0].expires_at_ms = 0;
                } else {
                    auth.devices.clear();
                }
            }
            // 原生/owner 预检期间授权变化，不能复用入口处已经通过的检查。
            let result = ServiceDispatchGuard::prepare_dispatch(&authorization).await;
            let Err(error) = result else {
                panic!("预检之后失效的授权不得取得 service 派发许可");
            };
            assert_eq!(error.downcast_ref::<ApiError>().unwrap().code, 401);
        }
    }

    #[tokio::test]
    async fn service_dispatch_rechecks_policy_at_the_queue_commit_boundary() {
        for change in ["readonly", "stop", "closing", "expired"] {
            let (_directory, authorization) = service_dispatch_fixture().await;
            let mut permit = authorization.prepare_dispatch().await.unwrap();
            match change {
                "readonly" => {
                    authorization
                        .controller
                        .publish_access_policy(&RemoteAccessSettings {
                            enabled: true,
                            read_only: true,
                            ..RemoteAccessSettings::default()
                        });
                }
                "stop" => authorization
                    .controller
                    .running
                    .store(false, Ordering::SeqCst),
                "closing" => authorization
                    .controller
                    .closing
                    .store(true, Ordering::Release),
                "expired" => permit.expires_at_ms = 0,
                _ => unreachable!(),
            }
            let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
            let error = ServiceDispatchPermit::commit(
                Box::new(permit),
                Box::new(move || {
                    sender.try_send("original-request")?;
                    Ok(())
                }),
            )
            .unwrap_err();
            assert_eq!(
                error.downcast_ref::<ApiError>().unwrap().code,
                match change {
                    "readonly" => 403,
                    "stop" | "closing" => 503,
                    _ => 401,
                }
            );
            assert!(receiver.try_recv().is_err(), "{change} 之后不得入队");
            assert!(authorization.controller.auth.try_lock().is_ok());
        }
    }

    #[tokio::test]
    async fn service_dispatch_holds_authorization_only_through_synchronous_enqueue() {
        let (_directory, authorization) = service_dispatch_fixture().await;
        let permit = ServiceDispatchGuard::prepare_dispatch(&authorization)
            .await
            .unwrap();
        let controller = authorization.controller.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        permit
            .commit(Box::new(move || {
                // 撤权及只读切换不能插入最终检查与请求入队之间。
                assert!(controller.auth.try_lock().is_err());
                assert!(controller.access_policy.try_write().is_err());
                sender.try_send("original-request")?;
                Ok(())
            }))
            .unwrap();
        assert_eq!(receiver.try_recv().unwrap(), "original-request");
        assert!(receiver.try_recv().is_err());
        // 实际网络写入/回执等待发生时，授权锁已释放。
        assert!(authorization.controller.auth.try_lock().is_ok());
        assert!(authorization.controller.access_policy.try_write().is_ok());
    }

    #[tokio::test]
    async fn prompt_optimization_http_works_without_native_or_cli_and_is_device_scoped() {
        use crate::settings::{
            BackendSettings, RelayMode, RelayModelMapping, RelayProfile, RelayProtocol,
        };
        let (directory, state) = login_lock_test_state();
        let controller = state.controller.clone();
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let router = Router::new().route(
            "/v1/responses",
            post({
                let captured = captured.clone();
                move |Json(body): Json<Value>| {
                    let captured = captured.clone();
                    async move {
                        captured.lock().await.push(body.clone());
                        if body.to_string().contains("cancel-after-start") {
                            tokio::time::sleep(Duration::from_millis(400)).await;
                        }
                        Json(json!({
                            "id":"optimization-response","object":"response","status":"completed",
                            "output":[{"type":"message","role":"assistant","content":[
                                {"type":"output_text","text":"优化结果"}]}],
                            "usage":{"total_tokens":23}
                        }))
                    }
                }
            }),
        );
        let upstream_task =
            tokio::spawn(async move { axum::serve(upstream, router).await.unwrap() });
        SettingsStore::new(directory.path().join("settings.json"))
            .save(&BackendSettings {
                active_relay_id: "optimizer".into(),
                relay_profiles: vec![RelayProfile {
                    id: "optimizer".into(),
                    name: "隔离优化接口".into(),
                    base_url: format!("http://127.0.0.1:{upstream_port}/v1"),
                    upstream_base_url: format!("http://127.0.0.1:{upstream_port}/v1"),
                    api_key: "isolated-key".into(),
                    auth_contents: json!({"OPENAI_API_KEY":"isolated-key"}).to_string(),
                    local_proxy_enabled: Some(true),
                    relay_mode: RelayMode::MixedApi,
                    model_mappings: vec![RelayModelMapping {
                        request_model: "gpt-optimizer-fixture".into(),
                        protocol: RelayProtocol::Responses,
                        alias: String::new(),
                        context_window: "200000".into(),
                        system_prompt_override: String::new(),
                    }],
                    ..RelayProfile::default()
                }],
                ..BackendSettings::default()
            })
            .unwrap();
        let mut preferences = crate::prompt_optimize::load_preferences(directory.path()).unwrap();
        preferences["model"] = json!("gpt-optimizer-fixture");
        preferences["includeRecentContext"] = json!(true);
        crate::prompt_optimize::save_preferences(directory.path(), &preferences).unwrap();
        {
            let mut auth = controller.auth.lock().await;
            let key_id = auth.access_key.as_ref().unwrap().id.clone();
            for (id, token) in [
                ("optimizer-a", "optimizer-cookie-a"),
                ("optimizer-b", "optimizer-cookie-b"),
            ] {
                auth.devices.push(Device {
                    id: id.into(),
                    name: id.into(),
                    token_hash: hash(token),
                    client_id_hash: String::new(),
                    key_id: key_id.clone(),
                    created_at_ms: now_ms(),
                    last_seen_at_ms: now_ms(),
                    expires_at_ms: now_ms() + 120_000,
                    public_origin: None,
                });
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = web_router(
            controller.clone(),
            RemoteAccessSettings {
                enabled: true,
                port,
                ..RemoteAccessSettings::default()
            },
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let post = |path: &str, request_id: &str, payload: Value, cookie: &str| {
            client
                .post(format!("{base}{path}"))
                .header("Origin", &base)
                .header("Cookie", cookie)
                .header("Idempotency-Key", request_id)
                .json(&payload)
        };
        let cookie = "codex_elves_device=optimizer-cookie-a";
        let other_cookie = "codex_elves_device=optimizer-cookie-b";
        let session = client
            .get(format!("{base}/api/session"))
            .header("Cookie", cookie)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(session["serviceConnected"], false);
        assert_eq!(session["readOnly"], false);
        assert_eq!(session["capabilities"]["promptOptimization"], true);
        // Claude 会话模型也可作为浏览器上下文；独立优化仍优先使用桌面保存的模型，不能转入 Claude CLI。
        let payload = json!({
            "input":"待优化文本","model":"claude-opus-5-5",
            "recentContext":{"user":"背景输入","assistant":"背景回复"}
        });
        let start = "/api/action/optimizePrompt";
        let cancel = "/api/action/cancelPromptOptimize";
        let status = "/api/read/promptOptimization";
        assert_eq!(
            post(start, "request-1", payload.clone(), "")
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            post(
                start,
                "invalid",
                json!({"input":"x","systemPrompt":"not allowed"}),
                cookie
            )
            .send()
            .await
            .unwrap()
            .status(),
            400
        );
        controller.publish_access_policy(&RemoteAccessSettings {
            enabled: true,
            read_only: true,
            ..RemoteAccessSettings::default()
        });
        assert_eq!(
            post(start, "readonly", payload.clone(), cookie)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        controller.publish_access_policy(&RemoteAccessSettings {
            enabled: true,
            ..RemoteAccessSettings::default()
        });
        let accepted = post(start, "request-1", payload.clone(), cookie)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(accepted["state"], "completed");
        let completed = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let result = post(status, "read", json!({"requestId":"request-1"}), cookie)
                    .send()
                    .await
                    .unwrap()
                    .json::<Value>()
                    .await
                    .unwrap();
                if result["state"] != "pending" {
                    break result;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(completed["state"], "completed", "{completed}");
        assert_eq!(completed["text"], "优化结果");
        assert_eq!(completed["totalTokens"], 23);
        assert_eq!(
            post(start, "request-1", payload, cookie)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap(),
            accepted
        );
        assert_eq!(captured.lock().await.len(), 1);
        let sent = captured.lock().await[0].clone();
        assert_eq!(sent["model"], "gpt-optimizer-fixture");
        assert!(
            sent["instructions"]
                .as_str()
                .unwrap()
                .starts_with(preferences["prompts"]["concise"].as_str().unwrap())
        );
        assert!(sent["input"].to_string().contains("背景输入"));
        assert_eq!(
            post(
                status,
                "read",
                json!({"requestId":"request-1"}),
                other_cookie
            )
            .send()
            .await
            .unwrap()
            .status(),
            404
        );
        post(
            cancel,
            "cancel-first",
            json!({"requestId":"cancel-before-start"}),
            cookie,
        )
        .send()
        .await
        .unwrap();
        let cancelled = post(
            start,
            "cancel-before-start",
            json!({"input":"不得发送"}),
            cookie,
        )
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
        assert_eq!(cancelled["result"]["state"], "cancelled");
        assert_eq!(captured.lock().await.len(), 1);
        post(start, "slow", json!({"input":"cancel-after-start"}), cookie)
            .send()
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while captured.lock().await.len() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        post(
            cancel,
            "cancel-slow",
            json!({"requestId":"slow"}),
            other_cookie,
        )
        .send()
        .await
        .unwrap();
        let still_running = post(status, "read", json!({"requestId":"slow"}), cookie)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(still_running["state"], "pending");
        let cancelled = post(cancel, "cancel-slow", json!({"requestId":"slow"}), cookie)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(cancelled["result"]["state"], "cancelled");
        tokio::time::sleep(Duration::from_millis(500)).await;
        let cancelled = post(status, "read", json!({"requestId":"slow"}), cookie)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(cancelled["state"], "cancelled");
        controller.auth.lock().await.devices.clear();
        assert_eq!(
            post(status, "read", json!({"requestId":"request-1"}), cookie)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        server.abort();
        upstream_task.abort();
    }

    #[test]
    fn related_threads_payload_rejects_extra_fields_and_parent_self_close() {
        for action in ["relatedThreads", "createSideThread"] {
            assert!(
                validate_related_threads_payload(action, &json!({"threadId":"parent-1"})).is_ok()
            );
            for payload in [
                Value::Null,
                json!([]),
                json!({}),
                json!({"threadId":null}),
                json!({"threadId":"../private"}),
                json!({"threadId":"parent-1","cwd":"C:\\private"}),
                json!({"threadId":"parent-1","parentThreadId":"other"}),
                json!({"threadId":"parent-1","prompt":"不要注入额外消息"}),
            ] {
                assert_eq!(
                    validate_related_threads_payload(action, &payload)
                        .unwrap_err()
                        .code,
                    400
                );
            }
        }
        for effort in [json!("xhigh"), Value::Null] {
            assert!(validate_related_threads_payload(
                "createSideThread",
                &json!({"threadId":"parent-1","model":"selected-model","reasoningEffort":effort})
            ).is_ok());
        }
        for payload in [
            json!({"threadId":"parent-1","model":""}),
            json!({"threadId":"parent-1","model":"bad\nmodel"}),
            json!({"threadId":"parent-1","model":42}),
            json!({"threadId":"parent-1","reasoningEffort":"unbounded"}),
            json!({"threadId":"parent-1","permissionMode":"full-access"}),
            json!({"threadId":"parent-1","permissionConfig":{"approvalPolicy":"never"}}),
            json!({"threadId":"parent-1","permissions":":danger-full-access"}),
        ] {
            assert_eq!(
                validate_related_threads_payload("createSideThread", &payload)
                    .unwrap_err()
                    .code,
                400
            );
        }
        assert!(
            validate_related_threads_payload(
                "relatedThreads",
                &json!({"threadId":"parent-1","model":"selected-model"})
            )
            .is_err()
        );
        assert!(
            validate_related_threads_payload(
                "closeSideThread",
                &json!({"threadId":"side-1","parentThreadId":"parent-1"})
            )
            .is_ok()
        );
        for payload in [
            json!({"threadId":"side-1"}),
            json!({"threadId":"side-1","parentThreadId":null}),
            json!({"threadId":"parent-1","parentThreadId":"parent-1"}),
            json!({"threadId":"side-1","parentThreadId":"parent-1","delete":true}),
        ] {
            assert_eq!(
                validate_related_threads_payload("closeSideThread", &payload)
                    .unwrap_err()
                    .code,
                400
            );
        }
    }

    #[tokio::test]
    async fn related_threads_routes_preserve_auth_read_only_and_idempotency() {
        let temp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../temp");
        std::fs::create_dir_all(&temp_root).unwrap();
        let state_dir = tempfile::Builder::new()
            .prefix("related-route-")
            .tempdir_in(temp_root)
            .unwrap();
        configure_access_key(state_dir.path(), "related-test-key").unwrap();
        let native = Arc::new(RecordingActionBridge {
            calls: Mutex::new(Vec::new()),
            result: Mutex::new(json!({"ok":true,"threadId":"side-1","parentThreadId":"parent-1"})),
        });
        let controller =
            HarnessController::with_test_backend(native.clone(), state_dir.path().to_path_buf())
                .unwrap();
        controller.publish_access_policy(&RemoteAccessSettings {
            enabled: true,
            ..RemoteAccessSettings::default()
        });
        controller.running.store(true, Ordering::SeqCst);
        {
            let mut auth = controller.auth.lock().await;
            let key_id = auth.access_key.as_ref().unwrap().id.clone();
            auth.devices.push(Device {
                id: "related-device".into(),
                name: "关联会话隔离验证".into(),
                token_hash: hash("related-cookie"),
                client_id_hash: String::new(),
                created_at_ms: now_ms(),
                expires_at_ms: now_ms() + 60_000,
                last_seen_at_ms: now_ms(),
                key_id,
                public_origin: None,
            });
        }
        let state = WebState {
            desktop: false,
            controller: controller.clone(),
            settings: RemoteAccessSettings::default(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("codex_elves_device=related-cookie"),
        );
        let invoke = |name: &str, state, headers, payload| {
            action(State(state), Path(name.to_string()), headers, Json(payload))
        };
        for (index, (name, payload)) in [
            ("createSideThread", json!({"threadId":"parent-1"})),
            (
                "closeSideThread",
                json!({"threadId":"side-1","parentThreadId":"parent-1"}),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("related-{index}");
            headers.insert("idempotency-key", HeaderValue::from_str(&id).unwrap());
            assert_eq!(
                invoke(name, state.clone(), HeaderMap::new(), payload.clone())
                    .await
                    .unwrap_err()
                    .code,
                401
            );
            let mut read_only = state.clone();
            read_only.settings.read_only = true;
            assert_eq!(
                invoke(name, read_only, headers.clone(), payload.clone())
                    .await
                    .unwrap_err()
                    .code,
                403
            );
            let mut no_id = headers.clone();
            no_id.remove("idempotency-key");
            assert_eq!(
                invoke(name, state.clone(), no_id, payload.clone())
                    .await
                    .unwrap_err()
                    .code,
                400
            );
            assert_eq!(native.calls.lock().await.len(), index);
            let first = invoke(name, state.clone(), headers.clone(), payload.clone())
                .await
                .unwrap()
                .0;
            assert_eq!(first["state"], "completed");
            assert_eq!(
                invoke(name, state.clone(), headers.clone(), payload)
                    .await
                    .unwrap()
                    .0,
                first
            );
            assert_eq!(native.calls.lock().await.len(), index + 1);
        }
        let list_payload = json!({"threadId":"parent-1"});
        assert_eq!(
            read(
                State(state.clone()),
                Path("relatedThreads".into()),
                HeaderMap::new(),
                Json(list_payload.clone())
            )
            .await
            .unwrap_err()
            .code,
            401
        );
        let mut read_only = state.clone();
        read_only.settings.read_only = true;
        *native.result.lock().await = json!({"ok":true,"threadId":"parent-1","subagents":[],
            "completedSubagentCount":0,"sideThreads":[]});
        assert_eq!(
            read(
                State(read_only),
                Path("relatedThreads".into()),
                headers.clone(),
                Json(list_payload)
            )
            .await
            .unwrap()
            .status(),
            StatusCode::OK
        );
        assert_eq!(native.calls.lock().await.len(), 3);
        controller.auth.lock().await.devices.clear();
        assert_eq!(
            invoke(
                "createSideThread",
                state,
                headers,
                json!({"threadId":"parent-1"})
            )
            .await
            .unwrap_err()
            .code,
            401
        );
        assert_eq!(native.calls.lock().await.len(), 3);
    }

    #[test]
    fn workspace_identity_requires_one_native_owner_and_never_accepts_a_cwd() {
        assert_eq!(
            workspace_identity(&json!({"threadId":"thread-1","path":"a.txt","section":"staged"}))
                .unwrap(),
            json!({"threadId":"thread-1"})
        );
        assert_eq!(
            workspace_identity(&json!({"projectId":"project-1"})).unwrap(),
            json!({"projectId":"project-1"})
        );
        assert_eq!(
            workspace_identity(&json!({"threadId":"thread-1","ifRevision":"a".repeat(64)}))
                .unwrap(),
            json!({"threadId":"thread-1"})
        );
        for payload in [
            json!({}),
            json!({"threadId":"thread-1","projectId":"project-1"}),
            json!({"threadId":"thread-1","cwd":"C:\\private"}),
            json!({"threadId":"thread-1","root":"C:\\private"}),
            json!({"threadId":"../../private"}),
            json!({"projectId":null}),
            json!({"projectId":""}),
            json!({"threadId":"thread-1","ifRevision":null}),
            json!({"threadId":"thread-1","ifRevision":"a".repeat(63)}),
            json!({"threadId":"thread-1","ifRevision":"A".repeat(64)}),
            json!([]),
        ] {
            assert_eq!(workspace_identity(&payload).unwrap_err().code, 400);
        }
    }

    fn context_usage_fixture() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::Builder::new()
            .prefix("remote-context-")
            .tempdir_in(FsPath::new(env!("CARGO_MANIFEST_DIR")).join("../../temp"))
            .unwrap();
        let path = directory.path().join("thread.jsonl");
        let db = rusqlite::Connection::open(directory.path().join("state_5.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT)")
            .unwrap();
        db.execute(
            "INSERT INTO threads VALUES (?1, ?2)",
            ["thread-1", "thread.jsonl"],
        )
        .unwrap();
        std::fs::write(&path, context_usage_event(175_828, 570_000)).unwrap();
        (directory, path)
    }

    fn context_usage_event(used: u64, capacity: u64) -> String {
        format!(
            "{}\n",
            json!({"type":"event_msg","payload":{"type":"token_count","info":{
                "last_token_usage":{"total_tokens":used},
                "total_token_usage":{"total_tokens":6_803_996},
                "model_context_window":capacity
            }}})
        )
    }

    #[test]
    fn context_usage_reads_last_request_by_exact_id_and_preserves_native_values() {
        let (directory, _) = context_usage_fixture();
        let missing =
            json!({"ok":true,"thread":{"id":"thread-1","status":"idle","tokenUsage":null}});
        let result = context_usage::fill_missing(directory.path(), "thread-1", missing.clone());
        assert_eq!(
            result["thread"]["tokenUsage"]["last"]["totalTokens"],
            175_828
        );
        assert_eq!(
            result["thread"]["tokenUsage"]["modelContextWindow"],
            570_000
        );
        assert_eq!(result["thread"]["status"], "idle");
        assert!(result["thread"].get("rollout_path").is_none());
        let mut native = missing.clone();
        native["thread"]["tokenUsage"] = json!({"last":{"totalTokens":0},"modelContextWindow":123});
        assert_eq!(
            context_usage::fill_missing(directory.path(), "thread-1", native.clone()),
            native
        );
        assert_eq!(
            context_usage::fill_missing(directory.path(), "other-thread", missing.clone()),
            missing
        );
        let failure = json!({"ok":false,"thread":{"id":"thread-1"}});
        assert_eq!(
            context_usage::fill_missing(directory.path(), "thread-1", failure.clone()),
            failure
        );
    }

    #[test]
    fn context_usage_cache_refreshes_after_append_and_truncate() {
        let (directory, path) = context_usage_fixture();
        let snapshot = || {
            context_usage::fill_missing(
                directory.path(),
                "thread-1",
                json!({"ok":true,"thread":{"id":"thread-1"}}),
            )["thread"]["tokenUsage"]
                .clone()
        };
        assert_eq!(snapshot()["last"]["totalTokens"], 175_828);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(context_usage_event(42, 1000).as_bytes())
            .unwrap();
        file.write_all(
            b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":null}}\n",
        )
        .unwrap();
        file.write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":")
            .unwrap();
        file.flush().unwrap();
        assert_eq!(snapshot()["last"]["totalTokens"], 42);
        drop(file);
        std::fs::write(path, context_usage_event(0, 2000)).unwrap();
        assert_eq!(
            snapshot(),
            json!({"last":{"totalTokens":0},"modelContextWindow":2000})
        );
    }

    #[test]
    fn context_usage_skips_fake_events_and_missing_measurements() {
        let (directory, path) = context_usage_fixture();
        let snapshot = || {
            context_usage::fill_missing(
                directory.path(),
                "thread-1",
                json!({"ok":true,"thread":{"id":"thread-1","tokenUsage":null}}),
            )
        };
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n{}\n",
                json!({"type":"response_item","payload":{"type":"token_count","info":{
                "last_token_usage":{"total_tokens":50},"model_context_window":100}}}),
                json!({"type":"event_msg","payload":{"type":"token_count","info":{
                "total_token_usage":{"total_tokens":50},"model_context_window":100}}}),
                context_usage_event(100, 0)
            ),
        )
        .unwrap();
        assert!(snapshot()["thread"]["tokenUsage"].is_null());
        std::fs::remove_file(path).unwrap();
        assert!(snapshot()["thread"]["tokenUsage"].is_null());
        let empty = directory.path().join("missing-home");
        context_usage::fill_missing(
            &empty,
            "thread-1",
            json!({"ok":true,"thread":{"id":"thread-1"}}),
        );
        assert!(!empty.exists());
    }

    #[test]
    fn context_usage_tail_expands_past_large_trailing_records() {
        let (directory, path) = context_usage_fixture();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"response_item","payload":{"text":"x".repeat(300_000)}})
        )
        .unwrap();
        file.flush().unwrap();
        let result = context_usage::fill_missing(
            directory.path(),
            "thread-1",
            json!({"ok":true,"thread":{"id":"thread-1"}}),
        );
        assert_eq!(
            result["thread"]["tokenUsage"]["last"]["totalTokens"],
            175_828
        );
    }

    #[tokio::test]
    async fn context_usage_route_keeps_authorization_and_works_in_read_only_mode() {
        let (directory, _) = context_usage_fixture();
        configure_access_key(directory.path(), "context-test-key").unwrap();
        SettingsStore::new(directory.path().join("settings.json"))
            .save(&crate::settings::BackendSettings {
                codex_home_path: directory.path().to_string_lossy().into_owned(),
                ..Default::default()
            })
            .unwrap();
        let native = Arc::new(RecordingActionBridge {
            calls: Mutex::new(Vec::new()),
            result: Mutex::new(json!({"ok":true,"thread":{"id":"thread-1","tokenUsage":null}})),
        });
        let controller =
            HarnessController::with_test_backend(native.clone(), directory.path().to_path_buf())
                .unwrap();
        controller.publish_access_policy(&RemoteAccessSettings {
            enabled: true,
            read_only: true,
            ..Default::default()
        });
        controller.running.store(true, Ordering::SeqCst);
        {
            let mut auth = controller.auth.lock().await;
            let key_id = auth.access_key.as_ref().unwrap().id.clone();
            auth.devices.push(Device {
                id: "context-device".into(),
                name: "上下文验证".into(),
                token_hash: hash("context-cookie"),
                client_id_hash: String::new(),
                created_at_ms: now_ms(),
                expires_at_ms: now_ms() + 60_000,
                last_seen_at_ms: now_ms(),
                key_id,
                public_origin: None,
            });
        }
        let state = WebState {
            desktop: false,
            controller: controller.clone(),
            settings: Default::default(),
        };
        let invoke = |headers| {
            read(
                State(state.clone()),
                Path("thread".into()),
                headers,
                Json(json!({"threadId":"thread-1"})),
            )
        };
        assert_eq!(invoke(HeaderMap::new()).await.unwrap_err().code, 401);
        assert!(native.calls.lock().await.is_empty());
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "codex_elves_device=context-cookie".parse().unwrap(),
        );
        let response = invoke(headers.clone()).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value["thread"]["tokenUsage"]["last"]["totalTokens"],
            175_828
        );
        controller.auth.lock().await.devices.clear();
        assert_eq!(invoke(headers).await.unwrap_err().code, 401);
        assert_eq!(native.calls.lock().await.len(), 1);
    }

    #[test]
    fn delete_thread_payload_requires_a_single_real_thread_uuid() {
        assert!(
            validate_delete_thread_payload(
                &json!({"threadId":"01234567-89ab-4cde-8123-456789abcdef"})
            )
            .is_ok()
        );
        for payload in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"threadId":42}),
            json!({"threadId":"client-new-thread-1"}),
            json!({"threadId":"local:01234567-89ab-4cde-8123-456789abcdef"}),
            json!({"threadId":"cloud:01234567-89ab-4cde-8123-456789abcdef"}),
            json!({"threadId":"0123456789ab4cde8123456789abcdef"}),
            json!({"threadId":"01234567-89ab-4cde-8123-456789abcdef","hostId":"local"}),
            json!({"threadId":"01234567-89ab-4cde-8123-456789abcdef","force":true}),
        ] {
            assert_eq!(
                validate_delete_thread_payload(&payload).unwrap_err().code,
                400
            );
        }
    }

    struct RecordingActionBridge {
        calls: Mutex<Vec<(String, Value)>>,
        result: Mutex<Value>,
    }

    #[async_trait::async_trait]
    impl TestRemoteBackend for RecordingActionBridge {
        async fn call(&self, action: &str, payload: Value) -> anyhow::Result<Value> {
            self.calls.lock().await.push((action.to_string(), payload));
            Ok(self.result.lock().await.clone())
        }
    }

    #[test]
    fn remote_fast_payload_requires_model_identity_and_explicit_boolean() {
        for payload in [
            json!({"model":"gpt-fixture"}),
            json!({"model":"gpt-fixture","threadId":"thread-1"}),
            json!({"model":"x".repeat(256),"threadId":"x".repeat(120)}),
        ] {
            assert!(validate_fast_payload("fast", &payload).is_ok());
        }
        for payload in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"model":null}),
            json!({"model":true}),
            json!({"model":""}),
            json!({"model":" "}),
            json!({"model":"x".repeat(257)}),
            json!({"model":"gpt-fixture","threadId":null}),
            json!({"model":"gpt-fixture","threadId":false}),
            json!({"model":"gpt-fixture","threadId":""}),
            json!({"model":"gpt-fixture","threadId":"../other"}),
            json!({"model":"gpt-fixture","threadId":"local:thread-1"}),
            json!({"model":"gpt-fixture","threadId":"x".repeat(121)}),
        ] {
            assert_eq!(
                validate_fast_payload("fast", &payload).unwrap_err().code,
                400,
                "payload: {payload}"
            );
        }
        for enabled in [false, true] {
            assert!(
                validate_fast_payload(
                    "setFast",
                    &json!({"model":"gpt-fixture","threadId":"thread-1","enabled":enabled})
                )
                .is_ok()
            );
        }
        for payload in [
            json!({"model":"gpt-fixture","enabled":true}),
            json!({"model":"gpt-fixture","threadId":"thread-1"}),
            json!({"model":"gpt-fixture","threadId":"thread-1","enabled":null}),
            json!({"model":"gpt-fixture","threadId":"thread-1","enabled":"true"}),
            json!({"model":"gpt-fixture","threadId":"thread-1","enabled":1}),
        ] {
            assert_eq!(
                validate_fast_payload("setFast", &payload).unwrap_err().code,
                400
            );
        }
        for action in ["send", "create"] {
            assert!(validate_fast_payload(action, &json!({})).is_ok());
            for enabled in [false, true] {
                assert!(validate_fast_payload(action, &json!({"fastMode":enabled})).is_ok());
            }
            for enabled in [Value::Null, json!("false"), json!(1), json!({})] {
                assert_eq!(
                    validate_fast_payload(action, &json!({"fastMode":enabled}))
                        .unwrap_err()
                        .code,
                    400
                );
            }
        }
        let goal = json!({"projectId":null,"prompt":"新目标","goalMode":true,"fastMode":true});
        assert!(validate_fast_payload("create", &goal).is_ok());
        assert!(validate_goal_composer_payload("create", &goal).is_ok());
    }

    #[tokio::test]
    async fn remote_fast_routes_preserve_authorization_read_only_and_idempotency() {
        let temp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../temp");
        std::fs::create_dir_all(&temp_root).unwrap();
        let state_dir = tempfile::Builder::new()
            .prefix("fast-route-")
            .tempdir_in(temp_root)
            .unwrap();
        configure_access_key(state_dir.path(), "fast-test-key").unwrap();
        let native = Arc::new(RecordingActionBridge {
            calls: Mutex::new(Vec::new()),
            result: Mutex::new(json!({"ok":true,"threadId":"thread-1",
                "model":"gpt-fixture","supported":true,"enabled":true})),
        });
        let controller =
            HarnessController::with_test_backend(native.clone(), state_dir.path().to_path_buf())
                .unwrap();
        controller.publish_access_policy(&RemoteAccessSettings {
            enabled: true,
            ..RemoteAccessSettings::default()
        });
        controller.running.store(true, Ordering::SeqCst);
        {
            let mut auth = controller.auth.lock().await;
            let key_id = auth.access_key.as_ref().unwrap().id.clone();
            auth.devices.push(Device {
                id: "fast-device".into(),
                name: "Fast 隔离验证".into(),
                token_hash: hash("fast-cookie"),
                client_id_hash: String::new(),
                created_at_ms: now_ms(),
                expires_at_ms: now_ms() + 60_000,
                last_seen_at_ms: now_ms(),
                key_id,
                public_origin: None,
            });
        }
        let state = WebState {
            desktop: false,
            controller: controller.clone(),
            settings: RemoteAccessSettings::default(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("codex_elves_device=fast-cookie"),
        );
        headers.insert("idempotency-key", HeaderValue::from_static("fast-1"));
        let payload = json!({"threadId":"thread-1","model":"gpt-fixture","enabled":true});
        let invoke = |state, headers, payload| {
            action(State(state), Path("setFast".into()), headers, Json(payload))
        };
        assert_eq!(
            invoke(state.clone(), HeaderMap::new(), payload.clone())
                .await
                .unwrap_err()
                .code,
            401
        );
        assert_eq!(
            read(
                State(state.clone()),
                Path("fast".into()),
                HeaderMap::new(),
                Json(json!({"model":"gpt-fixture"}))
            )
            .await
            .unwrap_err()
            .code,
            401
        );
        let mut read_only = state.clone();
        read_only.settings.read_only = true;
        assert_eq!(
            invoke(read_only.clone(), headers.clone(), payload.clone())
                .await
                .unwrap_err()
                .code,
            403
        );
        let mut no_id = headers.clone();
        no_id.remove("idempotency-key");
        assert_eq!(
            invoke(state.clone(), no_id, payload.clone())
                .await
                .unwrap_err()
                .code,
            400
        );
        assert_eq!(
            invoke(
                state.clone(),
                headers.clone(),
                json!({"threadId":"thread-1","model":"gpt-fixture","enabled":"true"})
            )
            .await
            .unwrap_err()
            .code,
            400
        );
        assert_eq!(
            read(
                State(state.clone()),
                Path("fast".into()),
                headers.clone(),
                Json(json!({"model":"gpt-fixture","threadId":"../other"}))
            )
            .await
            .unwrap_err()
            .code,
            400
        );
        assert!(native.calls.lock().await.is_empty());
        assert_eq!(
            read(
                State(read_only),
                Path("fast".into()),
                headers.clone(),
                Json(json!({"model":"gpt-fixture"}))
            )
            .await
            .unwrap()
            .status(),
            StatusCode::OK
        );
        controller.publish_access_policy(&RemoteAccessSettings {
            enabled: true,
            read_only: true,
            ..RemoteAccessSettings::default()
        });
        assert_eq!(
            invoke(state.clone(), headers.clone(), payload.clone())
                .await
                .unwrap_err()
                .code,
            403,
            "运行时只读策略也必须阻止 Fast 写入"
        );
        assert_eq!(native.calls.lock().await.len(), 1);
        controller.publish_access_policy(&RemoteAccessSettings {
            enabled: true,
            ..RemoteAccessSettings::default()
        });
        let first = invoke(state.clone(), headers.clone(), payload.clone())
            .await
            .unwrap()
            .0;
        assert_eq!(first["state"], "completed");
        assert_eq!(
            invoke(state.clone(), headers.clone(), payload.clone())
                .await
                .unwrap()
                .0,
            first
        );
        assert_eq!(
            invoke(
                state.clone(),
                headers.clone(),
                json!({"threadId":"thread-1","model":"gpt-fixture","enabled":false})
            )
            .await
            .unwrap_err()
            .code,
            409
        );
        let calls = native.calls.lock().await.clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "fast");
        assert_eq!(calls[1], ("setFast".into(), payload.clone()));
        controller.auth.lock().await.devices.clear();
        assert_eq!(invoke(state, headers, payload).await.unwrap_err().code, 401);
        assert_eq!(native.calls.lock().await.len(), 2);
    }

    #[test]
    fn image_preview_preserves_alpha_and_bounded_cache() {
        use image::GenericImageView;
        let original = image::RgbaImage::from_fn(1200, 800, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, (x * y % 256) as u8, 96])
        });
        let mut encoded = Cursor::new(Vec::new());
        original.write_to(&mut encoded, ImageFormat::Png).unwrap();
        let source = LoadedImage {
            bytes: encoded.into_inner(),
            mime: "image/png",
        };
        let preview = image_preview::thumbnail(&source).unwrap();
        assert!(preview.bytes.len() < source.bytes.len());
        assert_eq!(preview.mime, "image/webp");
        let decoded = image::load_from_memory(&preview.bytes).unwrap();
        assert_eq!(decoded.dimensions(), (256, 171));
        assert_eq!(decoded.to_rgba8().get_pixel(100, 80)[3], 96);
        let mut small = Cursor::new(Vec::new());
        image::RgbaImage::new(50, 50)
            .write_to(&mut small, ImageFormat::Png)
            .unwrap();
        assert!(
            image_preview::thumbnail(&LoadedImage {
                bytes: small.into_inner(),
                mime: "image/png",
            })
            .is_none()
        );
        let mut cache = image_preview::PreviewCache::default();
        cache.insert([1; 32], Some(preview));
        cache.insert([2; 32], None);
        assert!(cache.get(&[1; 32]).unwrap().is_some());
        assert!(cache.get(&[2; 32]).unwrap().is_none());
        for value in 3..=131 {
            cache.insert([value; 32], None);
        }
        assert!(cache.get(&[1; 32]).is_none());
        assert!(cache.get(&[131; 32]).is_some());
    }

    #[tokio::test]
    async fn image_http_cache_revalidates_content_and_authorization() {
        let directory = tempfile::tempdir().unwrap();
        configure_access_key(directory.path(), "image-cache-test-key").unwrap();
        let controller = HarnessController::with_test_backend(
            Arc::new(OfflineTestBackend),
            directory.path().to_path_buf(),
        )
        .unwrap();
        controller.publish_access_policy(&RemoteAccessSettings {
            enabled: true,
            ..RemoteAccessSettings::default()
        });
        controller.running.store(true, Ordering::SeqCst);
        {
            let mut auth = controller.auth.lock().await;
            let key_id = auth.access_key.as_ref().unwrap().id.clone();
            auth.devices.push(Device {
                id: "image-cache-device".into(),
                name: "图片缓存回归".into(),
                token_hash: hash("image-cache-cookie"),
                client_id_hash: String::new(),
                created_at_ms: now_ms(),
                expires_at_ms: now_ms() + 120_000,
                last_seen_at_ms: now_ms(),
                key_id,
                public_origin: None,
            });
        }
        let source_path = directory.path().join("image.png");
        let write_source = |seed: u32| {
            image::RgbaImage::from_fn(1200, 800, |x, y| {
                image::Rgba([
                    ((x + seed) % 256) as u8,
                    (y % 256) as u8,
                    (x * y % 256) as u8,
                    255,
                ])
            })
            .save(&source_path)
            .unwrap();
        };
        write_source(0);
        let image_id = controller
            .images
            .write()
            .unwrap()
            .register(source_path.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = web_router(
            controller.clone(),
            RemoteAccessSettings {
                enabled: true,
                port,
                ..RemoteAccessSettings::default()
            },
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let base = format!("http://127.0.0.1:{port}/api/images/{image_id}");
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let request = |url: String| {
            client.get(url).header(
                reqwest::header::COOKIE,
                "codex_elves_device=image-cache-cookie",
            )
        };
        let unauthorized = client.get(&base).send().await.unwrap();
        assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(
            unauthorized.headers()[reqwest::header::CACHE_CONTROL],
            "no-store"
        );
        let mut etags = Vec::new();
        for (url, expected_mime) in [
            (base.clone(), "image/png"),
            (format!("{base}/thumbnail"), "image/webp"),
        ] {
            let response = request(url.clone()).send().await.unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(
                response.headers()[reqwest::header::CONTENT_TYPE],
                expected_mime
            );
            assert_eq!(
                response.headers()[reqwest::header::CACHE_CONTROL],
                "private, no-cache"
            );
            assert_eq!(response.headers()[reqwest::header::VARY], "Cookie");
            let etag = response.headers()[reqwest::header::ETAG].clone();
            let bytes = response.bytes().await.unwrap();
            if expected_mime == "image/png" {
                assert_eq!(bytes.as_ref(), std::fs::read(&source_path).unwrap());
            }
            let unchanged = request(url)
                .header(reqwest::header::IF_NONE_MATCH, &etag)
                .send()
                .await
                .unwrap();
            assert_eq!(unchanged.status(), reqwest::StatusCode::NOT_MODIFIED);
            assert_eq!(unchanged.headers()[reqwest::header::VARY], "Cookie");
            assert!(unchanged.bytes().await.unwrap().is_empty());
            etags.push(etag);
        }
        write_source(40);
        for (index, url) in [base.clone(), format!("{base}/thumbnail")]
            .into_iter()
            .enumerate()
        {
            let changed = request(url)
                .header(reqwest::header::IF_NONE_MATCH, &etags[index])
                .send()
                .await
                .unwrap();
            assert_eq!(changed.status(), reqwest::StatusCode::OK);
            assert_ne!(changed.headers()[reqwest::header::ETAG], etags[index]);
        }
        // 服务重启会丢失内存媒体 ID；原图存在时，旧 URL 的重试也不能代替按消息重新注册。
        *controller.images.write().unwrap() = ImageRegistry::default();
        assert!(source_path.is_file());
        for url in [base.clone(), format!("{base}/thumbnail")] {
            let expired = request(url).send().await.unwrap();
            assert_eq!(expired.status(), reqwest::StatusCode::NOT_FOUND);
        }
        let mut reread = json!({"scope":"items","data":[{
            "type":"agentMessage","id":"preview-message",
            "text":format!("![预览](<{}>)", source_path.display())
        }]});
        controller.attach_history_images(&mut reread);
        let fresh_path = reread["data"][0]["remoteImages"][0]["url"]
            .as_str()
            .expect("重读原消息应登记新的受鉴权图片地址");
        let base = format!("http://127.0.0.1:{port}{fresh_path}");
        let image_id = fresh_path.rsplit('/').next().unwrap().to_owned();
        let restored = request(base.clone()).send().await.unwrap();
        assert_eq!(restored.status(), reqwest::StatusCode::OK);
        assert_eq!(
            restored.bytes().await.unwrap().as_ref(),
            std::fs::read(&source_path).unwrap()
        );
        std::fs::remove_file(&source_path).unwrap();
        let missing = request(base.clone()).send().await.unwrap();
        assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
        assert_eq!(
            missing.headers()[reqwest::header::CACHE_CONTROL],
            "no-store"
        );
        controller.auth.lock().await.devices.clear();
        for (index, url) in [
            base,
            format!("http://127.0.0.1:{port}/api/images/{image_id}/thumbnail"),
        ]
        .into_iter()
        .enumerate()
        {
            let revoked = request(url)
                .header(reqwest::header::IF_NONE_MATCH, &etags[index])
                .send()
                .await
                .unwrap();
            assert_eq!(revoked.status(), reqwest::StatusCode::UNAUTHORIZED);
            assert_eq!(
                revoked.headers()[reqwest::header::CACHE_CONTROL],
                "no-store"
            );
            assert!(!revoked.headers().contains_key(reqwest::header::ETAG));
        }
        assert_eq!(controller.image_preview_slots.available_permits(), 2);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn read_response_compression_preserves_auth_and_private_payloads() {
        let state_dir = tempfile::tempdir().unwrap();
        configure_access_key(state_dir.path(), "compression-test-key").unwrap();
        let expected = json!({
            "ok": true,
            "data": [{
                "id": "isolated-turn",
                "items": [{
                    "id": "isolated-message",
                    "type": "agentMessage",
                    "text": "这是用于弱网传输验证的合成会话内容，不涉及真实会话。".repeat(1000)
                }]
            }]
        });
        let original_bytes = serde_json::to_vec(&expected).unwrap();
        let native = Arc::new(RecordingActionBridge {
            calls: Mutex::new(Vec::new()),
            result: Mutex::new(expected.clone()),
        });
        let controller =
            HarnessController::with_test_backend(native.clone(), state_dir.path().to_path_buf())
                .unwrap();
        controller.publish_access_policy(&RemoteAccessSettings {
            enabled: true,
            ..RemoteAccessSettings::default()
        });
        controller.running.store(true, Ordering::SeqCst);
        {
            let mut auth = controller.auth.lock().await;
            let key_id = auth.access_key.as_ref().unwrap().id.clone();
            auth.devices.push(Device {
                id: "compression-device".into(),
                name: "隔离传输验证".into(),
                token_hash: hash("compression-cookie"),
                client_id_hash: String::new(),
                created_at_ms: now_ms(),
                expires_at_ms: now_ms() + 120_000,
                last_seen_at_ms: now_ms(),
                key_id,
                public_origin: None,
            });
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = web_router(
            controller,
            RemoteAccessSettings {
                enabled: true,
                port,
                ..RemoteAccessSettings::default()
            },
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let base_url = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let request = |encoding: &str| {
            client
                .post(format!("{base_url}/api/read/history"))
                .header(reqwest::header::ORIGIN, &base_url)
                .header(reqwest::header::ACCEPT_ENCODING, encoding)
                .header(reqwest::header::IF_NONE_MATCH, "*")
                .json(&json!({"threadId":"isolated-thread"}))
        };
        let denied = request("gzip").send().await.unwrap();
        assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert!(native.calls.lock().await.is_empty());
        for (encoding, compressed) in [
            ("gzip", true),
            ("identity", false),
            ("gzip;q=0, *;q=1", false),
        ] {
            let response = request(encoding)
                .header(
                    reqwest::header::COOKIE,
                    "codex_elves_device=compression-cookie",
                )
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(
                response.headers()[reqwest::header::CACHE_CONTROL],
                "no-store"
            );
            assert_eq!(response.headers()[reqwest::header::VARY], "Accept-Encoding");
            assert!(!response.headers().contains_key(reqwest::header::ETAG));
            assert_eq!(
                response.headers().get(reqwest::header::CONTENT_ENCODING),
                compressed.then_some(&HeaderValue::from_static("gzip"))
            );
            let body = response.bytes().await.unwrap();
            let decoded = if compressed {
                assert!(body.len() < original_bytes.len() / 4);
                println!(
                    "合成历史正文：identity={} bytes，gzip={} bytes",
                    original_bytes.len(),
                    body.len()
                );
                let mut decoded = Vec::new();
                flate2::read::GzDecoder::new(&body[..])
                    .read_to_end(&mut decoded)
                    .unwrap();
                decoded
            } else {
                body.to_vec()
            };
            assert!(
                serde_json::from_slice::<Value>(&decoded).unwrap() == expected,
                "压缩与未压缩响应必须保留完整相同的 JSON 正文"
            );
        }
        *native.result.lock().await = json!({"ok":true,"data":[]});
        let small = request("gzip")
            .header(
                reqwest::header::COOKIE,
                "codex_elves_device=compression-cookie",
            )
            .send()
            .await
            .unwrap();
        assert_eq!(small.status(), reqwest::StatusCode::OK);
        assert!(
            !small
                .headers()
                .contains_key(reqwest::header::CONTENT_ENCODING)
        );
        assert_eq!(
            small.json::<Value>().await.unwrap(),
            json!({"ok":true,"data":[]})
        );
        assert_eq!(native.calls.lock().await.len(), 4);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn delete_thread_action_preserves_auth_read_only_and_idempotency() {
        let temp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../temp");
        std::fs::create_dir_all(&temp_root).unwrap();
        let state_dir = tempfile::Builder::new()
            .prefix("delete-route-")
            .tempdir_in(temp_root)
            .unwrap();
        configure_access_key(state_dir.path(), "delete-test-key").unwrap();
        let thread_id = "01234567-89ab-4cde-8123-456789abcdef";
        let native = Arc::new(RecordingActionBridge {
            calls: Mutex::new(Vec::new()),
            result: Mutex::new(
                json!({"ok":true,"threadId":thread_id,"deleted":true,"status":"local_deleted"}),
            ),
        });
        let controller =
            HarnessController::with_test_backend(native.clone(), state_dir.path().to_path_buf())
                .unwrap();
        controller.publish_access_policy(&RemoteAccessSettings {
            enabled: true,
            ..RemoteAccessSettings::default()
        });
        controller.running.store(true, Ordering::SeqCst);
        {
            let mut auth = controller.auth.lock().await;
            let key_id = auth.access_key.as_ref().unwrap().id.clone();
            auth.devices.push(Device {
                id: "test-device".into(),
                name: "隔离删除测试".into(),
                token_hash: hash("test-cookie"),
                client_id_hash: String::new(),
                created_at_ms: now_ms(),
                expires_at_ms: now_ms() + 60_000,
                last_seen_at_ms: now_ms(),
                key_id,
                public_origin: None,
            });
        }
        let state = WebState {
            desktop: false,
            controller: controller.clone(),
            settings: RemoteAccessSettings::default(),
        };
        let payload = json!({"threadId":thread_id});
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("codex_elves_device=test-cookie"),
        );
        headers.insert("idempotency-key", HeaderValue::from_static("delete-1"));
        let invoke = |state, headers, payload| {
            action(
                State(state),
                Path("deleteThread".to_string()),
                headers,
                Json(payload),
            )
        };
        assert_eq!(
            invoke(state.clone(), HeaderMap::new(), payload.clone())
                .await
                .unwrap_err()
                .code,
            401
        );
        let mut read_only = state.clone();
        read_only.settings.read_only = true;
        assert_eq!(
            invoke(read_only, headers.clone(), payload.clone())
                .await
                .unwrap_err()
                .code,
            403
        );
        assert_eq!(
            invoke(
                state.clone(),
                headers.clone(),
                json!({"threadId":"client-new-thread-1"})
            )
            .await
            .unwrap_err()
            .code,
            400
        );
        let mut no_id = headers.clone();
        no_id.remove("idempotency-key");
        assert_eq!(
            invoke(state.clone(), no_id, payload.clone())
                .await
                .unwrap_err()
                .code,
            400
        );
        assert!(native.calls.lock().await.is_empty());
        assert_eq!(
            action(
                State(state.clone()),
                Path("deleteLocalThread".to_string()),
                headers.clone(),
                Json(payload.clone()),
            )
            .await
            .unwrap_err()
            .code,
            404,
            "本机管理器的删除入口不能通过远程 HTTP 调用"
        );
        assert!(native.calls.lock().await.is_empty());
        let first = invoke(state.clone(), headers.clone(), payload.clone())
            .await
            .unwrap()
            .0;
        assert_eq!(first["state"], "completed");
        assert_eq!(first["result"]["deleted"], true);
        assert_eq!(
            invoke(state.clone(), headers.clone(), payload.clone())
                .await
                .unwrap()
                .0,
            first
        );
        assert_eq!(
            invoke(
                state.clone(),
                headers.clone(),
                json!({"threadId":"fedcba98-7654-4321-8abc-0123456789ab"})
            )
            .await
            .unwrap_err()
            .code,
            409
        );
        assert_eq!(
            *native.calls.lock().await,
            vec![("deleteThread".to_string(), payload.clone())]
        );
        *native.result.lock().await =
            json!({"ok":false,"threadId":thread_id,"outcomeUnknown":true});
        headers.insert("idempotency-key", HeaderValue::from_static("delete-2"));
        let unknown = invoke(state.clone(), headers.clone(), payload.clone())
            .await
            .unwrap()
            .0;
        assert_eq!(unknown["state"], "unknown");
        assert_eq!(
            invoke(state.clone(), headers.clone(), payload.clone())
                .await
                .unwrap()
                .0,
            unknown
        );
        assert_eq!(native.calls.lock().await.len(), 2);
        *native.result.lock().await = json!({"ok":true,"threadId":thread_id,"deleted":true,"status":"partial","message":"mock 最近列表更新失败"});
        headers.insert("idempotency-key", HeaderValue::from_static("delete-3"));
        let partial = invoke(state.clone(), headers.clone(), payload.clone())
            .await
            .unwrap()
            .0;
        assert_eq!(partial["state"], "completed");
        assert_eq!(partial["result"]["status"], "partial");
        controller.auth.lock().await.devices.clear();
        assert_eq!(invoke(state, headers, payload).await.unwrap_err().code, 401);
        assert_eq!(native.calls.lock().await.len(), 3);
    }

    #[test]
    fn claude_question_answer_images_use_authorized_image_urls() {
        let directory = tempfile::tempdir().unwrap();
        let controller = HarnessController::new(directory.path().to_owned()).unwrap();
        let mut encoded = Cursor::new(Vec::new());
        image::RgbaImage::new(1, 1)
            .write_to(&mut encoded, ImageFormat::Png)
            .unwrap();
        let data_url = format!(
            "data:image/png;base64,{}",
            STANDARD.encode(encoded.get_ref())
        );
        let question = |tool: &str| {
            json!({"scope":"items","data":[{"id":"q1","type":"dynamicToolCall","source":"claude",
                "tool":tool,"status":"completed","questionAnswers":[
                    {"question":"第一题？","answer":"按截图处理","images":[{"url":data_url}]},
                    {"question":"第二题？","answer":null,"images":[]}]}]})
        };
        let mut history = question("AskUserQuestion");
        controller.attach_history_images(&mut history);
        let item = &history["data"][0];
        let source = item["questionAnswers"][0]["images"][0]["url"]
            .as_str()
            .unwrap();
        assert!(source.starts_with("inline:"), "原图 data URL 不随条目下发");
        assert_eq!(item["remoteImages"][0]["source"], source);
        assert!(
            item["remoteImages"][0]["url"]
                .as_str()
                .unwrap()
                .starts_with("/api/images/")
        );
        // 只有 Claude 提问条目登记回答截图；其他工具的同名字段保持原样。
        let mut other = question("Read");
        controller.attach_history_images(&mut other);
        assert!(other["data"][0]["remoteImages"].is_null());
        assert_eq!(
            other["data"][0]["questionAnswers"][0]["images"][0]["url"],
            data_url
        );
    }

    #[test]
    fn remote_input_images_keep_eight_for_both_backends_and_reject_the_ninth() {
        let mut encoded = Cursor::new(Vec::new());
        image::RgbaImage::new(1, 1)
            .write_to(&mut encoded, ImageFormat::Png)
            .unwrap();
        let bytes = encoded.into_inner();
        let image = json!({"name":"test.png","mime":"image/png","size":bytes.len(),
            "dataUrl":format!("data:image/png;base64,{}", STANDARD.encode(&bytes))});
        for backend in ["codex", "claude"] {
            let mut payload = json!({"backend":backend,"images":vec![image.clone();8]});
            assert!(validate_input_images(&payload).is_ok());
            if backend == "claude" {
                let content = claude::input_content(&payload).unwrap();
                assert_eq!(
                    content
                        .iter()
                        .filter(|part| part["type"] == "image")
                        .count(),
                    8
                );
            }
            payload["images"]
                .as_array_mut()
                .unwrap()
                .push(image.clone());
            let error = validate_input_images(&payload).unwrap_err();
            assert_eq!(error.code, 400);
            assert!(error.message.contains("8 张图片"));
        }
        let mut edit = json!({"threadId":"thread","editId":"edit","prompt":"编辑",
            "retainedImageSources":["one","two","three","four"],"images":vec![image.clone();4]});
        assert!(validate_queued_action_payload("editQueued", &edit).is_ok());
        edit["images"].as_array_mut().unwrap().push(image);
        assert!(
            validate_queued_action_payload("editQueued", &edit)
                .unwrap_err()
                .message
                .contains("8 张图片")
        );
    }

    #[test]
    fn goal_composer_payload_requires_explicit_mode_and_validated_inputs() {
        let payload = json!({
            "threadId":"existing-thread","prompt":"前文 [$example](C:/skills/example/SKILL.md) 后文",
            "goalMode":true,"model":"test-model","reasoningEffort":"xhigh",
            "skills":[{"name":"example","path":"C:/skills/example/SKILL.md"}]
        });
        assert!(validate_goal_composer_payload("setGoal", &payload).is_ok());
        assert!(
            validate_goal_composer_payload(
                "create",
                &json!({"projectId":null,"prompt":"新目标","goalMode":true})
            )
            .is_ok()
        );
        assert!(
            validate_goal_composer_payload(
                "create",
                &json!({"prompt":"普通消息","goalMode":false})
            )
            .is_ok()
        );
        assert_eq!(
            validate_goal_composer_payload("send", &payload)
                .unwrap_err()
                .code,
            400
        );
        for (field, value) in [
            ("goalMode", json!("true")),
            ("goalMode", Value::Null),
            ("goalMode", json!(false)),
            ("threadId", json!("local:other-thread")),
            ("prompt", json!(" ")),
            ("status", json!("active")),
            ("tokenBudget", json!(600_000)),
            ("directoryPath", json!("C:/private")),
            ("model", json!(12)),
            ("reasoningEffort", json!("")),
            (
                "skills",
                json!([{"name":"example","path":"C:/skill.md","content":"unsafe"}]),
            ),
            (
                "images",
                json!([{"name":"sample.png","localPath":"C:/private.png"}]),
            ),
        ] {
            let mut invalid = payload.clone();
            invalid[field] = value;
            assert_eq!(
                validate_goal_composer_payload("setGoal", &invalid)
                    .unwrap_err()
                    .code,
                400,
                "field: {field}"
            );
        }
        let mut image_bytes = Cursor::new(Vec::new());
        image::RgbaImage::new(1, 1)
            .write_to(&mut image_bytes, ImageFormat::Png)
            .unwrap();
        let bytes = image_bytes.into_inner();
        let image = json!({"name":"sample.png","mime":"image/png","size":bytes.len(),
            "dataUrl":format!("data:image/png;base64,{}",STANDARD.encode(bytes))});
        let mut images_only =
            json!({"threadId":"existing-thread","goalMode":true,"prompt":"","images":[image]});
        assert!(validate_goal_composer_payload("setGoal", &images_only).is_ok());
        images_only["images"][0]["size"] = json!(1);
        assert_eq!(
            validate_goal_composer_payload("setGoal", &images_only)
                .unwrap_err()
                .code,
            400
        );
    }

    #[tokio::test]
    async fn goal_composer_actions_preserve_auth_read_only_and_idempotent_phase_receipts() {
        let temp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../temp");
        std::fs::create_dir_all(&temp_root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("goal-composer-route-")
            .tempdir_in(temp_root)
            .unwrap();
        configure_access_key(directory.path(), "isolated-goal-key").unwrap();
        let native = Arc::new(RecordingActionBridge {
            calls: Mutex::new(Vec::new()),
            result: Mutex::new(json!({
                "ok":true,"threadId":"existing-thread",
                "goalSubmission":{"action":"setGoal","phase":"complete","threadId":"existing-thread",
                    "firstTurnAccepted":false,"goalSet":"succeeded"}
            })),
        });
        let controller =
            HarnessController::with_test_backend(native.clone(), directory.path().to_path_buf())
                .unwrap();
        controller.publish_access_policy(&RemoteAccessSettings {
            enabled: true,
            ..RemoteAccessSettings::default()
        });
        controller.running.store(true, Ordering::SeqCst);
        controller.harness_connected.store(true, Ordering::SeqCst);
        {
            let mut auth = controller.auth.lock().await;
            let key_id = auth.access_key.as_ref().unwrap().id.clone();
            auth.devices.push(Device {
                id: "goal-test-device".into(),
                name: "隔离目标测试".into(),
                token_hash: hash("goal-test-cookie"),
                client_id_hash: String::new(),
                created_at_ms: now_ms(),
                expires_at_ms: now_ms() + 60_000,
                last_seen_at_ms: now_ms(),
                key_id,
                public_origin: None,
            });
        }
        let state = WebState {
            desktop: false,
            controller: controller.clone(),
            settings: RemoteAccessSettings::default(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("codex_elves_device=goal-test-cookie"),
        );
        headers.insert("idempotency-key", HeaderValue::from_static("goal-1"));
        let payload = json!({"threadId":"existing-thread","prompt":"完成目标","goalMode":true});
        let invoke = |state, action_name: &str, headers, payload| {
            action(
                State(state),
                Path(action_name.to_string()),
                headers,
                Json(payload),
            )
        };
        assert_eq!(
            invoke(state.clone(), "setGoal", HeaderMap::new(), payload.clone())
                .await
                .unwrap_err()
                .code,
            401
        );
        let mut read_only = state.clone();
        read_only.settings.read_only = true;
        assert_eq!(
            invoke(read_only, "setGoal", headers.clone(), payload.clone())
                .await
                .unwrap_err()
                .code,
            403
        );
        assert_eq!(
            invoke(state.clone(), "send", headers.clone(), payload.clone())
                .await
                .unwrap_err()
                .code,
            400
        );
        let mut no_id = headers.clone();
        no_id.remove("idempotency-key");
        assert_eq!(
            invoke(state.clone(), "setGoal", no_id, payload.clone())
                .await
                .unwrap_err()
                .code,
            400
        );
        assert!(native.calls.lock().await.is_empty());
        let capabilities = session(State(state.clone()), headers.clone())
            .await
            .unwrap()
            .0;
        assert_eq!(capabilities["capabilities"]["goalComposer"], true);
        assert!(capabilities["toolRuntime"]["source"].is_string());
        assert!(capabilities["toolRuntime"]["verification"].is_string());
        assert!(capabilities["toolRuntime"]["paths"].is_object());
        assert_eq!(capabilities["toolRuntime"]["available"]["nodeRepl"], true);
        let completed = invoke(state.clone(), "setGoal", headers.clone(), payload.clone())
            .await
            .unwrap()
            .0;
        assert_eq!(completed["state"], "completed");
        assert_eq!(
            invoke(state.clone(), "setGoal", headers.clone(), payload.clone())
                .await
                .unwrap()
                .0,
            completed
        );
        let mut changed = payload.clone();
        changed["prompt"] = json!("另一个目标");
        assert_eq!(
            invoke(state.clone(), "setGoal", headers.clone(), changed)
                .await
                .unwrap_err()
                .code,
            409
        );
        assert_eq!(native.calls.lock().await.len(), 1);
        let unknown = json!({
            "ok":false,"outcomeUnknown":true,"threadId":"new-thread",
            "goalSubmission":{"action":"create","phase":"goalSet","threadId":"new-thread",
                "firstTurnAccepted":true,"goalSet":"unknown"}
        });
        *native.result.lock().await = unknown.clone();
        headers.insert("idempotency-key", HeaderValue::from_static("goal-2"));
        let creating = json!({"projectId":"project1","prompt":"新目标","goalMode":true});
        let first = invoke(state.clone(), "create", headers.clone(), creating.clone())
            .await
            .unwrap()
            .0;
        assert_eq!(first["state"], "unknown");
        assert_eq!(first["result"], unknown);
        assert_eq!(
            invoke(state.clone(), "create", headers.clone(), creating)
                .await
                .unwrap()
                .0,
            first
        );
        let reread = receipt(
            State(state.clone()),
            Path("goal-2".to_owned()),
            headers.clone(),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(reread, first);
        assert_eq!(native.calls.lock().await.len(), 2);
        let control = json!({"threadId":"existing-thread","objective":"完成目标",
            "expectedCreatedAt":123,"expectedObjective":"完成目标","expectedStatus":"active",
            "operation":"pause"});
        for (field, value) in [
            ("operation", json!("complete")),
            ("operation", Value::Null),
            ("objective", json!("偷偷修改目标")),
            ("expectedStatus", json!("budgetLimited")),
            ("expectedCreatedAt", json!(9_007_199_254_740_992_u64)),
            ("expectedObjective", json!("")),
            ("tokenBudget", json!(900_000)),
        ] {
            let mut invalid = control.clone();
            invalid[field] = value;
            assert_eq!(
                invoke(state.clone(), "updateGoal", headers.clone(), invalid)
                    .await
                    .unwrap_err()
                    .code,
                400,
                "invalid goal control: {field}"
            );
        }
        assert_eq!(native.calls.lock().await.len(), 2);
        for (index, (operation, status)) in [
            ("edit", "active"),
            ("pause", "active"),
            ("resume", "paused"),
            ("resume", "blocked"),
            ("delete", "usageLimited"),
        ]
        .into_iter()
        .enumerate()
        {
            let mut payload = control.clone();
            payload["operation"] = json!(operation);
            payload["expectedStatus"] = json!(status);
            *native.result.lock().await = json!({"ok":true,"threadId":"existing-thread",
                "goal":if operation == "delete" { Value::Null } else { json!({"status":status}) }});
            headers.insert(
                "idempotency-key",
                HeaderValue::from_str(&format!("goal-control-{index}")).unwrap(),
            );
            let completed = invoke(
                state.clone(),
                "updateGoal",
                headers.clone(),
                payload.clone(),
            )
            .await
            .unwrap()
            .0;
            assert_eq!(completed["state"], "completed", "{operation}");
            assert_eq!(
                invoke(
                    state.clone(),
                    "updateGoal",
                    headers.clone(),
                    payload.clone()
                )
                .await
                .unwrap()
                .0,
                completed
            );
            let calls = native.calls.lock().await;
            assert_eq!(calls.len(), index + 3);
            assert_eq!(calls.last().unwrap(), &("updateGoal".to_string(), payload));
        }
    }

    struct OfflineTestBackend;

    #[async_trait::async_trait]
    impl TestRemoteBackend for OfflineTestBackend {
        async fn call(&self, _action: &str, _payload: Value) -> anyhow::Result<Value> {
            anyhow::bail!("native bridge unavailable during test")
        }
    }

    const LOGIN_TEST_KEY: &str = "isolated-login-key";

    #[test]
    fn channel_journal_replays_in_order_and_reports_expired_cursors() {
        let directory = tempfile::tempdir().unwrap();
        let controller = HarnessController::with_test_backend(
            Arc::new(OfflineTestBackend),
            directory.path().to_owned(),
        )
        .unwrap();
        let epoch = controller.channels.epoch.clone();
        for number in 0..1030 {
            controller.publish_event(json!({"method":"test/event","params":{"number":number}}));
        }
        let replay = channels::replay(
            &controller,
            &format!("/api/channel-events?epoch={epoch}&after=1025")
                .parse()
                .unwrap(),
        );
        assert_eq!(replay["reset"], false);
        let entries = replay["events"].as_array().unwrap();
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0]["channelSequence"], 1026);
        assert_eq!(entries[4]["channelSequence"], 1030);
        assert_eq!(entries[4]["channelEpoch"], epoch);
        for query in [
            format!("epoch={epoch}&after=0"),
            "epoch=retired&after=1025".into(),
            format!("epoch={epoch}&after=9999999999999999999"),
        ] {
            let replay = channels::replay(
                &controller,
                &format!("/api/channel-events?{query}").parse().unwrap(),
            );
            assert_eq!(replay["reset"], true);
            assert_eq!(replay["cursor"], 1030);
        }
    }

    #[tokio::test]
    async fn channel_ticket_cors_websockets_and_revocation_share_one_device() {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let (_directory, state) = login_lock_test_state();
        let response = login_lock_attempt(&state, LOGIN_TEST_KEY, "192.0.2.1:1000")
            .await
            .unwrap();
        let cookie = login_cookie(&response);
        let controller = state.controller.clone();
        let primary = "https://channel-test.shares.zrok.io";
        let secondary = "https://channel-test.ngrok-free.app";
        controller.configure_secondary_channel(true);
        *controller.public_origin.write().unwrap() = Some(primary.into());
        *controller.channels.secondary.write().unwrap() = Some(secondary.into());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = web_router(
            controller.clone(),
            RemoteAccessSettings {
                enabled: true,
                port: address.port(),
                ..Default::default()
            },
        );
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let discovery: Value = client
            .get(format!("http://{address}/api/channels"))
            .header("Host", "channel-test.shares.zrok.io")
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(discovery["ok"], true);
        assert_eq!(discovery["origins"], json!([primary, secondary]));
        assert_eq!(discovery["requestRoutingVersion"], 2);
        let token = discovery["token"].as_str().unwrap();
        let media_token = discovery["mediaToken"].as_str().unwrap();
        assert_ne!(token, media_token);
        let second = client
            .get(format!("http://{address}/api/session"))
            .header("Host", "channel-test.ngrok-free.app")
            .header("Origin", primary)
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(second.status(), reqwest::StatusCode::OK);
        assert_eq!(second.headers()["access-control-allow-origin"], primary);
        assert_eq!(
            second.headers()["x-codex-channel-server"],
            controller.channels.epoch
        );
        assert_eq!(
            second.json::<Value>().await.unwrap()["device"]["id"],
            discovery["deviceId"]
        );
        let preflight = client
            .request(
                reqwest::Method::OPTIONS,
                format!("http://{address}/api/read/history"),
            )
            .header("Host", "channel-test.ngrok-free.app")
            .header("Origin", primary)
            .header("access-control-request-method", "POST")
            .header(
                "access-control-request-headers",
                "authorization,content-type,ngrok-skip-browser-warning",
            )
            .send()
            .await
            .unwrap();
        assert_eq!(preflight.status(), reqwest::StatusCode::NO_CONTENT);
        for path in [
            "/api/action/optimizePrompt",
            "/api/action/cancelPromptOptimize",
            "/api/action/answer",
            "/api/action/approve",
            "/api/action/stop",
            "/api/action/createSideThread",
            "/api/action/deleteThread",
            "/api/read/promptOptimization",
            "/api/read/models",
            "/api/workspace/context",
            "/api/workspace/changes",
            "/api/workspace/change",
            "/api/workspace/files",
            "/api/workspace/file",
            "/api/logout",
        ] {
            let preflight = client
                .request(reqwest::Method::OPTIONS, format!("http://{address}{path}"))
                .header("Host", "channel-test.ngrok-free.app")
                .header("Origin", primary)
                .header("access-control-request-method", "POST")
                .header(
                    "access-control-request-headers",
                    "authorization,content-type,idempotency-key",
                )
                .send()
                .await
                .unwrap();
            assert_eq!(
                preflight.status(),
                reqwest::StatusCode::NO_CONTENT,
                "{path}"
            );
        }
        for (path, origin) in [
            ("/api/connect", primary),
            ("/api/read/history", "https://attacker.example"),
        ] {
            let rejected = client
                .post(format!("http://{address}{path}"))
                .header("Host", "channel-test.ngrok-free.app")
                .header("Origin", origin)
                .bearer_auth(token)
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            assert_eq!(rejected.status(), reqwest::StatusCode::FORBIDDEN);
        }
        let invalid_optimization = client
            .post(format!("http://{address}/api/action/optimizePrompt"))
            .header("Host", "channel-test.ngrok-free.app")
            .header("Origin", primary)
            .bearer_auth(token)
            .header("Idempotency-Key", "routing-optimization")
            .json(&json!({"unexpected": true}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            invalid_optimization.status(),
            reqwest::StatusCode::BAD_REQUEST
        );
        let image_path = _directory.path().join("channel.png");
        image::RgbaImage::new(4, 4).save(&image_path).unwrap();
        let image_id = controller.images.write().unwrap().register(image_path);
        let image_url =
            format!("http://{address}/api/images/{image_id}?channel_token={media_token}");
        let image = client
            .get(&image_url)
            .header("Host", "channel-test.ngrok-free.app")
            .header("sec-fetch-site", "cross-site")
            .send()
            .await
            .unwrap();
        assert_eq!(image.status(), reqwest::StatusCode::OK);
        assert_eq!(image.headers()["content-type"], "image/png");
        for scheme in ["Bearer", "Media"] {
            let denied_media_api = client
                .get(format!("http://{address}/api/session"))
                .header("Host", "channel-test.ngrok-free.app")
                .header("Origin", primary)
                .header("Authorization", format!("{scheme} {media_token}"))
                .send()
                .await
                .unwrap();
            assert!(matches!(denied_media_api.status().as_u16(), 401 | 403));
        }
        let api_token_media_link = client
            .get(format!(
                "http://{address}/api/images/{image_id}?channel_token={token}"
            ))
            .header("Host", "channel-test.ngrok-free.app")
            .send()
            .await
            .unwrap();
        assert_eq!(
            api_token_media_link.status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        let token_on_api = client
            .get(format!(
                "http://{address}/api/session?channel_token={token}"
            ))
            .header("Host", "channel-test.ngrok-free.app")
            .send()
            .await
            .unwrap();
        assert_eq!(token_on_api.status(), reqwest::StatusCode::UNAUTHORIZED);
        let duplicate_token = client
            .get(format!("{image_url}&channel_token={token}"))
            .header("Host", "channel-test.ngrok-free.app")
            .send()
            .await
            .unwrap();
        assert_eq!(duplicate_token.status(), reqwest::StatusCode::UNAUTHORIZED);
        let mut first_request = format!("ws://{address}/api/events/ws")
            .into_client_request()
            .unwrap();
        first_request
            .headers_mut()
            .insert("host", "channel-test.shares.zrok.io".parse().unwrap());
        first_request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (mut first, _) = tokio_tungstenite::connect_async(first_request)
            .await
            .unwrap();
        let ready: Value =
            serde_json::from_str(first.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(ready["method"], "remote/streamReady");
        let initial_cursor = ready["cursor"].as_u64().unwrap();
        let mut second_request = format!(
            "ws://{address}/api/events/ws?epoch={}&after={}",
            ready["channelEpoch"].as_str().unwrap(),
            ready["cursor"]
        )
        .into_client_request()
        .unwrap();
        second_request
            .headers_mut()
            .insert("host", "channel-test.ngrok-free.app".parse().unwrap());
        second_request
            .headers_mut()
            .insert("origin", primary.parse().unwrap());
        second_request.headers_mut().insert(
            "sec-websocket-protocol",
            format!("codex-elves-channel-v1, codex-channel.{token}")
                .parse()
                .unwrap(),
        );
        let (mut second, _) = tokio_tungstenite::connect_async(second_request)
            .await
            .unwrap();
        second.next().await.unwrap().unwrap();
        for sequence in 1..=3 {
            controller.publish_event(json!({"method":"test/event","value":sequence}));
            let one: Value =
                serde_json::from_str(first.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            let two: Value =
                serde_json::from_str(second.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            assert_eq!(one, two);
            assert_eq!(one["channelSequence"], initial_cursor + sequence);
        }
        controller.configure_secondary_channel(false);
        let changed: Value =
            serde_json::from_str(first.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(changed["method"], "remote/channelsChanged");
        assert_eq!(changed["params"]["secondaryEnabled"], false);
        let closed_discovery: Value = client
            .get(format!("http://{address}/api/channels"))
            .header("Host", "channel-test.shares.zrok.io")
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(closed_discovery["origins"], json!([primary]));
        assert_eq!(closed_discovery["secondaryEnabled"], false);
        assert!(
            closed_discovery["revision"].as_u64().unwrap()
                > discovery["revision"].as_u64().unwrap()
        );
        controller.auth.lock().await.devices.clear();
        let revoked = client
            .get(format!("http://{address}/api/session"))
            .header("Host", "channel-test.ngrok-free.app")
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(revoked.status(), reqwest::StatusCode::UNAUTHORIZED);
        let revoked_image = client
            .get(&image_url)
            .header("Host", "channel-test.ngrok-free.app")
            .send()
            .await
            .unwrap();
        assert_eq!(revoked_image.status(), reqwest::StatusCode::UNAUTHORIZED);
        first.close(None).await.unwrap();
        second.close(None).await.unwrap();
        server.abort();
    }

    #[tokio::test]
    #[ignore = "仅在明确授权 ngrok 真实网络验证后手工运行"]
    async fn channel_ngrok_live_token_validation() {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        assert_eq!(
            std::env::var("CODEX_ELVES_NGROK_LIVE_TEST").as_deref(),
            Ok("1")
        );
        let config: Value = serde_json::from_slice(
            &std::fs::read(crate::paths::default_app_state_dir().join("remote-access-ngrok.json"))
                .unwrap(),
        )
        .unwrap();
        let (_directory, state) = login_lock_test_state();
        let controller = state.controller.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = web_router(
            controller.clone(),
            RemoteAccessSettings {
                enabled: true,
                port: address.port(),
                ..Default::default()
            },
        );
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let result = controller
            .live
            .lock()
            .await
            .ngrok
            .save_and_test(
                config["agent"]["authtoken"].as_str().unwrap(),
                &format!("http://{address}"),
            )
            .await;
        let verified = async {
            anyhow::ensure!(result?["status"] == "ok", "ngrok HTTP 验证失败");
            let login = login_lock_attempt(&state, LOGIN_TEST_KEY, "192.0.2.1:1000")
                .await
                .map_err(|_| anyhow::anyhow!("隔离设备登录失败"))?;
            let mut headers = HeaderMap::new();
            headers.insert(header::COOKIE, login_cookie(&login).parse()?);
            let discovery = channels::discover(State(state.clone()), headers)
                .await
                .map_err(|_| anyhow::anyhow!("双通道发现失败"))?
                .0;
            let origin = controller.channels.origins().remove(0);
            let mut request = format!("{}/api/events/ws", origin.replacen("https:", "wss:", 1))
                .into_client_request()?;
            request.headers_mut().insert("origin", origin.parse()?);
            request.headers_mut().insert(
                "sec-websocket-protocol",
                format!(
                    "codex-elves-channel-v1, codex-channel.{}",
                    discovery["token"].as_str().unwrap()
                )
                .parse()?,
            );
            let (mut socket, _) = tokio::time::timeout(
                Duration::from_secs(20),
                tokio_tungstenite::connect_async(request),
            )
            .await??;
            let ready = tokio::time::timeout(Duration::from_secs(20), socket.next())
                .await?
                .context("公网 WebSocket 已关闭")??;
            let ready: Value = serde_json::from_str(ready.to_text()?)?;
            anyhow::ensure!(
                ready["method"] == "remote/streamReady",
                "公网 WebSocket 未返回事件基线"
            );
            controller.publish_event(json!({"method":"test/dualChannel"}));
            let event = tokio::time::timeout(Duration::from_secs(20), socket.next())
                .await?
                .context("公网事件连接已关闭")??;
            let event: Value = serde_json::from_str(event.to_text()?)?;
            anyhow::ensure!(
                event["method"] == "test/dualChannel" && event["channelSequence"] == 1,
                "公网事件未按序送达"
            );
            socket.close(None).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        controller.live.lock().await.ngrok.stop().await;
        server.abort();
        verified.unwrap();
    }

    fn login_lock_test_state() -> (tempfile::TempDir, WebState) {
        let temp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../temp");
        std::fs::create_dir_all(&temp_root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("remote-login-lock-")
            .tempdir_in(temp_root)
            .unwrap();
        configure_access_key(directory.path(), LOGIN_TEST_KEY).unwrap();
        let state = reopened_login_lock_state(directory.path());
        (directory, state)
    }

    fn reopened_login_lock_state(directory: &std::path::Path) -> WebState {
        let controller = HarnessController::with_test_backend(
            Arc::new(OfflineTestBackend),
            directory.to_path_buf(),
        )
        .unwrap();
        controller.publish_access_policy(&RemoteAccessSettings {
            enabled: true,
            ..RemoteAccessSettings::default()
        });
        controller.running.store(true, Ordering::SeqCst);
        WebState {
            desktop: false,
            controller,
            settings: RemoteAccessSettings::default(),
        }
    }

    async fn login_lock_attempt(
        state: &WebState,
        candidate: &str,
        peer: &str,
    ) -> Result<Response, ApiError> {
        connect(
            State(state.clone()),
            ConnectInfo(peer.parse().unwrap()),
            Extension(RequestTrustContext::default()),
            HeaderMap::new(),
            Json(json!({"key":candidate})),
        )
        .await
    }

    fn login_failure_count(directory: &std::path::Path) -> u64 {
        let record: Value = serde_json::from_slice(
            &std::fs::read(directory.join("remote-access-key.json")).unwrap(),
        )
        .unwrap();
        record["login_failures"].as_u64().unwrap()
    }

    fn login_cookie(response: &Response) -> String {
        response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn claude_recap_settings_reject_unauthorized_read_only_and_invalid_writes() {
        let (_directory, mut state) = login_lock_test_state();
        let payload = json!({"instanceId":"claude:fixture","operation":"save","enabled":false});
        assert_eq!(
            permissions::claude_recap_settings(
                State(state.clone()),
                HeaderMap::new(),
                Json(payload.clone()),
            )
            .await
            .unwrap_err()
            .code,
            401
        );
        let response = login_lock_attempt(&state, LOGIN_TEST_KEY, "192.0.2.1:1000")
            .await
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&login_cookie(&response)).unwrap(),
        );
        state.settings.read_only = true;
        assert_eq!(
            permissions::claude_recap_settings(
                State(state.clone()),
                headers.clone(),
                Json(payload.clone()),
            )
            .await
            .unwrap_err()
            .code,
            403
        );
        state.settings.read_only = false;
        for invalid in [
            json!({"instanceId":"claude:fixture","operation":"save","enabled":"false"}),
            json!({"instanceId":"claude:fixture","operation":"delete"}),
            json!({"operation":"save","enabled":false}),
        ] {
            assert_eq!(
                permissions::claude_recap_settings(
                    State(state.clone()),
                    headers.clone(),
                    Json(invalid),
                )
                .await
                .unwrap_err()
                .code,
                400
            );
        }
        state.controller.auth.lock().await.devices.clear();
        assert_eq!(
            permissions::claude_recap_settings(State(state), headers, Json(payload))
                .await
                .unwrap_err()
                .code,
            401
        );
    }

    #[tokio::test]
    async fn claude_recap_detected_defaults_preserve_choices_and_require_write_authorization() {
        let (directory, state) = login_lock_test_state();
        state
            .controller
            .publish_access_policy(&RemoteAccessSettings {
                enabled: true,
                read_only: false,
                ..RemoteAccessSettings::default()
            });
        let response = login_lock_attempt(&state, LOGIN_TEST_KEY, "192.0.2.1:1000")
            .await
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&login_cookie(&response)).unwrap(),
        );
        let device = authorized(&state, &headers).await.unwrap();
        let authorization = state
            .controller
            .authorization(device, Some("initializeClaudeRecap"));
        let fixture_root = std::fs::canonicalize(directory.path()).unwrap();
        let environment = |index| {
            let path = fixture_root.join(format!("claude-{index}"));
            let path_text = path.to_string_lossy().replace('\\', "/");
            let path_text = path_text.strip_prefix("//?/").unwrap_or(&path_text);
            let home = if cfg!(windows) {
                format!("/mnt/{}/{}", path_text[..1].to_lowercase(), &path_text[3..])
            } else {
                path_text.to_owned()
            };
            serde_json::from_value::<preferences::Environment>(json!({
                "id":format!("fixture-{index}"), "label":"fixture", "distribution":"fixture",
                "user":format!("fixture-{index}"), "home":home, "claudeExecutable":"/fixture/claude",
                "executables":{"claude":"/fixture/claude"}, "configured":false, "error":null,
                "codexHome":"", "claudeHome":home
            }))
            .unwrap()
        };
        let environments: Vec<_> = (0..4).map(environment).collect();
        let settings = |index| {
            directory
                .path()
                .join(format!("claude-{index}/settings.json"))
        };
        for (index, text) in [
            (0, r#"{"model":"preserved"}"#),
            (1, r#"{"awaySummaryEnabled":true,"model":"preserved"}"#),
            (2, r#"{"awaySummaryEnabled":false}"#),
            (3, "{broken"),
        ] {
            std::fs::create_dir_all(settings(index).parent().unwrap()).unwrap();
            std::fs::write(settings(index), text).unwrap();
        }
        let original_true = std::fs::read(settings(1)).unwrap();
        let results =
            permissions::initialize_claude_recaps(&environments, &authorization, false).await;
        assert_eq!(results.len(), 4);
        assert_eq!(results[0]["initialized"], true, "{results:?}");
        assert_eq!(results[1]["initialized"], false);
        assert_eq!(results[2]["initialized"], false);
        assert_eq!(results[3]["code"], 422);
        let saved: Value = serde_json::from_slice(&std::fs::read(settings(0)).unwrap()).unwrap();
        assert_eq!(
            saved,
            json!({"model":"preserved","awaySummaryEnabled":false})
        );
        assert_eq!(std::fs::read(settings(1)).unwrap(), original_true);
        assert_eq!(std::fs::read_to_string(settings(3)).unwrap(), "{broken");
        // 用户手动开启后再检测，默认初始化不得覆盖，且已有显式配置不能产生无意义重写。
        std::fs::write(settings(0), r#"{"awaySummaryEnabled":true}"#).unwrap();
        let repeated =
            permissions::initialize_claude_recaps(&environments[..3], &authorization, false).await;
        assert!(
            repeated
                .iter()
                .all(|value| value["initialized"] == false && value.get("error").is_none())
        );
        assert_eq!(
            std::fs::read_to_string(settings(0)).unwrap(),
            r#"{"awaySummaryEnabled":true}"#
        );
        let missing = [environment(4)];
        let read_only = permissions::initialize_claude_recaps(&missing, &authorization, true).await;
        assert_eq!(read_only[0]["code"], 403);
        assert!(!settings(4).exists());
        state
            .controller
            .publish_access_policy(&RemoteAccessSettings {
                enabled: true,
                read_only: true,
                ..RemoteAccessSettings::default()
            });
        let denied = permissions::initialize_claude_recaps(&missing, &authorization, false).await;
        assert_eq!(denied[0]["code"], 403);
        assert!(!settings(4).exists());
        state.controller.auth.lock().await.devices.clear();
        let revoked = permissions::initialize_claude_recaps(&missing, &authorization, false).await;
        assert_eq!(revoked[0]["code"], 401);
        assert!(!settings(4).exists());
    }

    #[tokio::test]
    async fn claude_recap_http_detection_toggle_conflict_and_revocation_roundtrip() {
        let (directory, state) = login_lock_test_state();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let config_root = root.join("isolated-claude");
        std::fs::create_dir_all(&config_root).unwrap();
        let text = config_root.to_string_lossy().replace('\\', "/");
        let text = text.strip_prefix("//?/").unwrap_or(&text);
        let home = if cfg!(windows) {
            format!("/mnt/{}/{}", text[..1].to_lowercase(), &text[3..])
        } else {
            text.to_owned()
        };
        let config_path = config_root.join("settings.json");
        std::fs::write(
            &config_path,
            r#"{"model":"gpt-6.1-sol","env":{"FIXTURE_SECRET":"must-not-be-returned"}}"#,
        )
        .unwrap();
        let environment: preferences::Environment = serde_json::from_value(json!({
            "id":"fixture", "label":"fixture", "distribution":"fixture", "user":"fixture",
            "home":home, "claudeExecutable":"/fixture/claude",
            "executables":{"claude":"/fixture/claude"}, "configured":true, "error":null,
            "codexHome":"", "claudeHome":home
        }))
        .unwrap();
        let instance = instances::id("claude", "fixture", "fixture");
        preferences::fixture_environments(directory.path(), vec![environment]).await;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let settings = RemoteAccessSettings {
            enabled: true,
            read_only: false,
            port: listener.local_addr().unwrap().port(),
            ..RemoteAccessSettings::default()
        };
        state.controller.publish_access_policy(&settings);
        let router = web_router(state.controller.clone(), settings.clone());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let base = format!("http://127.0.0.1:{}", settings.port);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let request = |path: &str, payload: Value, cookie: &str| {
            client
                .post(format!("{base}{path}"))
                .header("Origin", &base)
                .header("Cookie", cookie)
                .json(&payload)
        };
        let endpoint = "/api/settings/claude-recap";
        let original = std::fs::read(&config_path).unwrap();
        assert_eq!(
            request("/api/settings/environments", json!({}), "")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(std::fs::read(&config_path).unwrap(), original);
        let connected = request("/api/connect", json!({"key":LOGIN_TEST_KEY}), "")
            .send()
            .await
            .unwrap();
        assert_eq!(connected.status(), StatusCode::OK);
        let cookie = connected.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        // 同时识别同一个未配置实例时只允许一次初始化，不能创建重复写入或覆盖其他字段。
        let first = request("/api/settings/environments", json!({"force":true}), &cookie).send();
        let second = request("/api/settings/environments", json!({"force":true}), &cookie).send();
        let (first, second) = tokio::join!(first, second);
        let first = first.unwrap().json::<Value>().await.unwrap();
        let second = second.unwrap().json::<Value>().await.unwrap();
        assert_ne!(
            first["claudeRecapDefaults"][0]["initialized"],
            second["claudeRecapDefaults"][0]["initialized"]
        );
        let read = request(endpoint, json!({"instanceId":instance}), &cookie)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(read["enabled"], false);
        assert_eq!(read["explicit"], true);
        assert!(!read.to_string().contains("must-not-be-returned"));
        let saved = request(endpoint, json!({"instanceId":instance,"operation":"save","enabled":true,"expectedRevision":read["revision"]}), &cookie)
            .send().await.unwrap().json::<Value>().await.unwrap();
        assert_eq!(saved["enabled"], true);
        let chosen = std::fs::read(&config_path).unwrap();
        let detected = request("/api/settings/environments", json!({"force":true}), &cookie)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(detected["claudeRecapDefaults"][0]["initialized"], false);
        assert_eq!(std::fs::read(&config_path).unwrap(), chosen);
        assert_eq!(request(endpoint, json!({"instanceId":instance,"operation":"save","enabled":false,"expectedRevision":read["revision"]}), &cookie)
            .send().await.unwrap().status(), StatusCode::CONFLICT);
        assert_eq!(std::fs::read(&config_path).unwrap(), chosen);
        assert_eq!(
            request(
                endpoint,
                json!({"instanceId":"claude:wsl:unknown"}),
                &cookie
            )
            .send()
            .await
            .unwrap()
            .status(),
            StatusCode::NOT_FOUND
        );
        std::fs::write(&config_path, "{broken").unwrap();
        let invalid = request("/api/settings/environments", json!({}), &cookie)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(invalid["claudeRecapDefaults"][0]["code"], 422);
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), "{broken");
        std::fs::write(&config_path, r#"{"model":"gpt-6.1-sol"}"#).unwrap();
        state
            .controller
            .publish_access_policy(&RemoteAccessSettings {
                read_only: true,
                ..settings
            });
        let denied = request("/api/settings/environments", json!({}), &cookie)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(denied["claudeRecapDefaults"][0]["code"], 403);
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            r#"{"model":"gpt-6.1-sol"}"#
        );
        state.controller.auth.lock().await.devices.clear();
        assert_eq!(
            request(endpoint, json!({"instanceId":instance}), &cookie)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        server.abort();
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "需要指定测试实例中事先创建的隔离配置；不访问真实用户配置"]
    async fn claude_recap_native_wsl_file_preserves_linux_permissions() {
        let root = std::env::var("CODEX_ELVES_RECAP_TEST_ROOT")
            .expect("请设置 CODEX_ELVES_RECAP_TEST_ROOT 指向测试实例的隔离配置目录");
        assert!(root.starts_with("/home/elves-test/workspaces/temp/codex-elves-recap-"));
        assert!(!root.split('/').any(|part| part == ".."));
        let inspect = || {
            let output = std::process::Command::new("wsl.exe")
                .args([
                    "-d", "codex-elves-claude-test", "-u", "elves-test", "--", "python3", "-c",
                    "import json,sys;from pathlib import Path;p=Path(sys.argv[1]);s=p.stat();print(json.dumps({'mode':s.st_mode&0o777,'uid':s.st_uid,'gid':s.st_gid}))",
                    &format!("{root}/settings.json"),
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            serde_json::from_slice::<Value>(&output.stdout).unwrap()
        };
        let before = inspect();
        assert_eq!(
            before["mode"], 0o600,
            "原生夹具必须从仅本人可读写的权限开始"
        );
        let (directory, mut state) = login_lock_test_state();
        state.settings.read_only = false;
        state
            .controller
            .publish_access_policy(&RemoteAccessSettings {
                enabled: true,
                read_only: false,
                ..RemoteAccessSettings::default()
            });
        let environment: preferences::Environment = serde_json::from_value(json!({
            "id":"native-recap-test", "label":"test", "distribution":"codex-elves-claude-test", "user":"elves-test",
            "home":"/home/elves-test", "claudeExecutable":"/home/elves-test/.local/bin/claude",
            "executables":{"claude":"/home/elves-test/.local/bin/claude"}, "configured":true, "error":null,
            "codexHome":"", "claudeHome":root
        })).unwrap();
        preferences::fixture_environments(directory.path(), vec![environment]).await;
        let instance = instances::id("claude", "codex-elves-claude-test", "elves-test");
        let response = login_lock_attempt(&state, LOGIN_TEST_KEY, "192.0.2.1:1000")
            .await
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&login_cookie(&response)).unwrap(),
        );
        let detected = preferences::environments(
            State(state.clone()),
            headers.clone(),
            Json(json!({"force":true})),
        )
        .await
        .unwrap();
        let detected: Value = serde_json::from_slice(
            &axum::body::to_bytes(detected.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(
            detected["claudeRecapDefaults"][0].get("error").is_none(),
            "{detected}"
        );
        assert_eq!(inspect(), before);
        for enabled in [true, false] {
            let read = permissions::claude_recap_settings(
                State(state.clone()),
                headers.clone(),
                Json(json!({"instanceId":instance})),
            )
            .await
            .unwrap();
            let read: Value = serde_json::from_slice(
                &axum::body::to_bytes(read.into_body(), 16 * 1024)
                    .await
                    .unwrap(),
            )
            .unwrap();
            let saved = permissions::claude_recap_settings(State(state.clone()), headers.clone(), Json(json!({
                "instanceId":instance,"operation":"save","enabled":enabled,"expectedRevision":read["revision"]
            }))).await.unwrap();
            let saved: Value = serde_json::from_slice(
                &axum::body::to_bytes(saved.into_body(), 16 * 1024)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(saved["enabled"], enabled);
            assert_eq!(inspect(), before, "原生 Linux 文件的权限和所有者必须保留");
        }
    }

    #[tokio::test]
    async fn login_client_identity_reuses_cookie_and_never_revives_revoked_device() {
        async fn attempt(state: &WebState, client: Option<&str>, cookie: &str) -> (String, String) {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::USER_AGENT,
                HeaderValue::from_static("Windows identical-UA"),
            );
            if !cookie.is_empty() {
                headers.insert(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
            }
            let response = connect(
                State(state.clone()),
                ConnectInfo("192.0.2.1:1000".parse().unwrap()),
                Extension(RequestTrustContext::default()),
                headers,
                Json(json!({"key":LOGIN_TEST_KEY,"clientId":client})),
            )
            .await
            .unwrap();
            let cookie = login_cookie(&response);
            let mut headers = HeaderMap::new();
            headers.insert(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
            let device = state.controller.authorize(&headers).await.unwrap();
            (device.id, cookie)
        }
        let (_directory, state) = login_lock_test_state();
        let client = uuid::Uuid::new_v4().to_string();
        let (id, legacy_cookie) = attempt(&state, None, "").await;
        let (migrated, cookie) = attempt(&state, Some(&client), &legacy_cookie).await;
        assert_eq!(migrated, id);
        assert_eq!(cookie, legacy_cookie, "有效 Cookie 迁移不使其他标签页失效");
        let (reused, cookie) = attempt(&state, Some(&client), "").await;
        assert_eq!(reused, id);
        assert_eq!(state.controller.auth.lock().await.devices.len(), 1);
        let (other, _) = attempt(&state, Some(&uuid::Uuid::new_v4().to_string()), "").await;
        assert_ne!(other, id, "相同 UA 和 IP 的不同客户端不能合并");
        state
            .controller
            .admin("revoke", json!({"deviceId":id}))
            .await
            .unwrap();
        let (renewed, _) = attempt(&state, Some(&client), "").await;
        assert_ne!(renewed, id);
        let mut old = HeaderMap::new();
        old.insert(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
        assert!(state.controller.authorize(&old).await.is_none());
        {
            let mut auth = state.controller.auth.lock().await;
            let template = auth.devices[0].clone();
            while auth.devices.len() < MAX_DEVICES {
                let mut filler = template.clone();
                filler.id = uuid::Uuid::new_v4().to_string();
                filler.token_hash = hash(&filler.id);
                filler.client_id_hash.clear();
                auth.devices.push(filler);
            }
        }
        assert_eq!(
            attempt(&state, Some(&client), "").await.0,
            renewed,
            "满额时仍允许同端复用"
        );
        assert_eq!(
            state.controller.auth.lock().await.devices.len(),
            MAX_DEVICES
        );
        let headers = HeaderMap::new();
        let failure = connect(
            State(state.clone()),
            ConnectInfo("192.0.2.1:1000".parse().unwrap()),
            Extension(RequestTrustContext::default()),
            headers,
            Json(json!({"key":LOGIN_TEST_KEY,"clientId":uuid::Uuid::new_v4().to_string()})),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.code, 409);
        assert_eq!(device_name(&HeaderMap::new(), true), "Android App");
    }

    #[tokio::test]
    async fn login_lock_is_global_at_four_failures_and_skips_non_key_errors() {
        let (directory, state) = login_lock_test_state();
        assert_eq!(
            login_lock_attempt(&state, "short", "192.0.2.1:1000")
                .await
                .unwrap_err()
                .code,
            400
        );
        state.controller.running.store(false, Ordering::SeqCst);
        assert_eq!(
            login_lock_attempt(&state, LOGIN_TEST_KEY, "192.0.2.1:1000")
                .await
                .unwrap_err()
                .code,
            503
        );
        state.controller.running.store(true, Ordering::SeqCst);
        let busy = state
            .controller
            .key_checks
            .clone()
            .acquire_many_owned(4)
            .await
            .unwrap();
        assert_eq!(
            login_lock_attempt(&state, "wrong-login-key", "192.0.2.1:1000")
                .await
                .unwrap_err()
                .code,
            429
        );
        drop(busy);
        assert_eq!(login_failure_count(directory.path()), 0);
        for (index, peer) in [
            "192.0.2.1:1000",
            "192.0.2.2:2000",
            "[2001:db8::1]:3000",
            "198.51.100.1:4000",
        ]
        .into_iter()
        .enumerate()
        {
            let error = login_lock_attempt(&state, "wrong-login-key", peer)
                .await
                .unwrap_err();
            assert_eq!(error.code, if index == 3 { 423 } else { 401 });
            assert!(
                error
                    .into_response()
                    .headers()
                    .get(header::SET_COOKIE)
                    .is_none()
            );
            assert_eq!(login_failure_count(directory.path()), index as u64 + 1);
            assert_eq!(
                login_status(State(state.clone())).await.unwrap().0["loginLocked"],
                index == 3
            );
        }
        assert_eq!(
            login_lock_attempt(&state, LOGIN_TEST_KEY, "203.0.113.5:5000")
                .await
                .unwrap_err()
                .code,
            423
        );
        assert_eq!(login_failure_count(directory.path()), 4);
        assert!(state.controller.auth.lock().await.devices.is_empty());
    }

    #[tokio::test]
    async fn login_lock_success_resets_counter_and_preserves_existing_device() {
        let (directory, state) = login_lock_test_state();
        let mut authorization_events = state.controller.events.subscribe();
        for _ in 0..3 {
            assert_eq!(
                login_lock_attempt(&state, "wrong-login-key", "192.0.2.1:1000")
                    .await
                    .unwrap_err()
                    .code,
                401
            );
        }
        let response = login_lock_attempt(&state, LOGIN_TEST_KEY, "192.0.2.1:1000")
            .await
            .unwrap();
        let cookie = login_cookie(&response);
        assert_eq!(login_failure_count(directory.path()), 0);
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
        for index in 0..4 {
            let error = login_lock_attempt(&state, "wrong-login-key", "192.0.2.2:2000")
                .await
                .unwrap_err();
            assert_eq!(error.code, if index == 3 { 423 } else { 401 });
            assert!(state.controller.authorize(&headers).await.is_some());
        }
        assert!(authorization_events.try_recv().is_err());
        let status = state.controller.admin_status(&state.settings).await;
        assert_eq!(status["loginLocked"], true);
        assert_eq!(status["devices"].as_array().unwrap().len(), 1);

        configure_access_key(directory.path(), "rotated-login-key").unwrap();
        assert!(state.controller.authorize(&headers).await.is_none());
        assert_eq!(
            login_lock_attempt(&state, LOGIN_TEST_KEY, "192.0.2.1:1000")
                .await
                .unwrap_err()
                .code,
            401
        );
        assert!(
            login_lock_attempt(&state, "rotated-login-key", "192.0.2.1:1000")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn login_lock_survives_restart_settings_reads_and_saving_same_key() {
        let (directory, state) = login_lock_test_state();
        for _ in 0..3 {
            let _ = login_lock_attempt(&state, "wrong-login-key", "192.0.2.1:1000").await;
        }
        let restarted = reopened_login_lock_state(directory.path());
        assert_eq!(login_failure_count(directory.path()), 3);
        assert_eq!(
            login_lock_attempt(&restarted, "wrong-login-key", "192.0.2.2:2000")
                .await
                .unwrap_err()
                .code,
            423
        );
        let key_id = restarted
            .controller
            .auth
            .lock()
            .await
            .access_key
            .as_ref()
            .unwrap()
            .id
            .clone();
        let settings_store = SettingsStore::new(directory.path().join("settings.json"));
        let mut settings = settings_store.load().unwrap();
        settings.remote_access.read_only = true;
        settings_store.save(&settings).unwrap();
        restarted.controller.reconcile(settings.remote_access).await;
        restarted
            .controller
            .publish_access_policy(&RemoteAccessSettings {
                enabled: true,
                read_only: true,
                ..RemoteAccessSettings::default()
            });
        restarted.controller.running.store(true, Ordering::SeqCst);
        let _unsaved_candidate = random_token();
        configure_access_key(directory.path(), LOGIN_TEST_KEY).unwrap();
        for _ in 0..2 {
            let status = access_credentials_status(directory.path()).unwrap();
            assert_eq!(status["loginLocked"], true);
            assert_eq!(
                login_status(State(restarted.clone())).await.unwrap().0["loginLocked"],
                true
            );
        }
        let restarted_again = reopened_login_lock_state(directory.path());
        assert_eq!(
            restarted_again
                .controller
                .auth
                .lock()
                .await
                .access_key
                .as_ref()
                .unwrap()
                .id,
            key_id
        );
        assert_eq!(login_failure_count(directory.path()), 4);
        assert_eq!(
            login_lock_attempt(&restarted_again, LOGIN_TEST_KEY, "203.0.113.1:3000")
                .await
                .unwrap_err()
                .code,
            423
        );
        configure_access_key(directory.path(), "new-isolated-key").unwrap();
        assert_eq!(
            login_status(State(restarted_again.clone()))
                .await
                .unwrap()
                .0["loginLocked"],
            false
        );
        assert_ne!(
            restarted_again
                .controller
                .auth
                .lock()
                .await
                .access_key
                .as_ref()
                .unwrap()
                .id,
            key_id
        );
        assert!(
            login_lock_attempt(&restarted_again, "new-isolated-key", "192.0.2.1:1000")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn login_lock_write_failure_is_durable_and_does_not_revoke_devices() {
        let (directory, state) = login_lock_test_state();
        let response = login_lock_attempt(&state, LOGIN_TEST_KEY, "192.0.2.1:1000")
            .await
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&login_cookie(&response)).unwrap(),
        );
        for _ in 0..3 {
            let _ = login_lock_attempt(&state, "wrong-login-key", "192.0.2.1:1000").await;
        }
        let blocked_temp = directory.path().join("remote-access-key.json.tmp");
        std::fs::create_dir(&blocked_temp).unwrap();
        let error = login_lock_attempt(&state, "wrong-login-key", "192.0.2.1:1000")
            .await
            .unwrap_err();
        assert_eq!(error.code, 503);
        assert!(
            error
                .into_response()
                .headers()
                .get(header::SET_COOKIE)
                .is_none()
        );
        assert_eq!(login_failure_count(directory.path()), 3);
        assert!(state.controller.authorize(&headers).await.is_some());
        let restarted = reopened_login_lock_state(directory.path());
        assert_eq!(
            login_lock_attempt(&restarted, LOGIN_TEST_KEY, "192.0.2.2:2000")
                .await
                .unwrap_err()
                .code,
            423
        );
        assert!(restarted.controller.authorize(&headers).await.is_some());
        assert!(configure_access_key(directory.path(), "new-isolated-key").is_err());
        assert_eq!(
            access_credentials_status(directory.path()).unwrap()["loginLocked"],
            true
        );
        std::fs::remove_dir(blocked_temp).unwrap();
        configure_access_key(directory.path(), LOGIN_TEST_KEY).unwrap();
        assert_eq!(
            access_credentials_status(directory.path()).unwrap()["loginLocked"],
            true
        );
        configure_access_key(directory.path(), "new-isolated-key").unwrap();
        assert_eq!(
            login_status(State(restarted.clone())).await.unwrap().0["loginLocked"],
            false
        );
    }

    #[tokio::test]
    async fn login_lock_corrupt_counter_refuses_new_authorization_until_key_rotation() {
        let (directory, state) = login_lock_test_state();
        let response = login_lock_attempt(&state, LOGIN_TEST_KEY, "192.0.2.1:1000")
            .await
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&login_cookie(&response)).unwrap(),
        );
        let path = directory.path().join("remote-access-key.json");
        let mut stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        stored["login_failures"] = json!(5);
        crate::settings::atomic_write(&path, &serde_json::to_vec(&stored).unwrap()).unwrap();
        assert_eq!(
            login_lock_attempt(&state, LOGIN_TEST_KEY, "192.0.2.1:1000")
                .await
                .unwrap_err()
                .code,
            423
        );
        assert_eq!(
            login_status(State(state.clone())).await.unwrap().0["loginLocked"],
            true
        );
        let restarted = reopened_login_lock_state(directory.path());
        assert!(restarted.controller.authorize(&headers).await.is_some());
        configure_access_key(directory.path(), LOGIN_TEST_KEY).unwrap();
        assert_eq!(
            login_status(State(restarted)).await.unwrap().0["loginLocked"],
            true
        );
        configure_access_key(directory.path(), "new-isolated-key").unwrap();
        assert!(
            login_lock_attempt(&state, "new-isolated-key", "192.0.2.1:1000")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn login_lock_concurrent_failures_block_late_success_and_stale_rotation_results() {
        let (directory, state) = login_lock_test_state();
        let snapshot = state
            .controller
            .auth
            .lock()
            .await
            .access_key
            .clone()
            .unwrap();
        let verified = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let late_success = {
            let state = state.clone();
            let key = snapshot.clone();
            let verified = verified.clone();
            let release = release.clone();
            tokio::spawn(async move {
                let checked_key = key.clone();
                let valid = tokio::task::spawn_blocking(move || checked_key.verify(LOGIN_TEST_KEY))
                    .await
                    .unwrap();
                verified.notify_one();
                release.notified().await;
                complete_login(
                    state,
                    key,
                    valid,
                    HeaderMap::new(),
                    RequestTrustContext::default(),
                    LoginClient::default(),
                )
                .await
            })
        };
        verified.notified().await;
        let mut failures = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let state = state.clone();
            failures.spawn(async move {
                login_lock_attempt(&state, "wrong-login-key", "192.0.2.1:1000")
                    .await
                    .unwrap_err()
                    .code
            });
        }
        let mut codes = Vec::new();
        while let Some(result) = failures.join_next().await {
            codes.push(result.unwrap());
        }
        codes.sort_unstable();
        assert_eq!(codes, vec![401, 401, 401, 423]);
        release.notify_one();
        assert_eq!(late_success.await.unwrap().unwrap_err().code, 423);
        assert!(state.controller.auth.lock().await.devices.is_empty());
        configure_access_key(directory.path(), "new-isolated-key").unwrap();
        for valid in [false, true] {
            assert_eq!(
                complete_login(
                    state.clone(),
                    snapshot.clone(),
                    valid,
                    HeaderMap::new(),
                    RequestTrustContext::default(),
                    LoginClient::default()
                )
                .await
                .unwrap_err()
                .code,
                401
            );
            assert_eq!(login_failure_count(directory.path()), 0);
        }
        assert!(
            login_lock_attempt(&state, "new-isolated-key", "192.0.2.1:1000")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn login_lock_cancelled_request_still_records_completed_key_verification() {
        let (directory, state) = login_lock_test_state();
        let request = {
            let state = state.clone();
            tokio::spawn(async move {
                login_lock_attempt(&state, "wrong-login-key", "192.0.2.1:1000").await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while state.controller.key_checks.available_permits() == 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        request.abort();
        let _ = request.await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while login_failure_count(directory.path()) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(login_failure_count(directory.path()), 1);
    }

    #[tokio::test]
    async fn login_lock_public_routes_report_lock_without_secrets_or_new_cookie() {
        let (_directory, mut state) = login_lock_test_state();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        state.settings.port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{}", state.settings.port);
        let router = web_router(state.controller.clone(), state.settings);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = client
            .get(format!("{base_url}/api/login-status"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.headers()[reqwest::header::CACHE_CONTROL],
            "no-store"
        );
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({"ok":true,"loginLocked":false})
        );
        let authorized_response = client
            .post(format!("{base_url}/api/connect"))
            .header(reqwest::header::ORIGIN, &base_url)
            .json(&json!({"key":LOGIN_TEST_KEY}))
            .send()
            .await
            .unwrap();
        assert_eq!(authorized_response.status(), reqwest::StatusCode::OK);
        let existing_cookie = authorized_response.headers()[reqwest::header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        for index in 0..5 {
            let response = client
                .post(format!("{base_url}/api/connect"))
                .header(reqwest::header::ORIGIN, &base_url)
                .json(&json!({"key":if index == 4 { LOGIN_TEST_KEY } else { "wrong-login-key" }}))
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status().as_u16(),
                if index >= 3 { 423 } else { 401 }
            );
            assert!(
                response
                    .headers()
                    .get(reqwest::header::SET_COOKIE)
                    .is_none()
            );
        }
        let status: Value = client
            .get(format!("{base_url}/api/login-status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status, json!({"ok":true,"loginLocked":true}));
        assert_eq!(
            client
                .get(format!("{base_url}/api/session"))
                .header(reqwest::header::COOKIE, existing_cookie)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn ngrok_switch_preserves_web_listener_and_authorization() {
        let (directory, state) = login_lock_test_state();
        let controller = state.controller;
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut settings = RemoteAccessSettings {
            enabled: true,
            port,
            ..Default::default()
        };
        controller.reconcile(settings.clone()).await;
        let generation = controller.primary.generation.load(Ordering::SeqCst);
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = client
            .post(format!("{base}/api/connect"))
            .header(reqwest::header::ORIGIN, &base)
            .json(&json!({"key": LOGIN_TEST_KEY}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let cookie = response.headers()[reqwest::header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let mut channel_events = controller.events.subscribe();
        let mut revision = 0;
        for enabled in [true, false] {
            settings.ngrok_enabled = enabled;
            controller.reconcile(settings.clone()).await;
            let event = channel_events.try_recv().unwrap();
            assert_eq!(event["method"], "remote/channelsChanged");
            assert_eq!(event["params"]["secondaryEnabled"], enabled);
            assert_eq!(event["channelEpoch"], controller.channels.epoch);
            assert!(event["params"]["revision"].as_u64().unwrap() > revision);
            revision = event["params"]["revision"].as_u64().unwrap();
            assert_eq!(
                controller.primary.generation.load(Ordering::SeqCst),
                generation
            );
            assert_eq!(
                controller.live.lock().await.settings.as_ref(),
                Some(&settings)
            );
            let response = client
                .get(format!("{base}/api/session"))
                .header(reqwest::header::COOKIE, &cookie)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
        }
        controller.reconcile(RemoteAccessSettings::default()).await;
        drop(directory);
    }

    #[tokio::test]
    async fn missing_credentials_allow_listener_but_reject_session_access() {
        let state_dir = tempfile::tempdir().unwrap();
        let controller = HarnessController::with_test_backend(
            Arc::new(OfflineTestBackend),
            state_dir.path().to_path_buf(),
        )
        .unwrap();
        let port = {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            listener.local_addr().unwrap().port()
        };
        let settings = RemoteAccessSettings {
            enabled: true,
            port,
            ..RemoteAccessSettings::default()
        };

        controller.reconcile(settings.clone()).await;
        let status = controller.admin_status(&settings).await;
        let base_url = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();
        let root_status = client
            .get(format!("{base_url}/"))
            .send()
            .await
            .unwrap()
            .status();
        let connect_response = client
            .post(format!("{base_url}/api/connect"))
            .header(reqwest::header::ORIGIN, &base_url)
            .json(&json!({"key":"a".repeat(32)}))
            .send()
            .await
            .unwrap();
        let connect_status = connect_response.status();
        let connect_body: Value = connect_response.json().await.unwrap();
        let session_status = client
            .get(format!("{base_url}/api/session"))
            .send()
            .await
            .unwrap()
            .status();
        controller.reconcile(RemoteAccessSettings::default()).await;

        assert_eq!(status["running"], true);
        assert_eq!(status["accessKeyConfigured"], false);
        assert!(status["error"].is_null());
        assert_eq!(root_status, reqwest::StatusCode::OK);
        assert_eq!(connect_status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(connect_body["message"], "本机尚未设置访问密钥");
        assert_eq!(session_status, reqwest::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn short_keys_saved_after_startup_take_effect_without_restart() {
        let temp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../temp");
        let state_dir = tempfile::Builder::new()
            .prefix("remote-key-test-")
            .tempdir_in(temp_root)
            .unwrap();
        let controller = HarnessController::with_test_backend(
            Arc::new(OfflineTestBackend),
            state_dir.path().to_path_buf(),
        )
        .unwrap();
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let settings = RemoteAccessSettings {
            enabled: true,
            port,
            ..RemoteAccessSettings::default()
        };
        controller.reconcile(settings).await;
        let base_url = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let first_key = "firstkey";
        let second_key = "next-key9";

        assert!(configure_access_key(state_dir.path(), "short!!").is_err());
        assert!(!state_dir.path().join("remote-access-key.json").exists());
        let short_login = client
            .post(format!("{base_url}/api/connect"))
            .header(reqwest::header::ORIGIN, &base_url)
            .json(&json!({"key":"short!!"}))
            .send()
            .await
            .unwrap();
        assert_eq!(short_login.status(), reqwest::StatusCode::BAD_REQUEST);

        configure_access_key(state_dir.path(), first_key).unwrap();
        let credentials = access_credentials_status(state_dir.path()).unwrap();
        assert_eq!(credentials["accessKeyConfigured"], true);
        assert_eq!(credentials["key"], first_key);
        assert!(credentials.get("hash").is_none());
        let stored_key: Value = serde_json::from_slice(
            &std::fs::read(state_dir.path().join("remote-access-key.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(stored_key["value"], first_key);
        assert!(
            stored_key["hash"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        let first_login = client
            .post(format!("{base_url}/api/connect"))
            .header(reqwest::header::ORIGIN, &base_url)
            .json(&json!({"key":first_key}))
            .send()
            .await
            .unwrap();
        assert_eq!(first_login.status(), reqwest::StatusCode::OK);
        let first_cookie = first_login.headers()[reqwest::header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let first_session = client
            .get(format!("{base_url}/api/session"))
            .header(reqwest::header::COOKIE, &first_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(first_session.status(), reqwest::StatusCode::OK);

        configure_access_key(state_dir.path(), second_key).unwrap();
        let old_session = client
            .get(format!("{base_url}/api/session"))
            .header(reqwest::header::COOKIE, &first_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(old_session.status(), reqwest::StatusCode::UNAUTHORIZED);
        let old_login = client
            .post(format!("{base_url}/api/connect"))
            .header(reqwest::header::ORIGIN, &base_url)
            .json(&json!({"key":first_key}))
            .send()
            .await
            .unwrap();
        assert_eq!(old_login.status(), reqwest::StatusCode::UNAUTHORIZED);
        let new_login = client
            .post(format!("{base_url}/api/connect"))
            .header(reqwest::header::ORIGIN, &base_url)
            .json(&json!({"key":second_key}))
            .send()
            .await
            .unwrap();
        assert_eq!(new_login.status(), reqwest::StatusCode::OK);
        controller.reconcile(RemoteAccessSettings::default()).await;
    }

    #[test]
    fn app_resource_version_is_stable_and_changes_with_resource_content() {
        let first_assets = HashMap::from([
            (
                "/b.js",
                StaticWebAsset::new("application/javascript; charset=utf-8", b"second"),
            ),
            (
                "/a.js",
                StaticWebAsset::new("application/javascript; charset=utf-8", b"first"),
            ),
        ]);
        let reordered_assets = HashMap::from([
            (
                "/a.js",
                StaticWebAsset::new("application/javascript; charset=utf-8", b"first"),
            ),
            (
                "/b.js",
                StaticWebAsset::new("application/javascript; charset=utf-8", b"second"),
            ),
        ]);
        let changed_assets = HashMap::from([
            (
                "/a.js",
                StaticWebAsset::new("application/javascript; charset=utf-8", b"changed"),
            ),
            (
                "/b.js",
                StaticWebAsset::new("application/javascript; charset=utf-8", b"second"),
            ),
        ]);

        let first = build_app_resources_manifest(&first_assets, false);
        let reordered = build_app_resources_manifest(&reordered_assets, false);
        let changed = build_app_resources_manifest(&changed_assets, false);

        assert_eq!(first.version, reordered.version);
        assert_ne!(first.version, changed.version);
        assert_eq!(first.files[0].path, "/a.js");
        assert_eq!(first.files[1].path, "/b.js");
        assert_eq!(first.schema_version, 1);
        assert_eq!(first.min_shell_version, 1);
        assert_eq!(first.version.len(), 64);
        assert!(
            first
                .version
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
    }

    #[tokio::test]
    async fn app_resource_manifest_matches_public_routes_and_preserves_api_boundary() {
        let state_dir = tempfile::tempdir().unwrap();
        let controller = HarnessController::with_test_backend(
            Arc::new(OfflineTestBackend),
            state_dir.path().to_path_buf(),
        )
        .unwrap();
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let settings = RemoteAccessSettings {
            enabled: true,
            port,
            ..RemoteAccessSettings::default()
        };
        controller.reconcile(settings).await;

        let base_url = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let manifest_response = client
            .get(format!("{base_url}/app-resources.json"))
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .unwrap();
        assert_eq!(manifest_response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            manifest_response
                .headers()
                .get(reqwest::header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-cache")
        );
        let manifest_etag = manifest_response
            .headers()
            .get(reqwest::header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let manifest: Value = manifest_response.json().await.unwrap();
        let manifest_version = manifest["version"].as_str().unwrap();
        assert_eq!(manifest["schemaVersion"], 1);
        assert_eq!(manifest["minShellVersion"], 1);
        assert_eq!(manifest_version.len(), 64);
        assert!(
            manifest_version
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );

        let files = manifest["files"].as_array().unwrap();
        let paths: Vec<_> = files
            .iter()
            .map(|file| file["path"].as_str().unwrap())
            .collect();
        let mut sorted_paths = paths.clone();
        sorted_paths.sort_unstable();
        assert_eq!(paths, sorted_paths);
        assert!(!paths.contains(&"/app-resources.json"));
        assert!(!paths.contains(&"/cache-recovery.html"));
        assert!(!paths.contains(&"/cache-recovery.js"));
        assert!(!paths.contains(&"/side-thread"));
        assert!(!paths.contains(&PREVIEW_WINDOW_PATH));
        assert!(paths.iter().all(|path| !path.starts_with("/api/")));
        assert!(paths.contains(&"/session-files.js"));
        let desktop_manifest = build_app_resources_manifest(web_assets(), true);
        assert!(!web_assets()
            .keys()
            .any(|path| path.starts_with("/management")));
        assert!(
            desktop_manifest
                .files
                .iter()
                .all(|file| !file.path.starts_with("/management"))
        );
        assert!(
            desktop_manifest
                .files
                .iter()
                .any(|file| file.path == PREVIEW_WINDOW_PATH)
        );
        let legacy_preview = client
            .get(format!("{base_url}{PREVIEW_WINDOW_PATH}"))
            .header(reqwest::header::IF_NONE_MATCH, "*")
            .send()
            .await
            .unwrap();
        assert_eq!(legacy_preview.status(), StatusCode::FORBIDDEN);
        assert_eq!(legacy_preview.headers()[header::CACHE_CONTROL], "no-store");

        let mut main_page = None;
        for (path, frame_options, ancestors) in [
            ("/", "DENY", "frame-ancestors 'none'"),
            ("/side-thread", "SAMEORIGIN", "frame-ancestors 'self'"),
        ] {
            let response = client
                .get(format!("{base_url}{path}"))
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(response.headers()["x-frame-options"], frame_options);
            assert!(
                response.headers()["content-security-policy"]
                    .to_str()
                    .unwrap()
                    .contains(ancestors)
            );
            let body = response.bytes().await.unwrap();
            if path == "/" {
                main_page = Some(body);
            } else {
                assert_eq!(Some(body), main_page);
            }
        }

        for (path, content_type) in [
            ("/cache-recovery.html", "text/html; charset=utf-8"),
            (
                "/cache-recovery.js",
                "application/javascript; charset=utf-8",
            ),
        ] {
            let response = client
                .get(format!("{base_url}{path}"))
                .header(reqwest::header::IF_NONE_MATCH, "*")
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(
                response.headers()[reqwest::header::CONTENT_TYPE],
                content_type
            );
            assert_eq!(
                response.headers()[reqwest::header::CACHE_CONTROL],
                "no-store"
            );
            assert!(!response.headers().contains_key(reqwest::header::ETAG));
            assert!(!response.bytes().await.unwrap().is_empty());
        }

        for file in files {
            let path = file["path"].as_str().unwrap();
            let response = client
                .get(format!("{base_url}{path}"))
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK, "{path}");
            assert_eq!(
                response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
                file["contentType"].as_str(),
                "{path}"
            );
            let bytes = response.bytes().await.unwrap();
            assert_eq!(bytes.len() as u64, file["size"].as_u64().unwrap(), "{path}");
            assert_eq!(
                format!("{:x}", Sha256::digest(&bytes)),
                file["sha256"].as_str().unwrap(),
                "{path}"
            );
        }

        let repeated_manifest: Value = client
            .get(format!("{base_url}/app-resources.json"))
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(repeated_manifest["version"], manifest["version"]);
        let revalidated_manifest = client
            .get(format!("{base_url}/app-resources.json"))
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .header(reqwest::header::IF_NONE_MATCH, manifest_etag)
            .send()
            .await
            .unwrap();
        assert_eq!(
            revalidated_manifest.status(),
            reqwest::StatusCode::NOT_MODIFIED
        );

        let compressed_asset = client
            .get(format!("{base_url}/app.js"))
            .header(reqwest::header::ACCEPT_ENCODING, "gzip")
            .send()
            .await
            .unwrap();
        assert_eq!(compressed_asset.status(), reqwest::StatusCode::OK);
        assert_eq!(
            compressed_asset
                .headers()
                .get(reqwest::header::CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("gzip")
        );
        let compressed_etag = compressed_asset
            .headers()
            .get(reqwest::header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let cached_compressed_asset = client
            .get(format!("{base_url}/app.js"))
            .header(reqwest::header::ACCEPT_ENCODING, "gzip")
            .header(reqwest::header::IF_NONE_MATCH, compressed_etag)
            .send()
            .await
            .unwrap();
        assert_eq!(
            cached_compressed_asset.status(),
            reqwest::StatusCode::NOT_MODIFIED
        );

        let unauthenticated_api = client
            .get(format!("{base_url}/api/session"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            unauthenticated_api.status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            unauthenticated_api
                .headers()
                .get(reqwest::header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );

        let bad_host = client
            .get(format!("{base_url}/app-resources.json"))
            .header(reqwest::header::HOST, "untrusted.example")
            .send()
            .await
            .unwrap();
        assert_eq!(bad_host.status(), reqwest::StatusCode::FORBIDDEN);
        for path in ["/api/workspace/files", "/api/workspace/file"] {
            let response = client
                .post(format!("{base_url}{path}"))
                .header(reqwest::header::ORIGIN, &base_url)
                .json(&json!({"threadId":"thread-1","path":"README.md"}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        }
        let bad_origin = client
            .get(format!("{base_url}/app-resources.json"))
            .header(reqwest::header::ORIGIN, "https://untrusted.example")
            .send()
            .await
            .unwrap();
        assert_eq!(bad_origin.status(), reqwest::StatusCode::FORBIDDEN);

        controller.reconcile(RemoteAccessSettings::default()).await;
    }
}
