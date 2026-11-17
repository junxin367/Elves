// Harness 工作区资源清单的唯一规则：assets/harness 目录下的每个公开网页文件都是版本壳资源，
// 新增或拆分模块只需放进该目录，无需逐处登记。以下实现按同一规则从目录生成清单，修改规则时须同步核对：
// - crates/elves-harness/build.rs（服务端内嵌资源与静态路由）
// - apps/elves-android/app/build.gradle（APK 内置网页包）
// - apps/elves/src-tauri/build.rs（Windows 独立客户端内置网页包）
// - scripts/dev-harness.mjs（本机调试服务器）
// 规则：按扩展名确定 MIME，index.html 提供为 "/"；缓存恢复页及脚本独立提供、不进入版本清单；
// 其余扩展名（版本号、许可证文本等）不作为网页资源。
import { readdir } from "node:fs/promises";
import { join, relative, resolve, sep } from "node:path";

export const harnessUiRoot = resolve(import.meta.dirname, "../assets/harness");

export const harnessResourceContentTypes = Object.freeze({
  ".html": "text/html; charset=utf-8",
  ".js": "application/javascript; charset=utf-8",
  ".mjs": "application/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".png": "image/png",
  // 用户指定品牌 SVG 直接用于网页；与各端内嵌资源表和缓存 MIME 校验同步。
  ".svg": "image/svg+xml",
  ".webmanifest": "application/manifest+json; charset=utf-8",
});

// 缓存恢复入口不属于任一版本壳，服务端以 no-store 单独提供。
export const separatelyServedResources = Object.freeze(["cache-recovery.html", "cache-recovery.js"]);

function extension(file) {
  const index = file.lastIndexOf(".");
  return index > file.lastIndexOf("/") ? file.slice(index) : "";
}

export function harnessResourceForFile(file) {
  const contentType = harnessResourceContentTypes[extension(file)];
  if (!contentType || separatelyServedResources.includes(file)) return null;
  return { route: file === "index.html" ? "/" : `/${file}`, file, contentType };
}

export async function listHarnessUiResources(root = harnessUiRoot) {
  const resources = [];
  async function walk(directory) {
    for (const entry of await readdir(directory, { withFileTypes: true })) {
      const path = join(directory, entry.name);
      if (entry.isDirectory()) await walk(path);
      else {
        const resource = harnessResourceForFile(relative(root, path).split(sep).join("/"));
        if (resource) resources.push(resource);
      }
    }
  }
  await walk(root);
  return resources.sort((a, b) => a.route < b.route ? -1 : a.route > b.route ? 1 : 0);
}
