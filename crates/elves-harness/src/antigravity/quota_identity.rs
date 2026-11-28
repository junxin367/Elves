//! 额度弹窗只消费所属实例已有的登录展示信息和原生等级，不发起认证或目录探测。
use super::{Antigravity, acp::EnvConfig, now_ms};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const RETENTION_MS: u64 = 72 * 60 * 60 * 1000;

pub(super) fn email(raw: &str) -> Option<String> {
    let s = raw.trim();
    let (local, domain) = s.split_once('@')?;
    (!local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && s.len() <= 320
        && !s.chars().any(|c| c.is_control() || c.is_whitespace()))
    .then(|| s.to_lowercase())
}

pub(super) fn authenticated_email(line: &str) -> Option<String> {
    let (prefix, value) = line.split_once("] OAuth: authenticated successfully as ")?;
    let mut fields = prefix.split_ascii_whitespace();
    let date = fields.next()?.as_bytes();
    if date.len() != 5 || date[0] != b'I' || !date[1..].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let time = fields.next()?;
    if chrono::NaiveTime::parse_from_str(time, "%H:%M:%S%.f").is_err() {
        return None;
    }
    let _thread = fields.next()?.parse::<u64>().ok()?;
    let loc = fields.next()?.strip_prefix("server_oauth.go:")?;
    if fields.next().is_some() || loc.parse::<u32>().is_err() {
        return None;
    }
    email(value)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, limit: u64) -> Option<T> {
    if !path.is_absolute()
        || path
            .parent()
            .and_then(|p| p.symlink_metadata().ok())
            .is_none_or(|m| !m.is_dir())
        || path
            .symlink_metadata()
            .ok()
            .is_none_or(|m| !m.is_file() || m.len() > limit)
    {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= limit)
        .then(|| serde_json::from_slice(&bytes).ok())
        .flatten()
}

#[derive(Deserialize)]
struct AppLogin {
    #[serde(rename = "agy.onboarding.lastLoginUsername")]
    name: Option<String>,
}

pub(super) fn app_email(path: &Path) -> Option<String> {
    // 只反序列化 AGY 自己的上次登录邮箱；不读 Gemini CLI 的 google_accounts.json 或登录凭据。
    read_json::<AppLogin>(path, 2 * 1024 * 1024)?
        .name
        .as_deref()
        .and_then(email)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Identity {
    config: EnvConfig,
    desktop: Option<String>,
    pub scope: String,
    pub email: Option<String>,
    native_at: u64,
    tier: Option<String>,
    tier_at: u64,
    pub signed_out: bool,
    #[serde(default)]
    pub changed_at: u64,
    // 只在当前进程复用未改动的 AGY 显示配置，重启或换路径后重新读取，不延长 CLI 证据有效期。
    #[serde(skip)]
    desktop_stamp: Option<(PathBuf, u64, SystemTime)>,
}

impl Identity {
    fn new(config: &EnvConfig, desktop: Option<String>) -> Self {
        Self {
            config: config.clone(),
            email: desktop.clone(),
            desktop,
            scope: uuid::Uuid::new_v4().to_string(),
            native_at: 0,
            tier: None,
            tier_at: 0,
            signed_out: false,
            changed_at: 0,
            desktop_stamp: None,
        }
    }

    pub fn tier(&self) -> Option<&str> {
        self.tier
            .as_deref()
            .filter(|_| self.email.is_some() && fresh(self.tier_at) && !self.signed_out)
    }

    fn reconcile(&mut self, config: &EnvConfig, desktop: Option<String>) -> bool {
        if self.config==*config && self.desktop==desktop && desktop.is_some() && self.email==desktop &&
            self.native_at>0 && !fresh(self.native_at) {
            // 原生证明过期但 AGY 当前展示账号仍一致时，只退回该账号来源，不换掉长期额度快照的身份。
            self.native_at=0;return true;
        }
        if self.config != *config
            || self.desktop != desktop
            || self.scope.is_empty()
            || self.native_at > 0 && !fresh(self.native_at)
        {
            let stale_conflict = self.config == *config && self.desktop == desktop
                && self.native_at > 0 && !fresh(self.native_at) && self.email != desktop;
            let changed_at = if self.email != desktop || self.config != *config {
                now_ms()
            } else {
                self.changed_at
            };
            *self = Self::new(config, desktop);
            if stale_conflict { self.email = None; }
            self.changed_at = changed_at;
            true
        } else {
            false
        }
    }

    fn observe(&mut self, native: Option<String>, tier: Option<&str>, logout: bool) -> bool {
        if logout {
            if self.signed_out {
                return false;
            }
            self.scope = uuid::Uuid::new_v4().to_string();
            self.changed_at = now_ms();
            self.signed_out = true;
            self.email = None;
            self.native_at = 0;
            self.tier = None;
            self.tier_at = 0;
            return true;
        }
        // 显式退出后即使历史桌面邮箱仍在也不复活；等待新的本实例认证邮箱或已变更的桌面记录。
        let email = native.clone().or_else(|| self.email.clone());
        if self.email != email {
            self.scope = uuid::Uuid::new_v4().to_string();
            self.changed_at = now_ms();
            self.tier = None;
            self.tier_at = 0;
        }
        let was_out = self.signed_out;
        self.signed_out = false;
        self.email = email;
        if native.is_some() {
            self.native_at = now_ms();
        }
        if let Some(tier) = tier.filter(|_| self.email.is_some()) {
            self.tier = Some(tier.into());
            self.tier_at = now_ms();
        }
        was_out || native.is_some() || tier.is_some() && self.email.is_some()
    }
}

fn fresh(at: u64) -> bool {
    let now = now_ms();
    at > 0 && at <= now + 30_000 && now.saturating_sub(at) < RETENTION_MS
}

impl Antigravity {
    pub(super) async fn quota_gemini_root(&self, config: &EnvConfig) -> Option<PathBuf> {
        if config.is_local() {
            return Some(directories::BaseDirs::new()?.home_dir().join(".gemini"));
        }
        let home = crate::preferences::cached_user_home(
            &self.directory,
            &config.distribution,
            &config.user,
        )
        .await
        .or_else(|| {
            if config.user == "root" {
                Some("/root".into())
            } else if !config.user.is_empty() {
                Some(format!("/home/{}", config.user))
            } else {
                None
            }
        })?;
        crate::preferences::wsl_path(&config.distribution, &format!("{home}/.gemini")).ok()
    }
    pub(super) async fn remember_quota_login(
        &self,
        config: &EnvConfig,
        scope: &str,
        client: &super::AcpClient,
        tier: Option<&serde_json::Value>,
    ) {
        let tier = super::usage::sanitize_subscription_type(tier.and_then(|t| t["name"].as_str()));
        self.update_quota_identity(
            config,
            Some((scope, client.login_email().await, tier.as_deref(), false)),
        )
        .await;
    }
    async fn app_login_path(&self, config: &EnvConfig) -> Option<PathBuf> {
        #[cfg(test)]
        if config.is_local() && self.directory.join("fixture-agy").is_dir() {
            return Some(self.directory.join("fixture-agy/app_storage.json"));
        }
        if config.is_local() {
            return Some(
                directories::BaseDirs::new()?
                    .config_dir()
                    .join("antigravity/app_storage.json"),
            );
        }
        // WSL 从本发行版、本用户的 Home 读取；无桌面缓存的纯 CLI 实例只用自己的原生认证信息。
        let root = self.quota_gemini_root(config).await?;
        Some(root.parent()?.join(".config/antigravity/app_storage.json"))
    }

    pub(super) async fn quota_identity(&self, config: &EnvConfig) -> Identity {
        self.update_quota_identity(config, None).await
    }

    pub(super) async fn update_quota_identity(
        &self,
        config: &EnvConfig,
        observation: Option<(&str, Option<String>, Option<&str>, bool)>,
    ) -> Identity {
        let id = config.instance_id();
        let gate = self.lock(&format!("quota-identity:{id}")).await;
        let _guard = gate.lock().await;
        let app = self.app_login_path(config).await;
        let path = self
            .directory
            .join(format!("antigravity-account-{}.json", crate::hash(&id)));
        let cached = self.quota_identities.lock().await.get(&id).cloned();
        let cfg = config.clone();
        let (mut account, changed) = tokio::task::spawn_blocking({
            let path = path.clone();
            move || {
                let stamp=app.as_ref().and_then(|p|{
                    let m=p.symlink_metadata().ok()?;
                    (m.is_file() && m.len()<=2*1024*1024 && p.parent()?.symlink_metadata().ok()?.is_dir())
                        .then_some((p.clone(),m.len(),m.modified().ok()?))
                });
                let desktop = if stamp.is_some() && cached.as_ref().is_some_and(|i|i.desktop_stamp==stamp) {
                    cached.as_ref().and_then(|i|i.desktop.clone())
                } else {
                    app.as_deref().and_then(app_email)
                };
                let previous=cached.or_else(|| read_json::<Identity>(&path,4096));
                let first=previous.is_none() && desktop.is_some();
                let mut account = previous
                    .unwrap_or_else(|| Identity::new(&cfg, desktop.clone()));
                let changed = account.reconcile(&cfg, desktop) || first;
                account.desktop_stamp=stamp;
                (account, changed)
            }
        })
        .await
        .unwrap_or_else(|_| (Identity::new(config, None), false));
        let updated = if let Some((scope, who, tier, logout)) = observation
            && scope == account.scope
        {
            account.observe(who, tier, logout)
        } else {
            false
        };
        if changed || updated {
            let write = account.clone();
            let saved = tokio::task::spawn_blocking(move || {
                serde_json::to_vec(&write)
                    .is_ok_and(|bytes| crate::settings::atomic_write(&path, &bytes).is_ok())
            })
            .await;
            if !matches!(saved, Ok(true)) {
                let _ = crate::diagnostic_log::append_diagnostic_log(
                    "antigravity.account_cache_write_failed",
                    serde_json::json!({}),
                );
            }
        }
        let binding=crate::quota_snapshot::binding("antigravity",config);
        self.quota_snapshots.bind(&binding,&account.scope,
            account.email.is_some() && !account.signed_out);
        if account.signed_out && (changed || updated) {self.quota_snapshots.clear(&binding,&account.scope);}
        self.quota_identities
            .lock()
            .await
            .insert(id, account.clone());
        account
    }
}
