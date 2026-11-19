//! 公开发行版查询与安装包校验逻辑；当前桌面帮助菜单不提供更新入口。
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, ensure};
use semver::Version;
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

pub const PROJECT_URL: &str = "https://github.com/junxin367/Elves";
const RELEASE_API: &str = "https://api.github.com/repos/junxin367/Elves/releases/latest";
const MAX_INSTALLER_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Installer {
    pub name: String,
    pub url: Url,
    pub size: u64,
    pub sha256: Option<String>,
}

#[derive(Debug)]
pub struct Release {
    pub version: Version,
    pub page: Url,
    pub installer: Option<Installer>,
}

fn project_url(text: &str, kind: &str) -> anyhow::Result<Url> {
    let url = Url::parse(text)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("github.com")
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url
                .path()
                .starts_with(&format!("/junxin367/Elves/releases/{kind}/")),
        "发行版链接不属于 Elves 项目"
    );
    Ok(url)
}

impl Release {
    pub fn parse(value: &Value) -> anyhow::Result<Self> {
        ensure!(
            value["draft"] != true && value["prerelease"] != true,
            "仅支持公开正式发行版"
        );
        let tag = value["tag_name"].as_str().context("发行版缺少版本号")?;
        ensure!(tag.len() <= 160, "发行版本号过长");
        let version =
            Version::parse(tag.trim_start_matches(['v', 'V'])).context("发行版本号无效")?;
        let page = project_url(
            value["html_url"].as_str().context("发行版缺少项目地址")?,
            "tag",
        )?;
        let name = format!("Elves-{version}-windows-x64-setup.exe");
        let installer = value["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|asset| asset["name"] == name)
            .map(|asset| -> anyhow::Result<Installer> {
                let url = project_url(
                    asset["browser_download_url"]
                        .as_str()
                        .context("安装包地址缺失")?,
                    "download",
                )?;
                ensure!(
                    url.path_segments().and_then(|segments| segments.last()) == Some(name.as_str()),
                    "安装包文件名不匹配"
                );
                let size = asset["size"].as_u64().context("安装包大小缺失")?;
                ensure!(
                    size > 0 && size <= MAX_INSTALLER_BYTES,
                    "安装包大小超出限制"
                );
                let sha256 = match asset["digest"].as_str() {
                    None => None,
                    Some(digest) => {
                        let hash = digest
                            .strip_prefix("sha256:")
                            .context("安装包摘要类型无效")?;
                        ensure!(
                            hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
                            "安装包摘要无效"
                        );
                        Some(hash.to_ascii_lowercase())
                    }
                };
                Ok(Installer {
                    name,
                    url,
                    size,
                    sha256,
                })
            })
            .transpose()?;
        Ok(Self {
            version,
            page,
            installer,
        })
    }

    pub fn newer_than(&self, current: &str) -> anyhow::Result<bool> {
        Ok(self.version > Version::parse(current)?)
    }
}

fn client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .https_only(true)
        .user_agent(format!("Elves/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .build()?)
}

pub async fn check() -> anyhow::Result<Option<Release>> {
    let response = client()?
        .get(RELEASE_API)
        .timeout(Duration::from_secs(20))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .context("无法连接项目发布服务")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let mut response = response
        .error_for_status()
        .context("项目发布服务返回错误")?;
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= 1024 * 1024),
        "发行版信息过大"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= 1024 * 1024,
            "发行版信息过大"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(Some(Release::parse(&serde_json::from_slice(&bytes)?)?))
}

pub async fn download(installer: &Installer, directory: &Path) -> anyhow::Result<PathBuf> {
    download_with_client(installer, directory, client()?).await
}

async fn download_with_client(
    installer: &Installer,
    directory: &Path,
    client: reqwest::Client,
) -> anyhow::Result<PathBuf> {
    let expected = installer
        .sha256
        .as_ref()
        .context("此发行版未提供可校验的安装包")?;
    std::fs::create_dir_all(directory)?;
    let mut output = tempfile::NamedTempFile::new_in(directory)?;
    let mut response = client
        .get(installer.url.clone())
        .send()
        .await?
        .error_for_status()?;
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size == installer.size),
        "下载的安装包大小与发行信息不一致"
    );
    let mut size = 0u64;
    let mut digest = Sha256::new();
    while let Some(chunk) = response.chunk().await? {
        size = size
            .checked_add(chunk.len() as u64)
            .context("安装包大小无效")?;
        ensure!(
            size <= installer.size && size <= MAX_INSTALLER_BYTES,
            "安装包超过声明大小"
        );
        digest.update(&chunk);
        output.write_all(&chunk)?;
    }
    ensure!(
        size == installer.size && format!("{:x}", digest.finalize()) == *expected,
        "安装包校验失败，请重新检查更新"
    );
    output.as_file_mut().rewind()?;
    let mut signature = [0; 2];
    output.as_file_mut().read_exact(&mut signature)?;
    ensure!(&signature == b"MZ", "下载文件不是 Windows 安装程序");
    output.as_file().sync_all()?;
    let target = directory.join(&installer.name);
    output.persist(&target).map_err(|error| error.error)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn release() -> Value {
        json!({"tag_name":"v1.2.0","html_url":"https://github.com/junxin367/Elves/releases/tag/v1.2.0",
            "assets":[{"name":"Elves-1.2.0-windows-x64-setup.exe","size":100,
                "browser_download_url":"https://github.com/junxin367/Elves/releases/download/v1.2.0/Elves-1.2.0-windows-x64-setup.exe",
                "digest":format!("sha256:{}","a".repeat(64))}]})
    }

    #[test]
    fn updates_compare_versions_and_keep_missing_checksum_explicit() {
        let mut value = release();
        let parsed = Release::parse(&value).unwrap();
        assert!(parsed.newer_than("1.0.0").unwrap());
        assert!(!parsed.newer_than("1.3.0").unwrap());
        value["assets"][0].as_object_mut().unwrap().remove("digest");
        assert!(
            Release::parse(&value)
                .unwrap()
                .installer
                .unwrap()
                .sha256
                .is_none()
        );
        value["assets"][0]["digest"] = json!("sha256:bad");
        assert!(Release::parse(&value).is_err());
    }

    #[test]
    fn updates_reject_other_repositories_untrusted_urls_and_oversize() {
        for url in [
            "https://example.com/setup.exe",
            "file:///C:/setup.exe",
            "https://github.com/other/Elves/releases/download/v1.2.0/Elves-1.2.0-windows-x64-setup.exe",
            "https://github.com/junxin367/Elves/releases/download/v1.2.0/another.exe",
        ] {
            let mut value = release();
            value["assets"][0]["browser_download_url"] = json!(url);
            assert!(Release::parse(&value).is_err(), "{url}");
        }
        let mut value = release();
        value["assets"][0]["size"] = json!(MAX_INSTALLER_BYTES + 1);
        assert!(Release::parse(&value).is_err());
    }

    #[tokio::test]
    async fn downloaded_update_verifies_content_and_removes_failed_partial_files() {
        for failure in ["", "checksum", "size", "executable"] {
            let directory = tempfile::tempdir().unwrap();
            let body = if failure == "executable" {
                b"not-exe".to_vec()
            } else {
                b"MZ-test".to_vec()
            };
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url =
                Url::parse(&format!("http://{}/setup", listener.local_addr().unwrap())).unwrap();
            let response_body = body.clone();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 2048];
                let _ = stream.read(&mut request);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response_body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&response_body);
            });
            let asset = Installer {
                name: "Elves-1.2.0-windows-x64-setup.exe".into(),
                url,
                size: body.len() as u64 + u64::from(failure == "size"),
                sha256: Some(if failure == "checksum" {
                    "0".repeat(64)
                } else {
                    format!("{:x}", Sha256::digest(&body))
                }),
            };
            let result = download_with_client(
                &asset,
                directory.path(),
                reqwest::Client::builder().no_proxy().build().unwrap(),
            )
            .await;
            server.join().unwrap();
            if failure.is_empty() {
                assert_eq!(std::fs::read(result.unwrap()).unwrap(), body);
            } else {
                assert!(result.is_err(), "{failure}");
                assert_eq!(
                    std::fs::read_dir(directory.path()).unwrap().count(),
                    0,
                    "{failure}"
                );
            }
        }
    }
}
