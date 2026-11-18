// 定时任务页面：任务入口只在 Elves 本机主导航显示，实例选择完成后才加载模型目录。
// 任务实际执行由 Harness 服务端按保存的 instanceId/model 派发，不借用当前会话的模型选择。

import { button, element } from "../ui/dom.js";
import { icon } from "../ui/icons.js";
import { confirmAction } from "../ui/dialog.js";
import { notice } from "../ui/notice.js";
import { selectControl, setSelectOptions, setSelectValue } from "../ui/select.js";
import { api } from "./api.js";
import {
  availableSessionInstances, sessionInstanceEnabled
} from "./session/instances.js";
import { loadSessionInstances } from "./session/instance-catalog.js";

const effortOptions = [
  { value: "none", label: "无" },
  { value: "minimal", label: "Minimal" },
  { value: "low", label: "Low" },
  { value: "medium", label: "Medium" },
  { value: "high", label: "High" },
  { value: "xhigh", label: "Xhigh" },
  { value: "max", label: "Max" },
  { value: "ultra", label: "Ultra" }
];

function requestId() {
  return globalThis.crypto?.randomUUID?.() || `scheduled-${Date.now()}-${Math.random().toString(36).slice(2)}`;
}

async function action(path, payload) {
  const receipt = await api(path, payload, requestId());
  if (receipt?.state && receipt.state !== "completed") {
    throw new Error(receipt.result?.message || "定时任务操作未完成，请重试");
  }
  const result = receipt?.state ? receipt.result : receipt;
  if (result?.ok !== true) throw new Error(result?.message || "定时任务操作失败");
  return result;
}

function formatDateTime(value) {
  if (!Number.isFinite(value)) return "未安排";
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(new Date(value));
}

function toLocalInput(value) {
  const date = new Date(Number.isFinite(value) ? value : Date.now() + 5 * 60 * 1000);
  const pad = number => String(number).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function fromLocalInput(value) {
  const timestamp = Date.parse(value);
  return Number.isFinite(timestamp) ? timestamp : 0;
}

function field(label, control, className = "") {
  const wrapper = element("label", `scheduled-field ${className}`.trim());
  wrapper.append(element("span", "scheduled-field-label", label), control);
  return wrapper;
}

function instanceLabel(id) {
  return availableSessionInstances().find(instance => instance.id === id)?.label || id || "未知实例";
}

function scheduleLabel(task) {
  const schedule = task.schedule || {};
  if (schedule.kind === "once") return `一次 · ${formatDateTime(schedule.startAtMs)}`;
  if (schedule.kind === "interval") {
    const minutes = Number(schedule.intervalMinutes) || 0;
    const cadence = minutes % 10080 === 0 ? "每周" : minutes % 1440 === 0 ? "每天"
      : minutes % 60 === 0 ? `每 ${minutes / 60} 小时` : `每 ${minutes} 分钟`;
    return `${cadence} · 下次 ${formatDateTime(task.nextRunAtMs)}`;
  }
  return `Cron ${schedule.cron || ""} · 下次 ${formatDateTime(task.nextRunAtMs)}`;
}

function statusLabel(task) {
  if (task.status === "completed") return "已完成";
  if (task.status === "paused" || task.enabled === false) return "已暂停";
  if (task.status === "running") return "执行中";
  if (task.status === "error") return "上次失败";
  return "已启用";
}

export function mountScheduledTasksPage(root) {
  let disposed = false;
  let tasks = [];
  let editingId = "";
  let instancesReady = false;
  let modelRows = [];

  root.className = "scheduled-workspace";
  root.replaceChildren();

  const heading = element("header", "scheduled-heading");
  const title = element("div", "scheduled-title");
  title.append(element("h1", "", "定时任务"));
  const refresh = button("", "刷新定时任务", () => void refreshTasks(), "icon-button");
  refresh.append(icon("retry"));
  title.append(refresh);
  const add = button("新建任务", "新建定时任务", () => openEditor(), "scheduled-primary-button");
  add.prepend(icon("plus"));
  heading.append(title, add);

  const list = element("section", "scheduled-list");
  list.setAttribute("aria-live", "polite");

  const editor = element("section", "scheduled-editor");
  editor.hidden = true;
  const editorHeading = element("div", "scheduled-editor-heading");
  const editorTitle = element("h2", "", "新建任务");
  const closeEditor = button("", "关闭编辑", () => closeEditorPanel(), "icon-button");
  closeEditor.append(icon("close"));
  editorHeading.append(editorTitle, closeEditor);

  const nameInput = element("input", "scheduled-input");
  nameInput.type = "text";
  nameInput.maxLength = 160;
  nameInput.placeholder = "例如：每日检查 CI";
  nameInput.setAttribute("aria-label", "任务名称");

  const promptInput = element("textarea", "scheduled-textarea");
  promptInput.rows = 6;
  promptInput.maxLength = 128 * 1024;
  promptInput.placeholder = "输入每次执行时发送给模型的提示";
  promptInput.setAttribute("aria-label", "执行提示");

  const instanceSelect = selectControl("scheduled-instance", "选择会话实例", [], "",
    "scheduled-select", selected => [
      ...(selected?.icon ? [icon(selected.icon)] : []),
      element("span", "select-label", selected?.label || "先选择会话实例")
    ]);
  instanceSelect.selectAllowEmpty = true;
  instanceSelect.disabled = true;

  const modelSelect = selectControl("scheduled-model", "选择请求模型", [], "",
    "scheduled-select", selected => [element("span", "select-label", selected?.label || "先选择实例")]);
  modelSelect.selectAllowEmpty = true;
  modelSelect.disabled = true;

  const effortSelect = selectControl("scheduled-effort", "选择思考深度", effortOptions, "",
    "scheduled-select", selected => [element("span", "select-label", selected?.label || "默认")]);
  effortSelect.selectAllowEmpty = true;
  effortSelect.disabled = true;

  const scheduleMode = selectControl("scheduled-mode", "执行计划", [
    { value: "once", label: "一次性" },
    { value: "daily", label: "每天" },
    { value: "weekly", label: "每周" },
    { value: "cron", label: "自定义 Cron" }
  ], "once", "scheduled-select", selected => [element("span", "select-label", selected?.label || "一次性")]);

  const runAtInput = element("input", "scheduled-input");
  runAtInput.type = "datetime-local";
  runAtInput.setAttribute("aria-label", "首次执行时间");

  const cronInput = element("input", "scheduled-input");
  cronInput.type = "text";
  cronInput.placeholder = "例如：0 9 * * 1-5";
  cronInput.setAttribute("aria-label", "Cron 表达式");
  cronInput.hidden = true;

  const timeField = field("首次执行", runAtInput);
  const cronField = field("Cron 表达式", cronInput);
  cronField.hidden = true;

  const status = element("p", "scheduled-editor-status");
  const actions = element("div", "scheduled-editor-actions");
  const cancel = button("取消", "取消编辑", () => closeEditorPanel(), "text-button");
  const save = button("保存任务", "保存定时任务", () => void saveTask(), "primary-button");
  actions.append(cancel, save);
  editor.append(
    editorHeading,
    element("div", "scheduled-editor-grid", null),
    status,
    actions
  );
  const grid = editor.querySelector(".scheduled-editor-grid");
  grid.append(
    field("任务名称", nameInput, "scheduled-field-wide"),
    field("执行提示", promptInput, "scheduled-field-wide scheduled-field-prompt"),
    field("会话实例", instanceSelect),
    field("请求模型", modelSelect),
    field("思考深度", effortSelect),
    field("执行计划", scheduleMode),
    timeField,
    cronField
  );

  root.append(heading, editor, list);

  function setEditorStatus(message = "", error = false) {
    status.textContent = message;
    status.classList.toggle("error-text", error);
  }

  function renderInstances() {
    const options = availableSessionInstances()
      .filter(instance => instance.ready && sessionInstanceEnabled(instance.id))
      .map(instance => ({
        value: instance.id,
        label: instance.label,
        icon: instance.backend
      }));
    instancesReady = options.length > 0;
    instanceSelect.disabled = !instancesReady;
    setSelectOptions(instanceSelect, options, instanceSelect.value);
    if (!instancesReady) {
      setSelectOptions(modelSelect, [], "");
      modelSelect.disabled = true;
      effortSelect.disabled = true;
    }
  }

  async function loadModelsForInstance(instanceId, selectedModel = "") {
    modelRows = [];
    setSelectOptions(modelSelect, [], "");
    modelSelect.disabled = true;
    effortSelect.disabled = true;
    if (!instanceId || !instancesReady) return;
    setEditorStatus("正在读取模型…");
    try {
      const backend = instanceId.split(":")[0];
      const result = await api("/api/read/models", {
        backend, instanceId, includeAvailable: true
      });
      if (disposed || instanceSelect.value !== instanceId) return;
      modelRows = Array.isArray(result?.data) ? result.data : [];
      const options = modelRows
        .map(row => ({
          value: row.id || row.model || row.requestModel || "",
          label: row.displayName || row.label || row.id || row.model || "",
          description: row.description || ""
        }))
        .filter(row => row.value && row.label);
      setSelectOptions(modelSelect, options, selectedModel || options[0]?.value || "");
      modelSelect.disabled = options.length === 0;
      syncEfforts(modelSelect.value);
      setEditorStatus(options.length ? "" : "该实例未返回可用模型", !options.length);
    } catch (error) {
      if (!disposed) setEditorStatus(error.message, true);
    }
  }

  function syncEfforts(modelId) {
    const row = modelRows.find(item => (item.id || item.model || item.requestModel) === modelId);
    const supported = Array.isArray(row?.supportedReasoningEfforts)
      ? row.supportedReasoningEfforts.map(item => item.reasoningEffort || item.value).filter(Boolean)
      : [];
    const options = supported.length
      ? effortOptions.filter(option => supported.includes(option.value))
      : [{ value: "", label: "默认" }];
    setSelectOptions(effortSelect, options, effortSelect.value);
    effortSelect.disabled = modelSelect.disabled || options.length === 0;
  }

  instanceSelect.onchange = () => void loadModelsForInstance(instanceSelect.value);
  modelSelect.onchange = () => syncEfforts(modelSelect.value);
  scheduleMode.onchange = () => {
    const custom = scheduleMode.value === "cron";
    runAtInput.hidden = custom;
    timeField.hidden = custom;
    cronField.hidden = !custom;
  };

  function openEditor(task = null) {
    editingId = task?.id || "";
    editorTitle.textContent = task ? "编辑任务" : "新建任务";
    nameInput.value = task?.name || "";
    promptInput.value = task?.prompt || "";
    setEditorStatus("");
    const schedule = task?.schedule || {};
    const interval = Number(schedule.intervalMinutes) || 0;
    const mode = schedule.kind === "once" ? "once"
      : schedule.kind === "cron" ? "cron"
        : interval === 10080 ? "weekly" : interval === 1440 ? "daily" : "once";
    setSelectValue(scheduleMode, mode);
    runAtInput.value = toLocalInput(schedule.startAtMs || task?.nextRunAtMs);
    cronInput.value = schedule.cron || "";
    scheduleMode.dispatchEvent(new Event("change"));
    editor.hidden = false;
    add.disabled = true;
    void prepareEditor(task);
  }

  async function prepareEditor(task) {
    if (!instancesReady) {
      await loadSessionInstances(true).catch(error => setEditorStatus(error.message, true));
      if (disposed) return;
      renderInstances();
    }
    const instanceId = task?.instanceId || instanceSelect.value || "";
    setSelectValue(instanceSelect, instanceId);
    await loadModelsForInstance(instanceId, task?.model || "");
    if (task?.reasoningEffort) setSelectValue(effortSelect, task.reasoningEffort);
  }

  function closeEditorPanel() {
    editor.hidden = true;
    add.disabled = false;
    editingId = "";
    setEditorStatus("");
  }

  function schedulePayload() {
    const mode = scheduleMode.value;
    if (mode === "cron") {
      return {
        kind: "cron",
        cron: cronInput.value.trim(),
        timezone: Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC"
      };
    }
    const startAtMs = fromLocalInput(runAtInput.value);
    return {
      kind: mode === "once" ? "once" : "interval",
      startAtMs,
      ...(mode === "once" ? {} : { intervalMinutes: mode === "weekly" ? 10080 : 1440 })
    };
  }

  async function saveTask() {
    const task = {
      ...(editingId ? { taskId: editingId } : {}),
      name: nameInput.value.trim(),
      prompt: promptInput.value,
      instanceId: instanceSelect.value,
      model: modelSelect.value,
      reasoningEffort: effortSelect.value || "",
      schedule: schedulePayload(),
      enabled: true
    };
    if (!task.name || !task.prompt.trim() || !task.instanceId || !task.model) {
      setEditorStatus("请填写名称、提示，先选择实例，再选择模型。", true);
      return;
    }
    save.disabled = true;
    try {
      const action = editingId ? "updateScheduledTask" : "createScheduledTask";
      await action(`/api/action/${action}`, editingId ? { taskId: editingId, task } : { task });
      closeEditorPanel();
      await refreshTasks();
    } catch (error) {
      setEditorStatus(error.message, true);
    } finally {
      save.disabled = false;
    }
  }

  function renderTasks() {
    list.replaceChildren();
    if (!tasks.length) {
      const empty = element("div", "scheduled-empty");
      empty.append(icon("recent"), element("strong", "", "暂无定时任务"),
        element("span", "muted", "新建任务后会显示在这里。"));
      list.append(empty);
      return;
    }
    for (const task of tasks) {
      const row = element("article", `scheduled-row ${task.status === "error" ? "has-error" : ""}`);
      const copy = element("div", "scheduled-row-copy");
      const rowTitle = element("div", "scheduled-row-title");
      rowTitle.append(element("strong", "", task.name), element("span", `scheduled-status status-${task.status}`, statusLabel(task)));
      const meta = element("div", "scheduled-row-meta");
      meta.append(
        element("span", "", `${instanceLabel(task.instanceId)} · ${task.model}`),
        element("span", "", scheduleLabel(task))
      );
      const prompt = element("p", "scheduled-row-prompt", task.prompt.trim());
      copy.append(rowTitle, meta, prompt);
      if (task.lastError) copy.append(element("small", "error-text scheduled-row-error", task.lastError));
      const controls = element("div", "scheduled-row-actions");
      const run = button("", "立即运行", () => void runTask(task), "icon-button");
      run.append(icon("play"));
      const toggle = button("", task.enabled ? "暂停任务" : "启用任务", () => void toggleTask(task), "icon-button");
      toggle.append(icon(task.enabled ? "pause" : "play"));
      const edit = button("", "编辑任务", () => openEditor(task), "icon-button");
      edit.append(icon("edit"));
      const remove = button("", "删除任务", () => void deleteTask(task), "icon-button danger");
      remove.append(icon("trash"));
      controls.append(run, toggle, edit, remove);
      row.append(copy, controls);
      list.append(row);
    }
  }

  async function refreshTasks() {
    if (disposed) return;
    refresh.disabled = true;
    list.replaceChildren(element("div", "scheduled-loading", "正在读取定时任务…"));
    try {
      const result = await api("/api/read/scheduledTasks", {});
      if (disposed) return;
      tasks = Array.isArray(result?.tasks) ? result.tasks : [];
      renderTasks();
    } catch (error) {
      list.replaceChildren(element("div", "scheduled-error", error.message));
    } finally {
      refresh.disabled = false;
    }
  }

  async function runTask(task) {
    try {
      await action("/api/action/runScheduledTask", { taskId: task.id });
      notice(`已开始运行「${task.name}」`);
      await refreshTasks();
    } catch (error) {
      notice(error.message, true);
    }
  }

  async function toggleTask(task) {
    try {
      await action("/api/action/toggleScheduledTask", { taskId: task.id, enabled: !task.enabled });
      await refreshTasks();
    } catch (error) {
      notice(error.message, true);
    }
  }

  async function deleteTask(task) {
    if (!await confirmAction("删除定时任务", `将删除「${task.name}」，不会删除已经创建的会话。`, "删除", true)) return;
    try {
      await action("/api/action/deleteScheduledTask", { taskId: task.id });
      await refreshTasks();
    } catch (error) {
      notice(error.message, true);
    }
  }

  void loadSessionInstances(true)
    .then(() => {
      if (disposed) return;
      renderInstances();
      return refreshTasks();
    })
    .catch(error => {
      if (!disposed) list.replaceChildren(element("div", "scheduled-error", error.message));
    });
  const refreshOnEvent = () => void refreshTasks();
  window.addEventListener("codex-elves-scheduled-tasks-changed", refreshOnEvent);

  return () => {
    disposed = true;
    window.removeEventListener("codex-elves-scheduled-tasks-changed", refreshOnEvent);
    root.replaceChildren();
  };
}
