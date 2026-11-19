// 图片媒体共用右键菜单，避免缩略图、消息正文和放大预览出现不同的操作。

import { copyImage } from "./clipboard.js";
import { openPopup } from "./popup.js";

export function bindImageContextMenu(trigger, image, { host = null } = {}) {
  if (!trigger?.addEventListener || !image || trigger.imageContextMenuBound) return;
  trigger.imageContextMenuBound = true;
  trigger.addEventListener("contextmenu", event => {
    event.preventDefault();
    event.stopPropagation();
    // 用户要求三处图片展示都能复制原图；菜单只提供这一项，左键预览仍保持原有行为。
    trigger.menuOptions = [{
      value: "copyImage",
      label: "复制图片",
      icon: "copy",
      action: () => void copyImage(image)
    }];
    trigger.popupClassName = "media-context-popup";
    trigger.popupWidth = 180;
    openPopup(trigger, {
      position: { x: event.clientX, y: event.clientY },
      focus: false,
      host
    });
  });
}
