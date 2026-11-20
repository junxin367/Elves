import { useEffect, useRef, useState } from "react";
import { RefreshCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { TooltipLayer } from "@/components/ui/tooltip-layer";
import { copyToClipboard } from "@/lib/utils";
import {
  LocalProxyScreen, RelayScreen, SessionsScreen, defaultSettings, normalizeSettings,
  type Actions, type BackendSettings, type CcsProvidersResult, type CommandResult,
  type EnvConflictsResult, type LocalProxyLogDetailResult, type LocalProxyLogsResult,
  type LocalProxyStatusResult, type LocalSession, type LocalSessionsResult,
  type RelayFilesResult, type RelayProfileModelsResult, type RelayProfileTestResult,
  type RelaySwitchResult, type ResponsesWebsocketProbeResult, type SettingsResult
} from "./management-screens";
import "./styles.css";
import "./management.css";

type Page = "proxy" | "sessions";
const titles: Record<Page, string> = { proxy: "代理设置", sessions: "会话管理" };
const isPage = (value: unknown): value is Page => typeof value === "string" && Object.hasOwn(titles, value);
const initialPage = document.documentElement.dataset.managementPage || new URLSearchParams(location.search).get("page");

type TauriWindow = Window & {
  __TAURI__?: { core?: { invoke?: (command: string, args?: Record<string, unknown>) => Promise<unknown> } };
  __TAURI_INTERNALS__?: { invoke?: (command: string, args?: Record<string, unknown>) => Promise<unknown> };
};

function standaloneInvoke() {
  const current = window as TauriWindow;
  return current.__TAURI__?.core?.invoke || current.__TAURI_INTERNALS__?.invoke || null;
}

const standaloneManager = window.parent === window && standaloneInvoke() !== null;

export function App() {
  const [page, setPage] = useState<Page>(isPage(initialPage) ? initialPage : "proxy");
  const [theme] = useState<"light" | "dark">("dark");
  const [initialized, setInitialized] = useState(false);
  const [message, setMessage] = useState("");
  const [error, setError] = useState(false);
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
  const [reloadEpoch, setReloadEpoch] = useState(0);
  const revision = useRef("");
  const persisted = useRef<BackendSettings>(defaultSettings);
  const queue = useRef<Promise<unknown>>(Promise.resolve());
  const generation = useRef(0);
  const detailGeneration = useRef(0);
  const uncertain = useRef(false);
  const alive = useRef(true);
  const requests = useRef(new AbortController());
  const visible = useRef(true);
  const draft = useRef(form);
  draft.current = form;
  const localEdits = useRef(false), queued = useRef(0), checking = useRef(false), conflictNotice = useRef("");
  const report = (value: unknown, failed = false) => {
    if (!alive.current) return;
    setMessage(value instanceof Error ? value.message : String(value));
    setError(failed);
  };
  const noteEdit = () => { localEdits.current = true; generation.current++; };
  const edit = (value: BackendSettings) => { noteEdit(); draft.current = value; setForm(value); };
  async function call<T>(command: string, args: Record<string, unknown> = {}, write = false): Promise<T> {
    if (write && uncertain.current) throw new Error("上次保存结果尚未确认，请先刷新配置，再继续修改。");
    const invoke = standaloneInvoke();
    if (!invoke) throw new Error("代理管理器本机命令不可用，请从 Elves 代理管理器启动。");
    let value: any;
    try {
      value = await invoke("manager_command", {
        command,
        payload: { args, revision: revision.current }
      });
    } catch (failure) {
      if (write) uncertain.current = true;
      throw failure;
    }
    if (value.status === "failed") throw new Error(value.message || "操作失败。");
    if (write && typeof value.revision === "string") revision.current = value.revision;
    return value as T;
  }
  async function run<T>(operation: () => Promise<T>): Promise<T | null> {
    try { return await operation(); } catch (failure) { report(failure, true); return null; }
  }
  function enqueue<T>(operation: () => Promise<T>): Promise<T> {
    queued.current++;
    const result = queue.current.then(operation).finally(() => { queued.current--; });
    queue.current = result.catch(() => {});
    return result;
  }
  function acceptSettings(result: SettingsResult | RelaySwitchResult, editVersion: number) {
    const next = normalizeSettings(result.settings);
    persisted.current = next;
    if (generation.current === editVersion) { draft.current = next; localEdits.current = false; setForm(next); }
    if ("settings_path" in result) setSettings(result);
  }
  async function refreshSettings() {
    const editVersion = generation.current;
    const result = await call<SettingsResult & { revision: string }>("load_settings");
    if (!alive.current) return false;
    if (generation.current !== editVersion) {
      report("读取期间产生了新输入，已保留当前草稿，请刷新后合并配置。", true);
      return false;
    }
    revision.current = result.revision;
    uncertain.current = false;
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
      if (localEdits.current || uncertain.current || JSON.stringify(draft.current) !== JSON.stringify(persisted.current)) {
        if (conflictNotice.current !== result.revision) {
          conflictNotice.current = result.revision;
          report("服务器配置已更新，当前输入已保留，请刷新后合并配置。", true);
        }
        return;
      }
      await enqueue(async () => {
        if (localEdits.current || uncertain.current) return;
        if (await refreshSettings()) await refreshRelay();
      });
    } catch (failure) { report(failure, true); }
    finally { checking.current = false; }
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
      const result = await call<LocalProxyLogsResult>("read_local_proxy_logs", { request: { limit: 1000 } });
      if (alive.current) { setProxyLogs(result); setProxyError(null); }
      return result;
    } catch (failure) { if (alive.current) setProxyError(String(failure)); throw failure; }
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
  async function save(value: BackendSettings, silent = false) {
    const editVersion = generation.current;
    const base = persisted.current;
    const patch = Object.fromEntries(Object.entries(value).filter(([key, entry]) =>
      JSON.stringify(entry) !== JSON.stringify(base[key as keyof BackendSettings])));
    return enqueue(async () => {
      const next = { ...persisted.current, ...patch };
      const result = await call<SettingsResult>("save_settings", { settings: next }, true);
      if (alive.current) {
        acceptSettings(result, editVersion);
        if (!silent) report(result.message);
      }
    });
  }
  async function refresh() {
    if (initialized && (localEdits.current || JSON.stringify(form) !== JSON.stringify(persisted.current))
      && !window.confirm("刷新会重新读取服务器配置，并放弃当前页面尚未保存的配置，继续？")) return;
    generation.current++;
    if (!await enqueue(refreshSettings)) return;
    setReloadEpoch(value => value + 1);
    if (page === "proxy") await Promise.all([refreshRelay(), refreshConflicts(), refreshStatus(), refreshLogs()]);
    if (page === "sessions") await refreshSessions();
    report("已读取服务器当前配置。");
  }
  const actions: Actions = {
    saveSettingsValue: async (value, silent, requireSuccess) => {
      try { await save(value, silent); }
      catch (failure) { report(failure, true); if (requireSuccess) throw failure; }
    },
    refreshRelayFiles: () => run(refreshRelay),
    refreshEnvConflicts: () => run(refreshConflicts),
    removeEnvConflicts: async names => {
      const result = await run(() => call<CommandResult<unknown>>("remove_env_conflicts", { request: { names } }));
      if (result) { report(result.message); await run(refreshConflicts); }
    },
    refreshCcsProviders: () => run(refreshProviders),
    importCcsProviders: async () => {
      const editVersion = generation.current;
      await run(() => enqueue(async () => {
        const result = await call<SettingsResult>("import_ccs_providers", {}, true);
        acceptSettings(result, editVersion); report(result.message);
      }));
    },
    refreshLocalSessions: () => run(refreshSessions),
    deleteLocalSession: async thread => {
      if (!window.confirm(`永久删除“${thread.title || "未命名会话"}”及其会话记录？项目文件不受影响，此操作不可恢复。`)) return;
      await run(async () => {
        await call<CommandResult<unknown>>("delete_local_session", {
          request: { sessionId: thread.id.replace(/^local:/, ""), title: thread.title, dbPath: thread.dbPath }
        }, true);
        await refreshSessions();
      });
    },
    deleteLocalSessionsBatch: async threads => {
      if (!window.confirm(`永久删除筛选出的 ${threads.length} 个会话及其记录？项目文件不受影响，此操作不可恢复。`)) return;
      let deleted = 0;
      try {
        for (const thread of threads) {
          await call<CommandResult<unknown>>("delete_local_session", {
            request: { sessionId: thread.id.replace(/^local:/, ""), title: thread.title, dbPath: thread.dbPath }
          }, true);
          deleted++;
        }
        report(`已删除 ${deleted} 个会话。`);
      } catch (failure) { report(`已删除 ${deleted} 个会话；${failure instanceof Error ? failure.message : failure}`, true); }
      await run(refreshSessions);
    },
    saveRelayAuthFile: async (contents, silent) => {
      const result = await run(() => enqueue(() => call<RelayFilesResult>("save_relay_file", { request: { kind: "auth", contents } }, true)));
      if (result) { setRelayFiles(result); if (!silent) report(result.message); }
    },
    testRelayProfile: (profile, model, speedTest) => run(() => call<RelayProfileTestResult>("test_relay_profile", { profile, model, speedTest })),
    fetchRelayProfileModels: async profile => (await run(() => call<RelayProfileModelsResult>("fetch_relay_profile_models", { profile })))?.models ?? null,
    probeRelayProfileResponsesWebsocket: async profile =>
      (await run(() => enqueue(() => call<ResponsesWebsocketProbeResult>("probe_relay_profile_responses_websocket", { profile }, true))))?.capability ?? null,
    switchRelayProfile: async (value, previousActiveRelayId) => {
      const editVersion = generation.current;
      setSwitching(true);
      try {
        await enqueue(async () => {
          const result = await call<RelaySwitchResult>("switch_relay_profile",
            { request: { settings: value, previousActiveRelayId: previousActiveRelayId || "" } }, true);
          acceptSettings(result, editVersion); report(result.message);
          await refreshRelay(); await refreshStatus();
        });
      } catch (failure) { report(failure, true); }
      finally { if (alive.current) setSwitching(false); }
    },
    relaySwitching: switching,
    refreshLocalProxyStatus: () => run(refreshStatus),
    refreshLocalProxyLogs: () => run(refreshLogs),
    loadLocalProxyLogDetail: async id => {
      const version = ++detailGeneration.current;
      setSelectedLog(id); setLoadingLog(id); setDetail(null);
      const result = await run(() => call<LocalProxyLogDetailResult>("read_local_proxy_log_detail", { request: { id } }));
      if (alive.current && detailGeneration.current === version) { setDetail(result); setLoadingLog(null); }
    },
    closeLocalProxyLogDetail: () => { detailGeneration.current++; setSelectedLog(null); setDetail(null); setLoadingLog(null); },
    clearLocalProxyLogs: async () => {
      if (!window.confirm("清空全部代理请求日志及完整请求、返回内容？")) return;
      const result = await run(() => call<LocalProxyLogsResult>("clear_local_proxy_logs"));
      if (result) { setProxyLogs(result); actions.closeLocalProxyLogDetail(); report(result.message); }
    },
    showMessage: async (title, value, status) => report(`${title}：${value}`, status === "failed"),
    copyLocalProxyRequest: async text => { if (text) await run(() => copyToClipboard(text)); },
    copyLocalProxyResponse: async text => { if (text) await run(() => copyToClipboard(text)); },
    copyLocalProxyAddress: async text => { await run(async () => { await copyToClipboard(text); report("代理地址已复制。"); }); }
  };
  useEffect(() => {
    alive.current = true;
    void run(async () => {
      if (standaloneInvoke()) await standaloneInvoke()!("ensure_proxy_service");
      await refreshSettings();
      await Promise.all([refreshRelay(), refreshConflicts(), refreshStatus(), refreshLogs()]);
    });
    const settingsTimer = window.setInterval(() => void checkConfiguration(), 15000);
    return () => {
      clearInterval(settingsTimer); alive.current = false; requests.current.abort();
    };
  }, []);
  useEffect(() => {
    if (!initialized) return;
    if (page === "sessions") void run(refreshSessions);
    if (page !== "proxy") return;
    let polling = false;
    const poll = async () => {
      if (!visible.current || document.hidden || polling) return;
      polling = true;
      try { await Promise.all([refreshStatus(), refreshLogs()]); } catch (failure) { report(failure, true); }
      finally { polling = false; }
    };
    const timer = window.setInterval(() => void poll(), 10000);
    return () => window.clearInterval(timer);
  }, [page, initialized]);
  return <div className="management-app" onInputCapture={noteEdit} onChangeCapture={noteEdit}
    onClickCapture={event => {
      if (event.target instanceof Element && event.target.closest('[role="option"],[role="menuitemradio"],[role="combobox"]')) noteEdit();
    }}>
    <header className="management-header">
      <div className="management-page-width">
        <div><h1>{titles[page]}</h1><p>独立管理器直接管理本机代理与会话数据</p></div>
        <Button onClick={() => void run(refresh)} variant="outline" aria-label="刷新当前页面"><RefreshCw className="h-4 w-4" />刷新</Button>
      </div>
      {standaloneManager ? <nav className="manager-nav" aria-label="管理器页面">
        {(Object.keys(titles) as Page[]).map(item => (
          <button key={item} className={page === item ? "selected" : ""} aria-current={page === item ? "page" : undefined}
            onClick={() => setPage(item)} type="button">{titles[item]}</button>
        ))}
      </nav> : null}
    </header>
    {message ? <div className={`management-notice ${error ? "error" : ""}`} role={error ? "alert" : "status"}>
      <div className="management-page-width">{message}</div>
    </div> : null}
    <main className="management-content" key={reloadEpoch}>
      <div className="management-page-width">
        {!initialized ? <p>{error ? "配置未读取成功，请刷新后重试。" : "正在读取服务器配置…"}</p> : <>
          {page === "proxy" ? <>
            <RelayScreen settings={settings} relayFiles={relayFiles} envConflicts={conflicts} ccsProviders={providers}
              form={form} onFormChange={edit} actions={actions} />
            <LocalProxyScreen status={proxyStatus} logs={proxyLogs} logsError={proxyError} detail={detail}
              selectedId={selectedLog} loadingDetailId={loadingLog} form={form} actions={actions} />
          </> : null}
          {page === "sessions" ? <SessionsScreen sessions={sessions} actions={actions} /> : null}
        </>}
      </div>
    </main>
    <TooltipLayer theme={theme} />
  </div>;
}
