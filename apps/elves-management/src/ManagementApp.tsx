import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import {
  AlertCircle,
  CheckCircle2,
  KeyRound,
  MessageCircle,
  Moon,
  Network,
  RefreshCw,
  Sun,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { TooltipLayer } from "@/components/ui/tooltip-layer";
import { copyToClipboard } from "@/lib/utils";
import {
  LocalProxyScreen,
  RelayScreen,
  SessionsScreen,
  defaultSettings,
  normalizeSettings,
  type Actions,
  type BackendSettings,
  type CcsProvidersResult,
  type CommandResult,
  type EnvConflictsResult,
  type LocalProxyLogDetailResult,
  type LocalProxyLogsResult,
  type LocalProxyStatusResult,
  type LocalSessionsResult,
  type RelayFilesResult,
  type RelayProfileModelsResult,
  type RelayProfileTestResult,
  type RelaySwitchResult,
  type ResponsesWebsocketProbeResult,
  type SettingsResult,
  type Status,
} from "./management-screens";
import "./styles.css";
import "./management.css";
import elvesLogoUrl from "../../../assets/harness/icons/elves-logo.svg";

// 用户要求恢复历史管理器的三页交互：渠道管理、本地代理、会话管理；
// GPT 续接已经从设置、日志和请求链路移除，不能随旧页面恢复。
type Page = "relay" | "localProxy" | "sessions";
type Theme = "dark" | "light";
type Notice = {
  id: number;
  title: string;
  message: string;
  status: "ok" | "failed";
};

const THEME_STORAGE_KEY = "elves-manager-theme";
const LEGACY_THEME_STORAGE_KEY = "codex-elves-theme";
const TOAST_DURATION_MS = 4200;

const titles: Record<Page, string> = {
  relay: "渠道管理",
  localProxy: "本地代理",
  sessions: "会话管理",
};
const subtitles: Record<Page, string> = {
  relay: "管理 API 渠道、协议、Key 与模型配置",
  localProxy: "查看本地代理状态、请求日志和完整返回内容",
  sessions: "浏览、筛选和删除本机 Codex 会话",
};
const navItems: Array<{ id: Page; label: string; icon: typeof KeyRound }> = [
  { id: "relay", label: "渠道管理", icon: KeyRound },
  { id: "localProxy", label: "本地代理", icon: Network },
  { id: "sessions", label: "会话管理", icon: MessageCircle },
];
const isPage = (value: unknown): value is Page =>
  typeof value === "string" && Object.hasOwn(titles, value);
const initialPage =
  document.documentElement.dataset.managementPage ||
  new URLSearchParams(location.search).get("page");

const PROXY_AFFECTING_KEYS: ReadonlyArray<keyof BackendSettings> = [
  "localProxyStartupEnabled",
  "lanProxyEnabled",
  "relayProfilesEnabled",
  "activeRelayId",
  "activeAggregateRelayId",
  "relayProfiles",
  "aggregateRelayProfiles",
];

function loadInitialTheme(): Theme {
  if (typeof window === "undefined") return "dark";
  try {
    const saved =
      window.localStorage.getItem(THEME_STORAGE_KEY) ||
      window.localStorage.getItem(LEGACY_THEME_STORAGE_KEY);
    return saved === "light" ? "light" : "dark";
  } catch {
    return "dark";
  }
}

type TauriWindow = Window & {
  __TAURI__?: {
    core?: {
      invoke?: (command: string, args?: Record<string, unknown>) => Promise<unknown>;
    };
  };
  __TAURI_INTERNALS__?: {
    invoke?: (command: string, args?: Record<string, unknown>) => Promise<unknown>;
  };
};

function standaloneInvoke() {
  const current = window as TauriWindow;
  return current.__TAURI__?.core?.invoke || current.__TAURI_INTERNALS__?.invoke || null;
}

const standaloneManager = window.parent === window && standaloneInvoke() !== null;

function stringifyError(value: unknown): string {
  return value instanceof Error ? value.message : String(value);
}

/**
 * 顶部居中浮动 Toast（脱离文档流、高于 Modal/Drawer，支持悬停暂停与关闭按钮）。
 * 用户要求恢复旧版管理器的提示组件体验，不再使用挤压页面内容的内联条。
 */
function NoticeToast({
  notice,
  theme,
  onClose,
}: {
  notice: Notice;
  theme: Theme;
  onClose: () => void;
}) {
  const [paused, setPaused] = useState(false);
  const remainingRef = useRef(TOAST_DURATION_MS);
  const startedAtRef = useRef(Date.now());
  const failed = notice.status === "failed";

  useEffect(() => {
    if (paused) return;
    startedAtRef.current = Date.now();
    const timer = window.setTimeout(onClose, Math.max(0, remainingRef.current));
    return () => {
      window.clearTimeout(timer);
      remainingRef.current = Math.max(0, remainingRef.current - (Date.now() - startedAtRef.current));
    };
  }, [paused, onClose]);

  const content = (
    <div
      aria-live={failed ? "assertive" : "polite"}
      className={`toast-wrap ${theme}`}
      role={failed ? "alert" : "status"}
    >
      <div
        className={`toast-card ${failed ? "failed" : ""} ${paused ? "paused" : ""}`}
        onMouseEnter={() => setPaused(true)}
        onMouseLeave={() => setPaused(false)}
      >
        <div className="toast-progress" />
        <div className="toast-icon" aria-hidden="true">
          {failed ? <AlertCircle className="h-5 w-5" /> : <CheckCircle2 className="h-5 w-5" />}
        </div>
        <div className="toast-body">
          <h2>{notice.title}</h2>
          <p>{notice.message}</p>
        </div>
        <button
          aria-label="关闭提示"
          className="toast-close"
          onClick={onClose}
          type="button"
        >
          ×
        </button>
      </div>
    </div>
  );

  return typeof document === "undefined" ? content : createPortal(content, document.body);
}

export function App() {
  const initialRoute = initialPage === "proxy" ? "relay" : initialPage;
  const [page, setPage] = useState<Page>(isPage(initialRoute) ? initialRoute : "relay");
  // 用户要求独立代理管理器保留自己的浅色/深色主题切换并独立持久化。
  const [theme, setTheme] = useState<Theme>(loadInitialTheme);
  const [initialized, setInitialized] = useState(false);
  const [loadFailed, setLoadFailed] = useState(false);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [form, setForm] = useState<BackendSettings>(defaultSettings);
  const [settings, setSettings] = useState<SettingsResult | null>(null);
  const [relayFiles, setRelayFiles] = useState<RelayFilesResult | null>(null);
  const [conflicts, setConflicts] = useState<EnvConflictsResult | null>(null);
  const [providers, setProviders] = useState<CcsProvidersResult | null>(null);
  const [sessions, setSessions] = useState<LocalSessionsResult | null>(null);
  const [proxyStatus, setProxyStatus] = useState<LocalProxyStatusResult | null>(null);
  const [proxyLogs, setProxyLogs] = useState<LocalProxyLogsResult | null>(null);
  const [proxyError, setProxyError] = useState<string | null>(null);
  const [detail, setDetail] = useState<LocalProxyLogDetailResult | null>(null);
  const [selectedLog, setSelectedLog] = useState<string | null>(null);
  const [loadingLog, setLoadingLog] = useState<string | null>(null);
  const [switching, setSwitching] = useState(false);
  const [launchingProxy, setLaunchingProxy] = useState(false);
  const [reloadEpoch, setReloadEpoch] = useState(0);

  const revision = useRef("");
  const persisted = useRef<BackendSettings>(defaultSettings);
  const queue = useRef<Promise<unknown>>(Promise.resolve());
  const generation = useRef(0);
  const detailGeneration = useRef(0);
  const noticeSeq = useRef(0);
  const uncertain = useRef(false);
  const alive = useRef(true);
  const requests = useRef(new AbortController());
  const visible = useRef(true);
  const draft = useRef(form);
  draft.current = form;
  const localEdits = useRef(false);
  const queued = useRef(0);
  const checking = useRef(false);
  const conflictNotice = useRef("");

  const showNotice = (title: string, value: unknown, failed = false) => {
    if (!alive.current) return;
    const text = stringifyError(value).trim();
    if (!text) return;
    if (failed && !initialized) setLoadFailed(true);
    setNotice({
      id: ++noticeSeq.current,
      title,
      message: text,
      status: failed ? "failed" : "ok",
    });
  };

  const noteEdit = () => {
    localEdits.current = true;
    generation.current++;
  };

  const edit = (value: BackendSettings) => {
    noteEdit();
    draft.current = value;
    setForm(value);
  };

  useEffect(() => {
    document.documentElement.classList.toggle("dark", theme === "dark");
    document.documentElement.classList.toggle("light", theme === "light");
    try {
      window.localStorage.setItem(THEME_STORAGE_KEY, theme);
    } catch {
      // 忽略私有或受限存储环境写入失败
    }
  }, [theme]);

  async function call<T>(command: string, args: Record<string, unknown> = {}, write = false): Promise<T> {
    if (write && uncertain.current) {
      throw new Error("上次保存结果尚未确认，请先刷新配置，再继续修改。");
    }
    const invoke = standaloneInvoke();
    if (!invoke) throw new Error("代理管理器本机命令不可用，请从 Elves 代理管理器启动。");
    let value: any;
    try {
      value = await invoke("manager_command", {
        command,
        payload: { args, revision: revision.current },
      });
    } catch (failure) {
      if (write) uncertain.current = true;
      throw failure;
    }
    if (value.status === "failed") throw new Error(value.message || "操作失败。");
    if (write && typeof value.revision === "string") revision.current = value.revision;
    return value as T;
  }

  async function run<T>(operation: () => Promise<T>, errorTitle = "操作失败"): Promise<T | null> {
    try {
      return await operation();
    } catch (failure) {
      showNotice(errorTitle, failure, true);
      return null;
    }
  }

  function enqueue<T>(operation: () => Promise<T>): Promise<T> {
    queued.current++;
    const result = queue.current.then(operation).finally(() => {
      queued.current--;
    });
    queue.current = result.catch(() => {});
    return result;
  }

  function acceptSettings(result: SettingsResult | RelaySwitchResult, editVersion: number) {
    const next = normalizeSettings(result.settings);
    persisted.current = next;
    if (generation.current === editVersion) {
      draft.current = next;
      localEdits.current = false;
      setForm(next);
    }
    if ("settings_path" in result) setSettings(result);
  }

  async function refreshSettings() {
    const editVersion = generation.current;
    const result = await call<SettingsResult & { revision: string }>("load_settings");
    if (!alive.current) return false;
    if (generation.current !== editVersion) {
      showNotice("配置提醒", "读取期间产生了新输入，已保留当前草稿，请刷新后合并配置。", true);
      return false;
    }
    revision.current = result.revision;
    uncertain.current = false;
    setLoadFailed(false);
    acceptSettings(result, editVersion);
    setInitialized(true);
    return true;
  }

  async function checkConfiguration() {
    if (!visible.current || !revision.current || queued.current || checking.current) return;
    checking.current = true;
    try {
      const result = await call<{ revision: string }>("settings_revision");
      if (result.revision === revision.current) return;
      if (
        localEdits.current ||
        uncertain.current ||
        JSON.stringify(draft.current) !== JSON.stringify(persisted.current)
      ) {
        if (conflictNotice.current !== result.revision) {
          conflictNotice.current = result.revision;
          showNotice("配置冲突", "服务器配置已更新，当前输入已保留，请刷新后合并配置。", true);
        }
        return;
      }
      await enqueue(async () => {
        if (localEdits.current || uncertain.current) return;
        if (await refreshSettings()) await Promise.all([refreshRelay(), refreshStatus()]);
      });
    } catch (failure) {
      showNotice("配置同步失败", failure, true);
    } finally {
      checking.current = false;
    }
  }

  const refreshRelay = async () => {
    const result = await call<RelayFilesResult>("read_relay_files");
    if (alive.current) setRelayFiles(result);
    return result;
  };
  const refreshSessions = async () => {
    const result = await call<LocalSessionsResult>("list_local_sessions");
    if (alive.current) setSessions(result);
    return result;
  };
  const refreshStatus = async () => {
    const result = await call<LocalProxyStatusResult>("local_proxy_status");
    if (alive.current) setProxyStatus(result);
    return result;
  };
  const refreshLogs = async () => {
    try {
      const result = await call<LocalProxyLogsResult>("read_local_proxy_logs", {
        request: { limit: 1000 },
      });
      if (alive.current) {
        setProxyLogs(result);
        setProxyError(null);
      }
      return result;
    } catch (failure) {
      if (alive.current) setProxyError(stringifyError(failure));
      throw failure;
    }
  };
  const refreshConflicts = async () => {
    const result = await call<EnvConflictsResult>("check_env_conflicts");
    if (alive.current) setConflicts(result);
    return result;
  };
  const refreshProviders = async () => {
    const result = await call<CcsProvidersResult>("load_ccs_providers");
    if (alive.current) setProviders(result);
    return result;
  };

  async function save(
    value: BackendSettings,
    silent = false,
    explicitPatch?: Partial<BackendSettings>,
  ) {
    const editVersion = generation.current;
    // 在队列真正执行时基于最新已落盘快照 persisted.current 计算或合并补丁，
    // 避免快速连续保存（A 后接 B）时因 A 失败重置乐观基准导致前序修改丢失或互相覆盖。
    const snapshot = explicitPatch ? null : { ...value };
    const fixedPatch = explicitPatch ? { ...explicitPatch } : null;

    return enqueue(async () => {
      try {
        const base = persisted.current;
        const patch = fixedPatch
          ? fixedPatch
          : (Object.fromEntries(
              Object.entries(snapshot!).filter(
                ([key, entry]) =>
                  JSON.stringify(entry) !== JSON.stringify(base[key as keyof BackendSettings]),
              ),
            ) as Partial<BackendSettings>);
        const proxyChanged = PROXY_AFFECTING_KEYS.some((key) => Object.hasOwn(patch, key));
        const next = { ...base, ...patch };
        const result = await call<SettingsResult>("save_settings", { settings: next }, true);
        if (alive.current) {
          acceptSettings(result, editVersion);
          if (proxyChanged) {
            await refreshStatus().catch(() => {});
          }
          if (!silent) showNotice("设置保存", result.message, false);
        }
      } catch (failure) {
        if (alive.current && generation.current === editVersion) {
          draft.current = persisted.current;
          localEdits.current = false;
          setForm(persisted.current);
        }
        throw failure;
      }
    });
  }

  async function launchProxyFromTopbar() {
    if (launchingProxy || !proxyStatus || !proxyStatus.enabled || proxyStatus.listening) return;
    if (!proxyStatus.startupEnabled) {
      setPage("localProxy");
      showNotice("本地代理", "当前已关闭“随 Elves 启动”，请在本地代理页开启开关后再启动。", true);
      return;
    }
    const invoke = standaloneInvoke();
    if (!invoke) return;
    setLaunchingProxy(true);
    try {
      await invoke("ensure_proxy_service");
      const status = await refreshStatus();
      if (status?.listening) {
        showNotice("本地代理", status.message || "本地代理已启动。", false);
      } else {
        showNotice("本地代理", status?.message || "本地代理尚未监听端口，请检查配置。", true);
      }
    } catch (failure) {
      showNotice("本地代理", failure, true);
    } finally {
      if (alive.current) setLaunchingProxy(false);
    }
  }

  async function refresh() {
    if (
      initialized &&
      (localEdits.current || JSON.stringify(form) !== JSON.stringify(persisted.current)) &&
      !window.confirm("刷新会重新读取服务器配置，并放弃当前页面尚未保存的配置，继续？")
    ) {
      return;
    }
    generation.current++;
    if (!(await enqueue(refreshSettings))) return;
    setReloadEpoch((value) => value + 1);
    if (page === "relay") {
      await Promise.all([refreshRelay(), refreshConflicts(), refreshProviders(), refreshStatus()]);
    }
    if (page === "localProxy") await Promise.all([refreshStatus(), refreshLogs()]);
    if (page === "sessions") await Promise.all([refreshSessions(), refreshStatus()]);
    showNotice("刷新完成", "已读取服务器当前配置。", false);
  }

  const actions: Actions = {
    saveSettingsValue: async (value, silent, requireSuccess, patch) => {
      try {
        await save(value, silent, patch);
      } catch (failure) {
        showNotice("设置保存", failure, true);
        if (requireSuccess) throw failure;
      }
    },
    refreshRelayFiles: () => run(refreshRelay, "读取配置文件失败"),
    refreshEnvConflicts: () => run(refreshConflicts, "检查环境变量失败"),
    removeEnvConflicts: async (names) => {
      const result = await run(
        () => call<CommandResult<unknown>>("remove_env_conflicts", { request: { names } }),
        "清理环境变量失败",
      );
      if (result) {
        showNotice("环境变量", result.message, false);
        await run(refreshConflicts, "检查环境变量失败");
      }
    },
    refreshCcsProviders: () => run(refreshProviders, "读取第三方供应商失败"),
    importCcsProviders: async () => {
      const editVersion = generation.current;
      await run(
        () =>
          enqueue(async () => {
            const result = await call<SettingsResult>("import_ccs_providers", {}, true);
            acceptSettings(result, editVersion);
            showNotice("导入供应商", result.message, false);
          }),
        "导入供应商失败",
      );
    },
    refreshLocalSessions: () => run(refreshSessions, "读取会话失败"),
    deleteLocalSession: async (thread) => {
      if (
        !window.confirm(
          `永久删除“${thread.title || "未命名会话"}”及其会话记录？项目文件不受影响，此操作不可恢复。`,
        )
      ) {
        return;
      }
      await run(async () => {
        const result = await call<CommandResult<unknown>>(
          "delete_local_session",
          {
            request: {
              sessionId: thread.id.replace(/^local:/, ""),
              title: thread.title,
              dbPath: thread.dbPath,
            },
          },
          true,
        );
        await refreshSessions();
        showNotice("会话管理", result.message || "本地会话已删除。", false);
      }, "删除会话失败");
    },
    deleteLocalSessionsBatch: async (threads) => {
      if (
        !window.confirm(
          `永久删除筛选出的 ${threads.length} 个会话及其记录？项目文件不受影响，此操作不可恢复。`,
        )
      ) {
        return;
      }
      let deleted = 0;
      try {
        for (const thread of threads) {
          await call<CommandResult<unknown>>(
            "delete_local_session",
            {
              request: {
                sessionId: thread.id.replace(/^local:/, ""),
                title: thread.title,
                dbPath: thread.dbPath,
              },
            },
            true,
          );
          deleted++;
        }
        showNotice("会话管理", `已删除 ${deleted} 个会话。`, false);
      } catch (failure) {
        showNotice(
          "会话管理",
          `已删除 ${deleted} 个会话；${stringifyError(failure)}`,
          true,
        );
      }
      await run(refreshSessions, "读取会话失败");
    },
    saveRelayAuthFile: async (contents, silent) => {
      const result = await run(
        () =>
          enqueue(() =>
            call<RelayFilesResult>("save_relay_file", { request: { kind: "auth", contents } }, true),
          ),
        "保存认证文件失败",
      );
      if (result) {
        setRelayFiles(result);
        if (!silent) showNotice("认证配置", result.message, false);
      }
    },
    testRelayProfile: (profile, model, speedTest) =>
      run(() => call<RelayProfileTestResult>("test_relay_profile", { profile, model, speedTest }), "渠道测试失败"),
    fetchRelayProfileModels: async (profile) =>
      (await run(() => call<RelayProfileModelsResult>("fetch_relay_profile_models", { profile }), "获取模型失败"))
        ?.models ?? null,
    probeRelayProfileResponsesWebsocket: async (profile) =>
      (
        await run(
          () =>
            enqueue(() =>
              call<ResponsesWebsocketProbeResult>(
                "probe_relay_profile_responses_websocket",
                { profile },
                true,
              ),
            ),
          "WebSocket 探测失败",
        )
      )?.capability ?? null,
    switchRelayProfile: async (value, previousActiveRelayId) => {
      const editVersion = generation.current;
      setSwitching(true);
      try {
        await enqueue(async () => {
          const result = await call<RelaySwitchResult>(
            "switch_relay_profile",
            { request: { settings: value, previousActiveRelayId: previousActiveRelayId || "" } },
            true,
          );
          acceptSettings(result, editVersion);
          showNotice("供应商切换", result.message, false);
          await Promise.all([refreshRelay(), refreshStatus()]);
        });
      } catch (failure) {
        showNotice("供应商切换", failure, true);
      } finally {
        if (alive.current) setSwitching(false);
      }
    },
    relaySwitching: switching,
    refreshLocalProxyStatus: () => run(refreshStatus, "读取代理状态失败"),
    refreshLocalProxyLogs: () => run(refreshLogs, "读取代理日志失败"),
    loadLocalProxyLogDetail: async (id) => {
      const version = ++detailGeneration.current;
      setSelectedLog(id);
      setLoadingLog(id);
      setDetail(null);
      const result = await run(
        () => call<LocalProxyLogDetailResult>("read_local_proxy_log_detail", { request: { id } }),
        "读取日志详情失败",
      );
      if (alive.current && detailGeneration.current === version) {
        setDetail(result);
        setLoadingLog(null);
      }
    },
    closeLocalProxyLogDetail: () => {
      detailGeneration.current++;
      setSelectedLog(null);
      setDetail(null);
      setLoadingLog(null);
    },
    clearLocalProxyLogs: async () => {
      if (!window.confirm("清空全部代理请求日志及完整请求、返回内容？")) return;
      const result = await run(
        () => call<LocalProxyLogsResult>("clear_local_proxy_logs"),
        "清空代理日志失败",
      );
      if (result) {
        setProxyLogs(result);
        actions.closeLocalProxyLogDetail();
        showNotice("代理日志", result.message, false);
      }
    },
    showMessage: async (title, value, status?: Status) =>
      showNotice(title, value, status === "failed"),
    copyLocalProxyRequest: async (text) => {
      if (text) {
        await run(async () => {
          await copyToClipboard(text);
          showNotice("已复制", "请求内容已复制到剪贴板。", false);
        }, "复制失败");
      }
    },
    copyLocalProxyResponse: async (text) => {
      if (text) {
        await run(async () => {
          await copyToClipboard(text);
          showNotice("已复制", "返回内容已复制到剪贴板。", false);
        }, "复制失败");
      }
    },
    copyLocalProxyAddress: async (text) => {
      await run(async () => {
        await copyToClipboard(text);
        showNotice("已复制", "代理地址已复制到剪贴板。", false);
      }, "复制失败");
    },
  };

  useEffect(() => {
    alive.current = true;
    void run(async () => {
      if (standaloneInvoke()) await standaloneInvoke()!("ensure_proxy_service");
      await refreshSettings();
      await Promise.all([
        refreshRelay(),
        refreshConflicts(),
        refreshProviders(),
        refreshStatus(),
        refreshLogs(),
      ]);
    }, "初始化失败");
    const settingsTimer = window.setInterval(() => void checkConfiguration(), 15000);
    return () => {
      clearInterval(settingsTimer);
      alive.current = false;
      requests.current.abort();
    };
  }, []);

  useEffect(() => {
    if (!initialized) return;
    if (page === "sessions") void run(refreshSessions, "读取会话失败");
    if (page !== "localProxy") return;
    let polling = false;
    const poll = async () => {
      if (!visible.current || document.hidden || polling) return;
      polling = true;
      try {
        await Promise.all([refreshStatus(), refreshLogs()]);
      } catch (failure) {
        showNotice("代理状态刷新失败", failure, true);
      } finally {
        polling = false;
      }
    };
    const timer = window.setInterval(() => void poll(), 10000);
    return () => window.clearInterval(timer);
  }, [page, initialized]);

  const canLaunchProxyFromTopbar = Boolean(
    proxyStatus && !proxyStatus.settingsError && proxyStatus.enabled && !proxyStatus.listening,
  );
  const proxyStatusLabel = launchingProxy
    ? "代理启动中…"
    : !proxyStatus
      ? ""
      : proxyStatus.settingsError
        ? "设置读取失败"
        : proxyStatus.listening
          ? proxyStatus.lanListening
            ? `代理运行中（局域网）· ${proxyStatus.port}`
            : `代理运行中 · ${proxyStatus.port}`
          : proxyStatus.enabled
            ? proxyStatus.startupEnabled
              ? "代理未运行（点击启动）"
              : "代理已关闭随 Elves 启动"
            : "当前渠道未启用代理";

  return (
    <div className={`shell ${theme} ${standaloneManager ? "manager-shell" : ""}`}>
      <aside className="sidebar">
        <div className="brand">
          <img className="brand-mark" src={elvesLogoUrl} alt="" aria-hidden="true" />
          <div className="brand-copy">
            <div className="brand-title">Elves</div>
            <div className="brand-subtitle">代理管理器</div>
          </div>
        </div>
        <nav aria-label="管理器页面" className="nav">
          <div className="nav-groups">
            <div className="nav-group" role="group" aria-labelledby="manager-nav-group">
              <div className="nav-group-label" id="manager-nav-group">
                工作台
              </div>
              <div className="nav-group-items">
                {navItems.map(({ id, label, icon: Icon }) => {
                  const active = page === id;
                  return (
                    <button
                      aria-current={active ? "page" : undefined}
                      className={`nav-item ${active ? "active" : ""}`}
                      key={id}
                      onClick={() => setPage(id)}
                      type="button"
                    >
                      <span className="nav-icon" data-tooltip={label}>
                        <Icon className="h-4 w-4" aria-hidden="true" />
                      </span>
                      <span className="nav-label">{label}</span>
                    </button>
                  );
                })}
              </div>
            </div>
          </div>
        </nav>
      </aside>
      <main className="workspace">
        <header className="topbar">
          <div>
            <h1>{titles[page]}</h1>
            <p>{subtitles[page]}</p>
          </div>
          <div className="topbar-actions">
            {proxyStatus ? (
              canLaunchProxyFromTopbar ? (
                <button
                  className={`manager-proxy-status clickable ${launchingProxy ? "launching" : ""}`}
                  data-tooltip={proxyStatus.message || "点击尝试启动独立本地代理"}
                  disabled={launchingProxy}
                  onClick={() => void launchProxyFromTopbar()}
                  type="button"
                >
                  <span className="proxy-status-dot" aria-hidden="true" />
                  <span>{proxyStatusLabel}</span>
                </button>
              ) : (
                <span
                  className={`manager-proxy-status ${proxyStatus.settingsError ? "failed" : proxyStatus.listening ? "running" : ""}`}
                  data-tooltip={proxyStatus.message || undefined}
                >
                  <span className="proxy-status-dot" aria-hidden="true" />
                  <span>{proxyStatusLabel}</span>
                </span>
              )
            ) : null}
            <Button
              aria-label={theme === "dark" ? "切换到浅色主题" : "切换到深色主题"}
              onClick={() => setTheme((current) => (current === "dark" ? "light" : "dark"))}
              size="icon"
              title={theme === "dark" ? "切换到浅色" : "切换到深色"}
              variant="outline"
            >
              {theme === "dark" ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
            </Button>
            <Button
              aria-label="刷新当前页面"
              onClick={() => void run(refresh, "刷新失败")}
              variant="outline"
            >
              <RefreshCw className="h-4 w-4" />
              刷新
            </Button>
          </div>
        </header>
        <section className="screen" key={`${page}-${reloadEpoch}`}>
          {!initialized ? (
            <p>{loadFailed ? "配置未读取成功，请刷新后重试。" : "正在读取服务器配置…"}</p>
          ) : (
            <>
              {page === "relay" ? (
                <RelayScreen
                  settings={settings}
                  relayFiles={relayFiles}
                  envConflicts={conflicts}
                  ccsProviders={providers}
                  form={form}
                  onFormChange={edit}
                  actions={actions}
                />
              ) : null}
              {page === "localProxy" ? (
                <LocalProxyScreen
                  status={proxyStatus}
                  logs={proxyLogs}
                  logsError={proxyError}
                  detail={detail}
                  selectedId={selectedLog}
                  loadingDetailId={loadingLog}
                  form={form}
                  onFormChange={edit}
                  actions={actions}
                />
              ) : null}
              {page === "sessions" ? (
                <SessionsScreen
                  sessions={sessions}
                  form={form}
                  onFormChange={edit}
                  actions={actions}
                />
              ) : null}
            </>
          )}
        </section>
      </main>
      {notice ? (
        <NoticeToast
          key={notice.id}
          notice={notice}
          onClose={() => setNotice(null)}
          theme={theme}
        />
      ) : null}
      <TooltipLayer theme={theme} />
    </div>
  );
}
