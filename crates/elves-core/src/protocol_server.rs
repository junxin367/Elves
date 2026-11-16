//! Elves 本地协议代理；只处理模型协议，不启动或控制 Codex App。
use crate::settings::{RelayProtocol, SettingsStore};
use anyhow::Context;
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;
const MAX_CONCURRENT_HELPER_CONNECTIONS: usize = 512;
const HELPER_REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HELPER_REQUEST_BYTES: usize = crate::responses_websocket::RESPONSES_TRANSPORT_MAX_BYTES;
const MAX_HELPER_HEADER_BYTES: usize = 64 * 1024;
pub struct LocalProxyRuntime {
    shutdown: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}
impl LocalProxyRuntime {
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = self.task.await;
    }
}
pub async fn start(
    settings: &crate::settings::BackendSettings,
) -> anyhow::Result<LocalProxyRuntime> {
    // 用户要求已开启本地代理时随 Elves 启动；监听生命周期独立于网页和 Android 远程会话开关。
    start_on_port(settings, crate::protocol_proxy::DEFAULT_PROTOCOL_PROXY_PORT).await
}

pub async fn start_on_port(
    settings: &crate::settings::BackendSettings,
    port: u16,
) -> anyhow::Result<LocalProxyRuntime> {
    let listener = tokio::net::TcpListener::bind((
        crate::lan_proxy::helper_bind_host(settings.lan_proxy_enabled),
        port,
    ))
    .await
    .context("Elves 本地代理端口无法监听")?;
    let (shutdown, mut receiver) = tokio::sync::oneshot::channel();
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_HELPER_CONNECTIONS));
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut receiver => break,
                accepted = listener.accept() => if let Ok((stream, address)) = accepted {
                    if let Ok(permit) = permits.clone().try_acquire_owned() {
                        tokio::spawn(async move { let _permit=permit; let _ = handle_connection(stream, Some(address)).await; });
                    }
                }
            }
        }
    });
    Ok(LocalProxyRuntime { shutdown, task })
}
async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    remote_addr: Option<SocketAddr>,
) -> anyhow::Result<()> {
    let request_bytes = read_http_request(&mut stream).await?;
    let request = String::from_utf8_lossy(&request_bytes);
    let request_line = request.lines().next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let raw_path = parts.next().unwrap_or_default();
    let path = raw_path.split('?').next().unwrap_or(raw_path).to_string();
    drop(request);

    // 单客户端安装只开放现有三种模型协议端点，不能把旧本地增强或管理路由一并暴露出去。
    if !(crate::protocol_proxy::is_responses_proxy_path(&path)
        || crate::protocol_proxy::is_chat_completions_proxy_path(&path)
        || crate::protocol_proxy::is_models_proxy_path(&path))
    {
        write_http_response(
            &mut stream,
            "404 Not Found",
            "application/json",
            br#"{"error":{"message":"Unknown proxy endpoint"}}"#,
        )
        .await?;
        return Ok(());
    }

    if remote_addr.is_some_and(|address| !address.ip().is_loopback()) {
        let settings = SettingsStore::default().load().unwrap_or_default();
        if let Err(error) = crate::lan_proxy::remote_request_access(
            remote_addr,
            settings.lan_proxy_enabled,
            settings.active_relay_uses_protocol_proxy(),
            &method,
            &path,
        ) {
            let (status, code, message) = match error {
                crate::lan_proxy::RemoteAccessError::Disabled => {
                    ("403 Forbidden", "lan_proxy_disabled", "局域网代理未启用")
                }
                crate::lan_proxy::RemoteAccessError::ForbiddenNetwork => (
                    "403 Forbidden",
                    "lan_network_forbidden",
                    "只允许局域网设备访问此代理",
                ),
                crate::lan_proxy::RemoteAccessError::ForbiddenRoute => (
                    "403 Forbidden",
                    "lan_route_forbidden",
                    "局域网只允许访问模型代理接口",
                ),
            };
            let remote_addr_text = remote_addr.map(|address| address.to_string());
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "helper.lan_access_rejected",
                serde_json::json!({
                    "method": method,
                    "path": path,
                    "remote_addr": remote_addr_text,
                    "reason": code
                }),
            );
            let body = serde_json::to_vec(&serde_json::json!({
                "error": {
                    "code": code,
                    "message": message
                }
            }))?;
            write_http_response(
                &mut stream,
                status,
                "application/json; charset=utf-8",
                &body,
            )
            .await?;
            stream.shutdown().await?;
            return Ok(());
        }
    }

    if crate::responses_websocket_bridge::is_responses_websocket_upgrade(&request_bytes) {
        return crate::responses_websocket_bridge::handle_responses_websocket_connection(
            stream,
            request_bytes,
            remote_addr,
        )
        .await;
    }
    let request_context = crate::request_headers::RequestContext::from_http_request(&request_bytes)
        .with_responses_path(&path);
    let request = String::from_utf8_lossy(&request_bytes);
    let request_body = http_request_body(&request);
    let request_user_agent = request_context.user_agent();
    let remote_addr_text = remote_addr.map(|addr| addr.to_string());

    let _ = crate::diagnostic_log::append_diagnostic_log(
        "helper.request",
        serde_json::json!({
            "method": method,
            "path": path,
            "request_line": request_line,
            "remote_addr": remote_addr_text,
            "body_bytes": request_body.len()
        }),
    );

    if crate::protocol_proxy::is_responses_proxy_path(&path) && method == "POST" {
        return handle_protocol_proxy_connection(
            &mut stream,
            request_body,
            &request_context,
            &method,
            &path,
            remote_addr_text,
        )
        .await;
    }
    if crate::protocol_proxy::is_chat_completions_proxy_path(&path) && method == "POST" {
        return handle_chat_completions_proxy_connection(
            &mut stream,
            request_body,
            &request_context,
            &method,
            &path,
            remote_addr_text,
        )
        .await;
    }
    if crate::protocol_proxy::is_models_proxy_path(&path)
        && matches!(method.as_str(), "GET" | "OPTIONS")
    {
        return handle_models_proxy_connection(
            &mut stream,
            request_user_agent,
            &method,
            &path,
            remote_addr_text,
        )
        .await;
    }

    write_http_response(
        &mut stream,
        "404 Not Found",
        "application/json",
        br#"{"error":{"message":"Unknown proxy endpoint"}}"#,
    )
    .await?;
    Ok(())
}

async fn handle_models_proxy_connection(
    stream: &mut tokio::net::TcpStream,
    request_user_agent: Option<&str>,
    method: &str,
    path: &str,
    remote_addr_text: Option<String>,
) -> anyhow::Result<()> {
    if method == "OPTIONS" {
        write_http_response(
            stream,
            "204 No Content",
            "application/json; charset=utf-8",
            &[],
        )
        .await?;
        stream.shutdown().await?;
        return Ok(());
    }

    if let Some(response) = crate::protocol_proxy::local_models_proxy_response()? {
        write_http_response(
            stream,
            &response.status,
            &response.content_type,
            &response.body,
        )
        .await?;
        log_helper_response(
            "helper.models_proxy_ok",
            method,
            path,
            &response.status,
            remote_addr_text,
        );
        stream.shutdown().await?;
        return Ok(());
    }

    let upstream = match crate::protocol_proxy::open_models_proxy_request(request_user_agent).await
    {
        Ok(upstream) => upstream,
        Err(error) => {
            let body = serde_json::to_vec(&serde_json::json!({
                "status": "failed",
                "message": error.to_string()
            }))?;
            write_http_response(
                stream,
                "502 Bad Gateway",
                "application/json; charset=utf-8",
                &body,
            )
            .await?;
            log_helper_response(
                "helper.models_proxy_failed",
                method,
                path,
                "502 Bad Gateway",
                remote_addr_text,
            );
            stream.shutdown().await?;
            return Ok(());
        }
    };

    let status = upstream.status();
    let is_success = upstream.is_success();
    let content_type = if upstream.content_type.is_empty() {
        "application/json; charset=utf-8".to_string()
    } else {
        upstream.content_type.clone()
    };
    let body = upstream
        .into_body_bytes_with_idle_timeout(crate::protocol_proxy::upstream_models_header_timeout())
        .await?;
    write_http_response(stream, &status, &content_type, &body).await?;
    log_helper_response(
        if is_success {
            "helper.models_proxy_ok"
        } else {
            "helper.models_proxy_upstream_error"
        },
        method,
        path,
        &status,
        remote_addr_text,
    );
    stream.shutdown().await?;
    Ok(())
}

enum EitherResponsesStreamConverter<'a> {
    Chat(&'a mut crate::protocol_proxy::ChatSseToResponsesConverter),
    Anthropic(&'a mut crate::protocol_proxy::AnthropicSseToResponsesConverter),
}

impl EitherResponsesStreamConverter<'_> {
    fn push_bytes(&mut self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Self::Chat(converter) => converter.push_bytes(bytes),
            Self::Anthropic(converter) => converter.push_bytes(bytes),
        }
    }

    fn fail(&mut self, message: String, error_type: Option<String>) -> Vec<u8> {
        match self {
            Self::Chat(converter) => converter.fail(message, error_type),
            Self::Anthropic(converter) => converter.fail(message, error_type),
        }
    }

    fn finish(&mut self) -> Vec<u8> {
        match self {
            Self::Chat(converter) => converter.finish(),
            Self::Anthropic(converter) => converter.finish(),
        }
    }

    fn diagnostic_summary(&self) -> serde_json::Value {
        match self {
            Self::Chat(converter) => converter.diagnostic_summary(),
            Self::Anthropic(converter) => converter.diagnostic_summary(),
        }
    }
}

fn should_defer_protocol_stream(request_json: Option<&serde_json::Value>) -> anyhow::Result<bool> {
    let Some(request) = request_json else {
        return Ok(false);
    };
    if request.get("stream").and_then(serde_json::Value::as_bool) != Some(true) {
        return Ok(false);
    }
    // Remote Compaction V2 的正确性不能依赖请求前的供应商协议预测：
    // 聚合供应商可能在实际请求中从 Responses failover 到 Chat/Anthropic。
    // 因此所有流式 V2 请求统一进入 deferred，打开上游后再按实际协议透传或桥接。
    if crate::layered_compaction::is_remote_compaction_v2_request(Some(request)) {
        return Ok(true);
    }
    // Responses 直连流保留原处理逻辑；deferred 只用于需要协议转换的
    // Chat Completions / Anthropic 上游，避免伪流式供应商迟迟不返回响应头。
    Ok(matches!(
        deferred_stream_target_protocol(request)?,
        crate::protocol_proxy::UpstreamResponseProtocol::ChatCompletions
            | crate::protocol_proxy::UpstreamResponseProtocol::Anthropic
    ))
}

fn is_native_remote_compaction_v2_response(
    request_json: Option<&serde_json::Value>,
    response_protocol: crate::protocol_proxy::UpstreamResponseProtocol,
    bridge_remote_compaction_v2: bool,
) -> bool {
    request_json.is_some_and(|request| {
        crate::layered_compaction::is_remote_compaction_v2_request(Some(request))
            && response_protocol == crate::protocol_proxy::UpstreamResponseProtocol::Responses
            && !bridge_remote_compaction_v2
    })
}

fn should_run_continue_thinking_for_proxy_request(
    original_request: Option<&serde_json::Value>,
    forwarded_request: Option<&serde_json::Value>,
) -> bool {
    forwarded_request.is_some()
        && !original_request.is_some_and(crate::layered_compaction::is_any_compaction_request)
}

// 分层压缩：若命中 Codex 上下文压缩请求且功能已启用，则在 LLM 摘要
// 之上追加“最近原始对话 / 工具调用记录”，避免纯摘要压缩导致的断片。
// 作用于已经转换为 Responses 协议的 SSE 文本，与上游协议无关。
fn apply_layered_compaction_if_enabled(
    request_json: Option<&serde_json::Value>,
    sse_text: String,
    options: crate::protocol_proxy::LayeredCompactionOptions,
) -> (String, LocalLayeredCompactionLog) {
    let Some(request) = request_json else {
        return (sse_text, LocalLayeredCompactionLog::default());
    };
    if !crate::layered_compaction::is_compaction_request(Some(request)) {
        return (sse_text, LocalLayeredCompactionLog::default());
    }
    let result = crate::layered_compaction::apply_layered_compaction_to_responses_sse(
        request,
        options.enabled,
        options.retain_tokens,
        sse_text.clone(),
    );
    finish_layered_compaction_response(sse_text, result, options.retain_tokens)
}

fn finish_layered_compaction_response(
    sse_text: String,
    result: crate::layered_compaction::LayeredCompactionResult,
    retain_tokens: u32,
) -> (String, LocalLayeredCompactionLog) {
    if !result.triggered {
        return (result.sse_text, LocalLayeredCompactionLog::default());
    }
    let _ = crate::diagnostic_log::append_diagnostic_log(
        "layered_compaction.applied",
        serde_json::json!({
            "retainedItems": result.retained_items,
            "retainedChars": result.retained_chars,
            "retainTokens": retain_tokens
        }),
    );
    let log = LocalLayeredCompactionLog {
        triggered: true,
        retain_tokens: Some(retain_tokens),
        retained_items: Some(result.retained_items),
        retained_chars: Some(result.retained_chars),
        // 改写前的原始上游响应（纯摘要），供日志详情对比。
        before_response_body: Some(sse_text),
    };
    (result.sse_text, log)
}

fn apply_remote_compaction_v2_bridge(
    request_json: Option<&serde_json::Value>,
    sse_text: String,
    options: crate::protocol_proxy::LayeredCompactionOptions,
) -> (String, LocalLayeredCompactionLog) {
    let Some(request) = request_json else {
        return (sse_text, LocalLayeredCompactionLog::default());
    };
    let Some(result) =
        crate::layered_compaction::rewrite_remote_compaction_v2_responses_sse_with_layered_compaction(
            request,
            options.enabled,
            options.retain_tokens,
            sse_text.clone(),
        )
    else {
        return (sse_text, LocalLayeredCompactionLog::default());
    };
    let _ = crate::diagnostic_log::append_diagnostic_log(
        "remote_compaction_v2.bridge_response",
        serde_json::json!({
            "layeredCompactionTriggered": result.triggered,
            "retainedItems": result.retained_items,
            "retainedChars": result.retained_chars,
            "retainTokens": options.retain_tokens
        }),
    );
    let log = if result.triggered {
        LocalLayeredCompactionLog {
            triggered: true,
            retain_tokens: Some(options.retain_tokens),
            retained_items: Some(result.retained_items),
            retained_chars: Some(result.retained_chars),
            before_response_body: Some(sse_text),
        }
    } else {
        LocalLayeredCompactionLog::default()
    };
    (result.sse_text, log)
}

fn deferred_stream_target_protocol(
    request: &serde_json::Value,
) -> anyhow::Result<crate::protocol_proxy::UpstreamResponseProtocol> {
    let model = request
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    if model.is_empty() {
        anyhow::bail!("请求缺少 model，无法确定 deferred stream 上游协议");
    }

    let settings = SettingsStore::default()
        .load()
        .context("读取 deferred stream 供应商设置失败")?;
    let relay = crate::relay_rotation::select_relay_for_probe(&settings)?;
    Ok(match relay.resolve_protocol_for_model(model)? {
        RelayProtocol::Responses => crate::protocol_proxy::UpstreamResponseProtocol::Responses,
        RelayProtocol::ChatCompletions => {
            crate::protocol_proxy::UpstreamResponseProtocol::ChatCompletions
        }
        RelayProtocol::Anthropic => crate::protocol_proxy::UpstreamResponseProtocol::Anthropic,
    })
}

fn responses_stream_failure_bytes(
    request_json: Option<&serde_json::Value>,
    diagnostic_id: Option<&str>,
    response_protocol: Option<crate::protocol_proxy::UpstreamResponseProtocol>,
    message: String,
    error_type: Option<String>,
) -> Vec<u8> {
    if response_protocol == Some(crate::protocol_proxy::UpstreamResponseProtocol::Anthropic) {
        let mut converter = request_json
            .map(|request| {
                crate::protocol_proxy::AnthropicSseToResponsesConverter::with_request_and_diagnostic_id(
                    request,
                    diagnostic_id,
                )
            })
            .unwrap_or_default();
        return converter.fail(message, error_type);
    }

    let mut converter = request_json
        .map(|request| {
            crate::protocol_proxy::ChatSseToResponsesConverter::with_request_and_diagnostic_id(
                request,
                diagnostic_id,
            )
        })
        .unwrap_or_default();
    converter.fail(message, error_type)
}

async fn handle_protocol_proxy_connection(
    stream: &mut tokio::net::TcpStream,
    request_body: &str,
    request_context: &crate::request_headers::RequestContext,
    method: &str,
    path: &str,
    remote_addr_text: Option<String>,
) -> anyhow::Result<()> {
    let log_id = next_local_proxy_log_id();
    let mut progress = ProxyCancellationProgress::default();
    #[derive(serde::Deserialize)]
    struct StreamFlag {
        #[serde(default)]
        stream: bool,
    }
    // Ignore large input/image fields while selecting the cancellation path.
    // The forwarding routine owns the single full parsed request.
    if !serde_json::from_str::<StreamFlag>(request_body).is_ok_and(|request| request.stream) {
        return handle_protocol_proxy_connection_inner(
            stream,
            request_body,
            request_context,
            method,
            path,
            remote_addr_text,
            log_id,
            &mut progress,
        )
        .await;
    }
    let started_at = Instant::now();
    let timestamp_ms = crate::proxy_log::current_timestamp_ms();
    let (mut reader, writer) = stream.split();
    let mut writer = ProxyClientWriter {
        inner: writer,
        write_failed: false,
    };
    let result = tokio::select! {
        biased;
        result = handle_protocol_proxy_connection_inner(
            &mut writer, request_body, request_context, method, path,
            remote_addr_text.clone(), log_id.clone(), &mut progress,
        ) => Some(result),
        _ = wait_for_proxy_client_disconnect(&mut reader) => None,
    };
    match result {
        Some(Ok(())) => return Ok(()),
        Some(Err(error)) if !writer.write_failed => return Err(error),
        _ => {}
    }
    // The entire forwarding future has been dropped, including its active
    // WS/HTTP body and any pending reconnect or continuation request.
    let original_request_json = serde_json::from_str::<Value>(request_body).ok();
    let original_request_metadata = crate::proxy_log::extract_request_metadata_with_context(
        original_request_json.as_ref(),
        request_context,
    );
    let logged_request_body = progress.request_body.as_deref().unwrap_or(request_body);
    let logged_request_json = serde_json::from_str::<Value>(logged_request_body).ok();
    let mut metadata = request_metadata_for_upstream(
        crate::proxy_log::extract_request_metadata(logged_request_json.as_ref()),
        &original_request_metadata,
        &progress.audit,
    );
    metadata.independent_compaction_model = progress.independent_compaction_model;
    metadata.independent_compaction_usage = progress.independent_compaction_usage;
    append_local_proxy_record(
        log_id,
        timestamp_ms,
        started_at,
        method,
        path,
        remote_addr_text,
        &metadata,
        progress.relay_id,
        progress.relay_name,
        progress.endpoint,
        progress.response_protocol,
        499,
        true,
        logged_request_body,
        &[],
        0,
        false,
        Some("客户端已断开，已取消上游请求".to_string()),
    );
    Ok(())
}

#[derive(Default)]
struct ProxyCancellationProgress {
    audit: crate::proxy_log::ProxyRequestAudit,
    relay_id: Option<String>,
    relay_name: Option<String>,
    endpoint: Option<String>,
    response_protocol: Option<String>,
    request_body: Option<Arc<str>>,
    independent_compaction_model: Option<String>,
    independent_compaction_usage: Option<crate::settings::LayeredCompactionModelUsage>,
}

struct ProxyClientWriter<W> {
    inner: W,
    write_failed: bool,
}

impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for ProxyClientWriter<W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let result = std::pin::Pin::new(&mut self.inner).poll_write(cx, bytes);
        if matches!(result, std::task::Poll::Ready(Err(_))) {
            self.write_failed = true;
        }
        result
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let result = std::pin::Pin::new(&mut self.inner).poll_flush(cx);
        if matches!(result, std::task::Poll::Ready(Err(_))) {
            self.write_failed = true;
        }
        result
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let result = std::pin::Pin::new(&mut self.inner).poll_shutdown(cx);
        if matches!(result, std::task::Poll::Ready(Err(_))) {
            self.write_failed = true;
        }
        result
    }
}

impl ProxyCancellationProgress {
    fn observe_upstream(
        &mut self,
        upstream: &mut crate::protocol_proxy::UpstreamProxyResponse,
        original_body: &str,
    ) -> Arc<str> {
        let body: Arc<str> = if upstream.request_body.trim().is_empty() {
            Arc::from(original_body)
        } else {
            Arc::from(std::mem::take(&mut upstream.request_body))
        };
        self.relay_id = upstream.relay_id.clone();
        self.relay_name = upstream.relay_name.clone();
        self.endpoint = upstream.endpoint.clone();
        self.response_protocol = Some(response_protocol_label(upstream.response_protocol));
        self.independent_compaction_model = upstream.independent_compaction_model.clone();
        self.independent_compaction_usage = upstream.independent_compaction_usage;
        self.request_body = Some(body.clone());
        self.audit = upstream.audit.clone();
        body
    }
}

async fn wait_for_proxy_client_disconnect(reader: &mut tokio::net::tcp::ReadHalf<'_>) {
    let mut byte = [0; 1];
    match reader.peek(&mut byte).await {
        Ok(0) | Err(_) => {}
        // The helper advertises Connection: close. Do not consume unexpected
        // pipelined input or spin on it while the current response is running.
        Ok(_) => std::future::pending::<()>().await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_protocol_proxy_connection_inner(
    stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    request_body: &str,
    request_context: &crate::request_headers::RequestContext,
    method: &str,
    path: &str,
    remote_addr_text: Option<String>,
    log_id: String,
    progress: &mut ProxyCancellationProgress,
) -> anyhow::Result<()> {
    let started_at = Instant::now();
    let timestamp_ms = crate::proxy_log::current_timestamp_ms();
    let request_json = serde_json::from_str::<serde_json::Value>(request_body).ok();
    let upstream_idle_timeout =
        crate::protocol_proxy::stream_idle_timeout_for_request(request_json.as_ref());
    let original_request_metadata = crate::proxy_log::extract_request_metadata_with_context(
        request_json.as_ref(),
        request_context,
    );
    append_pending_local_proxy_record(
        &log_id,
        timestamp_ms,
        method,
        path,
        remote_addr_text.clone(),
        &original_request_metadata,
        request_body,
        request_json.as_ref(),
    );
    if request_context.responses_operation()
        == crate::request_headers::ResponsesRequestOperation::Create
        && should_defer_protocol_stream(request_json.as_ref())?
    {
        return handle_deferred_protocol_proxy_stream_connection(
            stream,
            request_body,
            request_context,
            method,
            path,
            remote_addr_text,
            started_at,
            timestamp_ms,
            request_json,
            original_request_metadata,
            log_id,
            progress,
        )
        .await;
    }
    let upstream_request = if request_json
        .as_ref()
        .and_then(|request| request.get("stream"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        let stream_header_timeout =
            crate::protocol_proxy::stream_idle_timeout_for_request(request_json.as_ref());
        crate::protocol_proxy::open_responses_proxy_request_with_request_context_and_stream_header_timeout(
            request_body,
            request_context,
            stream_header_timeout,
        )
        .await
    } else {
        crate::protocol_proxy::open_responses_proxy_request_with_request_context(
            request_body,
            request_context,
        )
        .await
    };
    let mut upstream = match upstream_request {
        Ok(upstream) => upstream,
        Err(error) => {
            let failure_context = crate::protocol_proxy::upstream_failure_context(&error).cloned();
            let mut failure_metadata = original_request_metadata.clone();
            if let Some(context) = &failure_context {
                failure_metadata = request_metadata_for_upstream(
                    failure_metadata,
                    &original_request_metadata,
                    &context.audit,
                );
                progress.audit = context.audit.clone();
            }
            let error_message = crate::protocol_proxy::upstream_error_message(&error);
            let upstream_timed_out = crate::protocol_proxy::upstream_error_is_timeout(&error);
            let bridge_remote_compaction_v2 = failure_context.as_ref().map_or_else(
                || {
                    request_json.as_ref().is_some_and(|request| {
                        crate::protocol_proxy::should_bridge_remote_compaction_v2_after_failure(
                            request,
                            None,
                            SettingsStore::default()
                                .load()
                                .unwrap_or_default()
                                .layered_compaction_enabled,
                        )
                    })
                },
                |context| context.bridge_remote_compaction_v2,
            );
            let (status, status_code, body) = if bridge_remote_compaction_v2 {
                let failure = crate::layered_compaction::remote_compaction_v2_failure_response(
                    request_json.as_ref().expect("V2 bridge request must exist"),
                    None,
                    "remote_compaction_upstream_request_failed",
                    "Remote Compaction V2 upstream request failed.",
                );
                ("200 OK", 200, serde_json::to_vec(&failure)?)
            } else if upstream_timed_out {
                (
                    "504 Gateway Timeout",
                    504,
                    serde_json::to_vec(&serde_json::json!({
                        "status": "failed",
                        "message": error_message.clone()
                    }))?,
                )
            } else {
                (
                    "502 Bad Gateway",
                    502,
                    serde_json::to_vec(&serde_json::json!({
                        "status": "failed",
                        "message": error_message.clone()
                    }))?,
                )
            };
            write_http_response(stream, status, "application/json; charset=utf-8", &body).await?;
            log_helper_response(
                if bridge_remote_compaction_v2 {
                    "helper.protocol_proxy_remote_compaction_failed"
                } else {
                    "helper.protocol_proxy_failed"
                },
                method,
                path,
                status,
                remote_addr_text.clone(),
            );
            let response_protocol = failure_context
                .as_ref()
                .and_then(|context| context.response_protocol.map(response_protocol_label));
            append_local_proxy_record(
                log_id.clone(),
                timestamp_ms,
                started_at,
                method,
                path,
                remote_addr_text,
                &failure_metadata,
                failure_context
                    .as_ref()
                    .and_then(|context| context.relay_id.clone()),
                failure_context
                    .as_ref()
                    .and_then(|context| context.relay_name.clone()),
                failure_context
                    .as_ref()
                    .and_then(|context| context.endpoint.clone()),
                response_protocol,
                status_code,
                false,
                request_body,
                &body,
                body.len(),
                false,
                Some(error_message),
            );
            stream.shutdown().await?;
            return Ok(());
        }
    };

    // 用户要求模型切换可续聊：普通响应和 deferred SSE 都按实际选中的上游记录签名来源。
    let mut request_json = request_json;
    if let Some(request) = request_json.as_mut() {
        upstream.annotate_anthropic_response_request(request);
    }
    let logged_request_body = progress.observe_upstream(&mut upstream, request_body);
    let logged_request_json = serde_json::from_str::<serde_json::Value>(&logged_request_body).ok();
    let logged_request_metadata =
        crate::proxy_log::extract_request_metadata(logged_request_json.as_ref());
    let mut request_metadata = request_metadata_for_upstream(
        logged_request_metadata,
        &original_request_metadata,
        &upstream.audit,
    );
    request_metadata.independent_compaction_model = upstream.independent_compaction_model.clone();
    request_metadata.independent_compaction_usage = upstream.independent_compaction_usage;
    request_metadata.upstream_response_model = upstream.upstream_response_model.clone();
    let request_body = logged_request_body.as_ref();

    if !upstream.is_success() {
        let relay_id = upstream.relay_id.clone();
        let relay_name = upstream.relay_name.clone();
        let endpoint = upstream.endpoint.clone();
        let status = upstream.status();
        let upstream_content_type = upstream.content_type.clone();
        let response_protocol = upstream.response_protocol;
        let response_protocol_label = response_protocol_label(response_protocol);
        let status_code = upstream.status_code;
        let bridge_remote_compaction_v2 = upstream.bridge_remote_compaction_v2;
        let upstream_body = match upstream
            .into_body_bytes_with_idle_timeout(upstream_idle_timeout)
            .await
        {
            Ok(body) => body,
            Err(error) if bridge_remote_compaction_v2 => {
                let message = format!(
                    "Remote Compaction V2 bridge could not read the upstream response: {error}"
                );
                let failure = crate::layered_compaction::remote_compaction_v2_failure_response(
                    request_json.as_ref().expect("V2 bridge request must exist"),
                    None,
                    "remote_compaction_response_read_failed",
                    &message,
                );
                let body = serde_json::to_vec(&failure)?;
                write_http_response(stream, "200 OK", "application/json; charset=utf-8", &body)
                    .await?;
                log_helper_response(
                    "helper.protocol_proxy_remote_compaction_read_failed",
                    method,
                    path,
                    "200 OK",
                    remote_addr_text.clone(),
                );
                append_local_proxy_record(
                    log_id.clone(),
                    timestamp_ms,
                    started_at,
                    method,
                    path,
                    remote_addr_text,
                    &request_metadata,
                    relay_id,
                    relay_name,
                    endpoint,
                    Some(response_protocol_label),
                    200,
                    false,
                    request_body,
                    &body,
                    body.len(),
                    false,
                    Some(message),
                );
                stream.shutdown().await?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if bridge_remote_compaction_v2 {
            let error = crate::protocol_proxy::responses_error_from_upstream(
                status_code,
                &upstream_content_type,
                &upstream_body,
            );
            let message = error
                .pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Remote Compaction V2 upstream returned a non-success status.");
            let failure = crate::layered_compaction::remote_compaction_v2_failure_response(
                request_json.as_ref().expect("V2 bridge request must exist"),
                None,
                "remote_compaction_upstream_http_error",
                message,
            );
            let body = serde_json::to_vec(&failure)?;
            write_http_response(stream, "200 OK", "application/json; charset=utf-8", &body).await?;
            log_helper_response(
                "helper.protocol_proxy_remote_compaction_upstream_error",
                method,
                path,
                "200 OK",
                remote_addr_text.clone(),
            );
            append_local_proxy_record(
                log_id.clone(),
                timestamp_ms,
                started_at,
                method,
                path,
                remote_addr_text,
                &request_metadata,
                relay_id,
                relay_name,
                endpoint,
                Some(response_protocol_label),
                200,
                false,
                request_body,
                &body,
                body.len(),
                false,
                Some(format!("上游返回 HTTP {status_code}: {message}")),
            );
            stream.shutdown().await?;
            return Ok(());
        }
        if response_protocol == crate::protocol_proxy::UpstreamResponseProtocol::Responses {
            let content_type = if upstream_content_type.is_empty() {
                "application/json; charset=utf-8".to_string()
            } else {
                upstream_content_type
            };
            write_http_response(stream, &status, &content_type, &upstream_body).await?;
            log_helper_response(
                "helper.protocol_proxy_upstream_error",
                method,
                path,
                &status,
                remote_addr_text.clone(),
            );
            append_local_proxy_record(
                log_id.clone(),
                timestamp_ms,
                started_at,
                method,
                path,
                remote_addr_text,
                &request_metadata,
                relay_id,
                relay_name,
                endpoint,
                Some(response_protocol_label),
                status_code,
                false,
                request_body,
                &upstream_body,
                upstream_body.len(),
                false,
                Some(format!("上游返回 HTTP {status_code}")),
            );
            stream.shutdown().await?;
            return Ok(());
        }
        let error = crate::protocol_proxy::responses_error_from_upstream(
            status_code,
            &upstream_content_type,
            &upstream_body,
        );
        let body = serde_json::to_vec(&error)?;
        write_http_response(stream, &status, "application/json; charset=utf-8", &body).await?;
        log_helper_response(
            "helper.protocol_proxy_upstream_error",
            method,
            path,
            &status,
            remote_addr_text.clone(),
        );
        append_local_proxy_record(
            log_id.clone(),
            timestamp_ms,
            started_at,
            method,
            path,
            remote_addr_text,
            &request_metadata,
            relay_id,
            relay_name,
            endpoint,
            Some(response_protocol_label),
            status_code,
            false,
            request_body,
            &body,
            body.len(),
            false,
            Some(format!("上游返回 HTTP {status_code}")),
        );
        stream.shutdown().await?;
        return Ok(());
    }

    let diagnostic_id = upstream.diagnostic_id.clone();
    let relay_id = upstream.relay_id.clone();
    let relay_name = upstream.relay_name.clone();
    let endpoint = upstream.endpoint.clone();
    let layered_compaction_options = upstream.layered_compaction_options;

    if upstream.is_stream {
        if upstream.response_protocol == crate::protocol_proxy::UpstreamResponseProtocol::Responses
        {
            let status = upstream.status();
            let status_code = upstream.status_code;
            let response_protocol_label = response_protocol_label(upstream.response_protocol);
            let content_type = if upstream.content_type.is_empty() {
                "text/event-stream; charset=utf-8".to_string()
            } else {
                upstream.content_type.clone()
            };
            write_http_stream_headers(stream, &status, &content_type, true).await?;
            let mut response_capture = Vec::new();
            let mut response_bytes = 0_usize;
            let mut response_truncated = false;
            let mut continue_thinking_log = LocalContinueThinkingLog::default();
            let mut layered_compaction_log = LocalLayeredCompactionLog::default();
            if upstream.body_override.is_some() {
                let raw_body = upstream.into_body_bytes().await?;
                record_observed_upstream_model(
                    &mut request_metadata,
                    observed_upstream_model(
                        &raw_body,
                        crate::protocol_proxy::UpstreamResponseProtocol::Responses,
                        true,
                    ),
                );
                let body = crate::image_generation::rewrite_body(
                    &raw_body,
                    true,
                    Some(request_context),
                    request_json.as_ref(),
                );
                response_bytes += body.len();
                response_truncated |=
                    crate::proxy_log::append_capture(&mut response_capture, &body);
                stream.write_all(&body).await?;
            } else {
                let mut cache_observer = upstream.take_cache_observer();
                let mut bytes_stream = upstream.into_response()?.bytes_stream();
                let mut first_round_bytes = Vec::new();
                let upstream_idle_timeout = crate::protocol_proxy::stream_idle_timeout_for_request(
                    logged_request_json.as_ref().or(request_json.as_ref()),
                );
                loop {
                    let chunk = match next_stream_chunk_with_idle_timeout(
                        &mut bytes_stream,
                        upstream_idle_timeout,
                    )
                    .await
                    {
                        Ok(Some(chunk)) => chunk,
                        Ok(None) => break,
                        Err(error) => {
                            let error_message = format!("Stream error: {error}");
                            let failed = responses_stream_failure_bytes(
                                logged_request_json.as_ref().or(request_json.as_ref()),
                                Some(&diagnostic_id),
                                Some(crate::protocol_proxy::UpstreamResponseProtocol::Responses),
                                error_message.clone(),
                                Some("stream_timeout".to_string()),
                            );
                            response_bytes += failed.len();
                            response_truncated |=
                                crate::proxy_log::append_capture(&mut response_capture, &failed);
                            stream.write_all(&failed).await?;
                            record_observed_upstream_model(
                                &mut request_metadata,
                                observed_upstream_model(
                                    &first_round_bytes,
                                    crate::protocol_proxy::UpstreamResponseProtocol::Responses,
                                    true,
                                ),
                            );
                            append_local_proxy_record(
                                log_id.clone(),
                                timestamp_ms,
                                started_at,
                                method,
                                path,
                                remote_addr_text,
                                &request_metadata,
                                relay_id,
                                relay_name,
                                endpoint,
                                Some(response_protocol_label),
                                status_code,
                                true,
                                request_body,
                                &response_capture,
                                response_bytes,
                                response_truncated,
                                Some(error_message),
                            );
                            stream.shutdown().await?;
                            return Ok(());
                        }
                    };
                    let bytes = match chunk {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            let error_message = format!("Stream error: {error}");
                            let failed = responses_stream_failure_bytes(
                                logged_request_json.as_ref().or(request_json.as_ref()),
                                Some(&diagnostic_id),
                                Some(crate::protocol_proxy::UpstreamResponseProtocol::Responses),
                                error_message.clone(),
                                Some("stream_error".to_string()),
                            );
                            response_bytes += failed.len();
                            response_truncated |=
                                crate::proxy_log::append_capture(&mut response_capture, &failed);
                            stream.write_all(&failed).await?;
                            record_observed_upstream_model(
                                &mut request_metadata,
                                observed_upstream_model(
                                    &first_round_bytes,
                                    crate::protocol_proxy::UpstreamResponseProtocol::Responses,
                                    true,
                                ),
                            );
                            append_local_proxy_record(
                                log_id.clone(),
                                timestamp_ms,
                                started_at,
                                method,
                                path,
                                remote_addr_text,
                                &request_metadata,
                                relay_id,
                                relay_name,
                                endpoint,
                                Some(response_protocol_label),
                                status_code,
                                true,
                                request_body,
                                &response_capture,
                                response_bytes,
                                response_truncated,
                                Some(error_message),
                            );
                            stream.shutdown().await?;
                            return Ok(());
                        }
                    };
                    if let Some(observer) = cache_observer.as_mut() {
                        observer.observe_bytes(&bytes);
                    }
                    first_round_bytes.extend_from_slice(&bytes);
                    if continue_thinking_log.first_token_ms.is_none() {
                        let first_token_ms = started_at.elapsed().as_millis() as u64;
                        continue_thinking_log.first_token_ms = Some(first_token_ms);
                        append_local_proxy_first_token_record(
                            &log_id,
                            timestamp_ms,
                            method,
                            path,
                            remote_addr_text.clone(),
                            &request_metadata,
                            relay_id.clone(),
                            relay_name.clone(),
                            endpoint.clone(),
                            Some(response_protocol_label.clone()),
                            status_code,
                            true,
                            request_body,
                            first_token_ms,
                        );
                    }
                }
                if let Some(observer) = cache_observer.take() {
                    observer.finish();
                }
                record_observed_upstream_model(
                    &mut request_metadata,
                    observed_upstream_model(
                        &first_round_bytes,
                        crate::protocol_proxy::UpstreamResponseProtocol::Responses,
                        true,
                    ),
                );
                let first_round_sse_text = String::from_utf8_lossy(&first_round_bytes).into_owned();
                let final_sse_text = if should_run_continue_thinking_for_proxy_request(
                    request_json.as_ref(),
                    logged_request_json.as_ref(),
                ) {
                    let request = logged_request_json
                        .as_ref()
                        .expect("只有可解析的上游请求才会进入自动续思考");
                    let settings = SettingsStore::default().load().unwrap_or_default();
                    let result =
                        crate::protocol_proxy::apply_continue_thinking_to_responses_stream_with_request_context_and_audit(
                            request,
                            settings,
                            request_context,
                            first_round_sse_text,
                            &mut request_metadata.audit,
                        )
                        .await;
                    continue_thinking_log = LocalContinueThinkingLog {
                        triggered: result.triggered,
                        rounds: result.rounds,
                        first_token_ms: continue_thinking_log.first_token_ms,
                        reasoning_tokens: result.reasoning_tokens,
                        request_body: result.request_body,
                        before_response_body: result.before_response_body,
                        after_response_body: result.after_response_body,
                    };
                    result.sse_text
                } else {
                    first_round_sse_text
                };
                let (final_sse_text, compaction_log) = apply_layered_compaction_if_enabled(
                    // 提示词替换只作用于上游转发副本；压缩识别和最近记录提取必须使用
                    // Codex 发来的原始请求，否则自定义/项目默认提示词会破坏固定前缀检测。
                    request_json.as_ref(),
                    final_sse_text,
                    layered_compaction_options,
                );
                layered_compaction_log = compaction_log;
                let final_bytes = crate::image_generation::rewrite_body(
                    final_sse_text.as_bytes(),
                    true,
                    Some(request_context),
                    request_json.as_ref(),
                );
                response_bytes += final_bytes.len();
                response_truncated |=
                    crate::proxy_log::append_capture(&mut response_capture, &final_bytes);
                stream.write_all(&final_bytes).await?;
            }
            log_helper_response(
                "helper.protocol_proxy_stream_ok",
                method,
                path,
                &status,
                remote_addr_text.clone(),
            );
            append_local_proxy_record_with_continue_thinking(
                log_id.clone(),
                timestamp_ms,
                started_at,
                method,
                path,
                remote_addr_text,
                &request_metadata,
                relay_id,
                relay_name,
                endpoint,
                Some(response_protocol_label),
                status_code,
                true,
                request_body,
                &response_capture,
                response_bytes,
                response_truncated,
                None,
                continue_thinking_log,
                layered_compaction_log,
            );
            stream.shutdown().await?;
            return Ok(());
        }

        write_http_stream_headers(stream, "200 OK", "text/event-stream; charset=utf-8", true)
            .await?;
        let mut chat_converter;
        let mut anthropic_converter;
        let response_protocol = upstream.response_protocol;
        let converter_kind = if response_protocol
            == crate::protocol_proxy::UpstreamResponseProtocol::ChatCompletions
        {
            chat_converter = request_json
                .as_ref()
                .map(|request| {
                    crate::protocol_proxy::ChatSseToResponsesConverter::with_request_and_diagnostic_id(
                        request,
                        Some(&diagnostic_id),
                    )
                })
                .unwrap_or_default();
            EitherResponsesStreamConverter::Chat(&mut chat_converter)
        } else {
            anthropic_converter = request_json
                .as_ref()
                .map(|request| {
                    crate::protocol_proxy::AnthropicSseToResponsesConverter::with_request_and_diagnostic_id(
                        request,
                        Some(&diagnostic_id),
                    )
                })
                .unwrap_or_default();
            EitherResponsesStreamConverter::Anthropic(&mut anthropic_converter)
        };
        let mut converter = converter_kind;
        let mut upstream_model_observer =
            crate::proxy_log::UpstreamResponseModelObserver::default();
        let mut cache_observer = upstream.take_cache_observer();
        let mut bytes_stream = upstream.into_response()?.bytes_stream();
        let mut stream_failed = false;
        let mut stream_error = None;
        let mut response_capture = Vec::new();
        let mut response_bytes = 0_usize;
        let mut response_truncated = false;
        let mut first_token_ms = None;

        loop {
            let chunk =
                match next_stream_chunk_with_idle_timeout(&mut bytes_stream, upstream_idle_timeout)
                    .await
                {
                    Ok(Some(chunk)) => chunk.map_err(anyhow::Error::from),
                    Ok(None) => break,
                    Err(error) => Err(error),
                };
            match chunk {
                Ok(bytes) => {
                    if let Some(observer) = cache_observer.as_mut() {
                        observer.observe_bytes(&bytes);
                    }
                    upstream_model_observer.observe_sse_chunk(&bytes, response_protocol);
                    let converted = converter.push_bytes(&bytes);
                    if !converted.is_empty() {
                        if first_token_ms.is_none() {
                            let elapsed_ms = started_at.elapsed().as_millis() as u64;
                            first_token_ms = Some(elapsed_ms);
                            append_local_proxy_first_token_record(
                                &log_id,
                                timestamp_ms,
                                method,
                                path,
                                remote_addr_text.clone(),
                                &request_metadata,
                                relay_id.clone(),
                                relay_name.clone(),
                                endpoint.clone(),
                                Some(response_protocol_label(response_protocol)),
                                200,
                                true,
                                request_body,
                                elapsed_ms,
                            );
                        }
                        response_bytes += converted.len();
                        response_truncated |=
                            crate::proxy_log::append_capture(&mut response_capture, &converted);
                        stream.write_all(&converted).await?;
                    }
                }
                Err(error) => {
                    let error_message = format!("Stream error: {error}");
                    let failed =
                        converter.fail(error_message.clone(), Some("stream_error".to_string()));
                    if !failed.is_empty() {
                        response_bytes += failed.len();
                        response_truncated |=
                            crate::proxy_log::append_capture(&mut response_capture, &failed);
                        stream.write_all(&failed).await?;
                    }
                    stream_error = Some(error_message);
                    stream_failed = true;
                    break;
                }
            }
        }
        if !stream_failed && let Some(observer) = cache_observer.take() {
            observer.finish();
        }
        upstream_model_observer.finish_sse(response_protocol);
        request_metadata
            .audit
            .observe_response(&upstream_model_observer);
        record_observed_upstream_model(&mut request_metadata, upstream_model_observer.model());

        if !stream_failed {
            let tail = converter.finish();
            if !tail.is_empty() {
                response_bytes += tail.len();
                response_truncated |=
                    crate::proxy_log::append_capture(&mut response_capture, &tail);
                stream.write_all(&tail).await?;
            }
        }
        let _ = crate::diagnostic_log::append_diagnostic_log(
            "protocol_proxy.stream_conversion_terminal",
            converter.diagnostic_summary(),
        );
        log_helper_response(
            "helper.protocol_proxy_stream_ok",
            method,
            path,
            "200 OK",
            remote_addr_text.clone(),
        );
        append_local_proxy_record_with_continue_thinking(
            log_id.clone(),
            timestamp_ms,
            started_at,
            method,
            path,
            remote_addr_text,
            &request_metadata,
            relay_id,
            relay_name,
            endpoint,
            Some(response_protocol_label(response_protocol)),
            200,
            true,
            request_body,
            &response_capture,
            response_bytes,
            response_truncated,
            stream_error,
            LocalContinueThinkingLog {
                first_token_ms,
                ..Default::default()
            },
            LocalLayeredCompactionLog::default(),
        );
        stream.shutdown().await?;
        return Ok(());
    }

    let response_protocol = upstream.response_protocol;
    let status_code = upstream.status_code;
    let upstream_status = upstream.status();
    let upstream_content_type = upstream.content_type.clone();
    let bridge_remote_compaction_v2 = upstream.bridge_remote_compaction_v2;
    let upstream_body = match upstream
        .into_body_bytes_with_idle_timeout(upstream_idle_timeout)
        .await
    {
        Ok(body) => body,
        Err(error) if bridge_remote_compaction_v2 => {
            let message = format!(
                "Remote Compaction V2 bridge could not read the upstream response: {error}"
            );
            let failure = crate::layered_compaction::remote_compaction_v2_failure_response(
                request_json.as_ref().expect("V2 bridge request must exist"),
                None,
                "remote_compaction_response_read_failed",
                &message,
            );
            let body = serde_json::to_vec(&failure)?;
            write_http_response(stream, "200 OK", "application/json; charset=utf-8", &body).await?;
            log_helper_response(
                "helper.protocol_proxy_remote_compaction_read_failed",
                method,
                path,
                "200 OK",
                remote_addr_text.clone(),
            );
            append_local_proxy_record(
                log_id.clone(),
                timestamp_ms,
                started_at,
                method,
                path,
                remote_addr_text,
                &request_metadata,
                relay_id,
                relay_name,
                endpoint,
                Some(response_protocol_label(response_protocol)),
                200,
                false,
                request_body,
                &body,
                body.len(),
                false,
                Some(message),
            );
            stream.shutdown().await?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    record_observed_upstream_model(
        &mut request_metadata,
        observed_upstream_model(&upstream_body, response_protocol, false),
    );
    if response_protocol == crate::protocol_proxy::UpstreamResponseProtocol::Responses {
        let body = if bridge_remote_compaction_v2 {
            let request = request_json.as_ref().expect("bridge request must exist");
            let response = match serde_json::from_slice::<serde_json::Value>(&upstream_body) {
                Ok(upstream_json) => crate::layered_compaction::rewrite_remote_compaction_v2_response_with_layered_compaction(
                    request,
                    &upstream_json,
                    layered_compaction_options.enabled,
                    layered_compaction_options.retain_tokens,
                )
                .expect("Remote Compaction V2 bridge request must produce a terminal response")
                .response,
                Err(error) => crate::layered_compaction::remote_compaction_v2_failure_response(
                    request,
                    None,
                    "remote_compaction_response_parse_failed",
                    &format!("Remote Compaction V2 bridge could not parse the upstream response: {error}"),
                ),
            };
            serde_json::to_vec(&response)?
        } else {
            crate::image_generation::rewrite_body(
                &upstream_body,
                false,
                Some(request_context),
                request_json.as_ref(),
            )
        };

        let content_type = if bridge_remote_compaction_v2 || upstream_content_type.is_empty() {
            "application/json; charset=utf-8".to_string()
        } else {
            upstream_content_type
        };
        let response_status = if bridge_remote_compaction_v2 {
            "200 OK"
        } else {
            upstream_status.as_str()
        };
        write_http_response(stream, response_status, &content_type, &body).await?;
        log_helper_response(
            "helper.protocol_proxy_ok",
            method,
            path,
            response_status,
            remote_addr_text.clone(),
        );
        append_local_proxy_record(
            log_id.clone(),
            timestamp_ms,
            started_at,
            method,
            path,
            remote_addr_text,
            &request_metadata,
            relay_id,
            relay_name,
            endpoint,
            Some(response_protocol_label(response_protocol)),
            if bridge_remote_compaction_v2 {
                200
            } else {
                status_code
            },
            false,
            request_body,
            &body,
            body.len(),
            false,
            None,
        );
        stream.shutdown().await?;
        return Ok(());
    }

    let response_result = match serde_json::from_slice::<serde_json::Value>(&upstream_body) {
        Ok(upstream_json) => match response_protocol {
            crate::protocol_proxy::UpstreamResponseProtocol::ChatCompletions => {
                if let Some(request_json) = request_json.as_ref() {
                    crate::protocol_proxy::chat_completion_to_response_with_request_and_options(
                        upstream_json,
                        request_json,
                        layered_compaction_options,
                    )
                } else {
                    crate::protocol_proxy::chat_completion_to_response(upstream_json)
                }
            }
            crate::protocol_proxy::UpstreamResponseProtocol::Anthropic => {
                if let Some(request_json) = request_json.as_ref() {
                    crate::protocol_proxy::anthropic_message_to_response_with_request_diagnostic_id_and_options(
                        upstream_json,
                        request_json,
                        Some(&diagnostic_id),
                        layered_compaction_options,
                    )
                } else {
                    crate::protocol_proxy::anthropic_message_to_response(upstream_json)
                }
            }
            crate::protocol_proxy::UpstreamResponseProtocol::Responses => unreachable!(),
        },
        Err(error)
            if request_json.as_ref().is_some_and(|request| {
                crate::layered_compaction::is_remote_compaction_v2_request(Some(request))
            }) =>
        {
            Ok(
                crate::layered_compaction::remote_compaction_v2_failure_response(
                    request_json.as_ref().expect("V2 request must exist"),
                    None,
                    "remote_compaction_response_parse_failed",
                    &format!(
                        "Remote Compaction V2 bridge could not parse the upstream response: {error}"
                    ),
                ),
            )
        }
        Err(error) => Err(error.into()),
    };
    let response_json = match response_result {
        Ok(response) => response,
        Err(error)
            if request_json.as_ref().is_some_and(|request| {
                crate::layered_compaction::is_remote_compaction_v2_request(Some(request))
            }) =>
        {
            crate::layered_compaction::remote_compaction_v2_failure_response(
                request_json.as_ref().expect("V2 request must exist"),
                None,
                "remote_compaction_response_conversion_failed",
                &format!(
                    "Remote Compaction V2 bridge could not convert the upstream response: {error}"
                ),
            )
        }
        Err(error) => return Err(error),
    };
    let body = serde_json::to_vec(&response_json)?;
    write_http_response(stream, "200 OK", "application/json; charset=utf-8", &body).await?;
    log_helper_response(
        "helper.protocol_proxy_ok",
        method,
        path,
        "200 OK",
        remote_addr_text.clone(),
    );
    append_local_proxy_record(
        log_id.clone(),
        timestamp_ms,
        started_at,
        method,
        path,
        remote_addr_text,
        &request_metadata,
        relay_id,
        relay_name,
        endpoint,
        Some(response_protocol_label(response_protocol)),
        200,
        false,
        request_body,
        &body,
        body.len(),
        false,
        None,
    );
    stream.shutdown().await?;
    Ok(())
}

async fn next_stream_chunk_with_idle_timeout<S, T, E>(
    bytes_stream: &mut S,
    upstream_idle_timeout: Duration,
) -> anyhow::Result<Option<Result<T, E>>>
where
    S: Stream<Item = Result<T, E>> + Unpin,
{
    tokio::time::timeout(upstream_idle_timeout, bytes_stream.next())
        .await
        .with_context(|| {
            format!(
                "上游流超过 {} 毫秒没有返回数据",
                upstream_idle_timeout.as_millis()
            )
        })
}

#[allow(clippy::too_many_arguments)]
async fn handle_deferred_protocol_proxy_stream_connection(
    stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    request_body: &str,
    request_context: &crate::request_headers::RequestContext,
    method: &str,
    path: &str,
    remote_addr_text: Option<String>,
    started_at: Instant,
    timestamp_ms: u64,
    request_json: Option<serde_json::Value>,
    original_request_metadata: crate::proxy_log::RequestMetadata,
    log_id: String,
    progress: &mut ProxyCancellationProgress,
) -> anyhow::Result<()> {
    let mut response_capture = Vec::new();
    let mut response_bytes = 0_usize;
    let mut response_truncated = false;
    let mut first_token_ms = None;
    let mut layered_compaction_log = LocalLayeredCompactionLog::default();
    // Only a request explicitly assigned to the native Responses protocol may
    // preserve a non-SSE upstream body and status. Chat Completions / Anthropic
    // Remote Compaction V2 requests still need an immediate local SSE response.
    let preserve_native_remote_compaction_response = match request_json.as_ref() {
        Some(request)
            if crate::layered_compaction::is_remote_compaction_v2_request(Some(request)) =>
        {
            deferred_stream_target_protocol(request)?
                == crate::protocol_proxy::UpstreamResponseProtocol::Responses
        }
        _ => false,
    };
    let mut downstream_stream_headers_sent = false;
    if !preserve_native_remote_compaction_response {
        write_http_stream_headers_once(
            stream,
            &mut downstream_stream_headers_sent,
            "200 OK",
            "text/event-stream; charset=utf-8",
            true,
        )
        .await?;
    }

    let stream_header_timeout =
        crate::protocol_proxy::stream_idle_timeout_for_request(request_json.as_ref());
    let upstream =
        crate::protocol_proxy::open_responses_proxy_request_with_request_context_and_stream_header_timeout(
            request_body,
            request_context,
            stream_header_timeout,
        )
        .await;

    let mut upstream = match upstream {
        Ok(upstream) => upstream,
        Err(error) => {
            let failure_context = crate::protocol_proxy::upstream_failure_context(&error).cloned();
            let mut failure_metadata = original_request_metadata.clone();
            if let Some(context) = &failure_context {
                failure_metadata = request_metadata_for_upstream(
                    failure_metadata,
                    &original_request_metadata,
                    &context.audit,
                );
                progress.audit = context.audit.clone();
            }
            let error_message = crate::protocol_proxy::upstream_error_message(&error);
            let failed = if request_json.as_ref().is_some_and(|request| {
                crate::layered_compaction::is_remote_compaction_v2_request(Some(request))
            }) {
                crate::layered_compaction::remote_compaction_v2_failure_sse(
                    request_json.as_ref().expect("V2 stream request must exist"),
                    "remote_compaction_upstream_request_failed",
                    "Remote Compaction V2 upstream request failed.",
                )
                .into_bytes()
            } else {
                responses_stream_failure_bytes(
                    request_json.as_ref(),
                    failure_context
                        .as_ref()
                        .map(|context| context.diagnostic_id.as_str()),
                    failure_context
                        .as_ref()
                        .and_then(|context| context.response_protocol),
                    error_message.clone(),
                    Some("upstream_error".to_string()),
                )
            };
            write_http_stream_headers_once(
                stream,
                &mut downstream_stream_headers_sent,
                "200 OK",
                "text/event-stream; charset=utf-8",
                true,
            )
            .await?;
            response_bytes += failed.len();
            response_truncated |= crate::proxy_log::append_capture(&mut response_capture, &failed);
            stream.write_all(&failed).await?;
            log_helper_response(
                "helper.protocol_proxy_deferred_stream_failed",
                method,
                path,
                "200 OK",
                remote_addr_text.clone(),
            );
            append_local_proxy_record(
                log_id.clone(),
                timestamp_ms,
                started_at,
                method,
                path,
                remote_addr_text,
                &failure_metadata,
                failure_context
                    .as_ref()
                    .and_then(|context| context.relay_id.clone()),
                failure_context
                    .as_ref()
                    .and_then(|context| context.relay_name.clone()),
                failure_context
                    .as_ref()
                    .and_then(|context| context.endpoint.clone()),
                failure_context
                    .as_ref()
                    .and_then(|context| context.response_protocol.map(response_protocol_label)),
                200,
                true,
                request_body,
                &response_capture,
                response_bytes,
                response_truncated,
                Some(error_message),
            );
            stream.shutdown().await?;
            return Ok(());
        }
    };

    // 与普通响应共用签名来源封装，避免流式会话遗漏供应商或账号作用域。
    let mut request_json = request_json;
    if let Some(request) = request_json.as_mut() {
        upstream.annotate_anthropic_response_request(request);
    }
    let logged_request_body = progress.observe_upstream(&mut upstream, request_body);
    let logged_request_json = serde_json::from_str::<serde_json::Value>(&logged_request_body).ok();
    let logged_request_metadata =
        crate::proxy_log::extract_request_metadata(logged_request_json.as_ref());
    let mut request_metadata = request_metadata_for_upstream(
        logged_request_metadata,
        &original_request_metadata,
        &upstream.audit,
    );
    request_metadata.independent_compaction_model = upstream.independent_compaction_model.clone();
    request_metadata.independent_compaction_usage = upstream.independent_compaction_usage;
    request_metadata.upstream_response_model = upstream.upstream_response_model.clone();
    let request_body = logged_request_body.as_ref();

    let diagnostic_id = upstream.diagnostic_id.clone();
    let relay_id = upstream.relay_id.clone();
    let relay_name = upstream.relay_name.clone();
    let endpoint = upstream.endpoint.clone();
    let response_protocol = upstream.response_protocol;
    let layered_compaction_options = upstream.layered_compaction_options;
    let response_protocol_label = response_protocol_label(response_protocol);
    let status_code = upstream.status_code;
    let upstream_status = upstream.status();
    let upstream_content_type = upstream.content_type.clone();
    let bridge_remote_compaction_v2 = upstream.bridge_remote_compaction_v2;
    let native_remote_compaction_v2 = preserve_native_remote_compaction_response
        && is_native_remote_compaction_v2_response(
            request_json.as_ref(),
            response_protocol,
            bridge_remote_compaction_v2,
        );
    let mut upstream_model_observer = crate::proxy_log::UpstreamResponseModelObserver::default();

    if !upstream.is_success() {
        let upstream_body = match upstream
            .into_body_bytes_with_idle_timeout(stream_header_timeout)
            .await
        {
            Ok(body) => body,
            Err(error) if bridge_remote_compaction_v2 => {
                let message = format!(
                    "Remote Compaction V2 bridge could not read the upstream response: {error}"
                );
                let failed = crate::layered_compaction::remote_compaction_v2_failure_sse(
                    request_json.as_ref().expect("V2 bridge request must exist"),
                    "remote_compaction_response_read_failed",
                    &message,
                )
                .into_bytes();
                write_http_stream_headers_once(
                    stream,
                    &mut downstream_stream_headers_sent,
                    "200 OK",
                    "text/event-stream; charset=utf-8",
                    true,
                )
                .await?;
                response_bytes += failed.len();
                response_truncated |=
                    crate::proxy_log::append_capture(&mut response_capture, &failed);
                stream.write_all(&failed).await?;
                log_helper_response(
                    "helper.protocol_proxy_deferred_remote_compaction_read_failed",
                    method,
                    path,
                    "200 OK",
                    remote_addr_text.clone(),
                );
                append_local_proxy_record(
                    log_id.clone(),
                    timestamp_ms,
                    started_at,
                    method,
                    path,
                    remote_addr_text,
                    &request_metadata,
                    relay_id,
                    relay_name,
                    endpoint,
                    Some(response_protocol_label),
                    200,
                    true,
                    request_body,
                    &response_capture,
                    response_bytes,
                    response_truncated,
                    Some(message),
                );
                stream.shutdown().await?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if native_remote_compaction_v2 {
            let content_type = if upstream_content_type.is_empty() {
                "application/json; charset=utf-8"
            } else {
                upstream_content_type.as_str()
            };
            write_http_response(stream, &upstream_status, content_type, &upstream_body).await?;
            log_helper_response(
                "helper.protocol_proxy_native_remote_compaction_upstream_error",
                method,
                path,
                &upstream_status,
                remote_addr_text.clone(),
            );
            append_local_proxy_record(
                log_id.clone(),
                timestamp_ms,
                started_at,
                method,
                path,
                remote_addr_text,
                &request_metadata,
                relay_id,
                relay_name,
                endpoint,
                Some(response_protocol_label),
                status_code,
                true,
                request_body,
                &upstream_body,
                upstream_body.len(),
                false,
                Some(format!("上游返回 HTTP {status_code}")),
            );
            stream.shutdown().await?;
            return Ok(());
        }
        let error = crate::protocol_proxy::responses_error_from_upstream(
            status_code,
            &upstream_content_type,
            &upstream_body,
        );
        let message = error
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("上游请求失败")
            .to_string();
        let error_type = error
            .pointer("/error/type")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string)
            .or_else(|| Some("upstream_error".to_string()));
        let failed = responses_stream_failure_bytes(
            request_json.as_ref(),
            Some(&diagnostic_id),
            Some(response_protocol),
            message.clone(),
            error_type,
        );
        write_http_stream_headers_once(
            stream,
            &mut downstream_stream_headers_sent,
            "200 OK",
            "text/event-stream; charset=utf-8",
            true,
        )
        .await?;
        response_bytes += failed.len();
        response_truncated |= crate::proxy_log::append_capture(&mut response_capture, &failed);
        stream.write_all(&failed).await?;
        log_helper_response(
            "helper.protocol_proxy_deferred_stream_upstream_error",
            method,
            path,
            "200 OK",
            remote_addr_text.clone(),
        );
        append_local_proxy_record(
            log_id.clone(),
            timestamp_ms,
            started_at,
            method,
            path,
            remote_addr_text,
            &request_metadata,
            relay_id,
            relay_name,
            endpoint,
            Some(response_protocol_label),
            status_code,
            true,
            request_body,
            &response_capture,
            response_bytes,
            response_truncated,
            Some(format!("上游返回 HTTP {status_code}: {message}")),
        );
        stream.shutdown().await?;
        return Ok(());
    }

    let response_status = if native_remote_compaction_v2 {
        upstream_status.as_str()
    } else {
        "200 OK"
    };
    let response_content_type = if native_remote_compaction_v2 && !upstream_content_type.is_empty()
    {
        upstream_content_type.as_str()
    } else {
        "text/event-stream; charset=utf-8"
    };
    write_http_stream_headers_once(
        stream,
        &mut downstream_stream_headers_sent,
        response_status,
        response_content_type,
        true,
    )
    .await?;

    if response_protocol == crate::protocol_proxy::UpstreamResponseProtocol::Responses {
        let mut remote_compaction_stream_error = None;
        if bridge_remote_compaction_v2 {
            let raw_body = if upstream.body_override.is_some() {
                upstream
                    .into_body_bytes_with_idle_timeout(stream_header_timeout)
                    .await?
                    .to_vec()
            } else {
                let mut bytes_stream = upstream.into_response()?.bytes_stream();
                let mut buffered = Vec::new();
                loop {
                    let chunk = match next_stream_chunk_with_idle_timeout(
                        &mut bytes_stream,
                        stream_header_timeout,
                    )
                    .await
                    {
                        Ok(Some(chunk)) => chunk,
                        Ok(None) => break,
                        Err(error) => {
                            let message = format!(
                                "Remote Compaction V2 upstream stream failed before completion: {error}"
                            );
                            remote_compaction_stream_error = Some(message.clone());
                            buffered.clear();
                            buffered.extend_from_slice(
                                crate::layered_compaction::remote_compaction_v2_failure_sse(
                                    request_json.as_ref().expect("bridge request must exist"),
                                    "remote_compaction_stream_error",
                                    &message,
                                )
                                .as_bytes(),
                            );
                            let _ = crate::diagnostic_log::append_diagnostic_log(
                                "remote_compaction_v2.stream_failed",
                                serde_json::json!({
                                    "transport": "http_sse",
                                    "protocol": "responses",
                                    "error": error.to_string()
                                }),
                            );
                            break;
                        }
                    };
                    let bytes = match chunk {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            let message = format!(
                                "Remote Compaction V2 upstream stream failed before completion: {error}"
                            );
                            remote_compaction_stream_error = Some(message.clone());
                            buffered.clear();
                            buffered.extend_from_slice(
                                crate::layered_compaction::remote_compaction_v2_failure_sse(
                                    request_json.as_ref().expect("bridge request must exist"),
                                    "remote_compaction_stream_error",
                                    &message,
                                )
                                .as_bytes(),
                            );
                            let _ = crate::diagnostic_log::append_diagnostic_log(
                                "remote_compaction_v2.stream_failed",
                                serde_json::json!({
                                    "transport": "http_sse",
                                    "protocol": "responses",
                                    "error": error.to_string()
                                }),
                            );
                            break;
                        }
                    };
                    if first_token_ms.is_none() {
                        let elapsed_ms = started_at.elapsed().as_millis() as u64;
                        first_token_ms = Some(elapsed_ms);
                        append_local_proxy_first_token_record(
                            &log_id,
                            timestamp_ms,
                            method,
                            path,
                            remote_addr_text.clone(),
                            &request_metadata,
                            relay_id.clone(),
                            relay_name.clone(),
                            endpoint.clone(),
                            Some(response_protocol_label.clone()),
                            status_code,
                            true,
                            request_body,
                            elapsed_ms,
                        );
                    }
                    buffered.extend_from_slice(&bytes);
                }
                buffered
            };
            upstream_model_observer.observe_sse_chunk(&raw_body, response_protocol);
            if first_token_ms.is_none() {
                let elapsed_ms = started_at.elapsed().as_millis() as u64;
                first_token_ms = Some(elapsed_ms);
                append_local_proxy_first_token_record(
                    &log_id,
                    timestamp_ms,
                    method,
                    path,
                    remote_addr_text.clone(),
                    &request_metadata,
                    relay_id.clone(),
                    relay_name.clone(),
                    endpoint.clone(),
                    Some(response_protocol_label.clone()),
                    status_code,
                    true,
                    request_body,
                    elapsed_ms,
                );
            }
            let raw_sse = String::from_utf8_lossy(&raw_body).into_owned();
            let (rewritten, log) = apply_remote_compaction_v2_bridge(
                request_json.as_ref(),
                raw_sse,
                layered_compaction_options,
            );
            layered_compaction_log = log;
            let rewritten = rewritten.into_bytes();
            response_bytes += rewritten.len();
            response_truncated |=
                crate::proxy_log::append_capture(&mut response_capture, &rewritten);
            stream.write_all(&rewritten).await?;
        } else if upstream.body_override.is_some() {
            let raw_body = upstream
                .into_body_bytes_with_idle_timeout(stream_header_timeout)
                .await?;
            upstream_model_observer.observe_sse_chunk(&raw_body, response_protocol);
            let body = crate::image_generation::rewrite_body(
                &raw_body,
                true,
                Some(request_context),
                request_json.as_ref(),
            );
            let elapsed_ms = started_at.elapsed().as_millis() as u64;
            first_token_ms = Some(elapsed_ms);
            append_local_proxy_first_token_record(
                &log_id,
                timestamp_ms,
                method,
                path,
                remote_addr_text.clone(),
                &request_metadata,
                relay_id.clone(),
                relay_name.clone(),
                endpoint.clone(),
                Some(response_protocol_label.clone()),
                status_code,
                true,
                request_body,
                elapsed_ms,
            );
            response_bytes += body.len();
            response_truncated |= crate::proxy_log::append_capture(&mut response_capture, &body);
            stream.write_all(&body).await?;
        } else {
            let mut cache_observer = upstream.take_cache_observer();
            let mut bytes_stream = upstream.into_response()?.bytes_stream();
            let mut images = crate::image_generation::ImageGenerationSseBridge::new(
                crate::image_generation::ImageGenerationBridge::for_request(
                    Some(request_context),
                    request_json.as_ref(),
                ),
            );
            loop {
                let chunk = match next_stream_chunk_with_idle_timeout(
                    &mut bytes_stream,
                    stream_header_timeout,
                )
                .await
                {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
                    Err(error) => {
                        let error_message = format!("Stream error: {error}");
                        let failed = responses_stream_failure_bytes(
                            request_json.as_ref(),
                            Some(&diagnostic_id),
                            Some(response_protocol),
                            error_message.clone(),
                            Some("stream_timeout".to_string()),
                        );
                        response_bytes += failed.len();
                        response_truncated |=
                            crate::proxy_log::append_capture(&mut response_capture, &failed);
                        stream.write_all(&failed).await?;
                        remote_compaction_stream_error = Some(error_message);
                        break;
                    }
                };
                let bytes = match chunk {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        let error_message = format!("Stream error: {error}");
                        let failed = responses_stream_failure_bytes(
                            request_json.as_ref(),
                            Some(&diagnostic_id),
                            Some(response_protocol),
                            error_message.clone(),
                            Some("stream_error".to_string()),
                        );
                        response_bytes += failed.len();
                        response_truncated |=
                            crate::proxy_log::append_capture(&mut response_capture, &failed);
                        stream.write_all(&failed).await?;
                        remote_compaction_stream_error = Some(error_message);
                        break;
                    }
                };
                if let Some(observer) = cache_observer.as_mut() {
                    observer.observe_bytes(&bytes);
                }
                upstream_model_observer.observe_sse_chunk(&bytes, response_protocol);
                if first_token_ms.is_none() {
                    let elapsed_ms = started_at.elapsed().as_millis() as u64;
                    first_token_ms = Some(elapsed_ms);
                    append_local_proxy_first_token_record(
                        &log_id,
                        timestamp_ms,
                        method,
                        path,
                        remote_addr_text.clone(),
                        &request_metadata,
                        relay_id.clone(),
                        relay_name.clone(),
                        endpoint.clone(),
                        Some(response_protocol_label.clone()),
                        status_code,
                        true,
                        request_body,
                        elapsed_ms,
                    );
                }
                let forwarded = images.push_bytes(&bytes);
                response_bytes += forwarded.len();
                response_truncated |=
                    crate::proxy_log::append_capture(&mut response_capture, &forwarded);
                stream.write_all(&forwarded).await?;
            }
            if remote_compaction_stream_error.is_none() {
                let tail = images.finish();
                response_bytes += tail.len();
                response_truncated |=
                    crate::proxy_log::append_capture(&mut response_capture, &tail);
                stream.write_all(&tail).await?;
                if let Some(observer) = cache_observer.take() {
                    observer.finish();
                }
            }
        }
        log_helper_response(
            if remote_compaction_stream_error.is_some() {
                "helper.protocol_proxy_deferred_stream_failed"
            } else {
                "helper.protocol_proxy_deferred_stream_ok"
            },
            method,
            path,
            "200 OK",
            remote_addr_text.clone(),
        );
        upstream_model_observer.finish_sse(response_protocol);
        request_metadata
            .audit
            .observe_response(&upstream_model_observer);
        record_observed_upstream_model(&mut request_metadata, upstream_model_observer.model());
        append_local_proxy_record_with_continue_thinking(
            log_id.clone(),
            timestamp_ms,
            started_at,
            method,
            path,
            remote_addr_text,
            &request_metadata,
            relay_id,
            relay_name,
            endpoint,
            Some(response_protocol_label),
            status_code,
            true,
            request_body,
            &response_capture,
            response_bytes,
            response_truncated,
            remote_compaction_stream_error,
            LocalContinueThinkingLog {
                first_token_ms,
                ..Default::default()
            },
            layered_compaction_log,
        );
        stream.shutdown().await?;
        return Ok(());
    }

    let mut chat_converter;
    let mut anthropic_converter;
    let converter_kind = if response_protocol
        == crate::protocol_proxy::UpstreamResponseProtocol::ChatCompletions
    {
        chat_converter = request_json
            .as_ref()
            .map(|request| {
                crate::protocol_proxy::ChatSseToResponsesConverter::with_request_and_diagnostic_id(
                    request,
                    Some(&diagnostic_id),
                )
            })
            .unwrap_or_default();
        EitherResponsesStreamConverter::Chat(&mut chat_converter)
    } else {
        anthropic_converter = request_json
                .as_ref()
                .map(|request| {
                    crate::protocol_proxy::AnthropicSseToResponsesConverter::with_request_and_diagnostic_id(
                        request,
                        Some(&diagnostic_id),
                    )
                })
                .unwrap_or_default();
        EitherResponsesStreamConverter::Anthropic(&mut anthropic_converter)
    };
    let mut converter = converter_kind;
    let upstream_has_body_override = upstream.body_override.is_some();
    let mut stream_failed = false;
    let mut stream_error = None;
    // 压缩请求必须缓冲完整的转换后 SSE，最后统一改写再一次性写出：
    // - legacy 分层压缩需要把摘要和原始 tail 合并；
    // - Remote Compaction V2 需要把普通 message/tool 输出封装成唯一 compaction item。
    // 缓冲期间不伪造 Responses 事件；Codex 的等待上限由 Provider 配置单独控制。
    let is_legacy_compaction = layered_compaction_options.enabled
        && crate::layered_compaction::is_compaction_request(request_json.as_ref());
    let is_remote_compaction_v2 = bridge_remote_compaction_v2;
    let is_compaction = is_legacy_compaction || is_remote_compaction_v2;
    let mut compaction_buffer = String::new();
    let mut cache_observer = None;

    if upstream_has_body_override {
        let body = upstream
            .into_body_bytes_with_idle_timeout(stream_header_timeout)
            .await?;
        upstream_model_observer.observe_sse_chunk(&body, response_protocol);
        let converted = converter.push_bytes(&body);
        if !converted.is_empty() {
            if first_token_ms.is_none() {
                let elapsed_ms = started_at.elapsed().as_millis() as u64;
                first_token_ms = Some(elapsed_ms);
                append_local_proxy_first_token_record(
                    &log_id,
                    timestamp_ms,
                    method,
                    path,
                    remote_addr_text.clone(),
                    &request_metadata,
                    relay_id.clone(),
                    relay_name.clone(),
                    endpoint.clone(),
                    Some(response_protocol_label.clone()),
                    status_code,
                    true,
                    request_body,
                    elapsed_ms,
                );
            }
            if is_compaction {
                compaction_buffer.push_str(&String::from_utf8_lossy(&converted));
            } else {
                response_bytes += converted.len();
                response_truncated |=
                    crate::proxy_log::append_capture(&mut response_capture, &converted);
                stream.write_all(&converted).await?;
            }
        }
    } else {
        cache_observer = upstream.take_cache_observer();
        let mut bytes_stream = upstream.into_response()?.bytes_stream();
        loop {
            let chunk =
                match next_stream_chunk_with_idle_timeout(&mut bytes_stream, stream_header_timeout)
                    .await
                {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
                    Err(error) => {
                        let error_message = format!("Stream error: {error}");
                        let failed = if is_remote_compaction_v2 {
                            crate::layered_compaction::remote_compaction_v2_failure_sse(
                                request_json
                                    .as_ref()
                                    .expect("Remote Compaction V2 request must exist"),
                                "remote_compaction_stream_error",
                                &error_message,
                            )
                            .into_bytes()
                        } else {
                            converter.fail(error_message.clone(), Some("stream_error".to_string()))
                        };
                        if !failed.is_empty() {
                            let failed = if is_compaction && !is_remote_compaction_v2 {
                                compaction_buffer.push_str(&String::from_utf8_lossy(&failed));
                                compaction_buffer.as_bytes()
                            } else {
                                failed.as_slice()
                            };
                            response_bytes += failed.len();
                            response_truncated |=
                                crate::proxy_log::append_capture(&mut response_capture, failed);
                            stream.write_all(failed).await?;
                        }
                        stream_error = Some(error_message);
                        stream_failed = true;
                        break;
                    }
                };
            match chunk {
                Ok(bytes) => {
                    if let Some(observer) = cache_observer.as_mut() {
                        observer.observe_bytes(&bytes);
                    }
                    upstream_model_observer.observe_sse_chunk(&bytes, response_protocol);
                    let converted = converter.push_bytes(&bytes);
                    if !converted.is_empty() {
                        if first_token_ms.is_none() {
                            let elapsed_ms = started_at.elapsed().as_millis() as u64;
                            first_token_ms = Some(elapsed_ms);
                            append_local_proxy_first_token_record(
                                &log_id,
                                timestamp_ms,
                                method,
                                path,
                                remote_addr_text.clone(),
                                &request_metadata,
                                relay_id.clone(),
                                relay_name.clone(),
                                endpoint.clone(),
                                Some(response_protocol_label.clone()),
                                status_code,
                                true,
                                request_body,
                                elapsed_ms,
                            );
                        }
                        if is_compaction {
                            compaction_buffer.push_str(&String::from_utf8_lossy(&converted));
                        } else {
                            response_bytes += converted.len();
                            response_truncated |=
                                crate::proxy_log::append_capture(&mut response_capture, &converted);
                            stream.write_all(&converted).await?;
                        }
                    }
                }
                Err(error) => {
                    let error_message = format!("Stream error: {error}");
                    let failed = if is_remote_compaction_v2 {
                        crate::layered_compaction::remote_compaction_v2_failure_sse(
                            request_json
                                .as_ref()
                                .expect("Remote Compaction V2 request must exist"),
                            "remote_compaction_stream_error",
                            &error_message,
                        )
                        .into_bytes()
                    } else {
                        converter.fail(error_message.clone(), Some("stream_error".to_string()))
                    };
                    if !failed.is_empty() {
                        let failed = if is_compaction && !is_remote_compaction_v2 {
                            compaction_buffer.push_str(&String::from_utf8_lossy(&failed));
                            compaction_buffer.as_bytes()
                        } else {
                            failed.as_slice()
                        };
                        response_bytes += failed.len();
                        response_truncated |=
                            crate::proxy_log::append_capture(&mut response_capture, failed);
                        stream.write_all(&failed).await?;
                    }
                    stream_error = Some(error_message);
                    stream_failed = true;
                    break;
                }
            }
        }
    }

    if !stream_failed && let Some(observer) = cache_observer.take() {
        observer.finish();
    }
    if !stream_failed {
        let tail = converter.finish();
        if is_compaction {
            compaction_buffer.push_str(&String::from_utf8_lossy(&tail));
            let rewritten = if is_remote_compaction_v2 {
                let (rewritten, log) = apply_remote_compaction_v2_bridge(
                    request_json.as_ref(),
                    compaction_buffer,
                    layered_compaction_options,
                );
                layered_compaction_log = log;
                rewritten
            } else {
                let (rewritten, log) = apply_layered_compaction_if_enabled(
                    request_json.as_ref(),
                    compaction_buffer,
                    layered_compaction_options,
                );
                layered_compaction_log = log;
                rewritten
            };
            let rewritten = rewritten.into_bytes();
            response_bytes += rewritten.len();
            response_truncated |=
                crate::proxy_log::append_capture(&mut response_capture, &rewritten);
            stream.write_all(&rewritten).await?;
        } else if !tail.is_empty() {
            response_bytes += tail.len();
            response_truncated |= crate::proxy_log::append_capture(&mut response_capture, &tail);
            stream.write_all(&tail).await?;
        }
    }
    let _ = crate::diagnostic_log::append_diagnostic_log(
        "protocol_proxy.stream_conversion_terminal",
        converter.diagnostic_summary(),
    );
    upstream_model_observer.finish_sse(response_protocol);
    request_metadata
        .audit
        .observe_response(&upstream_model_observer);
    record_observed_upstream_model(&mut request_metadata, upstream_model_observer.model());
    log_helper_response(
        if stream_failed {
            "helper.protocol_proxy_deferred_stream_failed"
        } else {
            "helper.protocol_proxy_deferred_stream_ok"
        },
        method,
        path,
        "200 OK",
        remote_addr_text.clone(),
    );
    append_local_proxy_record_with_continue_thinking(
        log_id.clone(),
        timestamp_ms,
        started_at,
        method,
        path,
        remote_addr_text,
        &request_metadata,
        relay_id,
        relay_name,
        endpoint,
        Some(response_protocol_label),
        status_code,
        true,
        request_body,
        &response_capture,
        response_bytes,
        response_truncated,
        stream_error,
        LocalContinueThinkingLog {
            first_token_ms,
            ..Default::default()
        },
        layered_compaction_log,
    );
    stream.shutdown().await?;
    Ok(())
}

async fn handle_chat_completions_proxy_connection(
    stream: &mut tokio::net::TcpStream,
    request_body: &str,
    request_context: &crate::request_headers::RequestContext,
    method: &str,
    path: &str,
    remote_addr_text: Option<String>,
) -> anyhow::Result<()> {
    let started_at = Instant::now();
    let timestamp_ms = crate::proxy_log::current_timestamp_ms();
    let request_json = serde_json::from_str::<serde_json::Value>(request_body).ok();
    let upstream_idle_timeout =
        crate::protocol_proxy::stream_idle_timeout_for_request(request_json.as_ref());
    let original_request_metadata = crate::proxy_log::extract_request_metadata_with_context(
        request_json.as_ref(),
        request_context,
    );
    let log_id = next_local_proxy_log_id();
    append_pending_local_proxy_record(
        &log_id,
        timestamp_ms,
        method,
        path,
        remote_addr_text.clone(),
        &original_request_metadata,
        request_body,
        request_json.as_ref(),
    );
    let upstream = match tokio::time::timeout(
        upstream_idle_timeout,
        crate::protocol_proxy::open_chat_completions_proxy_request(
            request_body,
            request_context.user_agent(),
        ),
    )
    .await
    .context("Chat Completions 上游响应头等待超时")
    .and_then(|result| result)
    {
        Ok(upstream) => upstream,
        Err(error) => {
            let mut failure_metadata = original_request_metadata.clone();
            if let Some(context) = crate::protocol_proxy::upstream_failure_context(&error) {
                failure_metadata = request_metadata_for_upstream(
                    failure_metadata,
                    &original_request_metadata,
                    &context.audit,
                );
            }
            let error_message = crate::protocol_proxy::upstream_error_message(&error);
            let body = serde_json::to_vec(&serde_json::json!({
                "status": "failed",
                "message": error_message
            }))?;
            write_http_response(
                stream,
                "502 Bad Gateway",
                "application/json; charset=utf-8",
                &body,
            )
            .await?;
            log_helper_response(
                "helper.chat_completions_proxy_failed",
                method,
                path,
                "502 Bad Gateway",
                remote_addr_text.clone(),
            );
            append_local_proxy_record(
                log_id.clone(),
                timestamp_ms,
                started_at,
                method,
                path,
                remote_addr_text,
                &failure_metadata,
                None,
                None,
                None,
                None,
                502,
                false,
                request_body,
                &body,
                body.len(),
                false,
                Some(error_message),
            );
            stream.shutdown().await?;
            return Ok(());
        }
    };

    let logged_request_body = if upstream.request_body.trim().is_empty() {
        request_body.to_string()
    } else {
        upstream.request_body.clone()
    };
    let logged_request_json = serde_json::from_str::<serde_json::Value>(&logged_request_body).ok();
    let logged_request_metadata =
        crate::proxy_log::extract_request_metadata(logged_request_json.as_ref());
    let mut request_metadata = request_metadata_for_upstream(
        logged_request_metadata,
        &original_request_metadata,
        &upstream.audit,
    );
    request_metadata.upstream_response_model = upstream.upstream_response_model.clone();
    let request_body = logged_request_body.as_str();

    let relay_id = upstream.relay_id.clone();
    let relay_name = upstream.relay_name.clone();
    let endpoint = upstream.endpoint.clone();
    let response_protocol = upstream.response_protocol;
    let status_code = upstream.status_code;
    let status = upstream.status();
    let is_success = upstream.is_success();
    let content_type = if upstream.content_type.is_empty() {
        "application/json; charset=utf-8".to_string()
    } else {
        upstream.content_type.clone()
    };

    if upstream.is_stream && is_success {
        write_http_stream_headers(stream, &status, &content_type, false).await?;
        let mut bytes_stream = upstream.into_response()?.bytes_stream();
        let mut upstream_model_observer =
            crate::proxy_log::UpstreamResponseModelObserver::default();
        let mut response_capture = Vec::new();
        let mut response_bytes = 0_usize;
        let mut response_truncated = false;
        let mut first_token_ms = None;
        loop {
            let chunk =
                match next_stream_chunk_with_idle_timeout(&mut bytes_stream, upstream_idle_timeout)
                    .await
                {
                    Ok(Some(chunk)) => chunk.map_err(anyhow::Error::from),
                    Ok(None) => break,
                    Err(error) => Err(error),
                };
            let bytes = match chunk {
                Ok(bytes) => bytes,
                Err(error) => {
                    upstream_model_observer.finish_sse(response_protocol);
                    request_metadata
                        .audit
                        .observe_response(&upstream_model_observer);
                    record_observed_upstream_model(
                        &mut request_metadata,
                        upstream_model_observer.model(),
                    );
                    append_local_proxy_record(
                        log_id.clone(),
                        timestamp_ms,
                        started_at,
                        method,
                        path,
                        remote_addr_text,
                        &request_metadata,
                        relay_id,
                        relay_name,
                        endpoint,
                        Some(response_protocol_label(response_protocol)),
                        status_code,
                        true,
                        request_body,
                        &response_capture,
                        response_bytes,
                        response_truncated,
                        Some(format!("Stream error: {error}")),
                    );
                    return Err(error.into());
                }
            };
            upstream_model_observer.observe_sse_chunk(&bytes, response_protocol);
            if first_token_ms.is_none() {
                let elapsed_ms = started_at.elapsed().as_millis() as u64;
                first_token_ms = Some(elapsed_ms);
                append_local_proxy_first_token_record(
                    &log_id,
                    timestamp_ms,
                    method,
                    path,
                    remote_addr_text.clone(),
                    &request_metadata,
                    relay_id.clone(),
                    relay_name.clone(),
                    endpoint.clone(),
                    Some(response_protocol_label(response_protocol)),
                    status_code,
                    true,
                    request_body,
                    elapsed_ms,
                );
            }
            response_bytes += bytes.len();
            response_truncated |= crate::proxy_log::append_capture(&mut response_capture, &bytes);
            stream.write_all(&bytes).await?;
        }
        upstream_model_observer.finish_sse(response_protocol);
        request_metadata
            .audit
            .observe_response(&upstream_model_observer);
        record_observed_upstream_model(&mut request_metadata, upstream_model_observer.model());
        log_helper_response(
            "helper.chat_completions_proxy_stream_ok",
            method,
            path,
            &status,
            remote_addr_text.clone(),
        );
        append_local_proxy_record_with_continue_thinking(
            log_id.clone(),
            timestamp_ms,
            started_at,
            method,
            path,
            remote_addr_text,
            &request_metadata,
            relay_id,
            relay_name,
            endpoint,
            Some(response_protocol_label(response_protocol)),
            status_code,
            true,
            request_body,
            &response_capture,
            response_bytes,
            response_truncated,
            None,
            LocalContinueThinkingLog {
                first_token_ms,
                ..Default::default()
            },
            LocalLayeredCompactionLog::default(),
        );
        stream.shutdown().await?;
        return Ok(());
    }

    let body = upstream
        .into_body_bytes_with_idle_timeout(upstream_idle_timeout)
        .await?;
    if is_success {
        record_observed_upstream_model(
            &mut request_metadata,
            observed_upstream_model(&body, response_protocol, false),
        );
    }
    write_http_response(stream, &status, &content_type, &body).await?;
    log_helper_response(
        if is_success {
            "helper.chat_completions_proxy_ok"
        } else {
            "helper.chat_completions_proxy_upstream_error"
        },
        method,
        path,
        &status,
        remote_addr_text.clone(),
    );
    append_local_proxy_record(
        log_id.clone(),
        timestamp_ms,
        started_at,
        method,
        path,
        remote_addr_text,
        &request_metadata,
        relay_id,
        relay_name,
        endpoint,
        Some(response_protocol_label(response_protocol)),
        status_code,
        false,
        request_body,
        &body,
        body.len(),
        false,
        if is_success {
            None
        } else {
            Some(format!("上游返回 HTTP {status_code}"))
        },
    );
    stream.shutdown().await?;
    Ok(())
}

#[derive(Debug, Clone, Default)]
struct LocalContinueThinkingLog {
    triggered: bool,
    rounds: u32,
    first_token_ms: Option<u64>,
    reasoning_tokens: Option<u64>,
    request_body: Option<String>,
    before_response_body: Option<String>,
    after_response_body: Option<String>,
}

#[derive(Default)]
struct LocalLayeredCompactionLog {
    triggered: bool,
    retain_tokens: Option<u32>,
    retained_items: Option<u32>,
    retained_chars: Option<u32>,
    before_response_body: Option<String>,
}

fn next_local_proxy_log_id() -> String {
    format!("local-{}", uuid::Uuid::new_v4())
}

fn upstream_log_transport(endpoint: Option<&str>) -> crate::proxy_log::ProxyRequestTransport {
    if endpoint
        .is_some_and(|endpoint| endpoint.starts_with("ws://") || endpoint.starts_with("wss://"))
    {
        crate::proxy_log::ProxyRequestTransport::Ws
    } else {
        crate::proxy_log::ProxyRequestTransport::Http
    }
}

#[allow(clippy::too_many_arguments)]
fn append_pending_local_proxy_record(
    id: &str,
    timestamp_ms: u64,
    method: &str,
    path: &str,
    remote_addr_text: Option<String>,
    request_metadata: &crate::proxy_log::RequestMetadata,
    request_body: &str,
    request_json: Option<&serde_json::Value>,
) {
    let stream = request_json
        .and_then(|request| request.get("stream"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let record = crate::proxy_log::ProxyRequestRecord {
        audit: request_metadata.audit.clone(),
        id: id.to_string(),
        state: crate::proxy_log::ProxyRequestState::Pending,
        transport: crate::proxy_log::ProxyRequestTransport::Http,
        timestamp_ms,
        method: method.to_string(),
        path: path.to_string(),
        remote_addr: remote_addr_text,
        model: request_metadata.model.clone(),
        upstream_request_model: request_metadata.upstream_request_model.clone(),
        upstream_response_model: None,
        independent_compaction_model: request_metadata.independent_compaction_model.clone(),
        independent_compaction_usage: request_metadata.independent_compaction_usage,
        reasoning_tokens: None,
        reasoning_effort: request_metadata.reasoning_effort.clone(),
        reasoning_source: request_metadata.reasoning_source.clone(),
        continue_thinking_triggered: false,
        continue_thinking_rounds: 0,
        continue_thinking_request_body: None,
        continue_thinking_before_response_body: None,
        continue_thinking_after_response_body: None,
        remote_compaction_triggered: crate::proxy_log::request_uses_remote_compaction_v2(
            request_json,
        ),
        layered_compaction_triggered: false,
        compaction_requested: request_metadata.compaction_requested,
        layered_compaction_retain_tokens: None,
        layered_compaction_retained_items: None,
        layered_compaction_retained_chars: None,
        layered_compaction_before_response_body: None,
        service_tier: request_metadata.service_tier.clone(),
        relay_id: None,
        relay_name: None,
        endpoint: None,
        response_protocol: None,
        status_code: None,
        first_token_ms: None,
        duration_ms: None,
        stream,
        request_bytes: request_body.len(),
        response_bytes: None,
        response_captured_bytes: None,
        response_truncated: false,
        request_body: request_body.to_string(),
        response_body: String::new(),
        error: None,
    };
    append_local_proxy_log_record(&record);
}

#[allow(clippy::too_many_arguments)]
fn append_local_proxy_first_token_record(
    id: &str,
    timestamp_ms: u64,
    method: &str,
    path: &str,
    remote_addr_text: Option<String>,
    request_metadata: &crate::proxy_log::RequestMetadata,
    relay_id: Option<String>,
    relay_name: Option<String>,
    endpoint: Option<String>,
    response_protocol: Option<String>,
    status_code: u16,
    stream: bool,
    request_body: &str,
    first_token_ms: u64,
) {
    let record = crate::proxy_log::ProxyRequestRecord {
        audit: request_metadata.audit.clone(),
        id: id.to_string(),
        state: crate::proxy_log::ProxyRequestState::Pending,
        transport: upstream_log_transport(endpoint.as_deref()),
        timestamp_ms,
        method: method.to_string(),
        path: path.to_string(),
        remote_addr: remote_addr_text,
        model: request_metadata.model.clone(),
        upstream_request_model: request_metadata.upstream_request_model.clone(),
        upstream_response_model: request_metadata.upstream_response_model.clone(),
        independent_compaction_model: request_metadata.independent_compaction_model.clone(),
        independent_compaction_usage: request_metadata.independent_compaction_usage,
        reasoning_tokens: None,
        reasoning_effort: request_metadata.reasoning_effort.clone(),
        reasoning_source: request_metadata.reasoning_source.clone(),
        continue_thinking_triggered: false,
        continue_thinking_rounds: 0,
        continue_thinking_request_body: None,
        continue_thinking_before_response_body: None,
        continue_thinking_after_response_body: None,
        remote_compaction_triggered: crate::proxy_log::request_body_uses_remote_compaction_v2(
            request_body,
        ),
        layered_compaction_triggered: false,
        compaction_requested: request_metadata.compaction_requested,
        layered_compaction_retain_tokens: None,
        layered_compaction_retained_items: None,
        layered_compaction_retained_chars: None,
        layered_compaction_before_response_body: None,
        service_tier: request_metadata.service_tier.clone(),
        relay_id,
        relay_name,
        endpoint,
        response_protocol,
        status_code: Some(status_code),
        first_token_ms: Some(first_token_ms),
        duration_ms: None,
        stream,
        request_bytes: request_body.len(),
        response_bytes: None,
        response_captured_bytes: None,
        response_truncated: false,
        request_body: request_body.to_string(),
        response_body: String::new(),
        error: None,
    };
    append_local_proxy_log_record(&record);
}

#[allow(clippy::too_many_arguments)]
fn append_local_proxy_record(
    id: String,
    timestamp_ms: u64,
    started_at: Instant,
    method: &str,
    path: &str,
    remote_addr_text: Option<String>,
    request_metadata: &crate::proxy_log::RequestMetadata,
    relay_id: Option<String>,
    relay_name: Option<String>,
    endpoint: Option<String>,
    response_protocol: Option<String>,
    status_code: u16,
    stream: bool,
    request_body: &str,
    response_body: &[u8],
    response_bytes: usize,
    response_truncated: bool,
    error: Option<String>,
) {
    append_local_proxy_record_with_continue_thinking(
        id,
        timestamp_ms,
        started_at,
        method,
        path,
        remote_addr_text,
        request_metadata,
        relay_id,
        relay_name,
        endpoint,
        response_protocol,
        status_code,
        stream,
        request_body,
        response_body,
        response_bytes,
        response_truncated,
        error,
        LocalContinueThinkingLog::default(),
        LocalLayeredCompactionLog::default(),
    )
}

#[allow(clippy::too_many_arguments)]
fn append_local_proxy_record_with_continue_thinking(
    id: String,
    timestamp_ms: u64,
    started_at: Instant,
    method: &str,
    path: &str,
    remote_addr_text: Option<String>,
    request_metadata: &crate::proxy_log::RequestMetadata,
    relay_id: Option<String>,
    relay_name: Option<String>,
    endpoint: Option<String>,
    response_protocol: Option<String>,
    status_code: u16,
    stream: bool,
    request_body: &str,
    response_body: &[u8],
    response_bytes: usize,
    response_truncated: bool,
    error: Option<String>,
    continue_thinking: LocalContinueThinkingLog,
    layered_compaction: LocalLayeredCompactionLog,
) {
    let record = crate::proxy_log::ProxyRequestRecord {
        audit: request_metadata.audit.clone(),
        id,
        state: crate::proxy_log::ProxyRequestState::Completed,
        transport: upstream_log_transport(endpoint.as_deref()),
        timestamp_ms,
        method: method.to_string(),
        path: path.to_string(),
        remote_addr: remote_addr_text,
        model: request_metadata.model.clone(),
        upstream_request_model: request_metadata.upstream_request_model.clone(),
        upstream_response_model: request_metadata.upstream_response_model.clone(),
        independent_compaction_model: request_metadata.independent_compaction_model.clone(),
        independent_compaction_usage: request_metadata.independent_compaction_usage,
        reasoning_tokens: continue_thinking.reasoning_tokens.or_else(|| {
            crate::proxy_log::extract_reasoning_tokens_from_response_body(response_body)
        }),
        reasoning_effort: request_metadata.reasoning_effort.clone(),
        reasoning_source: request_metadata.reasoning_source.clone(),
        continue_thinking_triggered: continue_thinking.triggered,
        continue_thinking_rounds: continue_thinking.rounds,
        continue_thinking_request_body: continue_thinking.request_body,
        continue_thinking_before_response_body: continue_thinking.before_response_body,
        continue_thinking_after_response_body: continue_thinking.after_response_body,
        remote_compaction_triggered: crate::proxy_log::request_body_uses_remote_compaction_v2(
            request_body,
        ),
        layered_compaction_triggered: layered_compaction.triggered,
        compaction_requested: request_metadata.compaction_requested,
        layered_compaction_retain_tokens: layered_compaction.retain_tokens,
        layered_compaction_retained_items: layered_compaction.retained_items,
        layered_compaction_retained_chars: layered_compaction.retained_chars,
        layered_compaction_before_response_body: layered_compaction.before_response_body,
        service_tier: request_metadata.service_tier.clone(),
        relay_id,
        relay_name,
        endpoint,
        response_protocol,
        status_code: Some(status_code),
        first_token_ms: continue_thinking.first_token_ms,
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
        stream,
        request_bytes: request_body.len(),
        // Cancellation can interrupt a write after an unknown number of bytes.
        response_bytes: if status_code == 499 && response_body.is_empty() {
            None
        } else {
            Some(response_bytes)
        },
        response_captured_bytes: Some(response_body.len()),
        response_truncated,
        request_body: request_body.to_string(),
        response_body: String::from_utf8_lossy(response_body).into_owned(),
        error,
    };
    append_local_proxy_log_record(&record);
}

fn append_local_proxy_log_record(record: &crate::proxy_log::ProxyRequestRecord) {
    #[cfg(test)]
    if crate::paths::proxy_log_path_for_tests().is_none() {
        return;
    }
    if let Err(error) = crate::proxy_log::enqueue_record_nonblocking(record) {
        let _ = crate::diagnostic_log::append_diagnostic_log(
            "helper.local_proxy_log_failed",
            serde_json::json!({
                "id": record.id,
                "dropped_intermediate": crate::proxy_log::dropped_intermediate_record_count(),
                "error": error.to_string()
            }),
        );
    }
}

fn response_protocol_label(protocol: crate::protocol_proxy::UpstreamResponseProtocol) -> String {
    serde_json::to_value(protocol)
        .ok()
        .and_then(|value| value.as_str().map(ToString::to_string))
        .unwrap_or_else(|| format!("{protocol:?}"))
}

fn request_metadata_for_upstream(
    mut upstream: crate::proxy_log::RequestMetadata,
    original: &crate::proxy_log::RequestMetadata,
    audit: &crate::proxy_log::ProxyRequestAudit,
) -> crate::proxy_log::RequestMetadata {
    let upstream_model = upstream.model.clone();
    upstream.model = original.model.clone().or_else(|| upstream_model.clone());
    upstream.upstream_request_model = audit
        .upstream_attempts
        .last()
        .map(|attempt| attempt.model.clone())
        .or(upstream_model);
    upstream.compaction_requested |= original.compaction_requested;
    let thread_id = original
        .audit
        .thread_id
        .clone()
        .or(upstream.audit.thread_id);
    let turn_id = original.audit.turn_id.clone().or(upstream.audit.turn_id);
    upstream.audit = audit.clone();
    upstream.audit.thread_id = thread_id;
    upstream.audit.turn_id = turn_id;
    upstream
}

fn record_observed_upstream_model(
    request_metadata: &mut crate::proxy_log::RequestMetadata,
    observed: Option<String>,
) {
    if request_metadata.upstream_response_model.is_none() {
        request_metadata.upstream_response_model = observed;
    }
}

fn observed_upstream_model(
    body: &[u8],
    protocol: crate::protocol_proxy::UpstreamResponseProtocol,
    stream: bool,
) -> Option<String> {
    let mut observer = crate::proxy_log::UpstreamResponseModelObserver::default();
    if stream {
        observer.observe_sse_chunk(body, protocol);
        observer.finish_sse(protocol);
    } else {
        observer.observe_json(body, protocol);
    }
    observer.model()
}

async fn write_http_response(
    stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    status: &str,
    content_type: &str,
    body: &[u8],
) -> anyhow::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.write_all(body).await?;
    Ok(())
}

async fn write_http_stream_headers(
    stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    status: &str,
    content_type: &str,
    reasoning_included: bool,
) -> anyhow::Result<()> {
    // Codex 收到该头后不再本地估算回放的加密推理；仅限 usage 已计入这些推理的 Responses 流。
    let reasoning_included_header = if reasoning_included {
        "X-Reasoning-Included: true\r\n"
    } else {
        ""
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\n{reasoning_included_header}Connection: close\r\n\r\n"
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

async fn write_http_stream_headers_once(
    stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    headers_sent: &mut bool,
    status: &str,
    content_type: &str,
    reasoning_included: bool,
) -> anyhow::Result<()> {
    if *headers_sent {
        return Ok(());
    }
    write_http_stream_headers(stream, status, content_type, reasoning_included).await?;
    stream.flush().await?;
    *headers_sent = true;
    Ok(())
}

fn log_helper_response(
    event: &str,
    method: &str,
    path: &str,
    status: &str,
    remote_addr_text: Option<String>,
) {
    let _ = crate::diagnostic_log::append_diagnostic_log(
        event,
        serde_json::json!({
            "method": method,
            "path": path,
            "status": status,
            "remote_addr": remote_addr_text
        }),
    );
}

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> anyhow::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut chunk = vec![0_u8; 4096];
    let mut header_end = None;
    let mut content_length = 0_usize;
    let deadline = tokio::time::Instant::now() + HELPER_REQUEST_READ_TIMEOUT;

    loop {
        let read = tokio::time::timeout_at(deadline, stream.read(&mut chunk))
            .await
            .context("读取 HTTP 请求超时")??;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if header_end.is_none() {
            if buffer.len() > MAX_HELPER_HEADER_BYTES && find_header_end(&buffer).is_none() {
                anyhow::bail!("HTTP 请求头过大");
            }
            header_end = find_header_end(&buffer);
            if let Some(end) = header_end {
                content_length = content_length_from_headers(&buffer[..end]).unwrap_or(0);
                if content_length > MAX_HELPER_REQUEST_BYTES {
                    anyhow::bail!("HTTP 请求体过大");
                }
            }
        }
        if let Some(end) = header_end {
            if buffer.len() >= end + 4 + content_length {
                break;
            }
        }
        if buffer.len() > MAX_HELPER_REQUEST_BYTES + MAX_HELPER_HEADER_BYTES {
            anyhow::bail!("HTTP 请求过大");
        }
    }

    Ok(buffer)
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn content_length_from_headers(headers: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(headers);
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}

fn http_request_body(request: &str) -> &str {
    request
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or_default()
}
