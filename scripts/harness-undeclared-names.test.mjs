import test from "node:test";
import assert from "node:assert/strict";
import { resolve } from "node:path";
import { findUndeclaredNames, loadTypeScript } from "./harness-undeclared-names.mjs";

const ts = loadTypeScript(resolve(import.meta.dirname, "../apps/elves-management/package.json"));

function check(files) {
  const sources = new Map(Object.entries(files).map(([name, text]) => [`/virtual/${name}`, text]));
  return findUndeclaredNames(ts, sources).map(({ file, line, name }) => `${file.slice("/virtual/".length)}:${line}:${name}`);
}

test("漏掉导入的调用被发现（页面有 id=notice 元素时运行时只会报 notice is not a function）", { skip: !ts }, () => {
  assert.deepEqual(check({
    "a.js": 'export function recover(record) {\n  if (record.action === "editMessage")\n    notice("失败", true);\n}\n'
  }), ["a.js:3:notice"]);
  assert.deepEqual(check({
    "a.js": 'import { notice } from "./notice.js";\nexport function recover() { notice("失败", true); }\n'
  }), []);
});

test("模块层函数不能使用别的函数的参数（侧边会话已读回调曾因此抛 ReferenceError）", { skip: !ts }, () => {
  assert.deepEqual(check({
    "a.js": "export function mount(options) { return side(); }\nfunction side() {\n  return options.onRead?.(1);\n}\n"
  }), ["a.js:3:options"]);
  assert.deepEqual(check({
    "a.js": "export function mount(options) { return side(options.onRead); }\nfunction side(onRead) { return onRead?.(1); }\n"
  }), []);
});

test("简写属性和展开里的未声明名字同样被发现", { skip: !ts }, () => {
  assert.deepEqual(check({ "a.js": "export const value = { renderSidebar, ...missing };\n" }),
    ["a.js:1:renderSidebar", "a.js:1:missing"]);
});

test("局部声明、解构、参数默认值、catch、类和提升的函数都算已声明", { skip: !ts }, () => {
  assert.deepEqual(check({
    "a.js": [
      "export function run({ a, b: [c] }, d = a, ...rest) {",
      "  try { later(); } catch (error) { return [error, c, d, rest, label]; }",
      "  for (const item of rest) { if (item) break; }",
      "  const { x = 1, ...others } = d; class Local { static value = x; method() { return others; } }",
      "  return new Local();",
      "  function later() {}",
      "}",
      "const label = 1;"
    ].join("\n")
  }), []);
});

test("typeof 探测不计入，浏览器与 Service Worker 全局不误报", { skip: !ts }, () => {
  assert.deepEqual(check({
    "a.js": [
      'export const ok = typeof notDefinedAnywhere === "function";',
      "export const globals = [document, window, navigator, isSecureContext, HTMLDetailsElement, XMLSerializer,",
      "  Infinity, NaN, fetch, structuredClone, matchMedia, requestAnimationFrame, indexedDB, EventSource];"
    ].join("\n"),
    "sw.js": "self.addEventListener('fetch', event => { event.respondWith(caches.match(event.request)); self.clients.claim(); });\n"
  }), []);
});
