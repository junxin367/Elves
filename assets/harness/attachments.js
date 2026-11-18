import { remoteMediaCandidates, remoteMediaUrl } from "./channels.js";
import { sha256Hex } from "./sha256.js";
import { copyLocalInput, isLocalInput, localInputBridge, validateLocalInputs } from "./local-inputs.js";
import { captureTransferInputs, hasTransferFiles, readTransferInputs } from "./transfer-inputs.js";
import { icon } from "./ui/icons.js";

// 用户要求 Codex/Claude 单条固定最多 8 张；添加、粘贴、拖拽与草稿/队列恢复共用此值，并与 Rust MAX_INPUT_IMAGE_COUNT 同步。
const MAX_IMAGE_COUNT = 8;
const MAX_IMAGE_BYTES = 5 * 1024 * 1024;
const MAX_TOTAL_BYTES = 12 * 1024 * 1024;
const MAX_IMAGE_DIMENSION = 16_384;
const MAX_IMAGE_PIXELS = 100_000_000;
const remoteImagePattern = /^\/api\/images\/[A-Za-z0-9_-]{20,128}$/u;
const supportedMimes = new Set(["image/png", "image/jpeg", "image/webp", "image/gif"]);
// 用户要求增加图片格式；额外格式在客户端解码并转为 PNG，发送和恢复仍使用原生引擎支持的四种格式。
const convertibleMimes = new Set(["image/bmp", "image/avif", "image/svg+xml", "image/x-icon"]);
const imageFileExtensions = /\.(?:png|apng|jpe?g|jpe|jfif|jif|jfi|pjpeg|pjp|gif|webp|bmp|avif|svg|ico|cur)$/iu;
function canonicalImageMime(value) {
  const mime = String(value || "").toLowerCase().split(";", 1)[0].trim();
  return ({ "image/jpg": "image/jpeg", "image/pjpeg": "image/jpeg", "image/jfif": "image/jpeg",
    "image/apng": "image/png", "image/x-png": "image/png", "image/x-ms-bmp": "image/bmp",
    "image/x-bmp": "image/bmp", "image/vnd.microsoft.icon": "image/x-icon" })[mime] || mime;
}
function imageInputFile(file) {
  const mime = canonicalImageMime(file?.type);
  return supportedMimes.has(mime) || convertibleMimes.has(mime) || imageFileExtensions.test(file?.name || "");
}
const remoteVideoPattern = /^\/api\/videos\/[a-f0-9]{64}$/u;
const MEDIA_RECOVERY_TIMEOUT_MS = 15_000;

export function requestWorkspaceMediaPreview(resource) {
  if (document.documentElement.dataset.workspaceSidePanel !== "true" || window.parent === window) return false;
  // 用户要求 PC 标签中的资源在整个页面预览；只转交当前可见侧边页，手机整页及主会话仍走原入口。
  // 仅传资源身份，由主页面沿用既有图片/视频校验与加载，不传 DOM、任意地址或新的文件读取权限。
  if (document.documentElement.dataset.workspaceSidePanelVisible !== "false")
    window.parent.postMessage({ ...resource, type: "codex-elves-workspace-media-preview" }, location.origin);
  return true;
}

function loadVideoPoster(image, path, loaded = () => {}) {
  if (image.posterRecovery?.path === path) return;
  image.posterRecovery?.dispose();
  const candidates = [...new Set(remoteMediaCandidates(path))];
  let next = 0, disposed = false, settled = false;
  const currentSource = () => !image.currentSrc
    || image.currentSrc === new URL(image.getAttribute("src"), location.href).href;
  const advance = () => {
    if (disposed || settled) return;
    const source = candidates[next++];
    if (source) image.src = source;
    else image.hidden = true;
  };
  image.posterRecovery = {
    path,
    dispose() { disposed = true; image.onload = null; image.onerror = null; }
  };
  image.crossOrigin = "anonymous";
  image.onload = () => {
    if (disposed || settled || !currentSource()) return;
    settled = true;
    image.hidden = false;
    loaded(image.getAttribute("src"));
  };
  image.onerror = () => { if (currentSource()) advance(); };
  advance();
}

export function videoPreviewMarkup(asset, label = "") {
  if (!remoteVideoPattern.test(asset?.url || "") || asset.poster !== `${asset.url}/poster`) return "";
  const escape = value => String(value).replaceAll("&", "&amp;").replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;").replaceAll('"', "&quot;").replaceAll("'", "&#39;");
  const name = escape(label || asset.name || "视频");
  // 正文只包含静态封面；这里不能创建 video、source 或指向视频的链接。
  return `<button type="button" class="message-video-preview" title="${name}">`
    + `<span class="message-video-cover"><img class="message-video-poster" crossorigin="anonymous" data-remote-media="${escape(asset.poster)}" alt="${name}" loading="lazy" decoding="async">`
    + `<span class="message-video-badge">▶</span></span><span class="message-video-name">${name}</span></button>`;
}

function videoTime(value) {
  const seconds = Number.isFinite(value) && value > 0 ? Math.floor(value) : 0;
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
}

function videoPreviewIcon(name) {
  const paths = {
    close: '<path d="m6 6 12 12M18 6 6 18"/>',
    play: '<circle cx="12" cy="12" r="9"/><path d="m10 8 6 4-6 4Z" fill="currentColor" stroke="none"/>',
    pause: '<circle cx="12" cy="12" r="9"/><path d="M9 8h2v8H9ZM13 8h2v8h-2Z" fill="currentColor" stroke="none"/>'
  };
  const icon = document.createElement("span");
  icon.className = "ui-icon";
  icon.setAttribute("aria-hidden", "true");
  icon.innerHTML = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">${paths[name]}</svg>`;
  return icon;
}

export function fitMediaPreview(dialog, panel, media, dimensions, { fillAvailable = false, alignedControls = null } = {}) {
  const pixels = (style, property) => Number.parseFloat(style.getPropertyValue(property)) || 0;
  const resize = () => {
    if (!dialog.open) return;
    const { width, height } = dimensions?.() || {};
    if (!fillAvailable && (!width || !height)) return;
    const dialogStyle = getComputedStyle(dialog), panelStyle = getComputedStyle(panel);
    const margin = pixels(dialogStyle, "--preview-margin");
    const maxWidth = Math.max(1, pixels(dialogStyle, "max-width"));
    const maxHeight = Math.min(window.innerHeight - margin * 2, pixels(dialogStyle, "max-height") || Infinity);
    const frameWidth = pixels(dialogStyle, "border-left-width") + pixels(dialogStyle, "border-right-width")
      + pixels(panelStyle, "padding-left") + pixels(panelStyle, "padding-right");
    const framePadding = pixels(dialogStyle, "border-top-width") + pixels(dialogStyle, "border-bottom-width")
      + pixels(panelStyle, "padding-top") + pixels(panelStyle, "padding-bottom");
    const frameHeight = () => {
      let value = framePadding;
      for (const child of panel.children) {
        if (child === media) continue;
        const style = getComputedStyle(child);
        if (style.position === "absolute" || style.position === "fixed" || style.display === "none") continue;
        value += child.getBoundingClientRect().height + pixels(style, "margin-top") + pixels(style, "margin-bottom");
      }
      return value;
    };
    // 先按可用宽度测量文案，再确定窗口宽度，避免沿用上一帧的换行结果反复缩放。
    // 测量时移除媒体的旧尺寸，避免临时滚动条挤窄文案、触发下一帧再次调整。
    media.style.width = "0px";
    media.style.height = "0px";
    dialog.style.width = `${maxWidth}px`;
    const availableWidth = Math.max(1, maxWidth - frameWidth);
    const availableHeight = () => Math.max(1, maxHeight - frameHeight());
    if (!width || !height) {
      media.style.width = `${availableWidth}px`;
      media.style.height = `${availableHeight()}px`;
      return;
    }
    if (alignedControls) alignedControls.style.width = `${availableWidth}px`;
    let scale = Math.min(fillAvailable ? Infinity : 1, availableWidth / width, availableHeight() / height);
    if (!fillAvailable) {
      dialog.style.width = `${Math.min(maxWidth, Math.max(pixels(dialogStyle, "--preview-min-width"), width * scale + frameWidth))}px`;
      // 最终宽度下文案可能换行；只收缩媒体区域，窗口宽度不再随换行来回变化。
      scale = Math.min(scale, availableHeight() / height);
    }
    if (alignedControls) {
      // 确认稿要求控件与实际视频同宽；控件依次换行时先扣除新增行高，避免越过预览高度上限。
      for (let pass = 0; pass < alignedControls.childElementCount; pass++) {
        alignedControls.style.width = `${width * scale}px`;
        const nextScale = Math.min(scale, availableHeight() / height);
        if (nextScale === scale) break;
        scale = nextScale;
      }
      alignedControls.style.width = `${width * scale}px`;
    }
    media.style.width = `${width * scale}px`;
    media.style.height = `${height * scale}px`;
  };
  const observer = new ResizeObserver(resize);
  observer.observe(panel);
  // 时长文本变化可能让控件换行，即使面板已达最大高度，也要重新给控件预留空间。
  if (alignedControls) observer.observe(alignedControls);
  window.addEventListener("resize", resize);
  return {
    resize,
    dispose() { observer.disconnect(); window.removeEventListener("resize", resize); }
  };
}

export function mediaPreviewContainsPoint(media, viewport, clientX, clientY) {
  const box = media.getBoundingClientRect(), clip = viewport.getBoundingClientRect();
  let { left, top, width, height } = box;
  const naturalWidth = media.naturalWidth || media.videoWidth, naturalHeight = media.naturalHeight || media.videoHeight;
  if (naturalWidth > 0 && naturalHeight > 0 && getComputedStyle(media).objectFit === "contain") {
    const scale = Math.min(width / naturalWidth, height / naturalHeight);
    left += (width - naturalWidth * scale) / 2; top += (height - naturalHeight * scale) / 2;
    width = naturalWidth * scale; height = naturalHeight * scale;
  }
  // 用户要求放大后点击图片不能关闭；按变换后的实际内容与可见视口交集判断，不按透明容器判断。
  return width > 0 && height > 0 && clientX >= Math.max(left, clip.left)
    && clientX <= Math.min(left + width, clip.right) && clientY >= Math.max(top, clip.top)
    && clientY <= Math.min(top + height, clip.bottom);
}

export function bindMediaPreviewDismiss(dialog, media, viewport = media) {
  const pointers = new Set();
  let pressed = null;
  const background = event => !event.target.closest?.("button,input,select,textarea,a,.image-preview-caption,.video-preview-controls,.video-preview-status,.media-preview-loading-label")
    && !mediaPreviewContainsPoint(media, viewport, event.clientX, event.clientY);
  const down = event => {
    pointers.add(event.pointerId);
    pressed = pointers.size === 1 && event.isPrimary !== false && event.button === 0 && background(event)
      ? { id: event.pointerId, x: event.clientX, y: event.clientY, released: false } : null;
  };
  const move = event => {
    // PC 和手机共用 6 个 CSS 像素的单击容差；拖动、滚动及双指操作结束后不能误关弹窗。
    if (pressed?.id === event.pointerId && (Math.hypot(event.clientX - pressed.x, event.clientY - pressed.y) > 6
        || !background(event))) pressed = null;
  };
  const up = event => {
    pointers.delete(event.pointerId);
    if (pressed?.id !== event.pointerId) return;
    move(event);
    if (pressed) pressed.released = true;
  };
  const cancel = event => { pointers.delete(event.pointerId); pressed = null; };
  const click = event => {
    const dismiss = pressed?.released && !pointers.size && background(event);
    pressed = null;
    // 用户要求内容以外单击关闭；消费完整 click 后再关闭，避免手机的合成点击穿透到会话。
    if (dismiss) { event.preventDefault(); event.stopPropagation(); dialog.close(); }
  };
  const handlers = { pointerdown: down, pointermove: move, pointerup: up, pointercancel: cancel, click };
  for (const [name, handler] of Object.entries(handlers)) dialog.addEventListener(name, handler);
  return () => {
    for (const [name, handler] of Object.entries(handlers)) dialog.removeEventListener(name, handler);
    pointers.clear(); pressed = null;
  };
}

export function openVideoPreview(trigger, source, poster, name, prepareAccess = () => {}) {
  if (!remoteVideoPattern.test(source) || poster !== `${source}/poster`) return;
  if (requestWorkspaceMediaPreview({ kind: "video", source, poster, name })) return;
  prepareAccess();
  document.querySelector("#video-preview-dialog")?.close();
  const dialog = document.createElement("dialog");
  dialog.id = "video-preview-dialog";
  dialog.className = "image-preview-dialog video-preview-dialog";
  dialog.setAttribute("aria-label", `视频预览：${name}`);
  const panel = document.createElement("div");
  panel.className = "video-preview-panel";
  const close = document.createElement("button");
  close.type = "button";
  close.className = "image-preview-close";
  close.append(videoPreviewIcon("close"));
  close.setAttribute("aria-label", "关闭视频预览");
  close.onclick = () => dialog.close();
  const video = document.createElement("video");
  video.className = "video-preview-media";
  video.preload = "none";
  video.playsInline = true;
  video.crossOrigin = "anonymous";
  const initialPoster = remoteMediaCandidates(poster)[0];
  if (initialPoster) video.poster = initialPoster;
  // 弹窗打开时仍然没有 src。只有下方播放按钮的点击事件可以设置 src。
  const controls = document.createElement("div");
  controls.className = "video-preview-controls";
  const play = document.createElement("button");
  play.type = "button";
  play.className = "icon-button video-preview-play";
  play.append(videoPreviewIcon("play"));
  play.setAttribute("aria-label", "播放视频");
  play.title = "播放视频";
  const progress = document.createElement("input");
  progress.type = "range";
  progress.className = "video-preview-progress";
  progress.min = "0";
  progress.max = "1";
  progress.step = "0.1";
  progress.value = "0";
  progress.disabled = true;
  progress.setAttribute("aria-label", "视频播放进度");
  const time = document.createElement("span");
  time.className = "video-preview-time";
  time.textContent = "0:00 / 0:00";
  const status = document.createElement("p");
  status.className = "video-preview-status";
  status.setAttribute("role", "status");
  const title = document.createElement("p");
  title.className = "image-preview-caption";
  title.textContent = name;
  const cover = new Image();
  const thumbnail = trigger?.querySelector(".message-video-poster");
  let playbackDimensions = null;
  // 确认稿保留整页居中的预览壳，画面和控件单独等比适配。先用封面比例（未知时 16:9），不为测量提前加载视频。
  const layout = fitMediaPreview(dialog, panel, video, () => {
    if (playbackDimensions) return playbackDimensions;
    const image = cover.naturalWidth && cover.naturalHeight ? cover : thumbnail;
    return image?.naturalWidth && image.naturalHeight
      ? { width: image.naturalWidth, height: image.naturalHeight } : { width: 16, height: 9 };
  }, { fillAvailable: true, alignedControls: controls });
  loadVideoPoster(cover, poster, source => {
    video.poster = source;
    if (thumbnail?.hidden) thumbnail.src = source;
    if (!playbackDimensions) layout.resize();
  });
  let closed = false, seeking = false, failed = false, pendingSeek = null;
  let wantsPlayback = false, recoveryTimer = null, attempt = 0, playRequest = 0;
  let candidates = [], lastMediaTime = 0;
  const attempted = new Set();
  const clearRecovery = () => { clearTimeout(recoveryTimer); recoveryTimer = null; };
  const currentSource = () => {
    const source = video.getAttribute("src");
    return source && (!video.currentSrc || video.currentSrc === new URL(source, location.href).href);
  };
  const resizeToVideo = () => {
    if (closed || !dialog.open || !currentSource() || !video.videoWidth || !video.videoHeight) return;
    const { videoWidth: width, videoHeight: height } = video;
    if (width === playbackDimensions?.width && height === playbackDimensions?.height) return;
    // 播放器的实际尺寸优先于封面；暂停、切换线路或封面迟到时不退回占位比例。
    playbackDimensions = { width, height };
    layout.resize();
  };
  video.addEventListener("loadedmetadata", resizeToVideo);
  video.addEventListener("resize", resizeToVideo);
  const sync = () => {
    const duration = Number.isFinite(video.duration) && video.duration > 0 ? video.duration : 0;
    if (duration && video.readyState >= 1 && currentSource() && pendingSeek !== null) {
      const target = pendingSeek;
      pendingSeek = null;
      video.currentTime = Math.min(target, duration);
    }
    progress.max = String(duration || 1);
    progress.disabled = !duration || !video.getAttribute("src");
    if (!seeking) progress.value = String(video.currentTime || 0);
    progress.setAttribute("aria-valuetext", `${videoTime(video.currentTime)} / ${videoTime(duration)}`);
    time.textContent = `${videoTime(video.currentTime)} / ${videoTime(duration)}`;
    play.replaceChildren(videoPreviewIcon(wantsPlayback ? "pause" : "play"));
    const playLabel = wantsPlayback ? "暂停视频" : video.ended ? "重播视频" : failed ? "重试播放视频" : "播放视频";
    play.setAttribute("aria-label", playLabel);
    play.title = playLabel;
  };
  const fail = message => {
    clearRecovery();
    wantsPlayback = false;
    failed = true;
    ++playRequest;
    video.pause();
    status.textContent = message;
    sync();
  };
  const armRecovery = () => {
    if (recoveryTimer !== null || !wantsPlayback || closed || !dialog.open) return;
    const observed = attempt;
    recoveryTimer = setTimeout(() => {
      recoveryTimer = null;
      if (closed || !dialog.open || !wantsPlayback || observed !== attempt) return;
      if (video.error && video.error.code !== 2) {
        fail("视频无法播放，格式可能不受支持或文件已不可用");
        return;
      }
      recover();
    }, MEDIA_RECOVERY_TIMEOUT_MS);
  };
  const requestPlayback = () => {
    const observed = attempt, request = ++playRequest;
    armRecovery();
    void video.play().catch(error => {
      if (closed || !dialog.open || observed !== attempt || request !== playRequest || !wantsPlayback) return;
      if (error?.name === "AbortError") { fail("播放已取消，请重试"); return; }
      if (video.error?.code === 2) { recover(); return; }
      fail("无法播放视频，请重试或在电脑端查看");
    });
  };
  const selectSource = (source, target) => {
    clearRecovery();
    ++attempt;
    ++playRequest;
    attempted.add(source);
    pendingSeek = target;
    lastMediaTime = target;
    if (video.getAttribute("src") !== source || video.error) {
      video.src = source;
      video.load();
    }
    status.textContent = "正在加载视频…";
    sync();
    requestPlayback();
  };
  const recover = () => {
    if (closed || !dialog.open || !wantsPlayback) return;
    const source = candidates.find(value => !attempted.has(value));
    if (!source) { fail("播放线路暂不可用，请重试或在电脑端查看"); return; }
    selectSource(source, pendingSeek ?? video.currentTime ?? 0);
    status.textContent = "正在切换播放线路…";
  };
  play.onclick = () => {
    if (closed) return;
    if (wantsPlayback) {
      wantsPlayback = false;
      ++playRequest;
      clearRecovery();
      video.pause();
      status.textContent = "已暂停";
      sync();
      return;
    }
    prepareAccess();
    failed = false;
    candidates = [...new Set(remoteMediaCandidates(source))];
    attempted.clear();
    const selectedSource = candidates[0];
    if (!selectedSource) {
      fail("当前没有可用的播放线路，请稍后重试");
      return;
    }
    wantsPlayback = true;
    selectSource(selectedSource, video.ended ? 0 : pendingSeek ?? video.currentTime ?? 0);
  };
  progress.oninput = () => { seeking = true; time.textContent = `${videoTime(Number(progress.value))} / ${videoTime(video.duration)}`; };
  progress.onchange = () => {
    seeking = false;
    if (!progress.disabled && Number.isFinite(Number(progress.value))) {
      const target = Number(progress.value), selectedSource = remoteMediaCandidates(source)[0];
      if (selectedSource && video.getAttribute("src") !== selectedSource) {
        candidates = [...new Set(remoteMediaCandidates(source))];
        attempted.clear();
        pendingSeek = target;
        if (wantsPlayback) selectSource(selectedSource, target);
        else {
          ++attempt;
          ++playRequest;
          attempted.add(selectedSource);
          video.src = selectedSource;
          video.load();
        }
      } else if (selectedSource) video.currentTime = target;
      lastMediaTime = target;
      clearRecovery();
      armRecovery();
    }
    sync();
  };
  for (const event of ["loadedmetadata", "durationchange", "timeupdate", "pause", "ended"]) video.addEventListener(event, sync);
  video.addEventListener("timeupdate", () => {
    if (closed || !dialog.open || !wantsPlayback || !currentSource()) return;
    if (video.currentTime > lastMediaTime + 0.01) {
      lastMediaTime = video.currentTime;
      clearRecovery();
      armRecovery();
    }
  });
  video.addEventListener("ended", () => {
    if (closed || !dialog.open || !currentSource() || !video.ended) return;
    wantsPlayback = false; clearRecovery(); sync();
  });
  video.onplaying = () => {
    if (closed || !dialog.open || !wantsPlayback || !currentSource()) return;
    status.textContent = ""; sync();
  };
  video.onwaiting = video.onstalled = () => {
    if (closed || !dialog.open || !wantsPlayback || !currentSource()) return;
    status.textContent = "正在缓冲…"; armRecovery();
  };
  video.onerror = () => {
    if (closed || !dialog.open || !video.getAttribute("src") || !video.error || !currentSource()) return;
    if (video.error.code === 2) {
      if (wantsPlayback) recover();
      else fail("播放线路暂不可用，请重试或在电脑端查看");
    } else fail(video.error.code === 1 ? "播放已取消，请重试" : "视频无法播放，格式可能不受支持或文件已不可用");
  };
  const disposeDismiss = bindMediaPreviewDismiss(dialog, video);
  dialog.oncancel = event => { event.preventDefault(); dialog.close(); };
  const stop = () => {
    if (closed) return;
    closed = true;
    wantsPlayback = false;
    ++attempt;
    ++playRequest;
    clearRecovery();
    video.pause();
    video.removeAttribute("src");
    video.load();
  };
  const hide = () => {
    if (document.visibilityState !== "hidden") return;
    wantsPlayback = false;
    ++playRequest;
    clearRecovery();
    video.pause();
    status.textContent = "已暂停";
    sync();
  };
  const leave = () => dialog.close();
  window.addEventListener("pagehide", leave);
  document.addEventListener("visibilitychange", hide);
  dialog.onclose = () => {
    disposeDismiss();
    layout.dispose();
    cover.posterRecovery?.dispose();
    cover.removeAttribute("src");
    stop();
    window.removeEventListener("pagehide", leave);
    document.removeEventListener("visibilitychange", hide);
    dialog.remove();
    document.body.classList.toggle("modal-open", !!document.querySelector("dialog[open]"));
    if (trigger?.isConnected) trigger.focus({ preventScroll: true });
  };
  controls.append(play, progress, time);
  panel.append(close, video, controls, status, title);
  dialog.append(panel);
  document.body.append(dialog);
  document.body.classList.add("modal-open");
  dialog.showModal();
  layout.resize();
  play.focus();
  return { close: () => { if (dialog.open) dialog.close(); } };
}

export function bindMessageVideos(root, prepareAccess = () => {}) {
  for (const button of root.querySelectorAll(".message-video-preview")) {
    const poster = button.querySelector(".message-video-poster");
    const posterUrl = poster?.getAttribute("data-remote-media") || poster?.getAttribute("src") || "";
    const source = posterUrl.replace(/\/poster$/u, "");
    if (!remoteVideoPattern.test(source) || posterUrl !== `${source}/poster`) continue;
    const name = button.querySelector(".message-video-name")?.textContent || "视频";
    button.setAttribute("aria-label", `预览视频：${name}`);
    button.setAttribute("aria-haspopup", "dialog");
    loadVideoPoster(poster, posterUrl);
    button.onclick = event => {
      event.preventDefault(); event.stopPropagation();
      openVideoPreview(button, source, posterUrl, name, prepareAccess);
    };
  }
}

export function retryOriginalImage(image) {
  const source = image.originalImageSource;
  if (!source) return false;
  const recovery = image.attachmentRecovery;
  if (!recovery || recovery.source !== source) return false;
  const selectedSource = recovery.candidates.find(value => !recovery.attempted.has(value));
  if (!selectedSource) return false;
  recovery.attempted.add(selectedSource);
  image.src = selectedSource;
  return true;
}

export function loadAttachmentThumbnail(image, source, mime = "") {
  image.originalImageSource = source;
  const thumbnail = remoteImagePattern.test(source) && mime !== "image/gif" ? `${source}/thumbnail` : source;
  if (remoteImagePattern.test(source)) {
    // 第二线路仍由服务端校验 Origin 和媒体凭证；无 CORS 的图片会被 CORP 拦截。
    image.crossOrigin = "anonymous";
    image.setAttribute("data-remote-media", source);
  }
  const candidates = remoteImagePattern.test(source)
    ? [...remoteMediaCandidates(thumbnail), ...remoteMediaCandidates(source)]
    : [remoteMediaUrl(source)];
  image.attachmentRecovery = { source, candidates: [...new Set(candidates)], attempted: new Set() };
  // 每条缩略图线路只尝试一次，再回退原图；显式重试时重新创建尝试列表。
  image.onerror = () => { retryOriginalImage(image); };
  if (!retryOriginalImage(image)) {
    const recovery = image.attachmentRecovery;
    queueMicrotask(() => {
      if (image.attachmentRecovery === recovery) image.dispatchEvent(new Event("error"));
    });
  }
}

let attachments = [];
let attachmentRoot = null;
let attachmentInput = null;
let composerPrompt = null;
let changeListener = () => {};
let showNotice = () => {};
let previewImage = () => {};
let disabled = false;
let locked = false;
let remoteRemovalAllowed = false;
let readingFiles = false;
let attachmentVersion = 0;
let localPasteShortcut = 0;

function attachmentName(value, fallback = "图片") {
  const name = String(value || "").replace(/[\u0000-\u001f\u007f]/gu, "").trim();
  return name ? name.slice(0, 255) : fallback;
}

function copyAttachment(entry) {
  if (isLocalInput(entry)) return copyLocalInput(entry);
  return entry.kind === "inline"
    ? {
        kind: "inline",
        name: entry.name,
        mime: entry.mime,
        dataUrl: entry.dataUrl,
        size: entry.size,
        width: entry.width,
        height: entry.height
      }
    : {
        kind: "remote",
        name: entry.name,
        source: entry.source,
        url: entry.url,
        mime: entry.mime || ""
      };
}

function safeRemoteImages(value) {
  if (!Array.isArray(value)) return [];
  const seen = new Set();
  return value.flatMap(image => {
    if (!image || typeof image.source !== "string" || !remoteImagePattern.test(image.url)
        || seen.has(image.url)) return [];
    seen.add(image.url);
    return [{
      kind: "remote",
      source: image.source,
      url: image.url,
      mime: typeof image.mime === "string" ? image.mime : "",
      name: attachmentName(image.name || image.source.split(/[\\/]/u).at(-1), "图片附件")
    }];
  });
}

function detectImageMime(bytes) {
  if (bytes.length >= 8 && bytes[0] === 0x89 && bytes[1] === 0x50 && bytes[2] === 0x4e
      && bytes[3] === 0x47 && bytes[4] === 0x0d && bytes[5] === 0x0a
      && bytes[6] === 0x1a && bytes[7] === 0x0a) return "image/png";
  if (bytes.length >= 3 && bytes[0] === 0xff && bytes[1] === 0xd8 && bytes[2] === 0xff)
    return "image/jpeg";
  const text = length => String.fromCharCode(...bytes.slice(0, length));
  if (bytes.length >= 6 && ["GIF87a", "GIF89a"].includes(text(6))) return "image/gif";
  if (bytes.length >= 12 && text(4) === "RIFF"
      && String.fromCharCode(...bytes.slice(8, 12)) === "WEBP") return "image/webp";
  if (bytes.length >= 14 && text(2) === "BM") return "image/bmp";
  if (bytes.length >= 6 && bytes[0] === 0 && bytes[1] === 0 && [1, 2].includes(bytes[2])
      && bytes[3] === 0 && (bytes[4] || bytes[5])) return "image/x-icon";
  if (bytes.length >= 16 && String.fromCharCode(...bytes.slice(4, 8)) === "ftyp") {
    const size = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(0);
    const brands = [String.fromCharCode(...bytes.slice(8, 12))];
    for (let offset = 16; offset + 4 <= Math.min(size || bytes.length, bytes.length, 256); offset += 4)
      brands.push(String.fromCharCode(...bytes.slice(offset, offset + 4)));
    if (brands.some(brand => brand === "avif" || brand === "avis")) return "image/avif";
  }
  return "";
}

function safeInlineSvgSource(source, maxBytes = 4 * 1024 * 1024) {
  if (typeof source !== "string" || source.length > Math.ceil(maxBytes / 3) * 4 + 64) return null;
  const header = /^data:image\/svg\+xml;base64,/iu.exec(source);
  if (!header) return null;
  const encoded = source.slice(header[0].length);
  if (!encoded.length || encoded.length % 4 || !/^[A-Za-z0-9+/]+={0,2}$/u.test(encoded)) return null;
  const padding = encoded.endsWith("==") ? 2 : encoded.endsWith("=") ? 1 : 0;
  if (encoded.length / 4 * 3 - padding > maxBytes) return null;
  try {
    const bytes = Uint8Array.from(atob(encoded), character => character.charCodeAt(0));
    const text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    const lower = text.toLowerCase();
    if (!/<svg(?:\s|>)/u.test(lower)
        || /<!doctype|<!entity|<script(?:\s|>)/u.test(lower)
        || /<(?:foreignobject|iframe|object|embed|animate|animatemotion|animatetransform|set)(?:\s|>)/u.test(lower)
        || /(?:javascript|vbscript):/u.test(lower)
        || /\bon[a-z][\w:-]*\s*=/u.test(lower)
        || /\b(?:href|xlink:href|src)\s*=\s*["']?\s*(?:https?:|\/\/|data:(?!image\/(?:png|jpeg|gif|webp);base64,))/u.test(lower)
        || /\burl\(\s*(?:https?:|\/\/|data:)/u.test(lower)
        || /@import\b/u.test(lower)) return null;
    return source;
  } catch {
    return null;
  }
}

export function safeInlineImageSource(source, { allowSvg = false } = {}) {
  if (typeof source !== "string" || source.length > Math.ceil(25 * 1024 * 1024 / 3) * 4 + 64) return null;
  if (allowSvg) {
    const svg = safeInlineSvgSource(source);
    if (svg) return svg;
  }
  const header = /^data:(image\/(?:png|jpeg|webp|gif));base64,/iu.exec(source);
  if (!header) return null;
  const encoded = source.slice(header[0].length);
  if (!encoded.length || encoded.length % 4 || !/^[A-Za-z0-9+/]+={0,2}$/u.test(encoded)) return null;
  const padding = encoded.endsWith("==") ? 2 : encoded.endsWith("=") ? 1 : 0;
  if (encoded.length / 4 * 3 - padding > 25 * 1024 * 1024) return null;
  try {
    const bytes = Uint8Array.from(atob(encoded.slice(0, 16)), character => character.charCodeAt(0));
    return detectImageMime(bytes) === header[1].toLowerCase() ? source : null;
  } catch {
    return null;
  }
}

function dataUrl(blob, mime) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(new Error("无法读取图片"));
    reader.onload = () => {
      const encoded = String(reader.result || "").split(",", 2)[1];
      if (!encoded) reject(new Error("无法读取图片"));
      else resolve(`data:${mime};base64,${encoded}`);
    };
    reader.readAsDataURL(blob);
  });
}

async function decodeInputImage(blob) {
  if (typeof createImageBitmap === "function") {
    try {
      const bitmap = await createImageBitmap(blob);
      return { image: bitmap, width: bitmap.width, height: bitmap.height, close: () => bitmap.close() };
    } catch {
      // SVG 等格式在部分客户端不支持 createImageBitmap，继续通过图片元素解码；不插入 SVG/HTML 原文。
    }
  }
  if (typeof Image !== "function") throw new Error("当前客户端无法解码此图片格式，请转换为 PNG 或 JPEG");
  return await new Promise((resolve, reject) => {
    const image = new Image();
    image.onload = () => {
      resolve({ image, width: image.naturalWidth, height: image.naturalHeight, close: () => { image.src = ""; } });
    };
    image.onerror = () => reject(new Error("图片内容损坏，或当前客户端无法解码此格式，请转换为 PNG 或 JPEG"));
    void dataUrl(blob, blob.type || "image/png").then(source => { image.src = source; }, reject);
  });
}

function checkImageDimensions({ width, height }) {
  if (!width || !height || width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION
      || width * height > MAX_IMAGE_PIXELS)
    throw new Error("图片尺寸过大，最大边不能超过 16384 像素");
}

async function imageDimensions(blob) {
  const decoded = await decodeInputImage(blob);
  try { checkImageDimensions(decoded); return { width: decoded.width, height: decoded.height }; }
  finally { decoded.close(); }
}

async function convertInputImage(blob) {
  const decoded = await decodeInputImage(blob);
  let canvas;
  try {
    checkImageDimensions(decoded);
    canvas = document.createElement("canvas");
    canvas.width = decoded.width; canvas.height = decoded.height;
    const context = canvas.getContext("2d");
    if (!context) throw new Error("当前客户端无法转换图片，请转换为 PNG 或 JPEG 后添加");
    context.drawImage(decoded.image, 0, 0);
    const output = await new Promise((resolve, reject) => canvas.toBlob(value =>
      value ? resolve(value) : reject(new Error("图片转换失败，请转换为 PNG 或 JPEG 后添加")), "image/png"));
    if (!output.size || output.size > MAX_IMAGE_BYTES) throw new Error("转换后的图片不能超过 5 MiB");
    const header = new Uint8Array(await output.slice(0, 16).arrayBuffer());
    if (detectImageMime(header) !== "image/png") throw new Error("图片转换未生成有效 PNG，未添加附件");
    return { blob: output, width: decoded.width, height: decoded.height };
  } finally {
    decoded.close();
    if (canvas) { canvas.width = 0; canvas.height = 0; }
  }
}

async function readImageFile(file) {
  if (!(file instanceof Blob)) throw new Error("请选择图片文件");
  if (file.size <= 0) throw new Error("图片文件为空");
  if (file.size > MAX_IMAGE_BYTES) throw new Error("单张图片不能超过 5 MiB");
  const bytes = new Uint8Array(await file.arrayBuffer());
  const declared = canonicalImageMime(file.type);
  let mime = detectImageMime(bytes);
  if (!mime && (declared === "image/svg+xml" || /\.svg$/iu.test(file.name || ""))) mime = "image/svg+xml";
  if (!supportedMimes.has(mime) && !convertibleMimes.has(mime))
    throw new Error("支持 PNG、JPEG、GIF、WebP、BMP、AVIF、SVG、ICO/CUR，请将其他格式转换后添加");
  if ((supportedMimes.has(declared) || convertibleMimes.has(declared)) && declared !== mime)
    throw new Error("图片扩展类型与实际内容不一致");
  let blob = new Blob([bytes], { type: mime }), dimensions;
  if (mime === "image/svg+xml") {
    const source = await dataUrl(blob, mime);
    if (!safeInlineSvgSource(source, MAX_IMAGE_BYTES)) throw new Error("SVG 含不支持的脚本、动态内容或外部资源");
    const text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    for (const match of text.matchAll(/\b(?:href|xlink:href|src)\s*=\s*(?:"([^"]*)"|'([^']*)')/giu)) {
      const target = (match[1] ?? match[2]).trim();
      if (!target.startsWith("#") && !safeInlineImageSource(target))
        throw new Error("SVG 不能读取外部文件或网络资源");
    }
    for (const match of text.matchAll(/\burl\(\s*([^)]*)\)/giu))
      if (!match[1].trim().replace(/^["']|["']$/gu, "").startsWith("#"))
        throw new Error("SVG 不能读取外部文件或网络资源");
  }
  if (convertibleMimes.has(mime)) {
    const converted = await convertInputImage(blob);
    blob = converted.blob; mime = "image/png";
    dimensions = { width: converted.width, height: converted.height };
  } else dimensions = await imageDimensions(blob);
  return {
    kind: "inline",
    name: attachmentName(file.name, `图片.${mime.split("/")[1]}`),
    mime,
    dataUrl: await dataUrl(blob, mime),
    size: blob.size,
    ...dimensions
  };
}

function renderComposerAttachments() {
  if (!attachmentRoot) return;
  attachmentRoot.replaceChildren();
  for (const [index, entry] of attachments.entries()) {
    const item = document.createElement("figure");
    item.className = "composer-attachment";
    if (isLocalInput(entry)) {
      // 用户要求文件/文件夹与现有图片同为 40×40；中间分别放两种图标，名称显示在下方。
      item.classList.add("composer-local-attachment");
      const preview = document.createElement("span");
      preview.className = "composer-attachment-preview composer-local-attachment-icon";
      preview.title = `${entry.kind === "folder" ? "文件夹" : "文件"}：${entry.name}`;
      preview.setAttribute("aria-label", preview.title);
      preview.append(icon(entry.kind === "folder" ? "folderClosed" : "document"));
      const caption = document.createElement("figcaption");
      caption.textContent = entry.name; caption.title = entry.name;
      item.append(preview, caption);
    } else {
      const image = document.createElement("img");
      image.className = "composer-attachment-image";
      loadAttachmentThumbnail(image, entry.kind === "inline" ? entry.dataUrl : entry.url, entry.mime);
      image.alt = entry.name;
      image.title = entry.name;
      image.decoding = "async";
      const preview = document.createElement("button");
      preview.type = "button";
      preview.className = "composer-attachment-preview";
      preview.setAttribute("aria-label", `查看图片 ${entry.name}`);
      preview.setAttribute("aria-haspopup", "dialog");
      preview.title = `查看图片 ${entry.name}`;
      preview.onclick = () => previewImage(preview, image);
      preview.append(image);
      const caption = document.createElement("figcaption");
      caption.textContent = entry.name;
      item.append(preview, caption);
    }
    const canRemove = !locked || remoteRemovalAllowed && entry.kind === "remote";
    if (canRemove) {
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "composer-attachment-remove";
      const label = isLocalInput(entry) ? entry.kind === "folder" ? "文件夹" : "文件" : "图片";
      remove.setAttribute("aria-label", `移除${label} ${entry.name}`);
      remove.title = `移除${label}`;
      remove.innerHTML = '<svg aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="m6 6 12 12M18 6 6 18"/></svg>';
      remove.disabled = disabled || readingFiles;
      remove.onclick = () => {
        if (disabled || readingFiles || locked && !(remoteRemovalAllowed && entry.kind === "remote")) return;
        attachments.splice(index, 1);
        renderComposerAttachments();
        changeListener();
      };
      item.append(remove);
    }
    attachmentRoot.append(item);
  }
  attachmentRoot.hidden = attachments.length === 0;
  attachmentRoot.classList.toggle("attachments-locked", locked);
  if (attachmentInput) attachmentInput.disabled = disabled || locked || readingFiles;
}

async function addFiles(files) {
  const candidates = [...files];
  if (!candidates.length) return false;
  if (disabled || locked || readingFiles) {
    showNotice(locked
      ? remoteRemovalAllowed ? "编辑排队消息时只能删除原图片，不能添加图片" : "编辑时保留原附件，不能增删图片"
      : "当前不能添加图片", true);
    return true;
  }
  if (attachments.filter(entry => !isLocalInput(entry)).length + candidates.length > MAX_IMAGE_COUNT) {
    showNotice(`每条消息最多添加 ${MAX_IMAGE_COUNT} 张图片`, true);
    return true;
  }
  const currentBytes = attachments.reduce((total, entry) =>
    total + (entry.kind === "inline" ? entry.size : 0), 0);
  if (currentBytes + candidates.reduce((total, file) => total + file.size, 0) > MAX_TOTAL_BYTES) {
    showNotice("图片总大小不能超过 12 MiB", true);
    return true;
  }
  readingFiles = true;
  renderComposerAttachments();
  changeListener();
  const version = attachmentVersion;
  try {
    const next = [], errors = [];
    for (const file of candidates) {
      try { next.push(await readImageFile(file)); }
      catch (error) { errors.push(`${attachmentName(file?.name)}：${error?.message || "无法添加图片"}`); }
      if (version !== attachmentVersion) return true;
    }
    validateAttachmentBatch([], next);
    attachments.push(...next);
    renderComposerAttachments();
    changeListener();
    if (errors.length) showNotice(`已添加 ${next.length} 张图片；${errors.join("；")}`, true);
  } catch (error) {
    showNotice(error?.message || "无法添加图片", true);
  } finally {
    if (version === attachmentVersion) {
      readingFiles = false;
      if (attachmentInput) attachmentInput.value = "";
      renderComposerAttachments();
      changeListener();
    }
  }
  return true;
}

export function mountComposerAttachments({ form, prompt, onChange, notice, onPreview } = {}) {
  if (!(form instanceof HTMLElement) || !(prompt instanceof HTMLElement))
    throw new Error("图片附件组件缺少编辑框");
  composerPrompt = prompt;
  changeListener = typeof onChange === "function" ? onChange : () => {};
  showNotice = typeof notice === "function" ? notice : () => {};
  previewImage = typeof onPreview === "function" ? onPreview : () => {};
  attachmentRoot = document.createElement("div");
  attachmentRoot.id = "composer-attachments";
  attachmentRoot.className = "composer-attachments";
  attachmentRoot.setAttribute("aria-label", "待发送附件");
  attachmentRoot.hidden = true;
  prompt.before(attachmentRoot);

  attachmentInput = document.createElement("input");
  attachmentInput.id = "composer-image-input";
  attachmentInput.type = "file";
  // Android 原生有按 image/* 单一类型识别的有序图片选择器；保留兼容，桌面额外列出常被漏判的扩展名。
  attachmentInput.accept = (globalThis.window?.ElvesAndroid || globalThis.window?.CodexElvesAndroid) ? "image/*"
    : "image/*,.apng,.jfif,.jpe,.avif,.bmp,.svg,.ico,.cur";
  attachmentInput.multiple = true;
  attachmentInput.hidden = true;
  attachmentInput.setAttribute("aria-hidden", "true");
  attachmentInput.onchange = event => void addFiles([...(event.target.files || [])]);
  form.append(attachmentInput);

  form.addEventListener("dragover", event => {
    if (!hasTransferFiles(event.dataTransfer)) return;
    event.preventDefault();
    if (!disabled && !locked && !readingFiles) form.classList.add("composer-image-dragover");
  });
  form.addEventListener("dragleave", event => {
    if (!form.contains(event.relatedTarget)) form.classList.remove("composer-image-dragover");
  });
  form.addEventListener("drop", event => {
    form.classList.remove("composer-image-dragover");
    if (!hasTransferFiles(event.dataTransfer)) return;
    event.preventDefault();
    // 用户要求拖入图片、文件及完整目录；在回调内取得入口，不能等异步操作后再读取受保护的 DataTransfer。
    try { void addTransferredInputs(captureTransferInputs(event.dataTransfer)); }
    catch (error) { showNotice(error?.message || "无法读取拖入的附件", true); }
  });
  renderComposerAttachments();
}

export function handleComposerImagePaste(event) {
  const files = [...event?.clipboardData?.files || []];
  if (!files.length || !files.every(imageInputFile)) return false;
  event.preventDefault();
  void addFiles(files);
  return true;
}

export function composerLocalInputsSupported() { return !!localInputBridge(); }
export function composerCanAddLocalInputs() {
  return composerLocalInputsSupported() && !disabled && !locked && !readingFiles;
}

async function addLocalInputs(read, fallback = null, classifyImages = false) {
  if (!composerCanAddLocalInputs()) { showNotice("当前不能添加文件和文件夹", true); return; }
  const version = attachmentVersion;
  readingFiles = true; renderComposerAttachments(); changeListener();
  try {
    const next = await read();
    if (version !== attachmentVersion) return;
    if (next.length) {
      const checked = validateLocalInputs(next), images = [], inputs = [];
      for (const entry of checked) {
        if (classifyImages && entry.kind === "file" && imageFileExtensions.test(entry.name)) {
          const bytes = Uint8Array.from(atob(entry.files[0].data), character => character.charCodeAt(0));
          images.push(Object.assign(new Blob([bytes]), { name: entry.name }));
        } else inputs.push(entry);
      }
      validateAttachmentBatch(inputs, images.map(file => ({ kind: "inline", size: file.size })));
      const converted = [];
      for (const file of images) {
        converted.push(await readImageFile(file));
        if (version !== attachmentVersion) return;
        validateAttachmentBatch(inputs, converted);
      }
      validateAttachmentBatch(inputs, converted);
      attachments.push(...inputs, ...converted);
    } else if (fallback) {
      // 截图/浏览器图片没有 CF_HDROP，继续使用现有图片粘贴；纯文字不转成路径或附件。
      readingFiles = false;
      await fallback();
    }
  } catch (error) {
    if (version === attachmentVersion) showNotice(error?.message || String(error) || "无法添加附件", true);
  } finally {
    if (version === attachmentVersion) {
      readingFiles = false; renderComposerAttachments(); changeListener();
      composerPrompt?.focus({ preventScroll: true });
    }
  }
}

function validateAttachmentBatch(inputs, images) {
  validateLocalInputs([...attachments.filter(isLocalInput), ...inputs]);
  if (attachments.filter(entry => !isLocalInput(entry)).length + images.length > MAX_IMAGE_COUNT)
    throw new Error(`每条消息最多添加 ${MAX_IMAGE_COUNT} 张图片`);
  const total = [...attachments, ...images].reduce((sum, entry) => sum + (entry.kind === "inline" ? entry.size : 0), 0);
  if (total > MAX_TOTAL_BYTES) throw new Error("图片总大小不能超过 12 MiB");
}

async function addTransferredInputs(captured) {
  if (disabled || locked || readingFiles) { showNotice("当前不能添加附件，请先完成读取或退出编辑", true); return; }
  const version = attachmentVersion;
  readingFiles = true; renderComposerAttachments(); changeListener();
  try {
    const batch = await readTransferInputs(captured, { isImageFile: imageInputFile,
      isCurrent: () => version === attachmentVersion });
    if (version !== attachmentVersion) return;
    validateAttachmentBatch(batch.inputs, batch.images.map(file => ({ kind: "inline", size: file.size })));
    const images = [];
    for (const file of batch.images) {
      images.push(await readImageFile(file));
      if (version !== attachmentVersion) return;
      validateAttachmentBatch(batch.inputs, images);
    }
    validateAttachmentBatch(batch.inputs, images);
    attachments.push(...batch.inputs, ...images);
  } catch (error) {
    if (version === attachmentVersion)
      showNotice(error?.message || "无法读取附件，请重新拖入或粘贴", true);
  } finally {
    if (version === attachmentVersion) {
      readingFiles = false; renderComposerAttachments(); changeListener();
      composerPrompt?.focus({ preventScroll: true });
    }
  }
}

export function chooseComposerLocalInputs() {
  const bridge = localInputBridge();
  if (!bridge || !composerCanAddLocalInputs()) return;
  if (typeof bridge.pickAny === "function") return addLocalInputs(() => bridge.pickAny());
  // 网页资源可能比客户端更新；旧版仍一键选文件，说明文件夹的替代入口，不恢复二级菜单。
  const version = attachmentVersion;
  return addLocalInputs(async () => {
    const inputs = await bridge.pick(false);
    if (version === attachmentVersion)
      showNotice("此客户端版本只能在窗口中选文件。可拖放或粘贴文件夹，更新 Elves 后可在同一窗口选择。");
    return inputs;
  });
}

export function handleComposerLocalPaste(event) {
  ++localPasteShortcut;
  const files = [...event.clipboardData?.files || []];
  const text = event.clipboardData?.getData("text/plain") || "";
  const fileTransfer = hasTransferFiles(event.clipboardData);
  let captured = [];
  if (fileTransfer) {
    try { captured = captureTransferInputs(event.clipboardData); }
    catch (error) {
      event.preventDefault(); showNotice(error?.message || "无法读取粘贴的附件", true); return true;
    }
  }
  // 文字及原有图片粘贴保持同步；只有文件型剪贴板（含目录）或无文本的原生文件剪贴板走客户端读取。
  const onlyImages = files.length && files.every(imageInputFile) && captured.every(candidate =>
    !candidate.entry?.isDirectory && !candidate.handle && candidate.file && imageInputFile(candidate.file));
  if (text && !fileTransfer || onlyImages) return false;
  if (!composerLocalInputsSupported()) {
    if (!hasTransferFiles(event.clipboardData)) return false;
    event.preventDefault();
    void addTransferredInputs(captured);
    return true;
  }
  event.preventDefault();
  void addLocalInputs(() => localInputBridge().paste(),
    captured.length ? () => addTransferredInputs(captured) : null, true);
  return true;
}

export function handleComposerLocalPasteShortcut(event) {
  if (!composerLocalInputsSupported() || !event.ctrlKey || event.altKey || event.shiftKey
      || event.key.toLowerCase() !== "v" || event.repeat) return;
  const version = attachmentVersion, shortcut = ++localPasteShortcut;
  // Windows 只复制目录时，WebView2 可能不产生 DOM paste；等原生默认粘贴完成再补读 CF_HDROP。
  // 文字、截图或文件已经产生 paste 时由上方处理，不能重复添加，也不截断浏览器的原有快捷键。
  setTimeout(() => {
    if (version === attachmentVersion && shortcut === localPasteShortcut)
      void addLocalInputs(() => localInputBridge().paste(), null, true);
  }, 0);
}

export function composerLocalInputsPayload() { return attachments.filter(isLocalInput).map(copyLocalInput); }

export function composerHasAttachments() {
  return attachments.length > 0;
}

export function composerAttachmentsLoading() {
  return readingFiles;
}

export function composerCanAddImages() {
  return !disabled && !locked && !readingFiles;
}

export function chooseComposerImages() {
  if (composerCanAddImages()) attachmentInput?.click();
}

export function composerAttachmentSnapshot() {
  return attachments.map(copyAttachment);
}

export function composerAttachmentsPayload() {
  return attachments.flatMap(entry => entry.kind === "inline"
    ? [{ name: entry.name, mime: entry.mime, dataUrl: entry.dataUrl, size: entry.size }]
    : []);
}

export function composerRemoteAttachmentSources() {
  return attachments.flatMap(entry =>
    entry.kind === "remote" && typeof entry.source === "string" ? [entry.source] : []);
}

export function setComposerAttachments(value = [], { readOnly = false, removableRemote = false } = {}) {
  attachmentVersion += 1;
  readingFiles = false;
  let imageCount = 0;
  attachments = Array.isArray(value) ? value.flatMap(entry => {
    if (isLocalInput(entry)) {
      try { return validateLocalInputs([entry]); } catch { return []; }
    }
    if (entry?.kind === "inline" && supportedMimes.has(entry.mime)
        && typeof entry.dataUrl === "string" && Number.isFinite(entry.size) && imageCount++ < MAX_IMAGE_COUNT)
      return [copyAttachment(entry)];
    if (entry?.kind === "remote" && (remoteImagePattern.test(entry.url) || safeInlineImageSource(entry.url))
        && imageCount++ < MAX_IMAGE_COUNT)
      return [copyAttachment(entry)];
    return [];
  }) : [];
  locked = readOnly;
  remoteRemovalAllowed = readOnly && removableRemote;
  renderComposerAttachments();
}

export function setComposerAttachmentsDisabled(value) {
  disabled = value === true;
  renderComposerAttachments();
}

export function clearComposerAttachments() {
  setComposerAttachments([]);
}

export function attachmentsFromRemoteImages(value) {
  return safeRemoteImages(value);
}

export async function attachmentsFromQueuedMessage(message) {
  const registeredImages = safeRemoteImages(message?.remoteImages);
  const images = message?.context?.imageAttachments;
  if (!Array.isArray(images)) return registeredImages;
  const attachments = [];
  for (const image of images) {
    const registered = registeredImages.find(entry => entry.source === image?.src);
    if (registered) {
      attachments.push({ ...registered, name: attachmentName(image.filename || image.name, registered.name) });
      continue;
    }
    const source = safeInlineImageSource(image?.src);
    if (!source) continue;
    const binary = atob(source.slice(source.indexOf(",") + 1));
    const bytes = Uint8Array.from(binary, character => character.charCodeAt(0));
    const reference = `inline:${await sha256Hex(bytes)}`;
    attachments.push(registeredImages.find(entry => entry.source === reference) || {
      kind: "remote",
      name: attachmentName(image.filename || image.name, "图片附件"),
      source: reference,
      url: source,
      mime: source.slice(5, source.indexOf(";")).toLowerCase()
    });
  }
  return attachments;
}

export function renderQueuedMessageAttachments(message) {
  const registeredImages = safeRemoteImages(message?.remoteImages);
  const images = [];
  const inlineAttachments = message?.context?.imageAttachments;
  for (const attachment of Array.isArray(inlineAttachments) ? inlineAttachments : []) {
    if (images.some(image => image.source === attachment?.src)) continue;
    const registered = registeredImages.find(image => image.source === attachment?.src);
    if (registered) { images.push(registered); continue; }
    const url = safeInlineImageSource(attachment?.src);
    if (url) images.push({ source: attachment.src, url, name: attachmentName(attachment.name, "图片附件") });
  }
  for (const image of registeredImages)
    if (!images.some(entry => entry.url === image.url)) images.push(image);
  if (!images.length) return null;
  const root = document.createElement("div");
  root.className = "queued-message-attachments";
  root.setAttribute("aria-label", `${images.length} 张图片附件，仅显示第一张`);
  const image = images[0];
  const thumbnail = document.createElement("img");
  thumbnail.className = "queued-message-attachment";
  loadAttachmentThumbnail(thumbnail, image.url, image.mime);
  thumbnail.alt = image.name;
  thumbnail.title = image.name;
  thumbnail.loading = "lazy";
  thumbnail.decoding = "async";
  thumbnail.draggable = false;
  root.append(thumbnail);
  return root;
}
