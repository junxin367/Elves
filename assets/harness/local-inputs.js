// 原生选择器和 CF_HDROP 仅由独立客户端提供；浏览器拖放/粘贴走用户授予的 File/目录对象，不猜测原生命令。
export function localInputBridge() {
  let bridge = globalThis.window?.__CODEX_ELVES_LOCAL_INPUTS__;
  // PC 侧边会话沿用同来源主窗口的原生入口，草稿及迟到结果仍由侧边自己的附件代次约束。
  if (!bridge) try { bridge = globalThis.window?.parent?.__CODEX_ELVES_LOCAL_INPUTS__; } catch {}
  return typeof bridge?.pick === "function" && typeof bridge?.paste === "function" ? bridge : null;
}

export function isLocalInput(entry) { return entry?.kind === "file" || entry?.kind === "folder"; }

const stagedSlotId = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/u;
export const desktopImagePattern = /^\/api\/desktop\/input-files\/[0-9a-f-]{36}\/(?:[0-9]|1[0-5])\/image$/u;

export function stagedImageUrl(entry) {
  const slot = entry?.staged;
  return slot && typeof slot.id === "string" && stagedSlotId.test(slot.id)
    && Number.isInteger(slot.index) && slot.index >= 0 && slot.index < 16 && slot.entries === 1
    ? `/api/desktop/input-files/${slot.id}/${slot.index}/image` : null;
}

export function imageAttachmentPayload(entry) {
  const { name, mime, size } = entry;
  return entry.kind === "staged-image"
    ? { name, mime, size, staged: { ...entry.staged } }
    : { name, mime, dataUrl: entry.dataUrl, size };
}

export function copyLocalInput(entry) {
  if (entry.staged) {
    const { id, index, entries } = entry.staged;
    return { kind: entry.kind, name: entry.name, size: entry.size, staged: { id, index, entries } };
  }
  return { kind: entry.kind, name: entry.name, size: entry.size,
    files: entry.files.map(({ path, data }) => ({ path, data })), directories: [...entry.directories] };
}

// 与原生 local_inputs.rs、服务端 input_files.rs 同步。12 MiB 仅用于内联内容，本机暂存只校验引用及目录项。
export function validateLocalInputs(inputs, { allowStaged = false } = {}) {
  if (!Array.isArray(inputs) || inputs.length > 16) throw new Error("每条消息最多添加 16 个文件或文件夹");
  let total = 0, entries = inputs.length;
  const component = name => typeof name === "string" && !!name && new TextEncoder().encode(name).length <= 255
    && ![".", ".."].includes(name) && !/[. ]$/u.test(name)
    && !/[\u0000-\u001f\u007f<>:"/\\|?*]/u.test(name)
    && !/^(?:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)/iu.test(name);
  const relative = path => typeof path === "string" && new TextEncoder().encode(path).length <= 4096
    && path.split("/").length <= 32 && path.split("/").every(component);
  for (const input of inputs) {
    if (!isLocalInput(input) || !component(input.name) || !Number.isSafeInteger(input.size) || input.size < 0)
      throw new Error("文件或文件夹附件无效，请重新选择");
    if (input.staged != null) {
      const { id, index, entries: count } = input.staged;
      if (!allowStaged || "files" in input || "directories" in input
          || typeof id !== "string" || !stagedSlotId.test(id)
          || !Number.isInteger(index) || index < 0 || index >= 16
          || !Number.isInteger(count) || count < 0 || count >= 512 || input.kind === "file" && count !== 1)
        throw new Error("本机附件引用无效，请重新添加");
      entries += count;
      if (entries > 512) throw new Error("每条消息最多包含 512 个文件和目录项");
      continue;
    }
    if (!Array.isArray(input.files) || !Array.isArray(input.directories)
        || input.kind === "file" && (input.directories.length || input.files.length !== 1 || input.files[0]?.path !== input.name))
      throw new Error("文件或文件夹附件无效，请重新选择");
    const paths = [...input.directories, ...input.files.map(file => file?.path)];
    if (paths.some(path => !relative(path)) || new Set(paths.map(path => path.toLowerCase())).size !== paths.length)
      throw new Error("附件包含越界、重复或无效路径");
    const filePaths = new Set(input.files.map(file => file.path.toLowerCase()));
    if (paths.some(path => path.split("/").slice(0, -1).some((_, index, parts) =>
      filePaths.has(parts.slice(0, index + 1).join("/").toLowerCase()))))
      throw new Error("附件文件与目录路径冲突");
    let size = 0;
    for (const file of input.files) {
      if (typeof file.data !== "string" || file.data.length > 16 * 1024 * 1024
          || file.data.length % 4 || !/^[A-Za-z0-9+/]*={0,2}$/u.test(file.data)
          || file.data.length > 0 && file.data.length < 4)
        throw new Error("附件内容编码无效或超过大小限制");
      size += file.data.length / 4 * 3 - (file.data.endsWith("==") ? 2 : file.data.endsWith("=") ? 1 : 0);
    }
    total += size; entries += paths.length;
    if (size !== input.size) throw new Error("附件大小与实际内容不一致");
    if (total > 12 * 1024 * 1024) throw new Error("文件和文件夹总大小不能超过 12 MiB");
    if (entries > 512) throw new Error("每条消息最多包含 512 个文件和目录项");
  }
  return inputs.map(copyLocalInput);
}
