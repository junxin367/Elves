//! 单 EXE 自带本机后台所需的固定工具；只在明确启动本机后台时释放到用户缓存，不修改系统 PATH。
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use sha2::{Digest, Sha256};

const MANIFEST: &[u8] = include_bytes!("../../../../tools/tool-manifest.json");
const FILES: &[(&str, &[u8])] = &[
    ("tool-manifest.json", MANIFEST),
    (
        "windows-x64/rg.exe",
        include_bytes!("../../../../tools/windows-x64/rg.exe"),
    ),
    (
        "windows-x64/node_repl.exe",
        include_bytes!("../../../../tools/windows-x64/node_repl.exe"),
    ),
    (
        "windows-x64/LICENSE-MIT.txt",
        include_bytes!("../../../../tools/windows-x64/LICENSE-MIT.txt"),
    ),
    (
        "windows-x64/LICENSE-UNLICENSE.txt",
        include_bytes!("../../../../tools/windows-x64/LICENSE-UNLICENSE.txt"),
    ),
    (
        "windows-x64/LICENSE-NODE.txt",
        include_bytes!("../../../../tools/windows-x64/LICENSE-NODE.txt"),
    ),
];

fn regular_target(path: &Path) -> anyhow::Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            !metadata.file_type().is_symlink() && metadata.file_attributes() & 0x400 == 0,
            "本机工具缓存不能是符号链接或重解析点"
        );
    }
    Ok(())
}

pub fn prepare(state_directory: &Path) -> anyhow::Result<PathBuf> {
    // 用户要求安装程序只安装远程客户端；固定版本工具随 EXE 内嵌，原安装目录不再需要额外程序。
    std::fs::create_dir_all(state_directory)?;
    let cache = state_directory.join("tool-runtime");
    regular_target(&cache)?;
    std::fs::create_dir_all(&cache)?;
    let directory = cache.join(format!("{:x}", Sha256::digest(MANIFEST)));
    regular_target(&directory)?;
    std::fs::create_dir_all(&directory)?;
    let bin_directory = directory.join("windows-x64");
    regular_target(&bin_directory)?;
    std::fs::create_dir_all(&bin_directory)?;
    ensure!(
        directory
            .canonicalize()?
            .starts_with(state_directory.canonicalize()?),
        "本机工具缓存目录越界"
    );
    let manifest: serde_json::Value = serde_json::from_slice(MANIFEST)?;
    for tool in ["rg", "node_repl"] {
        let specification = &manifest["tools"][tool]["targets"]["windows-x64"];
        let path = specification["path"]
            .as_str()
            .context("内置工具清单缺少路径")?;
        let expected = specification["sha256"]
            .as_str()
            .context("内置工具清单缺少校验值")?;
        let bytes = FILES
            .iter()
            .find(|(name, _)| *name == path)
            .context("内置工具缺失")?
            .1;
        ensure!(
            format!("{:x}", Sha256::digest(bytes)).eq_ignore_ascii_case(expected),
            "内置工具校验失败：{tool}"
        );
    }
    for &(name, bytes) in FILES {
        let target = directory.join(name);
        regular_target(&target)?;
        if std::fs::read(&target).is_ok_and(|saved| saved == bytes) {
            continue;
        }
        let mut temporary = tempfile::NamedTempFile::new_in(target.parent().unwrap())?;
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        if let Err(error) = temporary.persist(&target) {
            // 两个客户端同时首次启动时，另一进程可能已写入同一份已校验工具。
            ensure!(
                std::fs::read(&target).is_ok_and(|saved| saved == bytes),
                "释放本机工具失败：{error}"
            );
        }
    }
    Ok(directory)
}
