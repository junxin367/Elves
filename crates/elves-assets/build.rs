// 用户要求缩短网页改动后的编译：公开资源只在这个轻量 crate 内嵌一次，供 Harness 和客户端共用。
// 扫描规则与 scripts/harness-resources.mjs、Android build.gradle 保持一致；正式程序仍包含全部资源。
use std::fmt::Write as _;
use std::path::{Component, Path};

fn main() {
    // 构建脚本可能跨 worktree 复用；必须读取运行时的包路径，监视路径也按当前包相对解析。
    // 不能把首次编译脚本时的 CARGO_MANIFEST_DIR 固化到可执行文件，否则会嵌入另一个检出的网页。
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let root = Path::new(&manifest).join("../../assets/harness");
    println!("cargo:rerun-if-changed=../../assets/harness");
    // 用户要求管理页产物不入 Git；直接 Cargo 构建必须拦截缺失或不完整的输出，不能生成缺少管理页的客户端。
    // Windows 构建脚本与 CI 先运行前端构建；这里不隐式安装依赖或在 Rust 构建中调用 npm。
    if let Err(error) = validate_management_bundle(&root) {
        panic!(
            "{error}\n请先执行 npm --prefix apps/elves-management ci，然后执行 \
             npm --prefix apps/elves-management run build:frontend，再重试 Cargo。"
        );
    }
    let revision = std::fs::read_to_string(root.join("resource-revision.txt"))
        .expect("missing web resource revision")
        .trim()
        .parse::<u64>()
        .expect("invalid web resource revision");
    assert!(revision > 0, "web resource revision must be positive");
    let mut resources = Vec::new();
    walk(&root, &root, &mut resources);
    resources.sort();
    let mut code = String::from("static WEB_ASSETS: &[WebAsset] = &[\n");
    for (route, mime, file) in resources {
        writeln!(
            &mut code,
            "    ({route:?}, {mime:?}, include_bytes!({file:?})),"
        )
        .unwrap();
    }
    code.push_str("];\n");
    let target = Path::new(&std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("web_assets.rs");
    // 扫描到的文件集合未变时不重写生成文件，避免仅有时间戳变化也让 Rust 重新解析资源表。
    if std::fs::read(&target).ok().as_deref() != Some(code.as_bytes()) {
        std::fs::write(target, code).expect("write web asset table");
    }
}

fn validate_management_bundle(root: &Path) -> Result<(), String> {
    let management = root.join("management");
    let index = management.join("index.html");
    let html = std::fs::read_to_string(&index).map_err(|error| {
        format!(
            "管理页构建入口不存在或不可读取：{}：{error}",
            index.display()
        )
    })?;
    let mut has_script = false;
    let mut has_style = false;
    for attribute in ["src=\"", "href=\"", "src='", "href='"] {
        let quote = attribute.chars().last().unwrap();
        for value in html.split(attribute).skip(1) {
            let target = value.split(quote).next().unwrap_or("");
            let Some(relative) = target.strip_prefix("/management/") else {
                continue;
            };
            let relative = relative.split(['?', '#']).next().unwrap_or("");
            let path = Path::new(relative);
            if relative.is_empty()
                || !path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
            {
                return Err(format!("管理页构建资源路径无效：{target}"));
            }
            let file = management.join(path);
            if !file.is_file() {
                return Err(format!("管理页引用的构建资源缺失：{}", file.display()));
            }
            match path.extension().and_then(|extension| extension.to_str()) {
                Some("js" | "mjs") if attribute.starts_with("src=") => has_script = true,
                Some("css") if attribute.starts_with("href=") => has_style = true,
                _ => {}
            }
        }
    }
    if !has_script || !has_style {
        return Err("管理页不是完整的生产构建：缺少 /management/ 下的脚本或样式引用。".into());
    }
    Ok(())
}

fn walk(root: &Path, directory: &Path, resources: &mut Vec<(String, &'static str, String)>) {
    for entry in std::fs::read_dir(directory).expect("read web assets") {
        let path = entry.expect("read web asset entry").path();
        if path.is_dir() {
            walk(root, &path, resources);
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .expect("web asset under root")
            .to_string_lossy()
            .replace('\\', "/");
        // 缓存恢复页及脚本独立提供（no-store），不进入版本壳清单。
        if matches!(
            relative.as_str(),
            "cache-recovery.html" | "cache-recovery.js"
        ) {
            continue;
        }
        let mime = match path.extension().and_then(|value| value.to_str()) {
            Some("html") => "text/html; charset=utf-8",
            Some("js" | "mjs") => "application/javascript; charset=utf-8",
            Some("css") => "text/css; charset=utf-8",
            Some("png") => "image/png",
            Some("svg") => "image/svg+xml",
            Some("webmanifest") => "application/manifest+json; charset=utf-8",
            _ => continue,
        };
        let route = if relative == "index.html" {
            "/".to_owned()
        } else {
            format!("/{relative}")
        };
        resources.push((
            route,
            mime,
            path.canonicalize()
                .expect("canonical web asset path")
                .to_string_lossy()
                .into_owned(),
        ));
    }
}
