//! Antigravity 5h/7d 官方额度监控与本地转录窗口消耗统计。
//!
//! 边界说明（见 AGENTS.md）：
//! - 5h/7d 剩余额度比例与重置时间严格通过目标实例（本机或所属 WSL 登录环境）的官方 `agy -p "/usage" --output-format json` 获取；
//! - 登录展示邮箱从目标实例本地非敏感 `~/.gemini/google_accounts.json`（`active` 字段）只读获取，绝不读取 `oauth_creds.json`；
//! - 订阅等级标签复用 ACP 官方返回的 `_meta.antigravity.tier.name`（如 `Google AI Pro`）；
//! - 窗口已消耗 Token 与公开价格金额估算仅在用户显式展开弹层（`includeLocal: true`）时只读扫描目标实例本地 `transcript[_full].jsonl`。

use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::Context;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::acp::EnvConfig;
use super::{Antigravity, local_store, now_ms};

const USAGE_REFRESH: Duration = Duration::from_secs(60);
// 临时开关：true 时 `agy -p "/usage"` 仅在请求带 `manualRefresh`（用户手动点击额度弹窗）时执行，见 subscription_usage。
const QUOTA_CLI_MANUAL_ONLY: bool = true;
const USAGE_RETENTION_MS: u64 = 72 * 60 * 60 * 1000;
const QUERY_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_USAGE_OUTPUT: u64 = 256 * 1024;
const PRICE_SOURCE: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const PRICE_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const PRICE_RETRY: Duration = Duration::from_secs(5 * 60);
const MAX_PRICE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TRANSCRIPT_BYTES: u64 = 32 * 1024 * 1024;
const TIMEZONE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_TIMEZONE_PROBE_BYTES: usize = 16 * 1024;
const TIMEZONE_PROBE: &str = r#"printf '__CODEX_ELVES_TZ__%s\n' "${TZ-}"; printf '__CODEX_ELVES_DATE__%s\n' "$(date '+%Y-%m-%d %H:%M:%S %Z %z' 2>/dev/null || true)"; printf '__CODEX_ELVES_LOCALTIME__%s\n' "$(readlink -f /etc/localtime 2>/dev/null || true)""#;

pub(super) struct CachedQuotaUsage {
    pub(super) config: EnvConfig,
    pub(super) checked: Instant,
    pub(super) retained: Option<Value>,
}

pub(super) struct CachedLocalQuotaMetrics {
    pub(super) config: EnvConfig,
    pub(super) checked: Instant,
    pub(super) windows: Value,
    pub(super) retained: Option<(u64, Value)>,
}

#[derive(Default)]
pub(super) struct QuotaPriceCache {
    loaded: bool,
    fetched_at: u64,
    attempted: Option<Instant>,
    models: Value,
}

#[derive(Clone, Debug)]
struct ActiveWindowSpec {
    group_id: String,
    window_key: String,
    start_ms: u64,
    end_ms: u64,
}

impl Antigravity {
    pub(crate) async fn session_subscription_usage(
        self: &Arc<Self>,
        id: &str,
        allow_probe: bool,
        include_local: bool,
        manual_refresh: bool,
    ) -> anyhow::Result<Value> {
        let c = self.conversation(id).await?;
        self.subscription_usage(c.config, allow_probe, include_local, manual_refresh)
            .await
    }

    pub(crate) async fn subscription_usage(
        self: &Arc<Self>,
        config: EnvConfig,
        allow_probe: bool,
        include_local: bool,
        manual_refresh: bool,
    ) -> anyhow::Result<Value> {
        config.validate()?;
        // 用户要求（临时处理）：避免频繁调用官方 `agy -p "/usage"` 触发风控，额度只在用户手动点击额度弹窗时才读取；
        // 弹窗展开后的本地统计、后台轮询等未带 `manualRefresh` 的请求只返回已有缓存，不启动 agy 进程。
        // 官方开放后将 QUOTA_CLI_MANUAL_ONLY 改为 false 即可恢复自动刷新；前端 claude-usage.js 的
        // antigravityUsageManualOnly 须同步核对。本地转录统计仍沿用 allow_probe，不受此开关影响。
        let allow_cli = allow_probe && (manual_refresh || !QUOTA_CLI_MANUAL_ONLY);
        let mut response = self.quota_account_usage(&config, allow_cli).await?;
        if response["usage"].is_object() {
            // 若 ACP 目录缓存中已有订阅等级（如 Google AI Pro），实时附带；尚未探测时在后台异步预热，不阻塞额度返回。
            let inst_id = config.instance_id();
            let cached_tier = self
                .catalogs
                .lock()
                .await
                .get(&inst_id)
                .and_then(|cat| cat.tier.clone());
            if let Some(tier) = cached_tier {
                if let Some(name) = sanitize_subscription_type(tier["name"].as_str()) {
                    response["usage"]["subscriptionType"] = json!(name);
                }
            } else if allow_cli {
                let service = Arc::clone(self);
                let probe_cfg = config.clone();
                tokio::spawn(async move {
                    let _ = service.probe_catalog(&probe_cfg, "", false).await;
                });
            }

            let metrics = if include_local {
                self.local_quota_usage(&config, &response["usage"], allow_probe)
                    .await
            } else {
                self.cached_local_quota_usage(&config, &response["usage"])
                    .await
                    .unwrap_or(Value::Null)
            };
            attach_group_metrics(&mut response["usage"], &metrics);
        }
        Ok(response)
    }

    async fn quota_account_usage(
        self: &Arc<Self>,
        config: &EnvConfig,
        allow_probe: bool,
    ) -> anyhow::Result<Value> {
        let instance = config.instance_id();
        let (cached, refresh) = cached_quota_usage(
            self.quota_usage_cache.lock().await.get(&instance),
            config,
            allow_probe,
        );
        if refresh {
            let lock = self.lock(&format!("quota-usage:{instance}")).await;
            if lock.try_lock().is_ok() {
                let service = Arc::clone(self);
                let cfg = config.clone();
                tokio::spawn(async move {
                    let _ = service.read_quota_account_usage(&cfg).await;
                });
            }
        }
        match cached {
            Some(response) => Ok(response),
            None => self.read_quota_account_usage(config).await,
        }
    }

    async fn read_quota_account_usage(self: &Arc<Self>, config: &EnvConfig) -> anyhow::Result<Value> {
        let instance = config.instance_id();
        let lock = self.lock(&format!("quota-usage:{instance}")).await;
        let _guard = lock.lock().await;
        if let (Some(response), false) = cached_quota_usage(
            self.quota_usage_cache.lock().await.get(&instance),
            config,
            true,
        ) {
            return Ok(response);
        }

        let gemini_root = self.resolve_gemini_root(config).await;
        let account_email = if let Some(root) = gemini_root {
            tokio::task::spawn_blocking(move || local_store::read_google_account_email(&root))
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        let cached_tier_name = self
            .catalogs
            .lock()
            .await
            .get(&instance)
            .and_then(|cat| cat.tier.as_ref())
            .and_then(|t| sanitize_subscription_type(t["name"].as_str()));

        let usage = match native_cli_usage(config).await {
            Ok(raw) => {
                let mut normalized = normalize_cli_usage(
                    &raw,
                    now_ms(),
                    account_email.as_deref(),
                    cached_tier_name.as_deref(),
                    config.is_local(),
                );
                if normalized.is_object()
                    && !config.is_local()
                    && let Some(tz) = native_wsl_time_zone(config).await
                {
                    normalized["timeZone"] = json!(tz);
                }
                normalized
            }
            Err(_) => Value::Null,
        };

        let response = json!({
            "ok": true,
            "backend": "antigravity",
            "instanceId": instance,
            "usage": usage,
        });
        let mut cache = self.quota_usage_cache.lock().await;
        let retained = if response["usage"].is_object() {
            Some(response.clone())
        } else {
            cache
                .get(&instance)
                .filter(|c| c.config == *config)
                .and_then(|c| c.retained.clone())
        };
        cache.insert(
            instance,
            CachedQuotaUsage {
                config: config.clone(),
                checked: Instant::now(),
                retained,
            },
        );
        Ok(response)
    }

    async fn local_quota_usage(
        &self,
        config: &EnvConfig,
        usage: &Value,
        allow_probe: bool,
    ) -> Value {
        let instance = config.instance_id();
        let specs = collect_active_windows(usage);
        let windows_sig = serialize_window_specs(&specs);
        if specs.is_empty() {
            return Value::Null;
        }
        let lock = self.lock(&format!("quota-local:{instance}")).await;
        let _guard = lock.lock().await;
        let recent = self
            .local_quota_cache
            .lock()
            .await
            .get(&instance)
            .is_some_and(|cached| {
                cached.config == *config
                    && cached.windows == windows_sig
                    && cached.checked.elapsed() < USAGE_REFRESH
            });
        if recent || !allow_probe {
            return self
                .cached_local_quota_usage(config, usage)
                .await
                .unwrap_or(Value::Null);
        }
        let Some(gemini_root) = self.resolve_gemini_root(config).await else {
            return Value::Null;
        };
        let prices = self.quota_public_prices().await;
        let now = now_ms();
        let metrics = tokio::task::spawn_blocking(move || {
            scan_local_transcript_windows(&gemini_root, &specs, &prices, now)
        })
        .await
        .unwrap_or(Value::Null);
        let succeeded = metrics
            .as_object()
            .is_some_and(|map| !map.is_empty());
        let mut cache = self.local_quota_cache.lock().await;
        let retained = if succeeded {
            Some((now_ms(), metrics.clone()))
        } else {
            cache
                .get(&instance)
                .filter(|c| c.config == *config && c.windows == windows_sig)
                .and_then(|c| c.retained.clone())
        };
        cache.insert(
            instance,
            CachedLocalQuotaMetrics {
                config: config.clone(),
                checked: Instant::now(),
                windows: windows_sig,
                retained: retained.clone(),
            },
        );
        if succeeded {
            metrics
        } else {
            retained.map_or(Value::Null, |(_, m)| m)
        }
    }

    async fn cached_local_quota_usage(&self, config: &EnvConfig, usage: &Value) -> Option<Value> {
        let specs = collect_active_windows(usage);
        let windows_sig = serialize_window_specs(&specs);
        let cache = self.local_quota_cache.lock().await;
        let cached = cache.get(&config.instance_id())?;
        let (collected, metrics) = cached.retained.as_ref()?;
        (cached.config == *config
            && cached.windows == windows_sig
            && within_retention(Some(*collected)))
        .then(|| metrics.clone())
    }

    async fn quota_public_prices(&self) -> Value {
        let mut cache = self.quota_prices.lock().await;
        let path = self.directory.join("claude-public-model-prices.json");
        let now = now_ms();
        if !cache.loaded {
            cache.loaded = true;
            if let Ok(bytes) = std::fs::read(&path)
                && bytes.len() <= MAX_PRICE_BYTES
                && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                && value["source"] == PRICE_SOURCE
                && let Some(fetched) = value["fetchedAtMs"]
                    .as_u64()
                    .filter(|at| *at <= now + 30_000)
                && let Some(models) = normalize_prices(&value["models"])
            {
                cache.fetched_at = fetched;
                cache.models = models;
            }
        }
        if (cache.models.is_null() || now.saturating_sub(cache.fetched_at) >= PRICE_TTL_MS)
            && cache.attempted.is_none_or(|at| at.elapsed() >= PRICE_RETRY)
        {
            cache.attempted = Some(Instant::now());
            if let Ok(models) = download_prices().await {
                cache.fetched_at = now_ms();
                cache.models = models;
                let stored = json!({
                    "source": PRICE_SOURCE,
                    "fetchedAtMs": cache.fetched_at,
                    "models": cache.models,
                });
                if let Ok(bytes) = serde_json::to_vec(&stored) {
                    let _ = crate::settings::atomic_write(&path, &bytes);
                }
            }
        }
        cache.models.clone()
    }
}

/// 当 ACP `setup` 意外未返回模型列表时，通过官方 `agy models` 命令兜底解析可用模型。
pub(super) async fn probe_models_cli(config: &EnvConfig) -> Option<Value> {
    let mut cmd = build_isolated_cli_command(config).ok()?;
    cmd.arg("models");
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let text = tokio::time::timeout(Duration::from_secs(10), async {
        let mut out = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            stdout.take(64 * 1024).read_to_end(&mut out).await.ok()?;
        }
        child.wait().await.ok()?.success().then_some(out)
    })
    .await
    .ok()??;
    parse_agy_models_stdout(&String::from_utf8_lossy(&text))
}

pub(crate) fn parse_agy_models_stdout(stdout: &str) -> Option<Value> {
    let mut rows = Vec::new();
    let mut default_model = String::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("Available") {
            continue;
        }
        let is_default = trimmed.contains("(default)");
        let token = trimmed
            .trim_start_matches('*')
            .trim_start_matches('-')
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim();
        if token.is_empty()
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            continue;
        }
        if is_default || default_model.is_empty() {
            default_model = token.to_owned();
        }
        rows.push(json!({
            "id": token,
            "model": token,
            "displayName": token,
            "description": "",
            "isDefault": is_default,
            "supportedReasoningEfforts": [],
            "defaultReasoningEffort": "",
        }));
    }
    (!rows.is_empty()).then(|| {
        json!({
            "data": rows,
            "defaultModel": default_model,
            "defaultReasoningEffort": "",
            "catalogComplete": true,
        })
    })
}

fn build_isolated_cli_command(config: &EnvConfig) -> anyhow::Result<Command> {
    config.validate()?;
    // 用户要求额度、模型查询全程无闪窗；本机 CLI 与 WSL 包装入口共用创建时隐藏配置。
    if config.is_local() {
        let mut cmd = Command::from(elves_core::process::hidden_command(&config.executable));
        if let Some(base) = directories::BaseDirs::new() {
            let home = base.home_dir();
            if home.is_dir() {
                cmd.current_dir(home);
            }
        }
        Ok(cmd)
    } else {
        anyhow::ensure!(cfg!(windows), "WSL Antigravity 实例需要 Windows WSL2");
        let mut cmd = Command::from(elves_core::process::hidden_command("wsl.exe"));
        cmd.args([
            "--distribution",
            &config.distribution,
            "--user",
            &config.user,
            "--cd",
            "~",
            "--exec",
            "/bin/bash",
            "-lc",
            "exec \"$@\"",
            "--",
            &config.executable,
        ]);
        Ok(cmd)
    }
}

async fn native_cli_usage(config: &EnvConfig) -> anyhow::Result<Value> {
    let mut cmd = build_isolated_cli_command(config)?;
    cmd.args(["-p", "/usage", "--output-format", "json"]);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("无法启动 Antigravity /usage 读取进程")?;
    let outcome = tokio::time::timeout(QUERY_TIMEOUT, async {
        let mut stdout = child.stdout.take().context("Antigravity stdout 不可用")?;
        let mut bytes = Vec::new();
        (&mut stdout)
            .take(MAX_USAGE_OUTPUT + 1)
            .read_to_end(&mut bytes)
            .await?;
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_USAGE_OUTPUT,
            "Antigravity /usage 输出超过限制"
        );
        anyhow::ensure!(
            child.wait().await?.success(),
            "Antigravity /usage 进程未成功结束"
        );
        let text = String::from_utf8_lossy(&bytes);
        let json_slice = text
            .find('{')
            .map(|idx| &text[idx..])
            .unwrap_or(text.as_ref());
        let doc: Value = serde_json::from_str(json_slice.trim())?;
        Ok::<Value, anyhow::Error>(doc)
    })
    .await;
    if outcome.is_err() {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    }
    outcome.context("Antigravity /usage 读取超时")?
}

fn sanitize_subscription_type(raw: Option<&str>) -> Option<String> {
    let kind = raw?.trim();
    if !kind.is_empty()
        && kind.len() <= 64
        && kind.as_bytes()[0].is_ascii_alphanumeric()
        && kind
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b" _+()./-".contains(&byte))
    {
        Some(kind.to_owned())
    } else {
        None
    }
}

pub(crate) fn classify_group_id_and_label(name: &str) -> (String, String) {
    let lower = name.trim().to_ascii_lowercase();
    if lower.contains("gemini") {
        ("gemini".into(), "Gemini".into())
    } else if lower.contains("claude") || lower.contains("gpt") || lower.contains("3p") {
        ("thirdParty".into(), "Claude · GPT".into())
    } else {
        let slug: String = lower
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect::<String>()
            .trim_matches('-')
            .to_owned();
        let id = if slug.is_empty() { "default".into() } else { slug };
        let label = name.trim().chars().take(24).collect::<String>();
        (id, if label.is_empty() { "默认".into() } else { label })
    }
}

pub(crate) fn classify_model_group(model: &str) -> &'static str {
    let lower = model.trim().to_ascii_lowercase();
    if lower.starts_with("claude-")
        || lower.starts_with("gpt-")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
    {
        "thirdParty"
    } else {
        "gemini"
    }
}

fn normalize_bucket(bucket: &Value) -> Option<(&'static str, Value)> {
    let window = bucket["window"].as_str().unwrap_or("");
    let id = bucket["id"].as_str().unwrap_or("");
    let field = if window == "5h" || id.ends_with("-5h") {
        "fiveHour"
    } else if matches!(window, "weekly" | "7d") || id.ends_with("-weekly") {
        "sevenDay"
    } else {
        return None;
    };
    let remaining = bucket["remaining_fraction"]
        .as_f64()
        .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))?;
    let used_percentage = ((1.0 - remaining) * 100.0).clamp(0.0, 100.0);
    // 官方 CLI 对尚未开始计时的满额窗口返回 `1970-01-01T00:00:00Z`（epoch 0）；
    // 必须归一化为 null，避免前端将 epoch 0 判定为已过期的旧窗口而丢弃。
    let reset = bucket["reset_time"]
        .as_str()
        .and_then(|s| {
            let dt = chrono::DateTime::parse_from_rfc3339(s).ok()?;
            (dt.timestamp_millis() > 0).then(|| json!(s))
        })
        .unwrap_or(Value::Null);
    Some((
        field,
        json!({
            "usedPercentage": used_percentage,
            "resetsAt": reset,
        }),
    ))
}

pub(crate) fn normalize_cli_usage(
    raw: &Value,
    observed_at: u64,
    account_email: Option<&str>,
    subscription_type: Option<&str>,
    host_local: bool,
) -> Value {
    if raw["status"] != "SUCCESS" {
        return Value::Null;
    }
    let Some(raw_groups) = raw["command"]["data"]["groups"].as_array() else {
        return Value::Null;
    };
    let mut groups = Vec::new();
    for group in raw_groups {
        let Some(name) = group["name"].as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        let Some(buckets) = group["buckets"].as_array() else {
            continue;
        };
        let (group_id, label) = classify_group_id_and_label(name);
        let mut entry = json!({
            "id": group_id,
            "label": label,
            "name": name,
            "description": group["description"].as_str().unwrap_or("").trim(),
        });
        let mut bucket_count = 0usize;
        for bucket in buckets {
            if let Some((field, normalized)) = normalize_bucket(bucket) {
                entry[field] = normalized;
                bucket_count += 1;
            }
        }
        if bucket_count > 0 {
            groups.push(entry);
        }
    }
    if groups.is_empty() {
        return Value::Null;
    }
    let primary = groups
        .iter()
        .find(|g| g["id"] == "gemini")
        .or_else(|| groups.first())
        .cloned()
        .unwrap();
    let mut result = json!({
        "source": "antigravity-native-usage",
        "observedAtMs": observed_at,
        "hostLocal": host_local,
        "subscriptionType": sanitize_subscription_type(subscription_type),
        "groups": groups,
    });
    if let Some(email) = account_email.map(str::trim).filter(|s| !s.is_empty()) {
        result["accountEmail"] = json!(email);
    }
    for field in ["fiveHour", "sevenDay"] {
        if primary[field].is_object() {
            result[field] = primary[field].clone();
        }
    }
    result
}

fn cached_quota_usage(
    cached: Option<&CachedQuotaUsage>,
    config: &EnvConfig,
    allow_probe: bool,
) -> (Option<Value>, bool) {
    let cached = cached.filter(|c| c.config == *config);
    let recent = cached.is_some_and(|c| c.checked.elapsed() < USAGE_REFRESH);
    let retained = cached
        .and_then(|c| c.retained.as_ref())
        .filter(|resp| within_retention(resp["usage"]["observedAtMs"].as_u64()));
    match retained {
        Some(resp) if allow_probe && !recent => {
            let mut next = resp.clone();
            next["refreshing"] = json!(true);
            (Some(next), true)
        }
        Some(resp) => (Some(resp.clone()), false),
        None if !allow_probe || recent => (
            Some(json!({
                "ok": true,
                "backend": "antigravity",
                "instanceId": config.instance_id(),
                "usage": Value::Null,
            })),
            false,
        ),
        None => (None, false),
    }
}

fn within_retention(observed_at: Option<u64>) -> bool {
    let now = now_ms();
    observed_at.is_some_and(|at| {
        at > 0 && at <= now + 30_000 && now.saturating_sub(at) < USAGE_RETENTION_MS
    })
}

fn collect_active_windows(usage: &Value) -> Vec<ActiveWindowSpec> {
    let mut specs = Vec::new();
    for group in usage["groups"].as_array().into_iter().flatten() {
        let Some(group_id) = group["id"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        for (key, duration_ms) in [
            ("fiveHour", 5 * 60 * 60 * 1000u64),
            ("sevenDay", 7 * 24 * 60 * 60 * 1000u64),
        ] {
            let Some(reset_str) = group[key]["resetsAt"].as_str() else {
                continue;
            };
            let Some(end_ms) = chrono::DateTime::parse_from_rfc3339(reset_str)
                .ok()
                .and_then(|dt| u64::try_from(dt.timestamp_millis()).ok())
                .filter(|&ms| ms > 0)
            else {
                continue;
            };
            specs.push(ActiveWindowSpec {
                group_id: group_id.to_owned(),
                window_key: key.to_owned(),
                start_ms: end_ms.saturating_sub(duration_ms),
                end_ms,
            });
        }
    }
    specs
}

fn serialize_window_specs(specs: &[ActiveWindowSpec]) -> Value {
    json!(
        specs
            .iter()
            .map(|s| json!([s.group_id, s.window_key, s.start_ms, s.end_ms]))
            .collect::<Vec<_>>()
    )
}

fn attach_group_metrics(usage: &mut Value, metrics: &Value) {
    if !metrics.is_object() {
        return;
    }
    if let Some(groups) = usage["groups"].as_array_mut() {
        for group in groups.iter_mut() {
            let Some(gid) = group["id"].as_str().map(str::to_owned) else {
                continue;
            };
            for key in ["fiveHour", "sevenDay"] {
                let composite = format!("{gid}:{key}");
                if group[key].is_object() && metrics[&composite].is_object() {
                    group[key]["metrics"] = metrics[&composite].clone();
                }
            }
        }
    }
    let primary_id = usage["groups"]
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|g| g["id"] == "gemini")
                .or_else(|| arr.first())
        })
        .and_then(|g| g["id"].as_str())
        .unwrap_or("gemini")
        .to_owned();
    for key in ["fiveHour", "sevenDay"] {
        let composite = format!("{primary_id}:{key}");
        if usage[key].is_object() && metrics[&composite].is_object() {
            usage[key]["metrics"] = metrics[&composite].clone();
        }
    }
}

fn scan_local_transcript_windows(
    gemini_root: &Path,
    specs: &[ActiveWindowSpec],
    prices: &Value,
    observed_at_ms: u64,
) -> Value {
    if !gemini_root.is_absolute() || specs.is_empty() {
        return Value::Null;
    }
    let earliest_start = specs.iter().map(|s| s.start_ms).min().unwrap_or(0);

    #[derive(Default)]
    struct Accumulator {
        tokens: u64,
        cost_usd: f64,
        missing_price: bool,
    }

    let mut acc: Vec<Accumulator> = specs.iter().map(|_| Accumulator::default()).collect();
    let mut seen_convs = HashSet::new();
    for product_dir in ["antigravity", "antigravity-cli", "antigravity-ide"] {
        let brain_dir = gemini_root.join(product_dir).join("brain");
        let Ok(entries) = std::fs::read_dir(&brain_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let conv_id = entry.file_name().to_string_lossy().into_owned();
            if !local_store::valid_native_id(&conv_id) || !seen_convs.insert(conv_id) {
                continue;
            }
            let Ok(dir_meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
                continue;
            }
            let logs_dir = entry.path().join(".system_generated").join("logs");
            let mut chosen = None;
            for filename in ["transcript_full.jsonl", "transcript.jsonl"] {
                let candidate = logs_dir.join(filename);
                let Ok(meta) = std::fs::symlink_metadata(&candidate) else {
                    continue;
                };
                if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > MAX_TRANSCRIPT_BYTES {
                    continue;
                }
                let mtime_ms = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .and_then(|d| u64::try_from(d.as_millis()).ok())
                    .unwrap_or(0);
                // 文件最后修改时间早于最早活跃窗口起点时直接跳过，不读取正文。
                if mtime_ms > 0 && mtime_ms < earliest_start {
                    chosen = None;
                    break;
                }
                chosen = Some(candidate);
                break;
            }
            let Some(path) = chosen else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in text.lines() {
                if !line.contains("PLANNER_RESPONSE") {
                    continue;
                }
                let Ok(row) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                if row["type"] != "PLANNER_RESPONSE" {
                    continue;
                }
                let Some(ts_ms) = row["created_at"]
                    .as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .and_then(|dt| u64::try_from(dt.timestamp_millis()).ok())
                else {
                    continue;
                };
                if ts_ms < earliest_start || ts_ms > observed_at_ms.saturating_add(30_000) {
                    continue;
                }
                let uncached_in = row["input_tokens"].as_u64().unwrap_or(0);
                let cache_read = row["cache_read_tokens"].as_u64().unwrap_or(0);
                let out = row["output_tokens"].as_u64().unwrap_or(0);
                let step_tokens = uncached_in.saturating_add(cache_read).saturating_add(out);
                if step_tokens == 0 {
                    continue;
                }
                let model_raw = row["model"].as_str().unwrap_or("");
                let (base_model, _) = local_store::split_model_effort(model_raw.trim());
                let group_id = classify_model_group(&base_model);
                let step_cost = estimate_step_cost_usd(
                    prices,
                    &base_model,
                    uncached_in,
                    cache_read,
                    out,
                );
                for (idx, spec) in specs.iter().enumerate() {
                    if spec.group_id == group_id && ts_ms >= spec.start_ms && ts_ms <= spec.end_ms {
                        acc[idx].tokens = acc[idx].tokens.saturating_add(step_tokens);
                        match step_cost {
                            Some(cost) => acc[idx].cost_usd += cost,
                            None => acc[idx].missing_price = true,
                        }
                    }
                }
            }
        }
    }

    let mut out = serde_json::Map::new();
    for (idx, spec) in specs.iter().enumerate() {
        let item = &acc[idx];
        let cost_val = if item.tokens == 0 {
            Some(0.0)
        } else if !item.missing_price && item.cost_usd.is_finite() && item.cost_usd >= 0.0 {
            Some(item.cost_usd)
        } else {
            None
        };
        let mut metric = json!({
            "source": "antigravity-local-journal",
            "observedAtMs": observed_at_ms,
            "tokensObservedAtMs": observed_at_ms,
            "windowStartMs": spec.start_ms,
            "windowEndMs": spec.end_ms,
            "usedTokens": item.tokens,
        });
        if let Some(cost) = cost_val {
            metric["usedCostUsd"] = json!(cost);
            metric["costObservedAtMs"] = json!(observed_at_ms);
        }
        out.insert(format!("{}:{}", spec.group_id, spec.window_key), metric);
    }
    Value::Object(out)
}

fn estimate_step_cost_usd(
    prices: &Value,
    model: &str,
    uncached_input: u64,
    cache_read: u64,
    output: u64,
) -> Option<f64> {
    if model.is_empty() || !prices.is_object() {
        return None;
    }
    let candidates = [
        model.to_owned(),
        format!("gemini/{model}"),
        format!("vertex_ai/{model}"),
        format!("anthropic/{model}"),
        format!("openai/{model}"),
    ];
    let row = candidates
        .iter()
        .find_map(|key| prices.get(key).filter(|v| v.is_object()))?;
    let context_len = uncached_input.saturating_add(cache_read);
    let above_200k = context_len > 200_000;
    let rate = |base_key: &str| -> Option<f64> {
        if above_200k {
            let long_key = format!("{base_key}_above_200k_tokens");
            if let Some(v) = row[&long_key].as_f64().filter(|n| n.is_finite() && *n >= 0.0) {
                return Some(v);
            }
        }
        row[base_key].as_f64().filter(|n| n.is_finite() && *n >= 0.0)
    };
    let mut total = 0.0;
    if uncached_input > 0 {
        total += (uncached_input as f64) * rate("input_cost_per_token")?;
    }
    if cache_read > 0 {
        let cache_rate = rate("cache_read_input_token_cost").or_else(|| rate("input_cost_per_token"))?;
        total += (cache_read as f64) * cache_rate;
    }
    if output > 0 {
        total += (output as f64) * rate("output_cost_per_token")?;
    }
    total.is_finite().then_some(total)
}

fn normalize_prices(value: &Value) -> Option<Value> {
    let mut models = serde_json::Map::new();
    for (name, row) in value.as_object()? {
        if name.len() > 256
            || (!row["input_cost_per_token"].is_number()
                && !row["output_cost_per_token"].is_number())
        {
            continue;
        }
        let mut prices = serde_json::Map::new();
        for (key, val) in row.as_object()? {
            if key.len() <= 160
                && (key.starts_with("input_cost_per_token")
                    || key.starts_with("output_cost_per_token")
                    || key.starts_with("cache_read_input_token_cost")
                    || key.starts_with("cache_creation_input_token_cost"))
                && val.as_f64().is_some_and(|n| n.is_finite() && n >= 0.0)
            {
                prices.insert(key.clone(), val.clone());
            }
        }
        if !prices.is_empty() {
            models.insert(name.clone(), Value::Object(prices));
        }
    }
    (!models.is_empty()).then_some(Value::Object(models))
}

async fn download_prices() -> anyhow::Result<Value> {
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::limited(2))
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(6))
        .build()?;
    let mut response = client.get(PRICE_SOURCE).send().await?.error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            bytes.len() + chunk.len() <= MAX_PRICE_BYTES,
            "公开模型价格表过大"
        );
        bytes.extend_from_slice(&chunk);
    }
    normalize_prices(&serde_json::from_slice::<Value>(&bytes)?)
        .context("公开模型价格表不含可用价格")
}

fn valid_time_zone(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.len() <= 128
        && !value.starts_with('/')
        && !value.contains("..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-/".contains(&byte))
        && (value == "UTC" || value.contains('/'))
}

fn time_zone_from_probe(output: &str) -> Option<String> {
    let mut environment = None;
    let mut environment_present = false;
    let mut date_present = false;
    let mut localtime = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("__CODEX_ELVES_TZ__") {
            let value = value.trim();
            environment_present = !value.is_empty();
            if valid_time_zone(value) {
                environment = Some(value.to_owned());
            }
        } else if let Some(value) = line.strip_prefix("__CODEX_ELVES_DATE__") {
            date_present = !value.trim().is_empty();
        } else if let Some(value) = line.strip_prefix("__CODEX_ELVES_LOCALTIME__") {
            let value = value.trim();
            let value = value
                .strip_prefix("/usr/share/zoneinfo/")
                .or_else(|| value.strip_prefix("/usr/lib/zoneinfo/"))
                .unwrap_or(value);
            if valid_time_zone(value) {
                localtime = Some(value.to_owned());
            }
        }
    }
    if !date_present {
        return None;
    }
    environment.or_else(|| (!environment_present).then_some(localtime).flatten())
}

async fn native_wsl_time_zone(config: &EnvConfig) -> Option<String> {
    if config.is_local() || !cfg!(windows) {
        return None;
    }
    // 时区补读同样是后台命令，不能因仅调用 sh/date 就遗漏 WSL 宿主的隐藏设置。
    let mut command = Command::from(elves_core::process::hidden_command("wsl.exe"));
    command.args([
        "--distribution",
        &config.distribution,
        "--user",
        &config.user,
        "--cd",
        "~",
        "--exec",
        "/bin/bash",
        "-lc",
        "exec \"$@\"",
        "--",
        "sh",
        "-c",
        TIMEZONE_PROBE,
    ]);
    let output = tokio::time::timeout(TIMEZONE_PROBE_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() || output.stdout.len() > MAX_TIMEZONE_PROBE_BYTES {
        return None;
    }
    time_zone_from_probe(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_cli_commands_preserve_program_and_wsl_login_environment() {
        let executable = std::env::current_exe().unwrap();
        let local = EnvConfig {
            executable: executable.to_string_lossy().into_owned(),
            ..EnvConfig::default()
        };
        let command = build_isolated_cli_command(&local).unwrap();
        assert_eq!(command.as_std().get_program(), executable.as_os_str());
        assert_eq!(command.as_std().get_args().count(), 0);
        if let Some(base) = directories::BaseDirs::new() {
            assert_eq!(command.as_std().get_current_dir(), Some(base.home_dir()));
        }
        #[cfg(windows)]
        {
            let config = EnvConfig {
                distribution: "fixture-distro".into(),
                user: "fixture-user".into(),
                executable: "/fixture/bin/agy".into(),
            };
            let command = build_isolated_cli_command(&config).unwrap();
            assert_eq!(command.as_std().get_program(), "wsl.exe");
            assert_eq!(
                command.as_std().get_args().collect::<Vec<_>>(),
                [
                    "--distribution", "fixture-distro", "--user", "fixture-user",
                    "--cd", "~", "--exec", "/bin/bash", "-lc", "exec \"$@\"", "--",
                    "/fixture/bin/agy"
                ].map(std::ffi::OsStr::new)
            );
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn isolated_cli_is_windowless_and_preserves_stdio_and_failure_status() {
        use std::io::Read;
        use tokio::io::AsyncWriteExt;

        const STAGE: &str = "ELVES_AGY_HIDDEN_QUERY_TEST";
        const TEST: &str = "antigravity::usage::tests::isolated_cli_is_windowless_and_preserves_stdio_and_failure_status";
        if std::env::var(STAGE).as_deref() == Ok("leaf") {
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn GetConsoleWindow() -> isize;
            }
            assert_eq!(unsafe { GetConsoleWindow() }, 0, "后台查询不应创建控制台");
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input).unwrap();
            println!("hidden-query:{input}");
            eprintln!("hidden-query-error");
            std::process::exit(17);
        }
        let config = EnvConfig {
            executable: std::env::current_exe().unwrap().to_string_lossy().into_owned(),
            ..EnvConfig::default()
        };
        let mut child = build_isolated_cli_command(&config).unwrap()
            .args(["--exact", TEST, "--nocapture"])
            .env(STAGE, "leaf")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn().unwrap();
        child.stdin.take().unwrap().write_all(b"query-payload").await.unwrap();
        let output = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
            .await.unwrap().unwrap();
        assert_eq!(output.status.code(), Some(17));
        assert!(String::from_utf8_lossy(&output.stdout).contains("hidden-query:query-payload"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("hidden-query-error"));
    }

    #[test]
    fn normalizes_cli_usage_groups_and_epoch_zero_reset_times() {
        let raw = json!({
            "status": "SUCCESS",
            "command": {
                "name": "usage",
                "data": {
                    "groups": [
                        {
                            "name": "Gemini Models",
                            "description": "Gemini 4 Neon",
                            "buckets": [
                                {"id": "gemini-weekly", "window": "weekly", "remaining_fraction": 0.70188, "reset_time": "2026-12-03T07:09:10Z"},
                                {"id": "gemini-5h", "window": "5h", "remaining_fraction": 0.83749, "reset_time": "2026-11-26T17:09:10Z"}
                            ]
                        },
                        {
                            "name": "Claude and GPT models",
                            "description": "Claude Opus 5.5, GPT-OSS 120B",
                            "buckets": [
                                {"id": "3p-weekly", "window": "weekly", "remaining_fraction": 1.0, "reset_time": "1970-01-01T00:00:00Z"},
                                {"id": "3p-5h", "window": "5h", "remaining_fraction": 1.0, "reset_time": "1970-01-01T00:00:00Z"}
                            ]
                        }
                    ]
                }
            }
        });
        let usage = normalize_cli_usage(
            &raw,
            123456,
            Some("demo.user@example.com"),
            Some("Google AI Pro"),
            true,
        );
        assert_eq!(usage["source"], "antigravity-native-usage");
        assert_eq!(usage["accountEmail"], "demo.user@example.com");
        assert_eq!(usage["subscriptionType"], "Google AI Pro");
        assert_eq!(usage["hostLocal"], true);
        let groups = usage["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["id"], "gemini");
        assert!((groups[0]["fiveHour"]["usedPercentage"].as_f64().unwrap() - 16.251).abs() < 1e-6);
        assert_eq!(groups[0]["fiveHour"]["resetsAt"], "2026-11-26T17:09:10Z");
        assert_eq!(groups[1]["id"], "thirdParty");
        assert_eq!(groups[1]["fiveHour"]["usedPercentage"], 0.0);
        assert!(groups[1]["fiveHour"]["resetsAt"].is_null(), "epoch 0 必须归一化为 null");
        assert!(groups[1]["sevenDay"]["resetsAt"].is_null());
    }

    #[test]
    fn scans_local_transcript_windows_by_model_group_and_price_availability() {
        let dir = tempfile::tempdir().unwrap();
        let gemini = dir.path().join(".gemini");
        let logs = gemini
            .join("antigravity")
            .join("brain")
            .join("sess-1")
            .join(".system_generated")
            .join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("transcript_full.jsonl"),
            [
                r#"{"type":"PLANNER_RESPONSE","created_at":"2026-11-26T14:00:00Z","model":"gemini-2.5-pro-high","input_tokens":1000,"cache_read_tokens":4000,"output_tokens":500,"content":"ok"}"#,
                r#"{"type":"PLANNER_RESPONSE","created_at":"2026-11-26T14:10:00Z","model":"claude-sonnet-4-5-high","input_tokens":2000,"cache_read_tokens":0,"output_tokens":300,"content":"hi"}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        let end_ms = chrono::DateTime::parse_from_rfc3339("2026-11-26T17:00:00Z")
            .unwrap()
            .timestamp_millis() as u64;
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-26T15:00:00Z")
            .unwrap()
            .timestamp_millis() as u64;
        let specs = vec![
            ActiveWindowSpec {
                group_id: "gemini".into(),
                window_key: "fiveHour".into(),
                start_ms: end_ms - 5 * 3600 * 1000,
                end_ms,
            },
            ActiveWindowSpec {
                group_id: "thirdParty".into(),
                window_key: "fiveHour".into(),
                start_ms: end_ms - 5 * 3600 * 1000,
                end_ms,
            },
        ];
        let prices = json!({
            "gemini-2.5-pro": {
                "input_cost_per_token": 0.00000125,
                "cache_read_input_token_cost": 0.0000003125,
                "output_cost_per_token": 0.00001
            }
        });
        let metrics = scan_local_transcript_windows(&gemini, &specs, &prices, now);
        assert_eq!(metrics["gemini:fiveHour"]["usedTokens"], 5500);
        assert!(metrics["gemini:fiveHour"]["usedCostUsd"].as_f64().unwrap() > 0.0);
        assert_eq!(metrics["thirdParty:fiveHour"]["usedTokens"], 2300);
        assert!(
            metrics["thirdParty:fiveHour"].get("usedCostUsd").is_none(),
            "缺少公开模型价格时保留真实 Token 统计且不伪造金额"
        );
    }
}
