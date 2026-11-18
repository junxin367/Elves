//! 文件附件只能写入服务专用目录；本机文件内容流式暂存，远程仍使用有限的内联附件。
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::Json;
use axum::extract::{Multipart, State, multipart::Field};
use axum::http::HeaderMap;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use super::{ApiError, HarnessAuthorization, WebState, desktop};

// 数量和路径规则与网页及原生选择器同步。12 MiB 仅用于远程/旧客户端的内联数据，不限制本机流式附件。
const MAX_ROOTS: usize = 16;
const MAX_ENTRIES: usize = 512;
const MAX_BYTES: usize = 12 * 1024 * 1024;
// 512 个目录项，每条路径最多 4096 字节，再为 JSON 和大小字段留空间。这不是文件容量限制。
const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
const STAGED_ROOT: &str = "desktop-input-files";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input<T> {
    kind: String,
    name: String,
    size: u64,
    files: Vec<T>,
    directories: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputFile {
    path: String,
    data: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadFile {
    path: String,
    size: u64,
}

#[derive(Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct StagedInput {
    kind: String,
    name: String,
    size: u64,
    staged: StagedSlot,
}

#[derive(Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct StagedSlot {
    id: String,
    index: usize,
    entries: usize,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Attachment {
    Inline(Input<InputFile>),
    Staged(StagedInput),
}

fn invalid(message: &str) -> ApiError {
    ApiError::new(400, message)
}

fn safe_component(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or("").to_ascii_uppercase();
    !value.is_empty()
        && value.len() <= 255
        && !matches!(value, "." | "..")
        && !value.ends_with(['.', ' '])
        && !value
            .chars()
            .any(|ch| ch.is_control() || "<>:\"/\\|?*".contains(ch))
        && !matches!(
            stem.as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        )
}

fn safe_relative(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && value.split('/').count() <= 32
        && value.split('/').all(safe_component)
}

fn validate_layout<T>(input: &Input<T>, file_path: impl Fn(&T) -> &str) -> Result<usize, ApiError> {
    if !matches!(input.kind.as_str(), "file" | "folder") || !safe_component(&input.name) {
        return Err(invalid("附件名称或类型无效"));
    }
    if input.kind == "file"
        && (!input.directories.is_empty()
            || input.files.len() != 1
            || file_path(&input.files[0]) != input.name)
    {
        return Err(invalid("文件附件的目录结构无效"));
    }
    let count = input.directories.len() + input.files.len();
    if count >= MAX_ENTRIES {
        return Err(invalid("每条消息最多包含 512 个文件和目录项"));
    }
    let mut paths = HashSet::new();
    for path in input
        .directories
        .iter()
        .map(String::as_str)
        .chain(input.files.iter().map(&file_path))
    {
        if !safe_relative(path) || !paths.insert(path.to_lowercase()) {
            return Err(invalid("附件包含越界、重复或无效路径"));
        }
    }
    // 文件不能同时成为另一个附件项的父目录，Windows 大小写折叠后同样校验。
    let file_paths: HashSet<_> = input
        .files
        .iter()
        .map(|file| file_path(file).to_lowercase())
        .collect();
    for path in &paths {
        let mut parent = path.as_str();
        while let Some((prefix, _)) = parent.rsplit_once('/') {
            if file_paths.contains(prefix) {
                return Err(invalid("附件文件与目录路径冲突"));
            }
            parent = prefix;
        }
    }
    Ok(count)
}

fn valid_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|value| value.hyphenated().to_string() == id)
}

fn parse(value: &Value, desktop: bool) -> Result<Vec<Attachment>, ApiError> {
    let Some(files) = value.get("files") else {
        return Ok(Vec::new());
    };
    let inputs: Vec<Attachment> =
        serde_json::from_value(files.clone()).map_err(|_| invalid("文件和文件夹附件参数无效"))?;
    if inputs.len() > MAX_ROOTS {
        return Err(invalid("每条消息最多添加 16 个文件或文件夹"));
    }
    let mut total = 0usize;
    let mut count = inputs.len();
    for attachment in &inputs {
        let Attachment::Inline(input) = attachment else {
            let Attachment::Staged(input) = attachment else {
                unreachable!()
            };
            if !desktop {
                return Err(ApiError::new(403, "本机暂存附件不能从远程入口发送"));
            }
            if !matches!(input.kind.as_str(), "file" | "folder")
                || !safe_component(&input.name)
                || !valid_id(&input.staged.id)
                || input.staged.index >= MAX_ROOTS
                || input.staged.entries >= MAX_ENTRIES
                || input.kind == "file" && input.staged.entries != 1
            {
                return Err(invalid("本机附件引用无效，请重新添加"));
            }
            count += input.staged.entries;
            continue;
        };
        count += validate_layout(input, |file| &file.path)?;
        if count > MAX_ENTRIES {
            return Err(invalid("每条消息最多包含 512 个文件和目录项"));
        }
        let mut size = 0usize;
        for file in &input.files {
            if file.data.len() > MAX_BYTES.div_ceil(3) * 4 {
                return Err(invalid("文件和文件夹总大小不能超过 12 MiB"));
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&file.data)
                .map_err(|_| invalid("附件内容编码无效"))?;
            size = size.saturating_add(bytes.len());
        }
        total = total.saturating_add(size);
        if total > MAX_BYTES {
            return Err(invalid("文件和文件夹总大小不能超过 12 MiB"));
        }
        if size as u64 != input.size {
            return Err(invalid("附件大小与实际内容不一致"));
        }
    }
    if count > MAX_ENTRIES {
        return Err(invalid("每条消息最多包含 512 个文件和目录项"));
    }
    Ok(inputs)
}

pub(super) fn validate_for_device(value: &Value, desktop: bool) -> Result<(), ApiError> {
    parse(value, desktop).map(|_| ())
}

fn plain_metadata(path: &Path) -> Result<std::fs::Metadata, ApiError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| invalid("无法核对附件位置"))?;
    #[cfg(windows)]
    let reparse = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let reparse = false;
    if metadata.is_symlink() || reparse {
        return Err(invalid("附件不允许符号链接或重解析点"));
    }
    Ok(metadata)
}

fn checked_directory(path: &Path) -> Result<(), ApiError> {
    if !plain_metadata(path)?.is_dir() {
        return Err(invalid("附件位置不是普通目录"));
    }
    Ok(())
}

fn plain_directory(path: &Path) -> Result<(), ApiError> {
    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(invalid("无法创建附件目录")),
    }
    checked_directory(path)
}

fn checked_file(path: &Path) -> Result<std::fs::Metadata, ApiError> {
    let metadata = plain_metadata(path)?;
    if !metadata.is_file() {
        return Err(invalid("附件位置不是普通文件"));
    }
    Ok(metadata)
}

fn staged_slot(
    directory: &Path,
    device: &str,
    id: &str,
    index: usize,
) -> Result<(PathBuf, StagedInput), ApiError> {
    if !valid_id(id) || index >= MAX_ROOTS {
        return Err(invalid("本机附件引用无效，请重新添加"));
    }
    // 不接收客户端路径。只解析当前本机设备下已完整发布、并与服务端清单相符的引用。
    let root = directory.join(STAGED_ROOT);
    let owner = root.join(super::hash(device));
    let batch = owner.join(id);
    let slot = batch.join(index.to_string());
    for path in [&root, &owner, &batch, &slot] {
        checked_directory(path).map_err(|_| invalid("本机附件已不可用，请重新添加"))?;
    }
    let manifest = batch.join("inputs.json");
    if checked_file(&manifest)?.len() > MAX_MANIFEST_BYTES as u64 {
        return Err(invalid("本机附件清单无效，请重新添加"));
    }
    let manifest: Vec<StagedInput> = std::fs::read(&manifest)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(|| invalid("本机附件清单不可读，请重新添加"))?;
    let input = manifest
        .into_iter()
        .nth(index)
        .filter(|input| input.staged.id == id && input.staged.index == index && safe_component(&input.name))
        .ok_or_else(|| invalid("本机附件清单无效，请重新添加"))?;
    Ok((slot.join(&input.name), input))
}

fn staged_path(directory: &Path, device: &str, input: &StagedInput) -> Result<PathBuf, ApiError> {
    let (path, saved) = staged_slot(directory, device, &input.staged.id, input.staged.index)?;
    if &saved != input {
        return Err(invalid("本机附件与原选择不一致，请重新添加"));
    }
    if input.kind == "folder" {
        checked_directory(&path)?;
    } else if checked_file(&path)?.len() != input.size {
        return Err(invalid("本机附件内容已变化，请重新添加"));
    }
    Ok(path)
}

fn upload_io(error: std::io::Error) -> ApiError {
    if error.kind() == std::io::ErrorKind::StorageFull {
        ApiError::new(507, "本机磁盘空间不足，未添加附件")
    } else {
        ApiError::new(503, format!("无法保存本机附件：{error}"))
    }
}

async fn upload_chunk(field: &mut Field<'_>) -> Result<Option<axum::body::Bytes>, ApiError> {
    // 只限制等待下一块数据的空闲时间，不按总时长或文件容量拒绝大附件。
    tokio::time::timeout(Duration::from_secs(60), field.chunk())
        .await
        .map_err(|_| ApiError::new(408, "本机附件传输中断，请重新添加"))?
        .map_err(|_| invalid("本机附件传输不完整，请重新添加"))
}

struct DesktopBatch {
    pending: tempfile::TempDir,
    destination: PathBuf,
    inputs: Vec<StagedInput>,
    targets: Vec<(PathBuf, u64)>,
}

impl DesktopBatch {
    fn prepare(
        directory: &Path,
        device: &str,
        manifest: Vec<Input<UploadFile>>,
    ) -> Result<Self, ApiError> {
        if manifest.is_empty() || manifest.len() > MAX_ROOTS {
            return Err(invalid("每条消息最多添加 16 个文件或文件夹"));
        }
        let batch_id = uuid::Uuid::new_v4().to_string();
        let mut entries = manifest.len();
        let mut inputs = Vec::with_capacity(manifest.len());
        for (index, input) in manifest.iter().enumerate() {
            let count = validate_layout(input, |file| &file.path)?;
            entries += count;
            if entries > MAX_ENTRIES {
                return Err(invalid("每条消息最多包含 512 个文件和目录项"));
            }
            let size = input
                .files
                .iter()
                .try_fold(0u64, |total, file| total.checked_add(file.size));
            if size != Some(input.size) {
                return Err(invalid("本机附件大小与清单不一致"));
            }
            inputs.push(StagedInput {
                kind: input.kind.clone(),
                name: input.name.clone(),
                size: input.size,
                staged: StagedSlot {
                    id: batch_id.clone(),
                    index,
                    entries: count,
                },
            });
        }
        let root = directory.join(STAGED_ROOT);
        plain_directory(&root)?;
        let owner = root.join(super::hash(device));
        plain_directory(&owner)?;
        let pending = tempfile::Builder::new()
            .prefix(".upload-")
            .tempdir_in(&owner)
            .map_err(upload_io)?;
        let mut targets = Vec::new();
        for (index, input) in manifest.into_iter().enumerate() {
            let slot = pending.path().join(index.to_string());
            plain_directory(&slot)?;
            let root = if input.kind == "folder" {
                let root = slot.join(input.name);
                plain_directory(&root)?;
                for path in input.directories {
                    directories(&root, &path)?;
                }
                root
            } else {
                slot
            };
            for file in input.files {
                let (parent, name) = file.path.rsplit_once('/').unwrap_or(("", &file.path));
                let path = directories(&root, parent)?.join(name);
                targets.push((path, file.size));
            }
        }
        Ok(Self {
            pending,
            destination: owner.join(batch_id),
            inputs,
            targets,
        })
    }

    fn publish(self) -> Result<Vec<StagedInput>, ApiError> {
        let bytes =
            serde_json::to_vec(&self.inputs).map_err(|_| invalid("无法编码本机附件清单"))?;
        std::fs::write(self.pending.path().join("inputs.json"), bytes).map_err(upload_io)?;
        // 一次性发布整个批次。未完成的批次只由 TempDir 清理，不会被已发送消息或草稿引用。
        std::fs::rename(self.pending.path(), &self.destination).map_err(upload_io)?;
        Ok(self.inputs)
    }
}

/// 仅供独立客户端：从系统选择器或剪贴板已核验的打开句柄流式存盘，不经 HTTP 或 Base64。
pub fn stage_desktop_files(
    directory: &Path,
    device: &str,
    manifest: Value,
    sources: Vec<std::fs::File>,
) -> anyhow::Result<Value> {
    use std::io::Read;
    let manifest = serde_json::from_value(manifest).map_err(|_| invalid("本机附件清单无效"))?;
    let batch = DesktopBatch::prepare(directory, device, manifest)?;
    if batch.targets.len() != sources.len() {
        return Err(invalid("本机附件数量与清单不一致").into());
    }
    for ((path, size), source) in batch.targets.iter().zip(sources) {
        let mut target = std::fs::File::create_new(path).map_err(upload_io)?;
        let mut content = source.take(*size);
        let copied = std::io::copy(&mut content, &mut target).map_err(upload_io)?;
        if copied != *size || content.into_inner().read(&mut [0]).map_err(upload_io)? != 0 {
            return Err(invalid("本机附件内容已变化，请重新添加").into());
        }
    }
    Ok(json!(batch.publish()?))
}

pub(super) async fn desktop_upload(
    State(state): State<WebState>,
    headers: HeaderMap,
    mut parts: Multipart,
) -> Result<Json<Value>, ApiError> {
    let device = super::authorized(&state, &headers).await?;
    if !state.desktop || device.key_id != desktop::DEVICE_KEY_ID {
        return Err(ApiError::new(
            403,
            "本机附件暂存仅可在 Elves 独立客户端使用",
        ));
    }
    let owner = device.id.clone();
    let authorization = state.controller.authorization(device, None);
    let mut field = next_upload_part(&mut parts)
        .await?
        .filter(|field| field.name() == Some("manifest"))
        .ok_or_else(|| invalid("本机附件缺少清单"))?;
    let mut json_bytes = Vec::new();
    while let Some(chunk) = upload_chunk(&mut field).await? {
        if json_bytes.len() + chunk.len() > MAX_MANIFEST_BYTES {
            return Err(invalid("本机附件目录清单过长"));
        }
        json_bytes.extend_from_slice(&chunk);
    }
    drop(field);
    let manifest: Vec<Input<UploadFile>> =
        serde_json::from_slice(&json_bytes).map_err(|_| invalid("本机附件清单无效"))?;
    if manifest.is_empty() || manifest.len() > MAX_ROOTS {
        return Err(invalid("每条消息最多添加 16 个文件或文件夹"));
    }
    let directory = state.controller.state_dir.clone();
    let batch =
        tokio::task::spawn_blocking(move || DesktopBatch::prepare(&directory, &owner, manifest))
            .await
            .map_err(|_| ApiError::new(503, "本机附件暂存任务已中断"))??;
    for (path, size) in &batch.targets {
        let mut field = next_upload_part(&mut parts)
            .await?
            .filter(|field| field.name() == Some("file"))
            .ok_or_else(|| invalid("本机附件传输不完整，请重新添加"))?;
        let mut target = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .await
            .map_err(upload_io)?;
        let mut remaining = *size;
        while let Some(chunk) = upload_chunk(&mut field).await? {
            remaining = remaining
                .checked_sub(chunk.len() as u64)
                .ok_or_else(|| invalid("本机附件内容已变化，请重新添加"))?;
            target.write_all(&chunk).await.map_err(upload_io)?;
        }
        if remaining != 0 {
            return Err(invalid("本机附件传输不完整，请重新添加"));
        }
        target.flush().await.map_err(upload_io)?;
    }
    if next_upload_part(&mut parts).await?.is_some() {
        return Err(invalid("本机附件数据与清单不一致"));
    }
    let permit = authorization
        .prepare_dispatch()
        .await
        .map_err(super::authorization_error)?;
    let staged = tokio::task::spawn_blocking(move || permit.commit(|| Ok(batch.publish()?)))
        .await
        .map_err(|_| ApiError::new(503, "本机附件暂存任务已中断"))?
        .map_err(|error| {
            error
                .downcast_ref::<ApiError>()
                .cloned()
                .unwrap_or_else(|| ApiError::new(503, "无法发布本机附件"))
        })?;
    Ok(Json(json!({"ok":true,"inputs":staged})))
}

async fn next_upload_part(parts: &mut Multipart) -> Result<Option<Field<'_>>, ApiError> {
    tokio::time::timeout(Duration::from_secs(60), parts.next_field())
        .await
        .map_err(|_| ApiError::new(408, "本机附件传输中断，请重新添加"))?
        .map_err(|_| invalid("本机附件传输不完整，请重新添加"))
}

fn directories(root: &Path, relative: &str) -> Result<PathBuf, ApiError> {
    let mut current = root.to_owned();
    for part in relative.split('/').filter(|part| !part.is_empty()) {
        current.push(part);
        plain_directory(&current)?;
    }
    Ok(current)
}

fn store(
    directory: &Path,
    owner: &str,
    request: &str,
    inputs: &[Attachment],
) -> Result<Vec<PathBuf>, ApiError> {
    let mut paths = Vec::new();
    for (index, input) in inputs.iter().enumerate() {
        let input = match input {
            Attachment::Staged(input) => {
                paths.push(staged_path(directory, owner, input)?);
                continue;
            }
            Attachment::Inline(input) => input,
        };
        let root = directory.join("remote-input-files");
        plain_directory(&root)?;
        let owner_dir = root.join(super::hash(owner));
        plain_directory(&owner_dir)?;
        let batch = owner_dir.join(super::hash(request));
        plain_directory(&batch)?;
        let slot = batch.join(index.to_string());
        plain_directory(&slot)?;
        let root = if input.kind == "folder" {
            let root = slot.join(&input.name);
            plain_directory(&root)?;
            for path in &input.directories {
                directories(&root, path)?;
            }
            root
        } else {
            slot.clone()
        };
        for file in &input.files {
            let (parent, name) = file.path.rsplit_once('/').unwrap_or(("", &file.path));
            let parent = directories(&root, parent)?;
            let path = parent.join(name);
            // 只创建新文件，不覆盖现有文件，更不能跟随用户文件夹中的符号链接。
            use std::io::Write;
            let mut target = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|_| invalid("无法保存附件，文件已存在或目录不可写"))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&file.data)
                .map_err(|_| invalid("附件内容编码无效"))?;
            target
                .write_all(&bytes)
                .map_err(|_| invalid("无法写入附件内容"))?;
        }
        paths.push(if input.kind == "folder" {
            root
        } else {
            slot.join(&input.name)
        });
    }
    Ok(paths)
}

pub(super) async fn stage(
    directory: PathBuf,
    payload: &Value,
    authorization: &HarnessAuthorization,
) -> anyhow::Result<Vec<PathBuf>> {
    let inputs = parse(
        payload,
        authorization.device.key_id == desktop::DEVICE_KEY_ID,
    )?;
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    authorization.check().await?;
    let owner = authorization.device.id.clone();
    // 同一回执只有首次执行会写附件，回执未知时仍沿用已有核对流程，不能重新上传并发送。
    let request = authorization
        .request_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let result = tokio::task::spawn_blocking(move || store(&directory, &owner, &request, &inputs))
        .await
        .map_err(|_| ApiError::new(503, "保存附件任务已中断，消息尚未发送"))??;
    authorization.check().await?;
    Ok(result)
}

pub(super) fn append_paths(payload: &mut Value, paths: Vec<String>) {
    if paths.is_empty() {
        return;
    }
    // 文件路径来自服务端落盘结果；用 JSON 字符串保留空格/中文，不能拼接为 shell 命令。
    let references = paths
        .iter()
        .map(|path| serde_json::to_string(path).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = payload["prompt"].as_str().unwrap_or("");
    payload["prompt"] = serde_json::json!(format!(
        "{prompt}\n\n附加的本地文件和文件夹：\n{references}"
    ));
    payload.as_object_mut().unwrap().remove("files");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn validate(value: &Value) -> Result<(), ApiError> {
        validate_for_device(value, false)
    }

    fn sample() -> Value {
        json!({"files":[
            {"kind":"folder","name":"素材","size":3,"directories":["空目录","nested"],
                "files":[{"path":"nested/a.txt","data":"YWJj"}]},
            {"kind":"file","name":"empty.txt","size":0,"directories":[],
                "files":[{"path":"empty.txt","data":""}]}
        ]})
    }

    #[test]
    fn input_files_validate_names_paths_base64_sizes_and_collisions() {
        assert!(validate(&sample()).is_ok());
        for path in [
            "../escape",
            "/absolute",
            "C:/absolute",
            "a\\b",
            "a/../b",
            "NUL",
            "a:stream",
            "a.",
        ] {
            let mut value = sample();
            value["files"][0]["files"][0]["path"] = json!(path);
            assert!(validate(&value).is_err(), "{path}");
        }
        let mut value = sample();
        value["files"][0]["size"] = json!(2);
        assert!(validate(&value).is_err());
        let mut value = sample();
        value["files"][0]["files"][0]["data"] = json!("bad");
        assert!(validate(&value).is_err());
        let mut value = sample();
        value["files"][0]["directories"] = json!(["nested/A.TXT"]);
        assert!(validate(&value).is_err());
        let mut value = sample();
        value["files"][0]["directories"] = json!(["nested/a.txt/child"]);
        assert!(validate(&value).is_err());
    }

    #[test]
    fn input_files_store_preserves_structure_separates_devices_and_never_overwrites() {
        let temporary = tempfile::tempdir().unwrap();
        let parsed = parse(&sample(), false).unwrap();
        let first = store(temporary.path(), "device-a", "request-a", &parsed).unwrap();
        let other = store(temporary.path(), "device-b", "request-a", &parsed).unwrap();
        assert_ne!(first, other);
        assert!(first[0].join("空目录").is_dir());
        assert_eq!(
            std::fs::read(first[0].join("nested/a.txt")).unwrap(),
            b"abc"
        );
        assert_eq!(std::fs::read(&first[1]).unwrap(), b"");
        assert!(store(temporary.path(), "device-a", "request-a", &parsed).is_err());
        assert_eq!(
            std::fs::read(first[0].join("nested/a.txt")).unwrap(),
            b"abc"
        );
    }

    #[test]
    fn input_files_append_server_paths_and_remove_uploaded_content_before_dispatch() {
        let mut value = sample();
        value["prompt"] = json!("请处理");
        append_paths(&mut value, vec!["/tmp/含 空格/a.txt".into()]);
        assert!(value.get("files").is_none());
        assert!(value["prompt"].as_str().unwrap().starts_with("请处理\n\n"));
        assert!(
            value["prompt"]
                .as_str()
                .unwrap()
                .contains("\"/tmp/含 空格/a.txt\"")
        );
        let mut empty = json!({"prompt":"","files":[]});
        append_paths(&mut empty, vec![]);
        assert_eq!(empty["prompt"], "");
    }

    #[test]
    fn input_files_reject_oversized_trees_and_non_directory_storage_roots() {
        let mut value = sample();
        value["files"][0]["directories"] =
            json!((0..512).map(|i| format!("d{i}")).collect::<Vec<_>>());
        assert!(validate(&value).is_err());
        let temporary = tempfile::tempdir().unwrap();
        std::fs::write(temporary.path().join("remote-input-files"), b"occupied").unwrap();
        assert!(
            store(
                temporary.path(),
                "device",
                "request",
                &parse(&sample(), false).unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn desktop_input_references_keep_a_complete_snapshot_and_reject_remote_or_changed_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        let original = temporary.path().join("original.bin");
        let size = 40 * 1024 * 1024 + 17;
        let source = std::fs::File::create(&original).unwrap();
        source.set_len(size).unwrap();
        drop(source);
        let manifest = json!([{"kind":"folder","name":"大附件","size":size,"directories":["空目录"],
            "files":[{"path":"子目录/测试.bin","size":size}]}]);
        let entries = stage_desktop_files(
            temporary.path(),
            "local-a",
            manifest,
            vec![std::fs::File::open(&original).unwrap()],
        )
        .unwrap();
        std::fs::remove_file(original).unwrap();
        let payload = json!({"files":entries});
        assert_eq!(validate(&payload).unwrap_err().code, 403);
        let parsed = parse(&payload, true).unwrap();
        let paths = store(temporary.path(), "local-a", "send-a", &parsed).unwrap();
        assert_eq!(
            std::fs::metadata(paths[0].join("子目录/测试.bin"))
                .unwrap()
                .len(),
            size
        );
        assert!(paths[0].join("空目录").is_dir());
        assert!(store(temporary.path(), "local-b", "send-b", &parsed).is_err());
        let mut wrong = payload;
        wrong["files"][0]["name"] = json!("其他名字");
        assert!(
            store(
                temporary.path(),
                "local-a",
                "send-c",
                &parse(&wrong, true).unwrap()
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn desktop_input_upload_streams_above_the_action_limit_and_requires_local_identity() {
        let (temporary, authorization) = super::super::tests::service_dispatch_fixture().await;
        let authorization = super::super::tests::desktop_authorization(authorization).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let remote_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote_port = remote_listener.local_addr().unwrap().port();
        let local = super::super::scoped_router(
            authorization.controller.clone(),
            super::super::RemoteAccessSettings {
                port,
                ..Default::default()
            },
            true,
        );
        let remote = super::super::web_router(
            authorization.controller.clone(),
            super::super::RemoteAccessSettings {
                port: remote_port,
                ..Default::default()
            },
        );
        let local = tokio::spawn(async move { axum::serve(listener, local).await.unwrap() });
        let remote =
            tokio::spawn(async move { axum::serve(remote_listener, remote).await.unwrap() });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let request = |port, authenticated| {
            let origin = format!("http://127.0.0.1:{port}");
            let request = client
                .post(format!("{origin}/api/desktop/input-files"))
                .header("Origin", origin)
                .header(
                    "Content-Type",
                    "multipart/form-data; boundary=elves-fixture",
                );
            if authenticated {
                request.header("Cookie", "elves_desktop=isolated-service-dispatch-token")
            } else {
                request
            }
        };
        assert_eq!(request(port, false).send().await.unwrap().status(), 401);
        assert_eq!(
            request(remote_port, true).send().await.unwrap().status(),
            403
        );
        let size = 40 * 1024 * 1024 + 17;
        let manifest = |size| {
            json!([{"kind":"file","name":"big.bin","size":size,"directories":[],
            "files":[{"path":"big.bin","size":size}]}])
        };
        let start = |size| {
            format!(
                "--elves-fixture\r\nContent-Disposition: form-data; name=\"manifest\"\r\n\r\n{}\r\n\
            --elves-fixture\r\nContent-Disposition: form-data; name=\"file\"; filename=\"input\"\r\n\r\n",
                manifest(size)
            )
        };
        let end = b"\r\n--elves-fixture--\r\n";
        let bad = request(port, true)
            .body(format!(
                "{}x{}",
                start(2),
                std::str::from_utf8(end).unwrap()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 400);
        let owner = temporary
            .path()
            .join(STAGED_ROOT)
            .join(super::super::hash(&authorization.device.id));
        assert_eq!(
            std::fs::read_dir(owner).unwrap().count(),
            0,
            "不完整的批次不能留下可用副本"
        );
        let mut body = start(size).into_bytes();
        body.resize(body.len() + size, 0x5a);
        body.extend_from_slice(end);
        let response = request(port, true).body(body).send().await.unwrap();
        let status = response.status();
        let response: Value = response.json().await.unwrap();
        assert_eq!(status, 200, "{response}");
        let payload = json!({"files":response["inputs"]});
        let paths = stage(temporary.path().to_owned(), &payload, &authorization)
            .await
            .unwrap();
        let bytes = std::fs::read(&paths[0]).unwrap();
        assert_eq!(bytes.len(), size);
        assert!(bytes.iter().all(|byte| *byte == 0x5a));
        local.abort();
        remote.abort();
        let _ = local.await;
        let _ = remote.await;
    }
}
