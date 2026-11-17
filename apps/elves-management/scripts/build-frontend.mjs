import { createHash } from "node:crypto";
import { mkdir, readFile, readdir, rename, writeFile } from "node:fs/promises";
import { dirname, join, relative, resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");
const output = resolve(root, "../../assets/harness/management");
const cacheFile = join(root, "node_modules/.cache/elves/frontend-build.json");
// 产物在项目目录外；除依赖和 Git 元数据外完整扫描，新增 src/build 等业务目录也必须使缓存失效。
const excluded = new Set(["node_modules", ".git"]);
// Vite 会在构建时补齐 NODE_ENV；提前采用同一默认值，避免构建前后输入摘要仅因默认环境而不同。
if (!process.env.NODE_ENV) process.env.NODE_ENV = "production";

async function readOptional(path) {
  try { return await readFile(path); }
  catch (error) { if (error.code === "ENOENT") return null; throw error; }
}

async function fingerprint(directory, ignore = new Set()) {
  const files = [];
  async function walk(path) {
    for (const entry of await readdir(path, { withFileTypes: true })) {
      if (ignore.has(entry.name)) continue;
      const file = join(path, entry.name);
      if (entry.isDirectory()) await walk(file);
      else if (entry.isFile()) files.push(file);
    }
  }
  try { await walk(directory); }
  catch (error) { if (error.code === "ENOENT") return null; throw error; }
  const digest = createHash("sha256");
  for (const file of files.sort()) {
    const content = await readOptional(file);
    if (content === null) return null;
    digest.update(JSON.stringify([relative(directory, file), createHash("sha256").update(content).digest("hex")]));
  }
  return digest.digest("hex");
}

async function inputFingerprint() {
  const installed = await readOptional(join(root, "node_modules/.package-lock.json"));
  const environment = Object.entries(process.env)
    .filter(([key]) => key === "NODE_ENV" || key.startsWith("VITE_") && key !== "VITE_USER_NODE_ENV")
    .sort(([left], [right]) => left.localeCompare(right));
  return createHash("sha256")
    .update(JSON.stringify([1, process.version, process.platform, process.arch, environment]))
    .update(await fingerprint(root, excluded))
    .update(installed || "missing-installed-lock")
    .digest("hex");
}

if (process.argv.slice(2).some(argument => argument !== "--force")) {
  throw new Error("仅支持 --force；自定义 Vite 参数请使用 npm run vite:build。");
}

// 用户要求减少日常编译耗时：同时核对源码、配置、依赖锁定、环境及产物内容，不能仅按时间戳跳过。
// 缓存只影响前端是否重建；Harness/工具资源校验及正式 release 优化继续由 Windows 构建入口执行。
const input = await inputFingerprint();
let previous;
try { previous = JSON.parse((await readOptional(cacheFile))?.toString("utf8") || "null"); }
catch (error) { if (!(error instanceof SyntaxError)) throw error; }
if (!process.argv.includes("--force") && previous?.input === input
    && await readOptional(join(output, "index.html")) !== null
    && previous.output === await fingerprint(output)) {
  console.log("[CACHE] 共享管理页面源码、依赖及产物一致，复用已有构建。");
} else {
  const { build } = await import("vite");
  await build({ root });
  if (input === await inputFingerprint()) {
    const content = JSON.stringify({ input, output: await fingerprint(output) }) + "\n";
    await mkdir(dirname(cacheFile), { recursive: true });
    const temporary = `${cacheFile}.${process.pid}.tmp`;
    await writeFile(temporary, content);
    await rename(temporary, cacheFile);
  } else {
    console.warn("[INFO] 构建期间源码发生变化，下次将重新构建共享管理页面。");
  }
}
