// 用户要求图片、文件和文件夹可拖入/粘贴到编辑框；这里只读取浏览器在该次用户操作中授予的对象，不解析文字路径。
import { validateLocalInputs } from "./local-inputs.js";

const MAX_ROOTS = 16, MAX_ENTRIES = 512, MAX_BYTES = 12 * 1024 * 1024;

export function hasTransferFiles(transfer) {
  return Array.from(transfer?.items || []).some(item => item.kind === "file")
    || Array.from(transfer?.files || []).length > 0 || Array.from(transfer?.types || []).includes("Files");
}

export function captureTransferInputs(transfer) {
  const files = Array.from(transfer?.files || []);
  const items = Array.from(transfer?.items || []).filter(item => item.kind === "file");
  if (!items.length) return files.map(file => ({ file }));
  // DataTransfer 在事件结束后进入保护模式，文件、目录入口和异步 handle 必须在事件回调内取得。
  return items.map((item, index) => {
    const file = item.getAsFile?.() || files[index] || null;
    const entry = (item.getAsEntry || item.webkitGetAsEntry)?.call(item) || null;
    const result = { file, entry };
    if (!entry && (!file || !file.type && !file.size) && typeof item.getAsFileSystemHandle === "function") {
      // 立即捕获拒绝，避免编辑区已锁定而不继续读取时留下未处理的 Promise。
      result.handle = Promise.resolve(item.getAsFileSystemHandle())
        .then(handle => ({ handle }), error => ({ error }));
    }
    return result;
  });
}

function base64(bytes) {
  let binary = "";
  for (let offset = 0; offset < bytes.length; offset += 32768)
    binary += String.fromCharCode(...bytes.subarray(offset, offset + 32768));
  return btoa(binary);
}

export async function readTransferInputs(captured, { isImageFile = () => false, isCurrent = () => true } = {}) {
  if (!Array.isArray(captured) || captured.length > 24)
    throw new Error("一次最多添加 8 张图片和 16 个文件或文件夹");
  if (!captured.length) throw new Error("浏览器未提供可读取的附件，请重新拖入或使用独立客户端粘贴文件夹");
  const images = [], inputs = [];
  let total = 0, entries = 0, roots = 0;
  const deadline = Date.now() + 30000;
  const check = () => {
    if (!isCurrent()) { const error = new Error("附件读取已取消"); error.name = "AbortError"; throw error; }
    if (Date.now() >= deadline) throw new Error("附件读取超时，请重新拖入或粘贴");
  };
  async function wait(promise) {
    check();
    let timer;
    try {
      const value = await Promise.race([promise, new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error("附件读取超时，请重新拖入或粘贴")), deadline - Date.now());
      })]);
      check(); return value;
    } finally { clearTimeout(timer); }
  }
  const component = name => {
    // 与最终发送校验共用名称规则，在读取内容前拒绝越界名、设备名和跨平台路径冲突。
    validateLocalInputs([{ kind: "folder", name, size: 0, files: [], directories: [] }]);
    return name;
  };
  function count() {
    if (++entries > MAX_ENTRIES) throw new Error("每条消息最多包含 512 个文件和目录项");
  }
  async function readFile(file, path, input) {
    check();
    if (!(file instanceof Blob)) throw new Error("浏览器未提供可读取的文件内容");
    if (total + file.size > MAX_BYTES) throw new Error("文件和文件夹总大小不能超过 12 MiB");
    const bytes = new Uint8Array(await wait(file.arrayBuffer()));
    if (bytes.length !== file.size || total + bytes.length > MAX_BYTES)
      throw new Error("附件内容已变化或文件和文件夹总大小超过 12 MiB");
    total += bytes.length; input.size += bytes.length;
    input.files.push({ path, data: base64(bytes) });
  }
  function relative(parent, name) {
    const path = parent ? `${parent}/${component(name)}` : component(name);
    if (path.split("/").length > 32 || new TextEncoder().encode(path).length > 4096)
      throw new Error("文件夹目录层级不能超过 32 层，路径不能超过 4096 字节");
    return path;
  }
  async function visitEntry(entry, path, input) {
    check(); count();
    if (entry.isDirectory) {
      input.directories.push(path);
      await directoryEntry(entry, path, input);
    } else if (entry.isFile) {
      const file = await wait(new Promise((resolve, reject) => entry.file(resolve, reject)));
      await readFile(file, path, input);
    } else throw new Error("附件包含无法读取的目录项");
  }
  async function directoryEntry(entry, parent, input) {
    const reader = entry.createReader();
    for (;;) {
      check();
      const page = await wait(new Promise((resolve, reject) => reader.readEntries(resolve, reject)));
      if (!Array.isArray(page)) throw new Error("浏览器返回的目录内容无效");
      if (!page.length) break;
      if (entries + page.length > MAX_ENTRIES) throw new Error("每条消息最多包含 512 个文件和目录项");
      // Chromium 的目录入口分批返回，必须一直读到空页，不能只保留首批文件或丢弃空目录。
      for (const child of page) await visitEntry(child, relative(parent, child.name), input);
    }
  }
  async function visitHandle(handle, path, input) {
    check(); count();
    if (handle.kind === "directory") {
      input.directories.push(path);
      const iterator = handle.values()[Symbol.asyncIterator]();
      for (;;) {
        const next = await wait(iterator.next());
        if (next.done) break;
        await visitHandle(next.value, relative(path, next.value.name), input);
      }
    } else if (handle.kind === "file") {
      await readFile(await wait(handle.getFile()), path, input);
    } else throw new Error("浏览器未提供可读取的文件或文件夹");
  }
  for (const candidate of captured) {
    check();
    let { entry, file } = candidate, handle;
    if (candidate.handle) {
      const result = await wait(candidate.handle);
      if (result.error) throw new Error("浏览器未允许读取该文件或文件夹，请重新拖入");
      handle = result.handle;
    }
    const directory = entry?.isDirectory === true || handle?.kind === "directory";
    if (!directory && !file) {
      if (entry?.isFile) file = await wait(new Promise((resolve, reject) => entry.file(resolve, reject)));
      else if (handle?.kind === "file") file = await wait(handle.getFile());
    }
    if (!directory && file instanceof Blob && isImageFile(file)) {
      if (images.length >= 8) throw new Error("每条消息最多添加 8 张图片");
      images.push(file); continue;
    }
    if (++roots > MAX_ROOTS) throw new Error("每条消息最多添加 16 个文件或文件夹");
    count();
    const name = component(directory ? entry?.name || handle?.name : file?.name);
    const input = { kind: directory ? "folder" : "file", name, size: 0, files: [], directories: [] };
    if (directory) {
      if (entry) await directoryEntry(entry, "", input);
      else {
        const iterator = handle.values()[Symbol.asyncIterator]();
        for (;;) {
          const next = await wait(iterator.next());
          if (next.done) break;
          await visitHandle(next.value, relative("", next.value.name), input);
        }
      }
    } else {
      count(); await readFile(file, name, input);
    }
    inputs.push(input);
  }
  check();
  return { images, inputs: validateLocalInputs(inputs) };
}
