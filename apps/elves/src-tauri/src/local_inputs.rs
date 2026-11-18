//! 独立客户端本地附件：仅从用户选择或文件剪贴板取得路径，不接受网页传入任意本机路径。
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::Serialize;

// 与网页 local-inputs.js、服务端 input_files.rs 同步：16 个顶层附件、512 个目录项。
// 12 MiB 仅保留给旧网页的内联协议；新本机页面传文件流，不设固定容量上限。
const MAX_ROOTS: usize = 16;
const MAX_ENTRIES: usize = 512;
const LEGACY_MAX_BYTES: u64 = 12 * 1024 * 1024;
#[cfg(windows)]
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub(crate) struct LocalInput {
    kind: &'static str,
    name: String,
    size: u64,
    files: Vec<LocalFile>,
    directories: Vec<String>,
}

#[derive(Debug, Serialize)]
struct LocalFile {
    path: String,
    data: String,
}

pub(crate) struct CapturedInput {
    kind: &'static str,
    name: String,
    files: Vec<CapturedFile>,
    directories: Vec<String>,
}

struct CapturedFile {
    path: String,
    file: File,
    size: u64,
}

fn regular_metadata(path: &Path) -> Result<std::fs::Metadata, String> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| format!("读取附件失败：{error}"))?;
    #[cfg(windows)]
    let reparse = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let reparse = false;
    if metadata.is_symlink() || reparse || !metadata.is_file() && !metadata.is_dir() {
        return Err("附件包含符号链接、重解析点或特殊文件，请选择普通文件和文件夹。".into());
    }
    Ok(metadata)
}

pub(crate) fn capture(paths: Vec<PathBuf>) -> Result<Vec<CapturedInput>, String> {
    if paths.len() > MAX_ROOTS {
        return Err(format!("一次最多选择 {MAX_ROOTS} 个文件或文件夹。"));
    }
    let mut count = paths.len();
    let mut result = Vec::new();
    for path in paths {
        let metadata = regular_metadata(&path)?;
        let selected_root = std::fs::canonicalize(&path).map_err(|_| "无法确认附件的实际位置。")?;
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|name| !name.is_empty() && name.len() <= 255)
            .ok_or("附件名称无效或过长。")?
            .to_owned();
        let directory = metadata.is_dir();
        let mut input = CapturedInput {
            kind: if directory { "folder" } else { "file" },
            name: name.clone(),
            files: Vec::new(),
            directories: Vec::new(),
        };
        let mut pending = vec![(
            path.clone(),
            if directory { String::new() } else { name },
            0usize,
        )];
        while let Some((entry, relative, depth)) = pending.pop() {
            if !relative.is_empty() {
                count += 1;
            }
            if count > MAX_ENTRIES || depth > 32 {
                return Err("文件夹最多包含 512 个目录项，目录层级不能超过 32 层。".into());
            }
            let metadata = regular_metadata(&entry)?;
            if metadata.is_dir() {
                if !relative.is_empty() {
                    input.directories.push(relative.clone());
                }
                let children = std::fs::read_dir(&entry)
                    .map_err(|error| format!("无法读取文件夹：{error}"))?;
                for child in children {
                    let child = child.map_err(|error| format!("无法读取目录项：{error}"))?;
                    let name = child
                        .file_name()
                        .into_string()
                        .map_err(|_| "文件名称编码无效。")?;
                    let child_relative = if relative.is_empty() {
                        name
                    } else {
                        format!("{relative}/{name}")
                    };
                    pending.push((child.path(), child_relative, depth + 1));
                    if count + pending.len() > MAX_ENTRIES {
                        return Err("文件夹最多包含 512 个目录项。".into());
                    }
                }
            } else {
                let mut options = std::fs::OpenOptions::new();
                options.read(true);
                #[cfg(windows)]
                {
                    use std::os::windows::fs::OpenOptionsExt;
                    options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT：不跟随最终项被替换的链接。
                }
                let file = options
                    .open(&entry)
                    .map_err(|error| format!("无法打开附件：{error}"))?;
                let opened = file.metadata().map_err(|_| "无法确认附件文件类型。")?;
                if !opened.is_file() {
                    return Err("附件已被替换为链接或特殊文件，请重新选择。".into());
                }
                #[cfg(windows)]
                {
                    use std::os::windows::ffi::OsStringExt;
                    use std::os::windows::fs::MetadataExt;
                    use std::os::windows::io::AsRawHandle;
                    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
                    if opened.file_attributes() & 0x400 != 0 {
                        return Err("附件已被替换为链接或特殊文件，请重新选择。".into());
                    }
                    let mut buffer = vec![0u16; 32_768];
                    let length = unsafe {
                        GetFinalPathNameByHandleW(
                            file.as_raw_handle() as _,
                            buffer.as_mut_ptr(),
                            buffer.len() as u32,
                            0,
                        )
                    } as usize;
                    if length == 0
                        || length >= buffer.len()
                        || !PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length]))
                            .starts_with(&selected_root)
                    {
                        return Err("附件的实际位置已超出所选范围，请重新选择。".into());
                    }
                }
                #[cfg(not(windows))]
                if !std::fs::canonicalize(&entry)
                    .map_err(|_| "无法确认附件位置。")?
                    .starts_with(&selected_root)
                {
                    return Err("附件的实际位置已超出所选范围。".into());
                }
                // 持有已校验的打开句柄，流式发送时不再按路径重开。换成链接不能改变后续读取对象。
                regular_metadata(&entry)?;
                input.files.push(CapturedFile {
                    path: relative,
                    file,
                    size: opened.len(),
                });
            }
        }
        input
            .files
            .sort_by(|left, right| left.path.cmp(&right.path));
        input.directories.sort();
        result.push(input);
    }
    Ok(result)
}

fn encode_inline(input: CapturedInput, limit: u64, reason: &str) -> Result<LocalInput, String> {
    let mut result = LocalInput {
        kind: input.kind,
        name: input.name,
        size: 0,
        files: Vec::new(),
        directories: input.directories,
    };
    for file in input.files {
        let remaining = limit - result.size;
        if file.size > remaining {
            return Err(reason.into());
        }
        let mut bytes = Vec::new();
        file.file
            .take(remaining + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("无法读取附件：{error}"))?;
        if bytes.len() as u64 > remaining {
            return Err(reason.into());
        }
        if bytes.len() as u64 != file.size {
            return Err("附件内容已变化，请重新添加。".into());
        }
        result.size += bytes.len() as u64;
        result.files.push(LocalFile {
            path: file.path,
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
    }
    Ok(result)
}

pub(crate) fn read_legacy(inputs: Vec<CapturedInput>) -> Result<Vec<LocalInput>, String> {
    let mut total = 0u64;
    inputs
        .into_iter()
        .map(|input| {
            let input = encode_inline(
                input,
                LEGACY_MAX_BYTES - total,
                "文件和文件夹的总大小不能超过 12 MiB。",
            )?;
            total += input.size;
            Ok(input)
        })
        .collect()
}

#[cfg(windows)]
fn clipboard_image(input: &CapturedInput) -> bool {
    // 与 attachments.js 的图片扩展名一致；仅剪贴板的顶层图片走原有图片规则，文件夹一律保存原始内容。
    input.kind == "file"
        && Path::new(&input.name)
            .extension()
            .and_then(|part| part.to_str())
            .is_some_and(|extension| {
                matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "png"
                        | "apng"
                        | "jpg"
                        | "jpeg"
                        | "jpe"
                        | "jfif"
                        | "jif"
                        | "jfi"
                        | "pjpeg"
                        | "pjp"
                        | "gif"
                        | "webp"
                        | "bmp"
                        | "avif"
                        | "svg"
                        | "ico"
                        | "cur"
                )
            })
}

#[cfg(windows)]
pub(crate) fn stage(
    inputs: Vec<CapturedInput>,
    classify_images: bool,
    stage_images: bool,
    directory: &Path,
    device_id: &str,
) -> Result<serde_json::Value, String> {
    use serde_json::{Value, json};

    let mut manifest = Vec::new();
    let mut sources = Vec::new();
    let mut result = Vec::with_capacity(inputs.len());
    let mut image_bytes = 0;
    for input in inputs {
        // 本机 Codex 常用格式按原图落盘；WSL Claude、旧页面及需要兼容转码的格式保持原内联协议。
        let native_image = stage_images && Path::new(&input.name).extension().and_then(|value| value.to_str())
            .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp"));
        if classify_images && clipboard_image(&input) && !native_image {
            let available = LEGACY_MAX_BYTES - image_bytes;
            let reason = if available < MAX_IMAGE_BYTES {
                "图片总大小不能超过 12 MiB"
            } else {
                "单张图片不能超过 5 MiB"
            };
            let input = encode_inline(input, available.min(MAX_IMAGE_BYTES), reason)?;
            image_bytes += input.size;
            result.push(serde_json::to_value(input).map_err(|error| error.to_string())?);
            continue;
        }
        let size = input
            .files
            .iter()
            .try_fold(0u64, |total, file| total.checked_add(file.size))
            .ok_or("附件大小无法表示。")?;
        manifest.push(json!({"kind":input.kind,"name":input.name,"size":size,"directories":input.directories,
            "files":input.files.iter().map(|file| json!({"path":file.path,"size":file.size})).collect::<Vec<_>>()}));
        sources.extend(input.files.into_iter().map(|entry| entry.file));
        result.push(Value::Null);
    }
    if !manifest.is_empty() {
        // 用户要求本机大文件不经服务中转：原生进程直接存盘，只将回执引用交给 WebView。
        let staged =
            elves_harness::stage_desktop_files(directory, device_id, json!(manifest), sources)
                .map_err(|error| error.to_string())?;
        for (target, entry) in result.iter_mut().filter(|entry| entry.is_null()).zip(
            staged
                .as_array()
                .expect("stage_desktop_files returns an array"),
        ) {
            *target = entry.clone();
        }
    }
    Ok(json!(result))
}

#[cfg(windows)]
pub(crate) fn choose(parent: isize, folders: Option<bool>) -> Result<Vec<PathBuf>, String> {
    // 旧网页缓存仍传布尔值；新版“文件和文件夹”不传模式，直接打开可混选的一个窗口。
    let mode = match folders {
        Some(false) => picker::Mode::Files,
        Some(true) => picker::Mode::Folders,
        None => picker::Mode::Mixed,
    };
    picker::choose(parent, mode)
}

#[cfg(windows)]
pub(crate) fn choose_project_folder(parent: isize) -> Result<Option<PathBuf>, String> {
    Ok(picker::choose(parent, picker::Mode::Project)?
        .into_iter()
        .next())
}

#[cfg(windows)]
mod picker {
    use std::cell::RefCell;

    use super::{MAX_ROOTS, PathBuf};
    use windows::Win32::Foundation::{HWND, S_OK};
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
    use windows::Win32::UI::Shell::{
        CDCS_ENABLEDVISIBLE, CDCS_VISIBLE, FDE_OVERWRITE_RESPONSE, FDE_SHAREVIOLATION_RESPONSE,
        FDEOR_DEFAULT, FDESVR_DEFAULT, FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST,
        FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, IFileDialog,
        IFileDialogControlEvents, IFileDialogControlEvents_Impl, IFileDialogCustomize,
        IFileDialogEvents, IFileDialogEvents_Impl, IFileOpenDialog, IShellItem, IShellItemArray,
        SIGDN_FILESYSPATH,
    };
    use windows::core::{BOOL, ComObject, Interface, Ref, Result as WinResult, implement, w};

    pub(super) enum Mode {
        Files,
        Folders,
        Mixed,
        Project,
    }

    // 此 ID 仅用于当前系统对话框自定义控件及其事件。
    const ADD_SELECTED: u32 = 1001;
    type Selection = Result<Vec<PathBuf>, String>;

    #[implement(IFileDialogEvents, IFileDialogControlEvents)]
    struct MixedSelection {
        result: RefCell<Option<Selection>>,
    }

    impl IFileDialogEvents_Impl for MixedSelection_Impl {
        fn OnFileOk(&self, _: Ref<'_, IFileDialog>) -> WinResult<()> {
            Ok(())
        }
        fn OnFolderChanging(
            &self,
            _: Ref<'_, IFileDialog>,
            _: Ref<'_, IShellItem>,
        ) -> WinResult<()> {
            Ok(())
        }
        fn OnFolderChange(&self, dialog: Ref<'_, IFileDialog>) -> WinResult<()> {
            let controls: IFileDialogCustomize = dialog.ok()?.cast()?;
            unsafe { controls.SetControlState(ADD_SELECTED, CDCS_VISIBLE) }
        }
        fn OnSelectionChange(&self, dialog: Ref<'_, IFileDialog>) -> WinResult<()> {
            let dialog: IFileOpenDialog = dialog.ok()?.cast()?;
            let controls: IFileDialogCustomize = dialog.cast()?;
            let enabled = unsafe { dialog.GetSelectedItems().and_then(|items| items.GetCount()) }
                .is_ok_and(|count| count > 0);
            unsafe {
                controls.SetControlState(
                    ADD_SELECTED,
                    if enabled {
                        CDCS_ENABLEDVISIBLE
                    } else {
                        CDCS_VISIBLE
                    },
                )
            }
        }
        fn OnShareViolation(
            &self,
            _: Ref<'_, IFileDialog>,
            _: Ref<'_, IShellItem>,
        ) -> WinResult<FDE_SHAREVIOLATION_RESPONSE> {
            Ok(FDESVR_DEFAULT)
        }
        fn OnTypeChange(&self, _: Ref<'_, IFileDialog>) -> WinResult<()> {
            Ok(())
        }
        fn OnOverwrite(
            &self,
            _: Ref<'_, IFileDialog>,
            _: Ref<'_, IShellItem>,
        ) -> WinResult<FDE_OVERWRITE_RESPONSE> {
            Ok(FDEOR_DEFAULT)
        }
    }

    impl IFileDialogControlEvents_Impl for MixedSelection_Impl {
        fn OnItemSelected(
            &self,
            _: Ref<'_, IFileDialogCustomize>,
            _: u32,
            _: u32,
        ) -> WinResult<()> {
            Ok(())
        }
        fn OnButtonClicked(
            &self,
            controls: Ref<'_, IFileDialogCustomize>,
            id: u32,
        ) -> WinResult<()> {
            if id != ADD_SELECTED {
                return Ok(());
            }
            let dialog: IFileOpenDialog = controls.ok()?.cast()?;
            let paths = unsafe { dialog.GetSelectedItems() }
                .map_err(|error| format!("无法取得所选文件和文件夹：{error}"))
                .and_then(|items| selected_paths(&items));
            // 用户需要在一个窗口混选；“打开”仍用于进入文件夹，只在点击“添加所选项”时确认列表中的选取。
            // Close 后系统不提供 GetResults，先保存本次选择；不把未选中的当前目录作为附件。
            if matches!(&paths, Ok(paths) if paths.is_empty()) {
                return Ok(());
            }
            *self.result.borrow_mut() = Some(paths);
            let closed = unsafe { dialog.Close(S_OK) };
            if closed.is_err() {
                self.result.borrow_mut().take();
            }
            closed
        }
        fn OnCheckButtonToggled(
            &self,
            _: Ref<'_, IFileDialogCustomize>,
            _: u32,
            _: BOOL,
        ) -> WinResult<()> {
            Ok(())
        }
        fn OnControlActivating(&self, _: Ref<'_, IFileDialogCustomize>, _: u32) -> WinResult<()> {
            Ok(())
        }
    }

    pub(super) fn choose(parent: isize, mode: Mode) -> Selection {
        // 与项目选择器共用现有主线程 STA；只为输入区混选安装自定义确认，不改变项目选择和旧缓存。
        unsafe {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
                    .map_err(|error| format!("无法打开文件选择器：{error}"))?;
            let mut options = dialog.GetOptions().map_err(|error| error.to_string())?
                | FOS_FORCEFILESYSTEM
                | FOS_PATHMUSTEXIST
                | FOS_FILEMUSTEXIST;
            if !matches!(mode, Mode::Project) {
                options |= FOS_ALLOWMULTISELECT;
            }
            if matches!(mode, Mode::Folders | Mode::Project) {
                options |= FOS_PICKFOLDERS;
            }
            if matches!(mode, Mode::Project) {
                dialog
                    .SetTitle(w!("添加文件夹"))
                    .map_err(|error| error.to_string())?;
                dialog
                    .SetOkButtonLabel(w!("添加文件夹"))
                    .map_err(|error| error.to_string())?;
            }
            dialog
                .SetOptions(options)
                .map_err(|error| error.to_string())?;

            let mixed = if matches!(mode, Mode::Mixed) {
                dialog
                    .SetTitle(w!("添加文件和文件夹"))
                    .map_err(|error| error.to_string())?;
                let controls: IFileDialogCustomize =
                    dialog.cast().map_err(|error| error.to_string())?;
                controls
                    .AddPushButton(ADD_SELECTED, w!("添加所选项"))
                    .map_err(|error| error.to_string())?;
                controls
                    .MakeProminent(ADD_SELECTED)
                    .map_err(|error| error.to_string())?;
                controls
                    .SetControlState(ADD_SELECTED, CDCS_VISIBLE)
                    .map_err(|error| error.to_string())?;
                let events = ComObject::new(MixedSelection {
                    result: RefCell::new(None),
                });
                let cookie = dialog
                    .Advise(&events.to_interface::<IFileDialogEvents>())
                    .map_err(|error| error.to_string())?;
                Some((events, cookie))
            } else {
                None
            };
            let shown = dialog.Show(Some(HWND(parent as _)));
            let result = mixed.and_then(|(events, cookie)| {
                let _ = dialog.Unadvise(cookie);
                events.result.borrow_mut().take()
            });
            if let Err(error) = shown {
                if error.code().0 as u32 == 0x800704c7 {
                    return Ok(Vec::new());
                }
                return Err(format!("文件选择失败：{error}"));
            }
            result.unwrap_or_else(|| {
                let items = dialog.GetResults().map_err(|error| error.to_string())?;
                selected_paths(&items)
            })
        }
    }

    fn selected_paths(items: &IShellItemArray) -> Selection {
        unsafe {
            let count = items.GetCount().map_err(|error| error.to_string())? as usize;
            if count > MAX_ROOTS {
                return Err(format!("一次最多选择 {MAX_ROOTS} 个文件或文件夹。"));
            }
            let mut paths = Vec::with_capacity(count);
            for index in 0..count {
                let item = items
                    .GetItemAt(index as u32)
                    .map_err(|error| error.to_string())?;
                let name = item
                    .GetDisplayName(SIGDN_FILESYSPATH)
                    .map_err(|error| format!("所选项不是可读取的本机文件或文件夹：{error}"))?;
                let path = name.to_string().map_err(|error| error.to_string());
                CoTaskMemFree(Some(name.0 as _));
                paths.push(PathBuf::from(path?));
            }
            Ok(paths)
        }
    }
}

#[cfg(windows)]
pub(crate) fn clipboard() -> Result<Vec<PathBuf>, String> {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows_sys::Win32::UI::Shell::DragQueryFileW;
    // 用户要求资源管理器 Ctrl+C / 输入区 Ctrl+V；只读取 CF_HDROP，不把普通文字当作本机路径。
    const CF_HDROP: u32 = 15;
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP) == 0 {
            return Ok(Vec::new());
        }
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("剪贴板正被其他程序占用，请重新粘贴。".into());
        }
        struct Clipboard;
        impl Drop for Clipboard {
            fn drop(&mut self) {
                unsafe {
                    CloseClipboard();
                }
            }
        }
        let _clipboard = Clipboard;
        let data = GetClipboardData(CF_HDROP);
        if data.is_null() {
            return Err("无法读取复制的文件和文件夹。".into());
        }
        let count = DragQueryFileW(data as _, u32::MAX, std::ptr::null_mut(), 0);
        if count as usize > MAX_ROOTS {
            return Err("一次最多粘贴 16 个文件或文件夹。".into());
        }
        let mut paths = Vec::new();
        for index in 0..count {
            let length = DragQueryFileW(data as _, index, std::ptr::null_mut(), 0);
            if length == 0 || length > 32_767 {
                return Err("复制的文件路径无效。".into());
            }
            let mut buffer = vec![0u16; length as usize + 1];
            if DragQueryFileW(data as _, index, buffer.as_mut_ptr(), buffer.len() as u32) != length
            {
                return Err("剪贴板内容已变化，请重新复制。".into());
            }
            use std::os::windows::ffi::OsStringExt;
            paths.push(PathBuf::from(std::ffi::OsString::from_wide(
                &buffer[..length as usize],
            )));
        }
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_inputs_read_files_nested_folders_and_empty_directories() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("素材");
        std::fs::create_dir_all(directory.join("子目录/空目录")).unwrap();
        std::fs::write(directory.join("子目录/说明.txt"), b"hello").unwrap();
        let file = temporary.path().join("empty.txt");
        std::fs::write(&file, []).unwrap();
        let inputs = read_legacy(capture(vec![directory, file]).unwrap()).unwrap();
        assert_eq!(inputs[0].kind, "folder");
        assert_eq!(inputs[0].size, 5);
        assert_eq!(inputs[0].directories, ["子目录", "子目录/空目录"]);
        assert_eq!(inputs[0].files[0].path, "子目录/说明.txt");
        assert_eq!(inputs[0].files[0].data, "aGVsbG8=");
        assert_eq!(inputs[1].kind, "file");
        assert_eq!(inputs[1].size, 0);
        assert_eq!(inputs[1].files[0].data, "");
    }

    #[test]
    fn local_inputs_reject_missing_files_and_limits_without_skipping_entries() {
        let temporary = tempfile::tempdir().unwrap();
        assert!(capture(vec![temporary.path().join("missing")]).is_err());
        assert!(
            capture(vec![temporary.path().to_owned(); 17])
                .err()
                .unwrap()
                .contains("16")
        );
        let file = temporary.path().join("large.bin");
        let size = 40 * 1024 * 1024 + 13;
        std::fs::File::create(&file).unwrap().set_len(size).unwrap();
        assert!(
            read_legacy(capture(vec![file.clone()]).unwrap())
                .unwrap_err()
                .contains("12 MiB")
        );
        #[cfg(windows)]
        {
            let result = stage(
                capture(vec![file]).unwrap(),
                false,
                false,
                temporary.path(),
                "isolated-desktop",
            )
            .unwrap();
            assert_eq!(result[0]["size"], size);
            assert!(result[0].get("staged").is_some());
            assert!(result[0].get("files").is_none(), "原生命令的回包只含引用");
        }
    }
}
