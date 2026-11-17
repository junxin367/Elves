// 用户指定的 elves-logo.svg 是唯一品牌源；网页保留 SVG，原生图标按各平台用途生成 PNG / ICO。
import { readFile, writeFile, mkdir, copyFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");
const require = createRequire(resolve(root, "apps/elves-management/package.json"));
const { Resvg } = require("@resvg/resvg-js");
const source = resolve(root, "assets/harness/icons/elves-logo.svg");
const svg = await readFile(source, "utf8");
if (!/viewBox="0 0 1024 1024"/.test(svg) || /<(?:script|image|foreignObject)\b/i.test(svg)) {
  throw new Error("品牌源必须是 1024 方形矢量图，不能包含脚本、外部图片或嵌入页面。");
}

async function save(path, bytes) {
  const target = resolve(root, path);
  await mkdir(dirname(target), { recursive: true });
  await writeFile(target, bytes);
}

function png(value, size) {
  return new Resvg(value, {
    fitTo: { mode: "width", value: size },
    font: { loadSystemFonts: false },
  }).render().asPng();
}

function sourceTag(tag, attribute) {
  const matches = [...svg.matchAll(new RegExp(`<${tag}\\b[^>]*>`, "g"))]
    .map(match => match[0]).filter(value => value.includes(attribute));
  if (matches.length !== 1) throw new Error(`品牌单色图缺少唯一形状：${attribute}`);
  return matches[0];
}

function monochrome() {
  const body = sourceTag("path", 'fill="url(#body)"').replace('fill="url(#body)"', 'fill="white"');
  const window = sourceTag("rect", 'fill="url(#window)"')
    .replace(/fill="[^"]*"/, 'fill="black"').replace(/stroke="[^"]*"/, 'stroke="black"');
  const prompt = sourceTag("path", 'stroke="url(#btn)"').replace('stroke="url(#btn)"', 'stroke="white"');
  const cursor = sourceTag("path", 'stroke="#2CE9D0"').replace('stroke="#2CE9D0"', 'stroke="white"');
  return `<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">
    <defs><mask id="mark"><rect width="1024" height="1024" fill="black"/>${body}${window}${prompt}${cursor}</mask></defs>
    <rect width="1024" height="1024" fill="white" mask="url(#mark)"/></svg>`;
}

function adaptive(value) {
  // Android 前景画布为 108dp；缩放到 75% 后，原图约 78% 的图案占约 63dp，落在 66dp 安全区域内。
  // 外围由 launcher_background 补齐；这里必须与 Android 的背景色和两个 adaptive-icon XML 同步。
  const body = value.replace(/^[\s\S]*?<svg\b[^>]*>/, "").replace(/<\/svg>\s*$/, "");
  return `<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">
    <g transform="translate(128 128) scale(0.75)">${body}</g></svg>`;
}

function ico() {
  // Windows 窗口、托盘、任务栏与安装程序共享同一 ICO，包含小图标及高 DPI 尺寸，不能仅封装单张 256px 图片。
  const sizes = [16, 20, 24, 32, 48, 64, 128, 256];
  const frames = sizes.map(size => png(svg, size));
  const header = Buffer.alloc(6 + sizes.length * 16);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(sizes.length, 4);
  let offset = header.length;
  for (let index = 0; index < sizes.length; index += 1) {
    const entry = 6 + index * 16;
    header[entry] = header[entry + 1] = sizes[index] === 256 ? 0 : sizes[index];
    header.writeUInt16LE(1, entry + 4);
    header.writeUInt16LE(32, entry + 6);
    header.writeUInt32LE(frames[index].length, entry + 8);
    header.writeUInt32LE(offset, entry + 12);
    offset += frames[index].length;
  }
  return Buffer.concat([header, ...frames]);
}

for (const [path, size] of [
  ["apps/elves/src-tauri/icons/elves-app-icon.png", 256],
  ["assets/harness/icons/elves-pwa-192.png", 192],
  ["assets/harness/icons/elves-pwa-512.png", 512],
  ["assets/harness/icons/elves-apple-touch-icon.png", 180],
  ["apps/elves-android/app/src/main/res/drawable-nodpi/elves_login_logo.png", 512],
]) {
  await save(path, png(svg, size));
}
await save("apps/elves/src-tauri/icons/elves-app-icon.ico", ico());
await copyFile(source, resolve(root, "apps/elves/web/elves-startup-logo.svg"));
await save("apps/elves-android/app/src/main/res/drawable-nodpi/elves_launcher_foreground.png", png(adaptive(svg), 1024));
await save("apps/elves-android/app/src/main/res/drawable-nodpi/elves_launcher_monochrome.png", png(adaptive(monochrome()), 1024));
console.log("已从指定 SVG 生成 Windows、PWA、Apple touch、Android 图标和启动页 SVG。");
