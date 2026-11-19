// 剪贴板写入：文本优先使用异步接口，图片使用原图读取后写入系统图片剪贴板。

import { remoteMediaCandidates } from "../channels.js";
import { notice } from "./notice.js";

export async function copyText(text) {
  try {
    if (navigator.clipboard?.writeText) await navigator.clipboard.writeText(text);
    else {
      const previous = document.activeElement;
      const field = document.createElement("textarea");
      field.value = text;
      field.readOnly = true;
      field.style.position = "fixed";
      field.style.opacity = "0";
      document.body.append(field);
      try { field.select(); if (!document.execCommand("copy")) throw new Error("copy failed"); }
      finally { field.remove(); previous?.focus?.({ preventScroll: true }); }
    }
    notice("已复制");
  }
  catch { notice("无法访问剪贴板，请手动选择代码复制", true); }
}

async function imageBlob(image) {
  const source = image?.originalImageSource
    || image?.getAttribute?.("data-remote-media")
    || image?.currentSrc
    || image?.getAttribute?.("src")
    || image?.src;
  if (!source) throw new Error("图片地址不可用");
  const candidates = /^\/api\/images\//.test(source)
    ? remoteMediaCandidates(source) : [source];
  let blob = null, lastError = null;
  for (const candidate of candidates) {
    try {
      const response = await fetch(candidate, {
        cache: "no-store", credentials: "include", redirect: "error"
      });
      if (!response.ok) throw new Error(`图片读取失败（${response.status}）`);
      blob = await response.blob();
      break;
    } catch (error) {
      lastError = error;
    }
  }
  if (!blob) throw lastError || new Error("图片读取失败");
  if (!blob.type.startsWith("image/")) throw new Error("读取到的内容不是图片");
  if (blob.type === "image/png") return blob;
  if (typeof createImageBitmap !== "function") throw new Error("当前环境不支持图片转换");
  const bitmap = await createImageBitmap(blob);
  try {
    const canvas = document.createElement("canvas");
    canvas.width = bitmap.width;
    canvas.height = bitmap.height;
    canvas.getContext("2d").drawImage(bitmap, 0, 0);
    return await new Promise((resolve, reject) => {
      canvas.toBlob(value => value ? resolve(value) : reject(new Error("图片转换失败")), "image/png");
    });
  } finally {
    bitmap.close?.();
  }
}

export async function copyImage(image) {
  try {
    if (!navigator.clipboard?.write || !globalThis.ClipboardItem)
      throw new Error("当前环境不支持图片剪贴板");
    const blob = await imageBlob(image);
    await navigator.clipboard.write([new globalThis.ClipboardItem({ "image/png": blob })]);
    notice("图片已复制");
    return true;
  } catch {
    notice("无法复制图片，请检查剪贴板权限后重试", true);
    return false;
  }
}
