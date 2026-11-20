// 独立代理管理器复用当前 Elves 核心能力；不包含 Codex 注入、CDP、launcher 或远程桥接。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use elves_core::models::{DeleteResult, DeleteStatus, SessionRef};
use elves_core::settings::{
    BackendSettings, RelayProfile, ResponsesWebsocketCapability, SettingsStore,
};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize)]
pub struct CommandResult<T>
where
    T: Serialize,
{
    pub status: String,
    pub message: String,
    #[serde(flatten)]
    pub payload: T,
}
#[derive(Debug, Clone, Serialize)]
pub struct SettingsPayload {
    pub settings: BackendSettings,
    pub settings_path: String,
    pub codex_home: String,
    pub layered_compaction_default_prompt: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CcsProvidersPayload {
    pub db_path: String,
    pub providers: Vec<elves_core::ccs_import::CcsProviderImport>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSessionsPayload {
    pub db_path: String,
    pub db_paths: Vec<String>,
    pub projectless_workspace_root: String,
    pub sessions: Vec<elves_data::LocalSession>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteLocalSessionRequest {
    pub session_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub db_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayPayload {
    pub authenticated: bool,
    pub auth_source: String,
    pub account_label: Option<String>,
    pub config_path: String,
    pub configured: bool,
    pub requires_openai_auth: bool,
    pub has_bearer_token: bool,
    pub backup_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayFilesPayload {
    pub config_path: String,
    pub auth_path: String,
    pub config_contents: String,
    pub auth_contents: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelaySwitchPayload {
    pub settings: BackendSettings,
    pub relay: RelayPayload,
    pub settings_path: String,
    pub codex_home: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsBackfillPayload {
    pub settings: BackendSettings,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEntriesPayload {
    pub settings: BackendSettings,
    pub entries: elves_core::relay_config::CodexContextEntries,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveContextEntriesPayload {
    pub entries: elves_core::relay_config::CodexContextEntries,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayProfileTestPayload {
    pub http_status: u16,
    pub endpoint: String,
    pub response_preview: String,
    pub first_token_ms: Option<u64>,
    pub output_tokens: Option<u64>,
    pub duration_ms: Option<u64>,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayProfileModelsPayload {
    pub models: Vec<String>,
    pub endpoint: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponsesWebsocketProbePayload {
    pub profile_id: String,
    pub capability: ResponsesWebsocketCapability,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvConflictsPayload {
    pub conflicts: Vec<elves_core::env_conflicts::EnvConflict>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveEnvConflictsRequest {
    pub names: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveEnvConflictsPayload {
    pub removed: Vec<elves_core::env_conflicts::EnvConflictRemoval>,
    pub backup_path: Option<String>,
    pub remaining: Vec<elves_core::env_conflicts::EnvConflict>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveRelayFileRequest {
    pub kind: String,
    pub contents: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackfillRelayProfileRequest {
    pub settings: BackendSettings,
    pub profile_id: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextSettingsRequest {
    pub settings: BackendSettings,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextSyncTargetRequest {
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncLiveContextEntriesRequest {
    pub settings: BackendSettings,
    pub target: ContextSyncTargetRequest,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEntryRequest {
    pub settings: BackendSettings,
    pub kind: String,
    pub id: String,
    pub toml_body: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextDeleteRequest {
    pub settings: BackendSettings,
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogRequest {
    #[serde(default = "default_log_lines")]
    pub lines: usize,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalProxyLogsRequest {
    #[serde(default = "default_log_lines")]
    pub limit: usize,
}

fn default_log_lines() -> usize {
    200
}

struct CommonConfigAnchors {
    root_keys: std::collections::HashSet<String>,
    table_headers: std::collections::HashSet<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalProxyLogDetailRequest {
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalProxyStatusPayload {
    pub enabled: bool,
    pub startup_enabled: bool,
    pub listening: bool,
    pub host: String,
    pub port: u16,
    pub lan_listening: bool,
    pub lan_addresses: Vec<String>,
    pub active_relay_id: String,
    pub active_relay_name: String,
    pub active_relay_mode: String,
    pub aggregate_relay_name: Option<String>,
    pub upstream_base_url: String,
    pub log_path: String,
    pub latest_request_at_ms: Option<u64>,
    pub recent_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalProxyLogsPayload {
    pub path: String,
    pub entries: Vec<elves_core::proxy_log::ProxyRequestSummary>,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalProxyLogDetailPayload {
    pub path: String,
    pub entry: Option<elves_core::proxy_log::ProxyRequestRecord>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayProfileSwitchRequest {
    pub settings: BackendSettings,
    #[serde(default)]
    pub previous_active_relay_id: String,
}

pub fn load_settings() -> CommandResult<SettingsPayload> {
    let backfill_message = backfill_model_catalog_websocket_preferences_after_load();
    settings_payload(&format!("设置已加载。{backfill_message}"), "设置读取失败")
}

fn backfill_model_catalog_websocket_preferences_after_load() -> String {
    let settings = SettingsStore::default().load().unwrap_or_default();
    if !settings.relay_profiles_enabled || settings.active_aggregate_relay_profile().is_some() {
        return String::new();
    }
    let relay = settings.active_relay_profile();
    let home = elves_core::codex_home::codex_home_dir_for_settings(&settings);
    match elves_core::relay_config::backfill_applied_model_catalog_websocket_preferences(
        &home, &relay,
    ) {
        Ok(true) => " 已自动补齐模型目录的 WebSocket 偏好。".to_string(),
        Ok(false) => String::new(),
        Err(error) => format!(" 但模型目录 WebSocket 偏好补齐失败：{error}。"),
    }
}

fn settings_payload(message: &str, failure_context: &str) -> CommandResult<SettingsPayload> {
    match settings_payload_value() {
        Ok(payload) => ok(message, payload),
        Err((error, payload)) => failed(&format!("{failure_context}：{error}"), payload),
    }
}

fn settings_payload_value() -> Result<SettingsPayload, (anyhow::Error, SettingsPayload)> {
    let store = SettingsStore::default();
    let settings_path = elves_core::paths::default_settings_path()
        .to_string_lossy()
        .to_string();
    match store.load() {
        Ok(settings) => Ok(SettingsPayload {
            codex_home: elves_core::codex_home::codex_home_dir_for_settings(&settings)
                .to_string_lossy()
                .to_string(),
            settings,
            settings_path,
            layered_compaction_default_prompt:
                elves_core::layered_compaction::DEFAULT_COMPACTION_PROMPT,
        }),
        Err(error) => Err((
            error,
            SettingsPayload {
                settings: BackendSettings::default(),
                codex_home: elves_core::codex_home::default_codex_home_dir()
                    .to_string_lossy()
                    .to_string(),
                settings_path,
                layered_compaction_default_prompt:
                    elves_core::layered_compaction::DEFAULT_COMPACTION_PROMPT,
            },
        )),
    }
}

fn ok<T: Serialize>(message: &str, payload: T) -> CommandResult<T> {
    CommandResult {
        status: "ok".to_string(),
        message: message.to_string(),
        payload,
    }
}

fn failed<T: Serialize>(message: &str, payload: T) -> CommandResult<T> {
    CommandResult {
        status: "failed".to_string(),
        message: message.to_string(),
        payload,
    }
}

// 用户要求配置由服务统一保存；迁移后不刷新桌面注入、不启动 Codex App 或管理器。
pub async fn save_settings(settings: BackendSettings) -> CommandResult<SettingsPayload> {
    let _guard = settings_write_mutex().lock().await;
    let store = SettingsStore::default();
    let mut settings = normalize_settings_before_save(settings);
    if let Ok(saved) = store.load() {
        merge_saved_responses_websocket_capabilities(&mut settings, &saved);
    }
    if let Err(error) = ensure_codex_home_path_ready(&settings) {
        return failed(
            &format!("配置目录不可用：{error}"),
            settings_payload_value().unwrap_or_else(|(_, payload)| payload),
        );
    }
    if let Err(error) = store.save(&settings) {
        return failed(
            &format!("保存设置失败：{error}"),
            settings_payload_value().unwrap_or_else(|(_, payload)| payload),
        );
    }
    let provider = sync_applied_provider_name_after_settings_save(&settings);
    let base_url = sync_applied_base_url_after_settings_save(&settings);
    let timeout = sync_applied_stream_idle_timeout_after_settings_save(&settings);
    let catalog = sync_applied_model_catalog_after_settings_save(&settings);
    let websocket = sync_applied_websocket_after_settings_save(&settings);
    settings_payload(
        &format!("设置已保存。{provider}{base_url}{timeout}{catalog}{websocket}"),
        "设置回读失败",
    )
}

fn normalize_settings_before_save(mut settings: BackendSettings) -> BackendSettings {
    settings.codex_home_path =
        elves_core::settings::normalize_codex_home_path(&settings.codex_home_path);
    settings.relay_common_config_contents =
        elves_core::relay_config::sanitize_common_config_contents(
            &settings.relay_common_config_contents,
        );
    let (common_without_context, extracted_context) =
        split_relay_context_config_sections(&settings.relay_common_config_contents);
    settings.relay_common_config_contents = common_without_context;
    settings.relay_context_config_contents =
        relay_join_config_sections(&[&settings.relay_context_config_contents, &extracted_context]);
    settings.relay_context_config_contents =
        elves_core::relay_config::sanitize_common_config_contents(
            &settings.relay_context_config_contents,
        );
    for profile in &mut settings.relay_profiles {
        if let Err(error) = elves_core::relay_config::normalize_relay_profile_for_storage(profile) {
            log_manager_event(
                "manager.normalize_relay_profile_for_storage.failed",
                json!({
                    "profileId": profile.id,
                    "profileName": profile.name,
                    "error": error.to_string()
                }),
            );
        }
    }
    let common_config = relay_combined_common_config(&settings);
    if !common_config.trim().is_empty() {
        for profile in &mut settings.relay_profiles {
            if !profile.use_common_config || profile.config_contents.trim().is_empty() {
                continue;
            }
            match elves_core::relay_config::strip_common_config_from_config(
                &profile.config_contents,
                &common_config,
            ) {
                Ok(stripped) => {
                    profile.config_contents =
                        strip_common_config_text_fallback(&stripped, &common_config);
                }
                Err(_) => {
                    profile.config_contents =
                        strip_common_config_text_fallback(&profile.config_contents, &common_config);
                }
            }
        }
    }
    settings.provider_sync_saved_providers =
        normalize_provider_sync_provider_list(settings.provider_sync_saved_providers);
    settings.provider_sync_manual_providers =
        normalize_provider_sync_provider_list(settings.provider_sync_manual_providers);
    settings.provider_sync_last_selected_provider = settings
        .provider_sync_last_selected_provider
        .trim()
        .to_string();
    settings
}

fn normalize_provider_sync_provider_list(values: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
            continue;
        }
        if seen.insert(trimmed.to_string()) {
            result.push(trimmed.to_string());
        }
    }
    result.sort();
    result
}

fn relay_combined_common_config(settings: &BackendSettings) -> String {
    relay_join_config_sections(&[
        &settings.relay_common_config_contents,
        &settings.relay_context_config_contents,
    ])
}

fn relay_join_config_sections(sections: &[&str]) -> String {
    let sections = sections
        .iter()
        .map(|section| section.trim())
        .filter(|section| !section.is_empty())
        .collect::<Vec<_>>();
    if sections.is_empty() {
        String::new()
    } else {
        elves_core::relay_config::normalize_config_text(&format!("{}\n", sections.join("\n\n")))
    }
}

fn split_relay_context_config_sections(config: &str) -> (String, String) {
    let mut common = Vec::new();
    let mut context = Vec::new();
    let mut in_context_table = false;

    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_context_table = trimmed.starts_with("[mcp_servers.")
                || trimmed.starts_with("[skills.")
                || trimmed.starts_with("[plugins.");
        }
        if in_context_table {
            context.push(line);
        } else {
            common.push(line);
        }
    }

    (
        relay_join_config_sections(&[&common.join("\n")]),
        relay_join_config_sections(&[&context.join("\n")]),
    )
}

fn strip_common_config_text_fallback(config_contents: &str, common_config: &str) -> String {
    let common = common_config_anchors(common_config);
    if common.root_keys.is_empty() && common.table_headers.is_empty() {
        return ensure_text_newline(config_contents.trim_end());
    }

    let mut kept = Vec::new();
    let mut skipping_table = false;
    let mut in_root_section = true;
    let mut removed_root_keys = std::collections::HashSet::new();
    let source_root_keys = toml_root_keys_before_first_table(config_contents);

    for line in config_contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_root_section = false;
            let header = trimmed.to_string();
            skipping_table = common.table_headers.contains(&header);
            if skipping_table {
                continue;
            }
        }

        if skipping_table {
            continue;
        }

        if in_root_section && let Some(key) = toml_key_from_line(trimmed) {
            if common.root_keys.contains(key) {
                let is_duplicate_common_key = removed_root_keys.contains(key)
                    || source_root_keys.contains(key)
                    || common.table_headers.contains("[features]")
                    || common
                        .table_headers
                        .contains("[marketplaces.openai-bundled]")
                    || common
                        .table_headers
                        .contains("[plugins.\"superpowers@openai-curated\"]");
                if is_duplicate_common_key {
                    removed_root_keys.insert(key.to_string());
                    continue;
                }
            }
        }

        kept.push(line);
    }

    ensure_text_newline(kept.join("\n").trim_end())
}

fn toml_root_keys_before_first_table(config_contents: &str) -> std::collections::HashSet<String> {
    let mut keys = std::collections::HashSet::new();
    for line in config_contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            break;
        }
        if let Some(key) = toml_key_from_line(trimmed) {
            keys.insert(key.to_string());
        }
    }
    keys
}

fn toml_key_from_line(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (key, _) = trimmed.split_once('=')?;
    let key = key.trim();
    if key.is_empty() { None } else { Some(key) }
}

fn common_config_anchors(common_config: &str) -> CommonConfigAnchors {
    let mut root_keys = std::collections::HashSet::new();
    let mut table_headers = std::collections::HashSet::new();
    let mut in_table = false;

    for line in common_config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_table = true;
            table_headers.insert(trimmed.to_string());
            continue;
        }
        if !in_table {
            if let Some(key) = toml_key_from_line(trimmed) {
                root_keys.insert(key.to_string());
            }
        }
    }

    CommonConfigAnchors {
        root_keys,
        table_headers,
    }
}

fn ensure_text_newline(value: &str) -> String {
    if value.trim().is_empty() {
        String::new()
    } else {
        format!("{}\n", value.trim_end())
    }
}

fn log_manager_event(event: &str, detail: Value) {
    let _ = elves_core::diagnostic_log::append_diagnostic_log(event, detail);
}

fn ensure_codex_home_path_ready(settings: &BackendSettings) -> anyhow::Result<()> {
    let Some(home) = elves_core::codex_home::configured_codex_home_dir(settings) else {
        return Ok(());
    };
    std::fs::create_dir_all(&home).map_err(|error| {
        anyhow::anyhow!(
            "创建 Codex 配置目录 {} 失败：{error}",
            home.to_string_lossy()
        )
    })?;
    let probe = home.join(".codex-elves-write-test.tmp");
    std::fs::write(&probe, b"ok").map_err(|error| {
        anyhow::anyhow!("Codex 配置目录 {} 不可写：{error}", home.to_string_lossy())
    })?;
    let _ = std::fs::remove_file(probe);
    Ok(())
}

fn sync_applied_provider_name_after_settings_save(settings: &BackendSettings) -> String {
    if !settings.relay_profiles_enabled || settings.active_aggregate_relay_profile().is_some() {
        return String::new();
    }
    let relay = settings.active_relay_profile();
    if relay.relay_mode == elves_core::settings::RelayMode::Official && !relay.official_mix_api_key
    {
        return String::new();
    }
    let home = elves_core::codex_home::codex_home_dir_for_settings(settings);
    match elves_core::relay_config::sync_applied_relay_profile_provider_name_to_home(&home, &relay)
    {
        Ok(_) => String::new(),
        Err(error) => format!(" 但 Remote Compaction V2 配置同步失败：{error}。"),
    }
}

fn sync_applied_base_url_after_settings_save(settings: &BackendSettings) -> String {
    if !settings.relay_profiles_enabled || settings.active_aggregate_relay_profile().is_some() {
        return String::new();
    }
    let relay = settings.active_relay_profile();
    if relay.relay_mode == elves_core::settings::RelayMode::Official && !relay.official_mix_api_key
    {
        return String::new();
    }
    let home = elves_core::codex_home::codex_home_dir_for_settings(settings);
    match elves_core::relay_config::sync_applied_relay_profile_base_url_to_home(&home, &relay) {
        Ok(_) => String::new(),
        Err(error) => format!(" 但本地代理地址同步失败：{error}。"),
    }
}

fn sync_applied_stream_idle_timeout_after_settings_save(settings: &BackendSettings) -> String {
    let home = elves_core::codex_home::codex_home_dir_for_settings(settings);
    match sync_applied_stream_idle_timeout_after_settings_save_in_home(&home, settings) {
        Ok(true) => " 本地代理流式空闲超时已同步。".to_string(),
        Ok(false) => String::new(),
        Err(error) => format!(" 但本地代理流式空闲超时同步失败：{error}。"),
    }
}

fn sync_applied_stream_idle_timeout_after_settings_save_in_home(
    home: &Path,
    settings: &BackendSettings,
) -> anyhow::Result<bool> {
    if !settings.active_relay_uses_protocol_proxy() {
        return Ok(false);
    }
    let relay = settings.active_relay_profile();
    elves_core::relay_config::ensure_applied_local_proxy_stream_idle_timeout_to_home(home, &relay)
}

fn sync_applied_model_catalog_after_settings_save(settings: &BackendSettings) -> String {
    if !settings.relay_profiles_enabled || settings.active_aggregate_relay_profile().is_some() {
        return String::new();
    }
    let home = elves_core::codex_home::codex_home_dir_for_settings(settings);
    let relay = settings.active_relay_profile();
    match elves_core::relay_config::sync_applied_relay_profile_model_catalog_to_home(&home, &relay)
    {
        Ok(true) => " 模型目录已同步。".to_string(),
        Ok(false) => String::new(),
        Err(error) => format!(" 但模型目录同步失败：{error}。"),
    }
}

fn sync_applied_websocket_after_settings_save(settings: &BackendSettings) -> String {
    if !settings.relay_profiles_enabled || settings.active_aggregate_relay_profile().is_some() {
        return String::new();
    }
    let relay = settings.active_relay_profile();
    if relay.relay_mode == elves_core::settings::RelayMode::Official && !relay.official_mix_api_key
    {
        return String::new();
    }
    let home = elves_core::codex_home::codex_home_dir_for_settings(settings);
    match sync_relay_websocket_to_home(&home, settings, &relay) {
        Ok(_) => String::new(),
        Err(error) => format!(" 但 WebSocket 配置同步失败：{error}。"),
    }
}

fn sync_relay_websocket_to_home(
    home: &Path,
    settings: &BackendSettings,
    relay: &RelayProfile,
) -> anyhow::Result<bool> {
    elves_core::relay_config::sync_applied_relay_profile_websocket_to_home_with_enabled(
        home,
        relay,
        elves_core::responses_websocket::relay_websocket_enabled_for_settings(settings, relay),
    )
}

fn settings_write_mutex() -> &'static tokio::sync::Mutex<()> {
    static SETTINGS_WRITE_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    SETTINGS_WRITE_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn merge_saved_responses_websocket_capabilities(
    settings: &mut BackendSettings,
    saved_settings: &BackendSettings,
) {
    for target in &mut settings.relay_profiles {
        let explicitly_reset = target.responses_websocket.state
            == elves_core::settings::ResponsesWebsocketCapabilityState::Unknown
            && target.responses_websocket.endpoint.trim().is_empty();
        elves_core::responses_websocket::normalize_responses_websocket_capability(target);
        if explicitly_reset {
            continue;
        }
        let Some(saved) = saved_settings
            .relay_profiles
            .iter()
            .find(|profile| profile.id == target.id)
        else {
            continue;
        };
        let mut saved = saved.clone();
        elves_core::responses_websocket::normalize_responses_websocket_capability(&mut saved);
        if saved.responses_websocket.endpoint == target.responses_websocket.endpoint {
            target.responses_websocket = saved.responses_websocket;
        }
    }
}

pub fn load_ccs_providers() -> CommandResult<CcsProvidersPayload> {
    let db_path = elves_core::ccs_import::default_ccs_db_path();
    match elves_core::ccs_import::list_codex_providers_from_db(&db_path) {
        Ok(providers) => ok(
            &format!(
                "已读取 cc-switch Codex 供应商配置：{} 个。",
                providers.len()
            ),
            CcsProvidersPayload {
                db_path: db_path.to_string_lossy().to_string(),
                providers,
            },
        ),
        Err(error) => failed(
            &format!("读取 cc-switch 供应商配置失败：{error}"),
            CcsProvidersPayload {
                db_path: db_path.to_string_lossy().to_string(),
                providers: Vec::new(),
            },
        ),
    }
}

pub fn import_ccs_providers() -> CommandResult<SettingsPayload> {
    let providers = match elves_core::ccs_import::list_codex_providers_from_default_db() {
        Ok(providers) => providers,
        Err(error) => {
            let payload = settings_payload_value().unwrap_or_else(|(_, payload)| payload);
            return failed(&format!("读取 cc-switch 供应商配置失败：{error}"), payload);
        }
    };

    let store = SettingsStore::default();
    let mut settings = store.load().unwrap_or_default();
    let mut existing_keys: Vec<String> = settings
        .relay_profiles
        .iter()
        .map(elves_core::ccs_import::imported_provider_identity)
        .collect();
    let mut existing_ids: Vec<String> = settings
        .relay_profiles
        .iter()
        .map(|profile| profile.id.clone())
        .collect();
    let mut imported = 0usize;

    for provider in providers {
        let key = elves_core::ccs_import::provider_identity_from_ccs(&provider);
        if existing_keys.iter().any(|existing| existing == &key) {
            continue;
        }
        let profile = elves_core::ccs_import::relay_profile_from_ccs(&provider, &existing_ids);
        existing_ids.push(profile.id.clone());
        existing_keys.push(key);
        settings.relay_profiles.push(profile);
        imported += 1;
    }

    if imported == 0 {
        return settings_payload("没有新的 cc-switch 供应商配置需要导入。", "设置读取失败");
    }

    settings = normalize_settings_before_save(settings);
    match store.save(&settings) {
        Ok(()) => settings_payload(
            &format!("已从 cc-switch 导入供应商配置：{imported} 个。"),
            "导入供应商配置后重新读取设置失败",
        ),
        Err(error) => failed(
            &format!("保存 cc-switch 供应商配置失败：{error}"),
            settings_payload_value().unwrap_or_else(|(_, payload)| payload),
        ),
    }
}

pub fn list_local_sessions() -> CommandResult<LocalSessionsPayload> {
    let home = saved_codex_home_dir();
    let db_paths = elves_core::codex_sqlite::codex_session_db_paths_from_home(&home);
    let mut sessions = Vec::new();
    let mut errors = Vec::new();
    let projectless_workspace_root = match elves_core::codex_home::projectless_workspace_root(&home)
    {
        Ok(path) => path.to_string_lossy().to_string(),
        Err(error) => {
            errors.push(format!("读取无项目任务文件夹失败：{error}"));
            String::new()
        }
    };
    for db_path in &db_paths {
        let adapter = local_session_adapter(db_path);
        match adapter.list_local_sessions() {
            Ok(mut items) => sessions.append(&mut items),
            Err(error) if db_path.exists() => {
                errors.push(format!("{}: {error}", db_path.to_string_lossy()));
            }
            Err(_) => {}
        }
    }
    sessions.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| right.id.cmp(&left.id))
    });
    let mut seen_session_ids = std::collections::HashSet::new();
    sessions.retain(|session| seen_session_ids.insert(session.id.clone()));
    let dominant_db_path = dominant_session_db_path(&sessions, &db_paths);
    let payload = LocalSessionsPayload {
        db_path: dominant_db_path,
        projectless_workspace_root,
        db_paths: db_paths
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect(),
        sessions,
    };
    if errors.is_empty() {
        ok(
            &format!("已读取 {} 个本地会话。", payload.sessions.len()),
            payload,
        )
    } else {
        failed(
            &format!("读取部分本地会话失败：{}", errors.join("; ")),
            payload,
        )
    }
}

pub fn delete_local_session(
    request: DeleteLocalSessionRequest,
) -> CommandResult<DeleteResult> {
    let session = match SessionRef::new(request.session_id.trim(), request.title.trim()) {
        Ok(session) => session,
        Err(error) => {
            return failed(
                &format!("会话标识无效：{error}"),
                DeleteResult {
                    status: DeleteStatus::Failed,
                    session_id: request.session_id,
                    message: error.to_string(),
                },
            );
        }
    };
    let db_paths = if let Some(path) = request.db_path.filter(|value| !value.trim().is_empty()) {
        vec![PathBuf::from(path)]
    } else {
        let home = saved_codex_home_dir();
        elves_core::codex_sqlite::codex_session_db_paths_from_home(&home)
    };
    let result = elves_data::delete_local_from_paths(db_paths, &session);
    let message = match result.status {
        DeleteStatus::LocalDeleted | DeleteStatus::ServerDeleted => "本地会话已删除。",
        DeleteStatus::Partial => "本地会话已删除，但部分记录清理失败。",
        DeleteStatus::NotFound => "未找到指定本地会话。",
        DeleteStatus::Failed => "本地会话删除失败。",
    };
    CommandResult {
        status: if matches!(
            result.status,
            DeleteStatus::LocalDeleted | DeleteStatus::ServerDeleted
        ) {
            "ok"
        } else {
            "failed"
        }
        .to_string(),
        message: message.to_string(),
        payload: result,
    }
}

fn local_session_adapter(db_path: &Path) -> elves_data::SQLiteStorageAdapter {
    elves_data::SQLiteStorageAdapter::new(db_path)
}

fn dominant_session_db_path(sessions: &[elves_data::LocalSession], db_paths: &[PathBuf]) -> String {
    let mut counts = BTreeMap::<String, usize>::new();
    for session in sessions {
        *counts.entry(session.db_path.clone()).or_default() += 1;
    }
    let max_count = counts.values().copied().max().unwrap_or_default();
    sessions
        .iter()
        .find(|session| counts.get(&session.db_path).copied() == Some(max_count))
        .map(|session| session.db_path.clone())
        .unwrap_or_else(|| {
            db_paths
                .first()
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_default()
        })
}

fn saved_codex_home_dir() -> PathBuf {
    let settings = SettingsStore::default().load().unwrap_or_default();
    elves_core::codex_home::codex_home_dir_for_settings(&settings)
}

pub async fn local_proxy_status() -> CommandResult<LocalProxyStatusPayload> {
    let (settings, settings_load_error) = match SettingsStore::default().load() {
        Ok(settings) => (settings, None),
        Err(error) => {
            let message = error.to_string();
            let _ = elves_core::diagnostic_log::append_diagnostic_log(
                "manager.local_proxy_status_settings_load_failed",
                json!({
                    "message": message
                }),
            );
            (BackendSettings::default(), Some(message))
        }
    };
    let active_relay = settings.active_relay_profile();
    let active_aggregate = settings.active_aggregate_relay_profile();
    let enabled = settings.active_relay_uses_protocol_proxy();
    let port = elves_core::protocol_proxy::DEFAULT_PROTOCOL_PROXY_PORT;
    let listening = elves_core::ports::loopback_listener_is_running(port).await;
    // 用户要求单独展示“随 Elves 启动”策略；监听状态仍只取当前独立服务的真实端口。
    let startup_enabled = settings.local_proxy_startup_enabled;
    let lan_listening = listening && elves_core::local_proxy_service::lan_listening();
    let lan_addresses = if lan_listening {
        elves_core::lan_proxy::lan_ipv4_addresses()
            .into_iter()
            .map(|address| address.to_string())
            .collect()
    } else {
        Vec::new()
    };
    let summaries = tokio::task::spawn_blocking(|| elves_core::proxy_log::read_summaries(200))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let latest_request_at_ms = summaries.first().map(|entry| entry.timestamp_ms);
    let recent_count = summaries.len();
    let upstream_base_url = if active_relay.relay_mode == elves_core::settings::RelayMode::Aggregate
    {
        format!(
            "聚合供应商：{} 个成员",
            active_aggregate
                .as_ref()
                .map(|profile| profile.members.len())
                .unwrap_or(0)
        )
    } else if active_relay.base_url.trim().is_empty() {
        active_relay.upstream_base_url.clone()
    } else {
        active_relay.base_url.clone()
    };
    // 内置后台启动失败时展示实际原因，不能只沿用旧 launcher 的泛化监听提示。
    let startup_error = elves_core::local_proxy_service::last_error();
    let message = if !enabled {
        "当前供应商未启用本地代理。"
    } else if enabled && lan_listening {
        "本地代理正在通过局域网监听。"
    } else if enabled && listening {
        "本地代理正在监听。"
    } else if !startup_enabled {
        "本地代理已启用，但已关闭随 Elves 启动。"
    } else {
        startup_error
            .as_deref()
            .unwrap_or("本地代理已启用，但当前未监听端口。")
    };
    let _ = elves_core::diagnostic_log::append_diagnostic_log(
        "manager.local_proxy_status",
        json!({
            "enabled": enabled,
            "startupEnabled": startup_enabled,
            "listening": listening,
            "message": message,
            "port": port,
            "relayProfilesEnabled": settings.relay_profiles_enabled,
            "activeRelayId": active_relay.id,
            "activeRelayName": active_relay.name,
            "activeRelayMode": active_relay.relay_mode,
            "activeRelayLocalProxyEnabled": active_relay.local_proxy_enabled(),
            "aggregateRelayActive": active_aggregate.is_some(),
            "lanProxyEnabled": settings.lan_proxy_enabled,
            "settingsLoadError": settings_load_error
        }),
    );
    let payload = LocalProxyStatusPayload {
        enabled,
        startup_enabled,
        listening,
        host: "127.0.0.1".to_string(),
        port,
        lan_listening,
        lan_addresses,
        active_relay_id: active_relay.id,
        active_relay_name: active_relay.name,
        active_relay_mode: serde_json::to_value(active_relay.relay_mode)
            .ok()
            .and_then(|value| value.as_str().map(ToString::to_string))
            .unwrap_or_else(|| "unknown".to_string()),
        aggregate_relay_name: active_aggregate.map(|profile| profile.name),
        upstream_base_url,
        log_path: elves_core::proxy_log::default_log_path()
            .to_string_lossy()
            .to_string(),
        latest_request_at_ms,
        recent_count,
    };
    ok(message, payload)
}

pub async fn read_local_proxy_logs(
    request: LocalProxyLogsRequest,
) -> CommandResult<LocalProxyLogsPayload> {
    let path = elves_core::proxy_log::default_log_path();
    let limit = request.limit.clamp(1, 1000);
    match tokio::task::spawn_blocking(move || elves_core::proxy_log::read_summaries(limit)).await {
        Ok(Ok(entries)) => ok(
            "本地代理日志已读取。",
            LocalProxyLogsPayload {
                path: path.to_string_lossy().to_string(),
                entries,
                limit,
            },
        ),
        Ok(Err(error)) => failed(
            &format!("读取本地代理日志失败：{error}"),
            LocalProxyLogsPayload {
                path: path.to_string_lossy().to_string(),
                entries: Vec::new(),
                limit,
            },
        ),
        Err(error) => failed(
            &format!("读取本地代理日志后台任务失败：{error}"),
            LocalProxyLogsPayload {
                path: path.to_string_lossy().to_string(),
                entries: Vec::new(),
                limit,
            },
        ),
    }
}

pub async fn read_local_proxy_log_detail(
    request: LocalProxyLogDetailRequest,
) -> CommandResult<LocalProxyLogDetailPayload> {
    let path = elves_core::proxy_log::default_log_path();
    match tokio::task::spawn_blocking(move || elves_core::proxy_log::find_record(&request.id)).await
    {
        Ok(Ok(entry)) => {
            let message = if entry.is_some() {
                "本地代理日志详情已读取。"
            } else {
                "未找到指定本地代理日志。"
            };
            ok(
                message,
                LocalProxyLogDetailPayload {
                    path: path.to_string_lossy().to_string(),
                    entry,
                },
            )
        }
        Ok(Err(error)) => failed(
            &format!("读取本地代理日志详情失败：{error}"),
            LocalProxyLogDetailPayload {
                path: path.to_string_lossy().to_string(),
                entry: None,
            },
        ),
        Err(error) => failed(
            &format!("读取本地代理日志详情后台任务失败：{error}"),
            LocalProxyLogDetailPayload {
                path: path.to_string_lossy().to_string(),
                entry: None,
            },
        ),
    }
}

pub async fn clear_local_proxy_logs() -> CommandResult<LocalProxyLogsPayload> {
    let path = elves_core::proxy_log::default_log_path();
    match tokio::task::spawn_blocking(elves_core::proxy_log::clear_records).await {
        Ok(Ok(())) => ok(
            "本地代理日志已清空。",
            LocalProxyLogsPayload {
                path: path.to_string_lossy().to_string(),
                entries: Vec::new(),
                limit: 0,
            },
        ),
        Ok(Err(error)) => failed(
            &format!("清空本地代理日志失败：{error}"),
            LocalProxyLogsPayload {
                path: path.to_string_lossy().to_string(),
                entries: Vec::new(),
                limit: 0,
            },
        ),
        Err(error) => failed(
            &format!("清空本地代理日志后台任务失败：{error}"),
            LocalProxyLogsPayload {
                path: path.to_string_lossy().to_string(),
                entries: Vec::new(),
                limit: 0,
            },
        ),
    }
}

pub fn read_relay_files() -> CommandResult<RelayFilesPayload> {
    let home = saved_codex_home_dir();
    match relay_files_payload_from_home(&home) {
        Ok(payload) => ok("配置文件内容已读取。", payload),
        Err(error) => failed(
            &format!("读取配置文件失败：{error}"),
            RelayFilesPayload {
                config_path: home.join("config.toml").to_string_lossy().to_string(),
                auth_path: home.join("auth.json").to_string_lossy().to_string(),
                config_contents: String::new(),
                auth_contents: String::new(),
            },
        ),
    }
}

fn relay_files_payload_from_home(home: &std::path::Path) -> anyhow::Result<RelayFilesPayload> {
    let config_path = home.join("config.toml");
    let auth_path = home.join("auth.json");
    Ok(RelayFilesPayload {
        config_path: config_path.to_string_lossy().to_string(),
        auth_path: auth_path.to_string_lossy().to_string(),
        config_contents: read_optional_text_file(&config_path)?,
        auth_contents: read_optional_text_file(&auth_path)?,
    })
}

fn read_optional_text_file(path: &std::path::Path) -> anyhow::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

pub fn check_env_conflicts() -> CommandResult<EnvConflictsPayload> {
    let conflicts = elves_core::env_conflicts::detect_env_conflicts();
    let message = if conflicts.is_empty() {
        "未检测到会覆盖 Codex 供应商配置的 OPENAI 环境变量。"
    } else {
        "检测到可能覆盖 Codex 供应商配置的 OPENAI 环境变量。"
    };
    ok(message, EnvConflictsPayload { conflicts })
}

pub fn remove_env_conflicts(
    request: RemoveEnvConflictsRequest,
) -> CommandResult<RemoveEnvConflictsPayload> {
    let backup_dir = elves_core::paths::default_app_state_dir().join("backups");
    match elves_core::env_conflicts::remove_env_conflicts(&request.names, backup_dir) {
        Ok(result) => {
            let remaining = elves_core::env_conflicts::detect_env_conflicts();
            ok(
                "环境变量已按确认项删除；重新启动 ChatGPT/Codex 应用后生效。",
                RemoveEnvConflictsPayload {
                    removed: result.removed,
                    backup_path: result.backup_path,
                    remaining,
                },
            )
        }
        Err(error) => failed(
            &format!("删除环境变量失败：{error}"),
            RemoveEnvConflictsPayload {
                removed: Vec::new(),
                backup_path: None,
                remaining: elves_core::env_conflicts::detect_env_conflicts(),
            },
        ),
    }
}

pub fn save_relay_file(request: SaveRelayFileRequest) -> CommandResult<RelayFilesPayload> {
    let home = saved_codex_home_dir();
    match save_relay_file_in_home(&home, &request.kind, &request.contents)
        .and_then(|_| relay_files_payload_from_home(&home))
    {
        Ok(payload) => ok("auth.json 已保存。", payload),
        Err(error) => failed(
            &format!("保存 auth.json 失败：{error}"),
            relay_files_payload_from_home(&home).unwrap_or_else(|_| RelayFilesPayload {
                config_path: home.join("config.toml").to_string_lossy().to_string(),
                auth_path: home.join("auth.json").to_string_lossy().to_string(),
                config_contents: String::new(),
                auth_contents: String::new(),
            }),
        ),
    }
}

fn save_relay_file_in_home(
    home: &std::path::Path,
    kind: &str,
    contents: &str,
) -> anyhow::Result<()> {
    let path = match kind {
        "auth" => home.join("auth.json"),
        "config" => anyhow::bail!("供应商配置不再支持直接保存 config.toml"),
        other => anyhow::bail!("未知配置文件类型：{other}"),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    Ok(())
}

pub async fn switch_relay_profile(
    request: RelayProfileSwitchRequest,
) -> CommandResult<RelaySwitchPayload> {
    let _guard = settings_write_mutex().lock().await;
    let store = SettingsStore::default();
    let previous_active_relay_id = request.previous_active_relay_id;
    let mut settings = normalize_settings_before_save(request.settings);
    if let Ok(saved_settings) = store.load() {
        merge_saved_responses_websocket_capabilities(&mut settings, &saved_settings);
    }
    elves_core::responses_websocket::probe_active_relay_responses_websocket_if_needed(
        &mut settings,
    )
    .await;

    let home = elves_core::codex_home::codex_home_dir_for_settings(&settings);
    if let Err(error) = persist_active_responses_websocket_capability(&store, &settings) {
        let status = elves_core::relay_config::relay_status_from_home(&home);
        let persisted_settings = store.load().unwrap_or_default();
        return failed(
            &format!("保存 Responses WebSocket 探测结果失败：{error}"),
            relay_switch_payload(persisted_settings, status, None),
        );
    }
    log_manager_event(
        "manager.switch_relay_profile.start",
        json!({
            "previousActiveRelayId": previous_active_relay_id,
            "targetRelayId": settings.active_relay_id
        }),
    );
    match elves_core::relay_switch::switch_relay_profile_in_home(
        &store,
        &home,
        settings,
        &previous_active_relay_id,
    ) {
        Ok(result) => {
            let status = elves_core::relay_config::relay_status_from_home(&home);
            log_manager_event(
                "manager.switch_relay_profile.ok",
                json!({
                    "targetRelayId": result.settings.active_relay_id,
                    "configured": status.configured,
                    "backupPath": result.backup_path.as_ref()
                }),
            );
            ok(
                "供应商已切换。",
                relay_switch_payload(result.settings, status, result.backup_path),
            )
        }
        Err(error) => {
            let status = elves_core::relay_config::relay_status_from_home(&home);
            let settings = store.load().unwrap_or_default();
            log_manager_event(
                "manager.switch_relay_profile.failed",
                json!({
                    "previousActiveRelayId": previous_active_relay_id,
                    "activeRelayId": settings.active_relay_id,
                    "error": error.to_string()
                }),
            );
            failed(
                &format!("供应商切换失败：{error}"),
                relay_switch_payload(settings, status, None),
            )
        }
    }
}

fn relay_switch_payload(
    settings: BackendSettings,
    status: elves_core::relay_config::RelayStatus,
    backup_path: Option<String>,
) -> RelaySwitchPayload {
    let codex_home = elves_core::codex_home::codex_home_dir_for_settings(&settings)
        .to_string_lossy()
        .to_string();
    RelaySwitchPayload {
        settings,
        relay: relay_payload(status, backup_path),
        settings_path: elves_core::paths::default_settings_path()
            .to_string_lossy()
            .to_string(),
        codex_home,
    }
}

fn relay_payload(
    status: elves_core::relay_config::RelayStatus,
    backup_path: Option<String>,
) -> RelayPayload {
    RelayPayload {
        authenticated: status.authenticated,
        auth_source: status.auth_source,
        account_label: status.account_label,
        config_path: status.config_path,
        configured: status.configured,
        requires_openai_auth: status.requires_openai_auth,
        has_bearer_token: status.has_bearer_token,
        backup_path,
    }
}

fn persist_active_responses_websocket_capability(
    store: &SettingsStore,
    probed_settings: &BackendSettings,
) -> anyhow::Result<()> {
    let Some(probed) = probed_settings
        .relay_profiles
        .iter()
        .find(|profile| profile.id == probed_settings.active_relay_id)
    else {
        return Ok(());
    };
    let mut saved = store.load().unwrap_or_default();
    let Some(target) = saved
        .relay_profiles
        .iter_mut()
        .find(|profile| profile.id == probed.id)
    else {
        return Ok(());
    };
    if target.responses_websocket == probed.responses_websocket {
        return Ok(());
    }
    target.responses_websocket = probed.responses_websocket.clone();
    store.save(&saved)
}

pub async fn test_relay_profile(
    profile: RelayProfile,
    model: Option<String>,
    speed_test: Option<bool>,
) -> CommandResult<RelayProfileTestPayload> {
    let speed_test = speed_test.unwrap_or(false);
    let profile_name = if profile.name.trim().is_empty() {
        "未命名供应商"
    } else {
        profile.name.trim()
    };
    let settings = SettingsStore::default().load().unwrap_or_default();
    let requested_model = model.as_deref().unwrap_or("").trim();
    let test_model: String = if !requested_model.is_empty() {
        requested_model.to_string()
    } else {
        // 用户要求废弃供应商隐藏测试模型；留空使用当前供应商 config.toml 中的模型。
        let from_profile = elves_core::relay_config::relay_profile_model(&profile);
        if from_profile.trim().is_empty() {
            // 供应商没有默认模型时，才使用有明确设置入口的全局测试模型。
            settings.relay_test_model.trim().to_string()
        } else {
            from_profile
        }
    };
    match elves_core::relay_config::test_relay_profile_with_speed_test(
        &profile,
        &test_model,
        speed_test,
    )
    .await
    {
        Ok(result) => {
            let status = if result.http_status < 400 {
                "ok"
            } else {
                "failed"
            };
            CommandResult {
                status: status.to_string(),
                message: if speed_test {
                    format!(
                        "已向「{profile_name}」用模型「{test_model}」发送完整糖果题进行测速，HTTP {}。",
                        result.http_status
                    )
                } else {
                    format!(
                        "已向「{profile_name}」用模型「{test_model}」发送 hi，HTTP {}。",
                        result.http_status
                    )
                },
                payload: RelayProfileTestPayload {
                    http_status: result.http_status,
                    endpoint: result.endpoint,
                    response_preview: result.response_preview,
                    first_token_ms: result.first_token_ms,
                    output_tokens: result.output_tokens,
                    duration_ms: result.duration_ms,
                    request_body: result.request_body,
                    response_body: result.response_body,
                },
            }
        }
        Err(error) => failed(
            &format!("测试「{profile_name}」失败：{error}"),
            RelayProfileTestPayload {
                http_status: 0,
                endpoint: String::new(),
                response_preview: String::new(),
                first_token_ms: None,
                output_tokens: None,
                duration_ms: None,
                request_body: None,
                response_body: None,
            },
        ),
    }
}

pub async fn fetch_relay_profile_models(
    profile: RelayProfile,
) -> CommandResult<RelayProfileModelsPayload> {
    let profile_name = if profile.name.trim().is_empty() {
        "未命名供应商"
    } else {
        profile.name.trim()
    };
    match elves_core::model_catalog::fetch_relay_profile_model_ids(&profile).await {
        Ok((models, endpoint)) => ok(
            &format!("已从「{profile_name}」获取 {} 个模型。", models.len()),
            RelayProfileModelsPayload { models, endpoint },
        ),
        Err(error) => failed(
            &format!("从「{profile_name}」获取模型失败：{error}"),
            RelayProfileModelsPayload {
                models: Vec::new(),
                endpoint: String::new(),
            },
        ),
    }
}

pub async fn probe_relay_profile_responses_websocket(
    mut profile: RelayProfile,
) -> CommandResult<ResponsesWebsocketProbePayload> {
    let _guard = settings_write_mutex().lock().await;
    let profile_name = if profile.name.trim().is_empty() {
        "未命名供应商".to_string()
    } else {
        profile.name.trim().to_string()
    };
    profile.responses_websocket = ResponsesWebsocketCapability::default();
    elves_core::responses_websocket::normalize_responses_websocket_capability(&mut profile);
    if !elves_core::responses_websocket::relay_can_probe_native_responses_websocket(&profile) {
        return failed(
            "当前供应商没有可直接使用原生 Responses 协议的模型，或配置了系统提示词替换。",
            ResponsesWebsocketProbePayload {
                profile_id: profile.id,
                capability: profile.responses_websocket,
            },
        );
    }

    let capability = elves_core::responses_websocket::probe_responses_websocket(&profile).await;
    let store = SettingsStore::default();
    if let Err(error) =
        persist_relay_profile_responses_websocket_capability(&store, &profile.id, &capability)
    {
        return failed(
            &format!("Responses WebSocket 探测完成，但保存结果失败：{error}"),
            ResponsesWebsocketProbePayload {
                profile_id: profile.id,
                capability,
            },
        );
    }
    let sync_warning = match sync_active_responses_websocket_after_probe(&store, &profile.id) {
        Ok(_) => String::new(),
        Err(error) => format!(" 但实时配置或模型目录同步失败：{error}。"),
    };
    let detail = capability.message.trim();
    let message = match capability.state {
        elves_core::settings::ResponsesWebsocketCapabilityState::Supported => {
            format!("「{profile_name}」支持 Responses WebSocket。{detail}{sync_warning}")
        }
        elves_core::settings::ResponsesWebsocketCapabilityState::Unsupported => {
            format!("「{profile_name}」不支持 Responses WebSocket。{detail}{sync_warning}")
        }
        elves_core::settings::ResponsesWebsocketCapabilityState::Unknown => {
            format!(
                "暂时无法确认「{profile_name}」是否支持 Responses WebSocket。{detail}{sync_warning}"
            )
        }
    };
    CommandResult {
        status: if capability.state
            == elves_core::settings::ResponsesWebsocketCapabilityState::Supported
        {
            "ok"
        } else {
            "failed"
        }
        .to_string(),
        message,
        payload: ResponsesWebsocketProbePayload {
            profile_id: profile.id,
            capability,
        },
    }
}

fn persist_relay_profile_responses_websocket_capability(
    store: &SettingsStore,
    profile_id: &str,
    capability: &ResponsesWebsocketCapability,
) -> anyhow::Result<()> {
    let mut saved = store.load().unwrap_or_default();
    let Some(target) = saved
        .relay_profiles
        .iter_mut()
        .find(|profile| profile.id == profile_id)
    else {
        return Ok(());
    };
    if target.responses_websocket == *capability {
        return Ok(());
    }
    target.responses_websocket = capability.clone();
    store.save(&saved)
}

fn sync_active_responses_websocket_after_probe(
    store: &SettingsStore,
    profile_id: &str,
) -> anyhow::Result<bool> {
    let settings = store.load().unwrap_or_default();
    if !settings.relay_profiles_enabled
        || settings.active_relay_id != profile_id
        || settings.active_aggregate_relay_profile().is_some()
    {
        return Ok(false);
    }
    let relay = settings.active_relay_profile();
    let home = elves_core::codex_home::codex_home_dir_for_settings(&settings);
    sync_relay_websocket_to_home(&home, &settings, &relay)
}
