use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::GetProcessId;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    SHOpenFolderAndSelectItems, SHParseDisplayName, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::PCWSTR;

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain([0]).collect()
}

struct ShellFileItem(*mut ITEMIDLIST);

impl ShellFileItem {
    fn parse(path: &Path) -> io::Result<Self> {
        // 文件协议的相对路径使用 /；交给 Windows Shell 前转换为 \，仍使用已经校验过的根目录和原文件。
        let path = wide(OsStr::new(&super::display_path(path).replace('/', r"\")));
        let mut item = std::ptr::null_mut();
        unsafe { SHParseDisplayName(PCWSTR(path.as_ptr()), None, &mut item, 0, None) }
            .map_err(|error| io::Error::other(error.to_string()))?;
        if item.is_null() {
            return Err(io::Error::other("无法取得所选文件的原生位置"));
        }
        Ok(Self(item))
    }
}

impl Drop for ShellFileItem {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

pub(super) fn reveal_file(path: &Path) -> io::Result<()> {
    let _apartment = crate::windows_integration::ComApartment::init()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let item = ShellFileItem::parse(path)?;
    // 用户要求打开所点文件所在目录并选中文件；直接使用已校验文件的完整 Shell 身份，
    // cidl=0 表示定位该文件，避免 /select 被通用参数转义后误解析并退回“文档”等默认目录。
    // 解析或定位失败必须报错，不另开工作区根目录或自动重试。
    unsafe { SHOpenFolderAndSelectItems(item.0, None, 0) }
        .map_err(|error| io::Error::other(error.to_string()))
}

// ShellExecute 只接受参数字符串；按 Windows argv 规则转义每个独立参数，路径中的引号和尾部反斜杠不能变成额外命令。
fn quoted(value: &OsStr) -> Vec<u16> {
    let mut result = vec![b'"' as u16];
    let mut slashes = 0;
    for unit in value.encode_wide() {
        if unit == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        result.extend(std::iter::repeat_n(
            b'\\' as u16,
            if unit == b'"' as u16 {
                slashes * 2 + 1
            } else {
                slashes
            },
        ));
        result.push(unit);
        slashes = 0;
    }
    result.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    result.push(b'"' as u16);
    result
}

pub(super) fn launch(command: &mut Command) -> io::Result<Option<u32>> {
    // 保留 Electron 应用的既有环境隔离；存在该变量时继续使用已移除变量的 Command，禁止修改服务的全局环境。
    if std::env::var_os("ELECTRON_RUN_AS_NODE").is_some() {
        let program = command.get_program().to_string_lossy().to_lowercase();
        if ["\\pwsh.exe", "\\powershell.exe", "\\wsl.exe"]
            .iter()
            .any(|name| program.ends_with(name))
        {
            use std::os::windows::process::CommandExt;
            use std::process::Stdio;
            use windows::Win32::System::Threading::CREATE_NEW_CONSOLE;
            // GUI 后台没有可继承的交互控制台；直接启动终端的降级路径必须创建可见控制台并保留交互输入输出。
            command
                .creation_flags(CREATE_NEW_CONSOLE.0)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        }
        return command.spawn().map(|child| Some(child.id()));
    }
    let _apartment = crate::windows_integration::ComApartment::init().ok();
    let file = wide(command.get_program());
    let directory = command
        .get_current_dir()
        .map(|path| wide(OsStr::new(&super::display_path(path))));
    let terminal = command
        .get_program()
        .to_string_lossy()
        .to_lowercase()
        .ends_with("wt.exe");
    let mut parameters = Vec::new();
    for argument in command.get_args() {
        if !parameters.is_empty() {
            parameters.push(b' ' as u16);
        }
        // Windows Terminal 自己也解析分号；与其原生调用约定一致，保护目录中的分号，不能产生额外标签命令。
        if terminal {
            parameters.extend(quoted(OsStr::new(
                &argument.to_string_lossy().replace(';', r"\;"),
            )));
        } else {
            parameters.extend(quoted(argument));
        }
    }
    parameters.push(0);
    let verb = wide(OsStr::new("open"));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        lpDirectory: directory
            .as_ref()
            .map_or(PCWSTR::null(), |value| PCWSTR(value.as_ptr())),
        // 用户要求外部应用/终端可见打开；不用隐藏窗口或最小化启动，也不通过 cmd/PowerShell 中转。
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }.map_err(|error| io::Error::other(error.to_string()))?;
    if info.hProcess.is_invalid() {
        return Ok(None);
    }
    let process_id = unsafe { GetProcessId(info.hProcess) };
    let _ = unsafe { CloseHandle(info.hProcess) };
    Ok((process_id != 0).then_some(process_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_arguments_preserve_spaces_quotes_and_trailing_slashes() {
        for (input, expected) in [
            ("", "\"\""),
            ("D:\\项目 文件\\", "\"D:\\项目 文件\\\\\""),
            ("a\"b", "\"a\\\"b\""),
            ("E:\\a;b", "\"E:\\a;b\""),
        ] {
            assert_eq!(
                String::from_utf16(&quoted(OsStr::new(input))).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn shell_file_items_preserve_exact_paths_with_spaces_unicode_and_commas() {
        use windows::Win32::UI::Shell::{SHGetNameFromIDList, SIGDN_FILESYSPATH};

        let _apartment = crate::windows_integration::ComApartment::init().unwrap();
        let temp_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../temp");
        std::fs::create_dir_all(&temp_root).unwrap();
        let temp_root = std::fs::canonicalize(temp_root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("explorer-selection-")
            .tempdir_in(temp_root)
            .unwrap();
        for relative in ["plain.txt", "子目录 with spaces/文档,a;b #1.md"] {
            let file = directory.path().join(relative);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, b"shell file identity").unwrap();
            for path in [
                file.clone(),
                std::path::PathBuf::from(super::super::display_path(&file).replace('\\', "/")),
            ] {
                let item = ShellFileItem::parse(&path)
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
                let name = unsafe { SHGetNameFromIDList(item.0, SIGDN_FILESYSPATH) }.unwrap();
                let actual = unsafe { name.to_string() };
                unsafe { CoTaskMemFree(Some(name.0.cast())) };
                assert_eq!(
                    std::fs::canonicalize(actual.unwrap()).unwrap(),
                    std::fs::canonicalize(&file).unwrap()
                );
            }
        }
        assert!(ShellFileItem::parse(&directory.path().join("missing.md")).is_err());
    }
}
