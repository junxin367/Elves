// 网页模块里“用了却没有导入或声明”的名字检查。
//
// 为什么需要：缺少导入不会在 ES module 链接阶段报错（链接只核对“导入了但对方没导出”），截取源码的 vm 测试也不会发现。
// 更隐蔽的是，页面里有 id="notice" 这类元素时，浏览器会把它变成同名全局变量，漏掉导入的调用只会在运行时报
// “notice is not a function”，而不是“未定义”；历史编辑失败的提示因此变成一条看不懂的报错（2026-11-25 实际发生）。
//
// 做法：用 TypeScript 自带的绑定与名称解析，只看“找不到名字”（2304/2552，以及简写属性的 18004）。不加载完整类型库（太慢），
// 全局名字清单取自 Node 的 globalThis 加 TypeScript 随包的 lib.dom.d.ts / lib.webworker.d.ts，随 TypeScript 升级自动更新。
import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";

// 管理端前端依赖 typescript；这里只借用它的解析器，缺失时由调用方跳过检查。
export function loadTypeScript(packageJsonPath) {
  try { return createRequire(packageJsonPath)("typescript"); } catch { return null; }
}

function libraryGlobals(ts) {
  const names = new Set(Object.getOwnPropertyNames(globalThis));
  // typescript 包的 lib.*.d.ts 与 typescript.js 同级。
  const directory = dirname(ts.sys.getExecutingFilePath());
  for (const file of ["lib.dom.d.ts", "lib.webworker.d.ts"]) {
    let text = "";
    try { text = readFileSync(join(directory, file), "utf8"); } catch { continue; }
    for (const match of text.matchAll(/^declare (?:var|let|const|function|class|namespace|enum) ([A-Za-z_$][\w$]*)/gm))
      names.add(match[1]);
  }
  for (const name of ["undefined", "globalThis", "arguments"]) names.delete(name);
  return [...names].filter(name => /^[A-Za-z_$][\w$]*$/.test(name));
}

const virtualGlobals = "/__harness_globals__.d.ts";
// 2304 找不到名字、2552 找不到名字（并建议了相近名字）、18004 简写属性 { name } 没有对应的值。
const undeclaredCodes = new Set([2304, 2552, 18004]);

/**
 * 返回 [{ file, line, name }]：在 sources（文件名 → 源码）里使用了未声明、未导入、也不是已知全局的名字。
 * `typeof 名字` 的写法在运行时不会抛错，不计入。
 */
export function findUndeclaredNames(ts, sources) {
  const globals = libraryGlobals(ts).map(name => `declare var ${name}: ${name === "NaN" || name === "Infinity" ? "number" : "any"};`)
    .join("\n");
  const options = {
    allowJs: true, checkJs: true, noEmit: true, noResolve: true, noLib: true, skipLibCheck: true, types: [],
    target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext
  };
  const host = ts.createCompilerHost(options, true);
  const originals = { getSourceFile: host.getSourceFile.bind(host), fileExists: host.fileExists.bind(host),
    readFile: host.readFile.bind(host) };
  // TypeScript 内部统一用正斜杠的路径；Windows 上传入的反斜杠路径要先对齐，否则按文件名取不到源码。
  const byName = new Map([...sources].map(([name, source]) => [name.replaceAll("\\", "/"), source]));
  const text = name => name === virtualGlobals ? globals : byName.get(name);
  host.getSourceFile = (name, languageVersion, ...rest) => text(name) !== undefined
    ? ts.createSourceFile(name, text(name), languageVersion, true) : originals.getSourceFile(name, languageVersion, ...rest);
  host.fileExists = name => text(name) !== undefined || originals.fileExists(name);
  host.readFile = name => text(name) ?? originals.readFile(name);
  // 只做名称解析，不需要跟进导入：直接当作未解析，省掉对每个相对路径逐个探测磁盘（Windows 盘上数千次查询要十几秒）。
  // “导入了但对方没导出”由 check-harness-assets.mjs 的链接检查负责。
  host.resolveModuleNameLiterals = literals => literals.map(() => ({ resolvedModule: undefined }));
  host.resolveModuleNames = names => names.map(() => undefined);
  host.resolveTypeReferenceDirectiveReferences = references => references.map(() => ({ resolvedTypeReferenceDirective: undefined }));
  host.directoryExists = () => false;
  const files = [...byName.keys()];
  const program = ts.createProgram([virtualGlobals, ...files], options, host);
  const findings = [];
  for (const file of files) {
    const sourceFile = program.getSourceFile(file);
    if (!sourceFile) continue;
    for (const diagnostic of program.getSemanticDiagnostics(sourceFile)) {
      if (!undeclaredCodes.has(diagnostic.code)) continue;
      const name = sourceFile.text.slice(diagnostic.start, diagnostic.start + diagnostic.length);
      if (insideTypeof(ts, sourceFile, diagnostic.start)) continue;
      findings.push({ file, line: sourceFile.getLineAndCharacterOfPosition(diagnostic.start).line + 1, name });
    }
  }
  return findings;
}

function insideTypeof(ts, sourceFile, position) {
  let found = null;
  (function visit(node) {
    if (position < node.getStart(sourceFile) || position >= node.getEnd()) return;
    found = node;
    ts.forEachChild(node, visit);
  })(sourceFile);
  return !!found && found.parent?.kind === ts.SyntaxKind.TypeOfExpression;
}
