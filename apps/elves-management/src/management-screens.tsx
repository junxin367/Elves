// 原管理器的供应商、代理日志与会话列表组件迁入 Elves / Harness 工作区，不再提供独立管理器界面。
import { closestCenter, DndContext, KeyboardSensor, PointerSensor, useSensor, useSensors, type DragEndEvent } from "@dnd-kit/core";
import { SortableContext, sortableKeyboardCoordinates, useSortable, verticalListSortingStrategy } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { ArrowLeft, ArrowDownWideNarrow, ArrowUpNarrowWide, Calendar, Check, CheckCircle2, ChevronDown, Cloud, Copy, Download, Edit3, Eye, EyeOff, GripVertical, MessageCircle, MessageSquareText, Network, Plus, RefreshCw, Save, Search, ShieldCheck, ShieldAlert, Shrink, Sparkles, TestTube, Trash2, X, Zap } from "lucide-react";
import { ProviderPresetSelector } from "@/components/ProviderPresetSelector";
import type { PresetPatch } from "@/components/ProviderPresetSelector";
import type { RelayMode, RelayModelMapping, RelayProtocol } from "@/relay-types";
import { readProxyRequestCacheTtl, readProxyTokenUsage } from "@/proxy-token-usage";
import { formatProxyBody, readProxyResponseBody } from "@/proxy-response-body";
import { JsonViewer } from "@/components/JsonViewer";
import { defaultRemoteAccessSettings, type RemoteAccessSettings } from "@/remote-access-types";
import { defaultModelContextWindow, type ModelFamily } from "@/modelContextWindows";
import { useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from "react";
import { createPortal } from "react-dom";
import { Badge as UiBadge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";

export type Status = "ok" | "failed" | "not_implemented" | "not_checked" | string;

export type CommandResult<T> = T & {
  status: Status;
  message: string;
};

export type LayeredCompactionModelUsage = "cacheMiss" | "default";

export type BackendSettings = {
  codexHomePath: string;
  providerSyncEnabled: boolean;
  providerSyncSavedProviders: string[];
  providerSyncManualProviders: string[];
  providerSyncLastSelectedProvider: string;
  relayProfilesEnabled: boolean;
  lanProxyEnabled: boolean;
  remoteAccess: RemoteAccessSettings;
  gptReasoningContinuation: boolean;
  gptReasoningContinuationMaxRounds: number;
  layeredCompactionEnabled: boolean;
  layeredCompactionRetainRecentRoundEnabled: boolean;
  layeredCompactionRetainTokens: number;
  layeredCompactionPromptOverride: string;
  layeredCompactionModelOverrideEnabled: boolean;
  layeredCompactionModelUsage: LayeredCompactionModelUsage;
  layeredCompactionModels: Record<ModelFamily, string>;
  /** 旧版单模型字段，仅用于读取迁移。 */
  layeredCompactionModel: string;
  relayBaseUrl: string;
  relayApiKey: string;
  relayProfiles: RelayProfile[];
  aggregateRelayProfiles: AggregateRelayProfile[];
  activeAggregateRelayId: string;
  relayCommonConfigContents: string;
  relayContextConfigContents: string;
  activeRelayId: string;
  relayTestModel: string;
  cliWrapperEnabled: boolean;
  cliWrapperBaseUrl: string;
  cliWrapperApiKey: string;
  cliWrapperApiKeyEnv: string;
};


export type ResponsesWebsocketCapabilityState = "unknown" | "supported" | "unsupported";

export type ResponsesWebsocketCapability = {
  state: ResponsesWebsocketCapabilityState;
  endpoint: string;
  checkedAtMs: number | null;
  message: string;
};

export type RelayProfile = {
  id: string;
  name: string;
  model: string;
  baseUrl: string;
  upstreamBaseUrl: string;
  apiKey: string;
  protocol: RelayProtocol;
  localProxyEnabled: boolean;
  relayMode: RelayMode;
  officialMixApiKey: boolean;
  configContents: string;
  authContents: string;
  useCommonConfig: boolean;
  contextSelection: RelayContextSelection;
  contextSelectionInitialized: boolean;
  contextWindow: string;
  autoCompactLimit: string;
  modelMappings: RelayModelMapping[];
  modelList: string;
  responsesModelList: string;
  chatCompletionsModelList: string;
  anthropicModelList: string;
  responsesWebsocket: ResponsesWebsocketCapability;
  responsesWebsocketEnabled: boolean;
  userAgent: string;
  systemPromptOverride: string;
  aggregate?: RelayAggregateConfig | null;
};

export type RelayAggregateStrategy = "failover" | "conversationRoundRobin" | "requestRoundRobin" | "weightedRoundRobin";

export type RelayAggregateMember = {
  profileId: string;
  weight: number;
};

export type RelayAggregateConfig = {
  strategy: RelayAggregateStrategy;
  members: RelayAggregateMember[];
};

export type AggregateRelayMember = {
  relayId: string;
  weight: number;
};

export type AggregateRelayProfile = {
  id: string;
  name: string;
  strategy: RelayAggregateStrategy;
  members: AggregateRelayMember[];
};

export type ModelChoiceChangeSource = "input" | "select";

export type RelayContextSelection = {
  mcpServers: string[];
  skills: string[];
  plugins: string[];
};

export type ContextKind = "mcp" | "skill" | "plugin";

export type CodexContextEntry = {
  id: string;
  kind: ContextKind;
  title: string;
  summary: string;
  tomlBody: string;
  enabled: boolean;
};

export type CodexContextEntries = {
  mcpServers: CodexContextEntry[];
  skills: CodexContextEntry[];
  plugins: CodexContextEntry[];
};

export const PROTOCOL_PROXY_BASE_URL = "http://127.0.0.1:45221/v1";

export const CHAT_UPSTREAM_BASE_URL_KEY = "codex_elves_chat_base_url";

export const REMOTE_COMPACTION_V2_PROVIDER_NAME = "OpenAI";

export const MULTI_AGENT_V2_FEATURE_KEY = "multi_agent_v2";

export const MULTI_AGENT_V2_SECTION = `features.${MULTI_AGENT_V2_FEATURE_KEY}`;

export const MULTI_AGENT_V2_ENABLED_KEY = "enabled";

export const LEGACY_MULTI_AGENT_V2_MAX_THREADS_KEY = "max_concurrent_threads_per_session";

export const AGENTS_SECTION = "agents";

export const AGENTS_MAX_CONCURRENT_THREADS_KEY = "max_concurrent_threads_per_session";

export const DEFAULT_SUBAGENT_COUNT = 6;

export const emptyContextSelection = (): RelayContextSelection => ({
  mcpServers: [],
  skills: [],
  plugins: [],
});

export type UserScriptInventory = {
  enabled?: boolean;
  scripts?: Array<{
    key: string;
    name: string;
    source: string;
    enabled: boolean;
    status: string;
    error: string;
    market_id?: string;
    version?: string;
    installed?: boolean;
    source_url?: string;
    homepage?: string;
  }>;
};

export type SettingsResult = CommandResult<{
  settings: BackendSettings;
  settings_path: string;
  codex_home: string;
  user_scripts: UserScriptInventory;
  layered_compaction_default_prompt: string;
}>;

export type RelayResult = CommandResult<{
  authenticated: boolean;
  authSource: string;
  accountLabel: string | null;
  configPath: string;
  configured: boolean;
  requiresOpenaiAuth: boolean;
  hasBearerToken: boolean;
  backupPath: string | null;
}>;

export type RelayPayload = Omit<RelayResult, "status" | "message">;

export type RelayFilesResult = CommandResult<{
  configPath: string;
  authPath: string;
  configContents: string;
  authContents: string;
}>;

export type LocalSession = {
  id: string;
  title: string;
  cwd: string;
  modelProvider: string;
  archived: boolean;
  updatedAtMs: number | null;
  rolloutPath: string;
  dbPath: string;
};

export type LocalSessionsResult = CommandResult<{
  dbPath: string;
  dbPaths: string[];
  projectlessWorkspaceRoot: string;
  sessions: LocalSession[];
}>;

export type RelaySwitchResult = CommandResult<{
  settings: BackendSettings;
  settingsPath: string;
  codexHome: string;
  user_scripts: unknown;
  relay: RelayPayload;
}>;

export type RelayProfileTestResult = CommandResult<{
  httpStatus: number;
  endpoint: string;
  responsePreview: string;
  firstTokenMs?: number | null;
  outputTokens?: number | null;
  durationMs?: number | null;
  requestBody?: string | null;
  responseBody?: string | null;
}>;

export type RelayProfileSpeedTestItem = {
  model: string;
  elapsedMs: number;
  firstTokenMs: number | null;
  outputTokens: number | null;
  status: "ok" | "failed";
  httpStatus: number | null;
  message: string;
  endpoint?: string | null;
  requestBody?: string | null;
  responseBody?: string | null;
};

export type RelayProfileModelsResult = CommandResult<{
  models: string[];
  endpoint: string;
}>;

export type ResponsesWebsocketProbeResult = CommandResult<{
  profileId: string;
  capability: ResponsesWebsocketCapability;
}>;

export type CcsProviderImport = {
  sourceId: string;
  name: string;
  baseUrl: string;
  apiKey: string;
  protocol: RelayProtocol;
  configContents: string;
  authContents: string;
};

export type CcsProvidersResult = CommandResult<{
  dbPath: string;
  providers: CcsProviderImport[];
}>;

export type EnvConflict = {
  name: string;
  source: "process" | "user" | string;
  valuePresent: boolean;
};

export type EnvConflictsResult = CommandResult<{
  conflicts: EnvConflict[];
}>;

export type LocalProxyStatusResult = CommandResult<{
  enabled: boolean;
  listening: boolean;
  host: string;
  port: number;
  lanListening?: boolean;
  lanAddresses?: string[];
  activeRelayId: string;
  activeRelayName: string;
  activeRelayMode: RelayMode | string;
  aggregateRelayName?: string | null;
  upstreamBaseUrl: string;
  logPath: string;
  latestRequestAtMs?: number | null;
  recentCount: number;
}>;

export type LocalProxyRequestOutcome = "succeeded" | "failed" | "incomplete";

export type LocalProxyRequestAttempt = {
  attempt: number;
  model: string;
  transport?: "http" | "ws" | null;
  upstreamResponseModel?: string | null;
  statusCode?: number | null;
  outcome?: LocalProxyRequestOutcome | null;
  errorCode?: string | null;
  error?: string | null;
};

export type LocalProxyLogEntry = {
  id: string;
  outcome?: LocalProxyRequestOutcome | null;
  upstreamStatusCode?: number | null;
  errorCode?: string | null;
  upstreamAttempts?: LocalProxyRequestAttempt[] | null;
  threadId?: string | null;
  turnId?: string | null;
  state?: "pending" | "completed" | null;
  transport?: "http" | "ws" | null;
  timestampMs: number;
  method: string;
  path: string;
  remoteAddr?: string | null;
  model?: string | null;
  upstreamRequestModel?: string | null;
  upstreamResponseModel?: string | null;
  independentCompactionModel?: string | null;
  independentCompactionUsage?: LayeredCompactionModelUsage | null;
  /** 旧版缓存失效压缩日志字段，仅用于兼容读取。 */
  cacheMissCompactionModel?: string | null;
  reasoningTokens?: number | null;
  outputTokens?: number | null;
  reasoningEffort?: string | null;
  reasoningSource?: string | null;
  continueThinkingTriggered?: boolean | null;
  continueThinkingRounds?: number | null;
  remoteCompactionTriggered?: boolean | null;
  compactionRequested?: boolean | null;
  layeredCompactionTriggered?: boolean | null;
  layeredCompactionRetainTokens?: number | null;
  layeredCompactionRetainedItems?: number | null;
  layeredCompactionRetainedChars?: number | null;
  serviceTier?: string | null;
  relayId?: string | null;
  relayName?: string | null;
  endpoint?: string | null;
  responseProtocol?: string | null;
  statusCode?: number | null;
  firstTokenMs?: number | null;
  durationMs?: number | null;
  stream: boolean;
  requestBytes: number;
  responseBytes?: number | null;
  responseCapturedBytes?: number | null;
  responseTruncated: boolean;
  error?: string | null;
};

export type LocalProxyLogDetail = LocalProxyLogEntry & {
  requestBody: string;
  responseBody: string;
  continueThinkingRequestBody?: string | null;
  continueThinkingBeforeResponseBody?: string | null;
  continueThinkingAfterResponseBody?: string | null;
  layeredCompactionBeforeResponseBody?: string | null;
};

export type LocalProxyLogsResult = CommandResult<{
  path: string;
  entries: LocalProxyLogEntry[];
  limit: number;
}>;

export type LocalProxyLogDetailResult = CommandResult<{
  path: string;
  entry: LocalProxyLogDetail | null;
}>;

export const LOCAL_PROXY_LOG_PAGE_SIZE = 6;

export const defaultSettings: BackendSettings = {
  codexHomePath: "",
  providerSyncEnabled: false,
  providerSyncSavedProviders: [],
  providerSyncManualProviders: [],
  providerSyncLastSelectedProvider: "",
  relayProfilesEnabled: true,
  lanProxyEnabled: false,
  remoteAccess: { ...defaultRemoteAccessSettings },
  gptReasoningContinuation: false,
  gptReasoningContinuationMaxRounds: 3,
  layeredCompactionEnabled: false,
  layeredCompactionRetainRecentRoundEnabled: false,
  layeredCompactionRetainTokens: 20000,
  layeredCompactionPromptOverride: "",
  layeredCompactionModelOverrideEnabled: false,
  layeredCompactionModelUsage: "cacheMiss",
  layeredCompactionModels: {
    gpt: "",
    claude: "",
    other: "",
  },
  layeredCompactionModel: "",
  relayBaseUrl: "",
  relayApiKey: "",
  relayProfiles: [
    {
      id: "default",
      name: "默认中转",
      model: "",
      baseUrl: "",
      upstreamBaseUrl: "",
      apiKey: "",
      protocol: "responses",
      localProxyEnabled: false,
      relayMode: "official",
      officialMixApiKey: false,
      configContents: "",
      authContents: "",
      useCommonConfig: true,
      contextSelection: emptyContextSelection(),
      contextSelectionInitialized: true,
      contextWindow: "",
      autoCompactLimit: "",
      modelMappings: [],
      modelList: "",
      responsesModelList: "",
      chatCompletionsModelList: "",
      anthropicModelList: "",
      responsesWebsocket: emptyResponsesWebsocketCapability(),
      responsesWebsocketEnabled: true,
      userAgent: "",
      systemPromptOverride: "",
    },
  ],
  relayCommonConfigContents: "",
  relayContextConfigContents: "",
  activeRelayId: "default",
  aggregateRelayProfiles: [],
  activeAggregateRelayId: "",
  relayTestModel: "gpt-5.4-mini",
  cliWrapperEnabled: false,
  cliWrapperBaseUrl: "",
  cliWrapperApiKey: "",
  cliWrapperApiKeyEnv: "CUSTOM_OPENAI_API_KEY",
};

export function isBrowserPreview(): boolean { return new URLSearchParams(window.location.search).get("preview") === "1"; }

export type Actions = {

  saveSettingsValue: (settings: BackendSettings, silent?: boolean, requireSuccess?: boolean) => Promise<void>;

  refreshRelayFiles: () => Promise<RelayFilesResult | null>;

  refreshEnvConflicts: (silent?: boolean) => Promise<EnvConflictsResult | null>;

  removeEnvConflicts: (names: string[]) => Promise<void>;

  refreshCcsProviders: (silent?: boolean) => Promise<CcsProvidersResult | null>;

  importCcsProviders: () => Promise<void>;

  refreshLocalSessions: () => Promise<LocalSessionsResult | null>;

  deleteLocalSession: (session: LocalSession) => Promise<void>;

  deleteLocalSessionsBatch: (sessions: LocalSession[]) => Promise<void>;

  saveRelayAuthFile: (contents: string, silent?: boolean) => Promise<void>;

  testRelayProfile: (profile: RelayProfile, model?: string, speedTest?: boolean) => Promise<RelayProfileTestResult | null>;

  fetchRelayProfileModels: (profile: RelayProfile, silent?: boolean) => Promise<string[] | null>;

  probeRelayProfileResponsesWebsocket: (profile: RelayProfile) => Promise<ResponsesWebsocketCapability | null>;

  switchRelayProfile: (settings: BackendSettings, previousActiveRelayId?: string) => Promise<void>;

  relaySwitching: boolean;

  refreshLocalProxyStatus: () => Promise<LocalProxyStatusResult | null>;

  refreshLocalProxyLogs: () => Promise<LocalProxyLogsResult | null>;

  loadLocalProxyLogDetail: (id: string) => Promise<void>;

  closeLocalProxyLogDetail: () => void;

  clearLocalProxyLogs: () => Promise<void>;

  showMessage: (title: string, message: string, status?: Status) => Promise<void>;

  copyLocalProxyRequest: (text?: string) => Promise<void>;

  copyLocalProxyResponse: (text?: string) => Promise<void>;

  copyLocalProxyAddress: (text: string) => Promise<void>;
};

export function LocalProxyScreen({
  status,
  logs,
  logsError,
  detail,
  selectedId,
  loadingDetailId,
  form,
  actions,
}: {
  status: LocalProxyStatusResult | null;
  logs: LocalProxyLogsResult | null;
  logsError: string | null;
  detail: LocalProxyLogDetailResult | null;
  selectedId: string | null;
  loadingDetailId: string | null;
  form: BackendSettings;
  actions: Actions;
}) {
  const selectedEntry = detail?.entry && detail.entry.id === selectedId ? detail.entry : null;
  const entries = logs?.entries ?? [];
  const [page, setPage] = useState(1);
  const [modelFilter, setModelFilter] = useState("");
  const [logFilter, setLogFilter] = useState<"high" | "continuation" | "">("");
  const modelOptions = useMemo(
    () =>
      Array.from(new Set(entries.map(proxyLogDisplayModel).filter((model): model is string => Boolean(model))))
        .sort((left, right) => left.localeCompare(right)),
    [entries],
  );
  const modelFilteredEntries = useMemo(
    () =>
      entries.filter((entry) => {
        if (modelFilter && proxyLogDisplayModel(entry) !== modelFilter) return false;
        return true;
      }),
    [entries, modelFilter],
  );
  const requestRatio = useMemo(() => calculateRequestRatio(modelFilteredEntries), [modelFilteredEntries]);
  const modelSpeeds = useMemo(() => calculateProxyModelSpeeds(entries), [entries]);
  const filteredEntries = useMemo(
    () =>
      modelFilteredEntries.filter((entry) => {
        if (logFilter === "high" && classifyHighReasoningRequest(entry) !== "high") return false;
        if (logFilter === "continuation" && !isContinueThinkingEntry(entry)) return false;
        return true;
      }),
    [modelFilteredEntries, logFilter],
  );
  const totalPages = Math.max(1, Math.ceil(filteredEntries.length / LOCAL_PROXY_LOG_PAGE_SIZE));
  const lanListening = status?.listening === true && status.lanListening === true;
  const lanAddress = lanListening ? (status?.lanAddresses?.[0]?.trim() ?? "") : "";
  const lanProxyBaseUrl = lanAddress && status
    ? `http://${lanAddress}:${status.port}/v1`
    : "";
  const lanPendingText = !status
    ? "正在读取状态"
    : !status.listening
      ? "启动后生效"
      : !status.lanListening
        ? "重启后生效"
        : "未检测到局域网 IPv4";
  const visibleEntries = useMemo(
    () => filteredEntries.slice((page - 1) * LOCAL_PROXY_LOG_PAGE_SIZE, page * LOCAL_PROXY_LOG_PAGE_SIZE),
    [filteredEntries, page],
  );

  useEffect(() => {
    setPage(1);
  }, [modelFilter, logFilter]);

  useEffect(() => {
    setPage((current) => Math.min(Math.max(current, 1), totalPages));
  }, [totalPages]);

  useEffect(() => {
    if (modelFilter && !modelOptions.includes(modelFilter)) setModelFilter("");
  }, [modelFilter, modelOptions]);

  return (
    <>
      <Panel>
        <CardHead
          title="本地代理状态"
          detail="当前运行状态与最近代理活动"
          actions={
            <>
              <label className="proxy-inline-toggle">
                <input
                  checked={form.lanProxyEnabled}
                  onChange={(event) =>
                    void actions.saveSettingsValue(
                      { ...form, lanProxyEnabled: event.currentTarget.checked },
                      false,
                    )
                  }
                  type="checkbox"
                />
                <span>局域网代理</span>
              </label>
              <div className="proxy-continue-thinking-control">
                <label
                  className="proxy-inline-toggle"
                  data-tooltip="检测到 GPT 推理被中途截断时，自动让模型继续思考，减少因推理不足导致的错误"
                >
                  <input
                    checked={form.gptReasoningContinuation}
                    onChange={(event) =>
                      void actions.saveSettingsValue(
                        { ...form, gptReasoningContinuation: event.currentTarget.checked },
                        false,
                      )
                    }
                    type="checkbox"
                  />
                  <span>GPT 推理续接</span>
                </label>
                <span className="proxy-continue-thinking-limit-label">最大次数:</span>
                <span className="proxy-continue-thinking-limit-wrap" data-tooltip="GPT 推理续接最大次数，范围 1-9">
                  <Input
                    aria-label="GPT 推理续接最大次数"
                    className="proxy-continue-thinking-limit"
                    inputMode="numeric"
                    maxLength={1}
                    onChange={(event) => {
                      const digit = event.currentTarget.value.replace(/[^1-9]/g, "").slice(0, 1);
                      if (!digit) return;
                      void actions.saveSettingsValue(
                        { ...form, gptReasoningContinuationMaxRounds: Number(digit) },
                        true,
                      );
                    }}
                    pattern="[1-9]"
                    value={String(form.gptReasoningContinuationMaxRounds)}
                  />
                </span>
              </div>
              <Button
                onClick={() => {
                  void Promise.all([
                    actions.refreshLocalProxyStatus(),
                    actions.refreshLocalProxyLogs(),
                  ]);
                }}
                size="sm"
              >
                <RefreshCw className="h-4 w-4" />
                刷新
              </Button>
            </>
          }
        />
        <CardContent>
          <div className="proxy-status-strip">
            <div className={`proxy-status-cell proxy-status-state ${localProxyState(status)}`}>
              <span className="proxy-status-dot" />
              <div className="proxy-status-state-content">
                <strong>{localProxyStateLabel(status)}</strong>
                <div className="proxy-status-address-list">
                  <div className="proxy-status-address-row">
                    <span className="proxy-status-address-label">本机</span>
                    <code className="proxy-status-address-value">
                      {status ? `${status.host}:${status.port}` : "尚未读取监听地址"}
                    </code>
                  </div>
                  {form.lanProxyEnabled ? (
                    lanProxyBaseUrl ? (
                      <div className="proxy-status-address-row lan">
                        <span className="proxy-status-address-label">局域网</span>
                        <code className="proxy-status-address-value">
                          {lanAddress}:{status?.port}
                        </code>
                        <button
                          aria-label="复制局域网代理地址"
                          className="proxy-status-address-copy"
                          data-tooltip={`复制 ${lanProxyBaseUrl}`}
                          onClick={() => void actions.copyLocalProxyAddress(lanProxyBaseUrl)}
                          type="button"
                        >
                          <Copy aria-hidden="true" />
                        </button>
                      </div>
                    ) : (
                      <div className="proxy-status-address-row lan pending">
                        <span className="proxy-status-address-label">局域网</span>
                        <span className="proxy-status-address-value">{lanPendingText}</span>
                      </div>
                    )
                  ) : null}
                </div>
              </div>
            </div>
            <div className="proxy-status-cell">
              <span>供应商</span>
              <strong>{status?.activeRelayName ?? "未读取"}</strong>
            </div>
            <div className="proxy-status-cell">
              <span>最近记录</span>
              <strong>{status?.latestRequestAtMs ? formatTime(status.latestRequestAtMs) : "暂无"}</strong>
            </div>
          </div>
        </CardContent>
      </Panel>
      <section className="proxy-speed-section" aria-labelledby="proxy-speed-title">
        <h3 id="proxy-speed-title">模型速率</h3>
        <div className="proxy-speed-grid" aria-label="本地请求模型速率">
          {modelSpeeds.map((speed, index) => (
            <div className="proxy-speed-model" key={speed?.model ?? `empty-${index}`}>
              {speed ? (
                <>
                  <strong className="proxy-speed-name" title={speed.model}>{speed.model}</strong>
                  <div className="proxy-speed-values">
                    <div>
                      <span data-tooltip="最近一条有输出 token 和耗时的成功请求">最近有效请求</span>
                      <strong>{formatProxyTokenRate(speed.latestRate)} <small>tok/s</small></strong>
                    </div>
                    <div>
                      <span data-tooltip="当前日志范围内的输出 token 总数除以请求总耗时">平均速率</span>
                      <strong>{formatProxyTokenRate(speed.averageRate)} <small>tok/s</small></strong>
                    </div>
                  </div>
                </>
              ) : (
                <span className="proxy-speed-placeholder">{index === 0 ? "暂无请求模型" : ""}</span>
              )}
            </div>
          ))}
        </div>
      </section>
      <Panel>
        <CardHead
          title={`请求日志（${filteredEntries.length}）`}
          detail={logs?.path ?? "默认记录完整请求体和返回体，列表只展示摘要"}
          actions={
            <div className="proxy-log-filters" aria-label="请求日志筛选">
              <SelectMenu
                ariaLabel="按模型筛选"
                className="proxy-log-filter-select"
                value={modelFilter}
                options={[{ value: "", label: "全部模型" }, ...modelOptions.map((model) => ({ value: model, label: model }))]}
                onChange={(next) => setModelFilter(next)}
              />
              <div
                aria-label="请求高推理和 GPT 续接比例"
                className="proxy-iq-ratio"
              >
                <button
                  aria-pressed={logFilter === "high"}
                  className={`proxy-iq-ratio-item high ${logFilter === "high" ? "active" : ""}`}
                  onClick={() => setLogFilter((current) => (current === "high" ? "" : "high"))}
                  type="button"
                >
                  高 <strong>{requestRatio.highPercent}%</strong>
                </button>
                <button
                  aria-pressed={logFilter === "continuation"}
                  className={`proxy-iq-ratio-item continuation ${logFilter === "continuation" ? "active" : ""}`}
                  onClick={() => setLogFilter((current) => (current === "continuation" ? "" : "continuation"))}
                  type="button"
                >
                  续接 <strong>{requestRatio.continuationPercent}%</strong>
                </button>
              </div>
            </div>
          }
        />
        <CardContent>
          {logsError ? (
            <div className="mb-3 rounded-md border border-destructive/30 bg-destructive/5 px-3 py-2 text-sm" role="alert">
              <p>{logs ? "刷新失败，当前显示上次成功读取的数据。" : "日志读取失败，请重试。"}</p>
              <p className="mt-1 break-words text-muted-foreground">{logsError}</p>
            </div>
          ) : null}
          <div className="proxy-log-table">
            {filteredEntries.length ? (
              <>
                <div className="proxy-log-row proxy-log-head">
                  <span>模型</span>
                  <span>时间</span>
                  <span>推理</span>
                  <span>状态</span>
                  <span className="proxy-log-latency">
                    <span>首字</span>
                    <span>|</span>
                    <span>耗时</span>
                  </span>
                  <span>操作</span>
                </div>
                {visibleEntries.map((entry) => (
                  <div className={`proxy-log-row ${selectedId === entry.id ? "active" : ""}`} key={entry.id}>
                    <span className="proxy-log-main">
                      <strong className="proxy-log-model-title">
                        <span>{proxyLogDisplayModel(entry) || "未知模型"}</span>
                        {upstreamResponseModelMismatch(entry) ? (
                          <span className="proxy-upstream-model-mismatch" title={`上游响应模型：${entry.upstreamResponseModel}`}>
                            （上游响应模型：{entry.upstreamResponseModel}）
                          </span>
                        ) : null}
                        {formatIndependentCompactionDescription(entry) ? (
                          <span className="proxy-compaction-model-description">
                            {formatIndependentCompactionDescription(entry)}
                          </span>
                        ) : null}
                        {entry.remoteCompactionTriggered ? (
                          <span
                            aria-label="Remote Compaction V2 请求"
                            className="proxy-remote-compaction-badge"
                            data-tooltip="Remote Compaction V2 请求"
                          >
                            <Cloud aria-hidden="true" className="proxy-remote-compaction-icon" />
                          </span>
                        ) : entry.compactionRequested || entry.layeredCompactionTriggered ? (
                          <span
                            aria-label={formatLayeredCompactionTitle(entry)}
                            className="proxy-context-compaction-badge"
                            data-tooltip={formatLayeredCompactionTitle(entry)}
                          >
                            <Shrink aria-hidden="true" className="proxy-context-compaction-icon" />
                          </span>
                        ) : null}
                      </strong>
                      <small>{formatProtocolRoute(entry)}</small>
                    </span>
                    <span className="proxy-log-time">{formatRequestLogListTime(entry.timestampMs)}</span>
                    <span className="proxy-log-reasoning">
                      <strong>{entry.reasoningEffort || "无推理"}</strong>
                      <small className="proxy-log-reasoning-tokens">
                        <span>
                          Reason Tok{" "}
                          <span className={`proxy-reasoning-token ${reasoningTokenTone(entry.reasoningTokens)}`}>
                            {formatReasoningTokens(entry.reasoningTokens)}
                          </span>
                        </span>
                        {entry.continueThinkingTriggered ? (
                          <span
                            aria-label={formatContinueThinkingTitle(entry)}
                            className="proxy-continue-thinking-badge"
                            data-tooltip={formatContinueThinkingTitle(entry)}
                          >
                            <Sparkles aria-hidden="true" className="proxy-continue-thinking-icon" />
                            {typeof entry.continueThinkingRounds === "number" && entry.continueThinkingRounds > 0 ? (
                              <span className="proxy-continue-thinking-count">
                                {entry.continueThinkingRounds}
                              </span>
                            ) : null}
                          </span>
                        ) : null}
                      </small>
                    </span>
                    <span className="proxy-log-status" title={entry.error || entry.errorCode || undefined}>
                      <strong className={localProxyStatusCodeClass(entry)}>
                        {formatLocalProxyStatusCode(entry)}
                      </strong>
                      {localProxyFailureReason(entry) ? <small>{localProxyFailureReason(entry)}</small> : null}
                    </span>
                    <span
                      className="proxy-log-duration proxy-log-latency"
                      data-tooltip={formatRequestLatencyTitle(entry)}
                    >
                      <span className={isSlowRequestDuration(entry.firstTokenMs) ? "slow" : undefined}>
                        {formatOptionalRequestDurationMs(entry.firstTokenMs)}
                      </span>
                      <span>|</span>
                      <span className={isSlowRequestDuration(entry.durationMs) ? "slow" : undefined}>
                        {formatOptionalRequestDurationMs(entry.durationMs)}
                      </span>
                    </span>
                    <Button
                      size="sm"
                      variant={selectedId === entry.id ? "secondary" : "outline"}
                      disabled={loadingDetailId === entry.id}
                      onClick={() => void actions.loadLocalProxyLogDetail(entry.id)}
                    >
                      {loadingDetailId === entry.id ? (
                        <RefreshCw className="h-4 w-4 proxy-log-view-spinner" />
                      ) : (
                        "查看"
                      )}
                    </Button>
                  </div>
                ))}
              </>
            ) : (
              <div className="empty">
                {entries.length
                  ? "没有符合筛选条件的请求日志。"
                  : logsError
                    ? "日志暂时不可用，请刷新重试。"
                    : "暂无代理请求日志。启用本地代理后，经过此服务代理的请求会记录在这里。"}
              </div>
            )}
          </div>
          <div className="proxy-log-footer">
            <div className="proxy-log-pagination" aria-label="请求日志分页">
              <Button
                disabled={page <= 1}
                onClick={() => setPage(1)}
                size="sm"
                variant="outline"
              >
                首页
              </Button>
              <Button
                disabled={page <= 1}
                onClick={() => setPage((current) => Math.max(1, current - 1))}
                size="sm"
                variant="outline"
              >
                上一页
              </Button>
              <span>{`第 ${page} / ${totalPages} 页`}</span>
              <Button
                disabled={page >= totalPages}
                onClick={() => setPage((current) => Math.min(totalPages, current + 1))}
                size="sm"
                variant="outline"
              >
                下一页
              </Button>
            </div>
            <Toolbar>
              <Button onClick={() => void actions.refreshLocalProxyLogs()}>
                <RefreshCw className="h-4 w-4" />
                刷新日志
              </Button>
              <Button variant="outline" onClick={() => void actions.clearLocalProxyLogs()}>
                <Trash2 className="h-4 w-4" />
                清空日志
              </Button>
            </Toolbar>
          </div>
        </CardContent>
      </Panel>
      {selectedEntry ? (
        <LocalProxyLogDetailDialog
          entry={selectedEntry}
          onClose={actions.closeLocalProxyLogDetail}
          onCopyRequest={actions.copyLocalProxyRequest}
          onCopyResponse={actions.copyLocalProxyResponse}
        />
      ) : null}
    </>
  );
}

export function LocalProxyLogDetailDialog({
  entry,
  onClose,
  onCopyRequest,
  onCopyResponse,
}: {
  entry: LocalProxyLogDetail;
  onClose: () => void;
  onCopyRequest: (text?: string) => Promise<void>;
  onCopyResponse: (text?: string) => Promise<void>;
}) {
  const [requestView, setRequestView] = useState<"full" | "continue">("full");
  const [responseView, setResponseView] = useState<"full" | "before" | "after" | "precompaction">("full");
  const [responseFormat, setResponseFormat] = useState<"summary" | "raw">("summary");
  useEffect(() => { setResponseFormat("summary"); }, [entry.id, responseView]);
  const continueRequestBody = entry.continueThinkingRequestBody?.trim() || "";
  const continueBeforeBody = entry.continueThinkingBeforeResponseBody?.trim() || "";
  const continueAfterBody = entry.continueThinkingAfterResponseBody?.trim() || "";
  const hasContinueThinkingRequest = entry.continueThinkingTriggered && continueRequestBody;
  const hasContinueThinkingViews = entry.continueThinkingTriggered && (continueBeforeBody || continueAfterBody);
  const layeredCompactionBeforeBody = entry.layeredCompactionBeforeResponseBody?.trim() || "";
  const hasLayeredCompactionView = entry.layeredCompactionTriggered && layeredCompactionBeforeBody;
  const requestViewBody = requestView === "continue" ? continueRequestBody : entry.requestBody;
  const responseViewBody =
    responseView === "before"
      ? continueBeforeBody
      : responseView === "after"
        ? continueAfterBody
        : responseView === "precompaction"
          ? layeredCompactionBeforeBody
          : entry.responseBody;
  const formattedRequestViewBody = formatProxyBody(requestViewBody);
  const responseBodyView = useMemo(() => readProxyResponseBody(responseViewBody), [responseViewBody]);
  // 统计始终读取原始事件的格式化结果，切换汇总和原始视图不能改变 token 数。
  const formattedResponseViewBody = responseBodyView.formatted;
  const displayedResponseBody = responseBodyView.summary
    ? responseFormat === "raw" ? responseViewBody : responseBodyView.summary
    : formattedResponseViewBody;
  const tokenUsage = useMemo(
    () => readProxyTokenUsage(formattedResponseViewBody),
    [formattedResponseViewBody],
  );
  const requestCacheTtl = useMemo(
    () => readProxyRequestCacheTtl(formattedRequestViewBody),
    [formattedRequestViewBody],
  );
  const cacheTtl = tokenUsage.cacheTtl ?? requestCacheTtl;
  const cacheTtlText = cacheTtl
    ? { "5m": "5 分钟", "1h": "1 小时", mixed: "5 分钟 + 1 小时" }[cacheTtl]
    : "未知";
  const cacheTtlHint = tokenUsage.cacheTtl
    ? "根据当前返回内容中的缓存时长明细显示。"
    : requestCacheTtl
      ? "请求指定的缓存时长；上游未返回实际时长明细，是否生效以供应商为准。"
      : "请求未记录缓存时长，返回内容也没有时长明细，无法确认是 5 分钟还是 1 小时。";
  const tokenMetrics = [
    { label: "输入", value: tokenUsage.input, hint: "包含缓存命中和缓存写入的输入总量" },
    { label: "输出", value: tokenUsage.output, hint: "响应报告的输出总量，推理 Token 不重复相加" },
    { label: "总计", value: tokenUsage.total, hint: "优先使用响应总量；未提供时由输入与输出相加" },
    { label: "缓存命中", value: tokenUsage.cached, hint: "从缓存读取的输入 Token，已包含在输入总量中" },
    { label: "缓存写入", value: tokenUsage.cacheWrite, hint: "写入缓存的输入 Token，已包含在输入总量中" },
    { label: "推理", value: tokenUsage.reasoning, hint: "仅展示响应 usage 报告的推理用量，不以文本长度估算" },
  ];

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [onClose]);

  useEffect(() => {
    setRequestView("full");
    setResponseView("full");
  }, [entry.id]);

  return createPortal(
    <div className="modal-backdrop" role="dialog" aria-modal="true" aria-labelledby="proxy-log-detail-title" onClick={onClose}>
      <div className="modal-card proxy-log-detail-modal" onClick={(event) => event.stopPropagation()}>
        <div className="modal-head proxy-log-detail-head">
          <div className="proxy-detail-title-block">
            <div className="proxy-detail-title-row">
              <h2 id="proxy-log-detail-title">请求详情</h2>
              <span className={localProxyStatusCodeClass(entry)}>
                {formatLocalProxyStatusCode(entry)}
              </span>
              <span className="proxy-detail-endpoint" data-tooltip={entry.endpoint || entry.path}>
                {entry.endpoint || entry.path}
              </span>
            </div>
          </div>
          <Button onClick={onClose} size="icon" title="关闭请求详情" variant="ghost">
            <X className="h-4 w-4" />
          </Button>
        </div>
        <div className="proxy-detail-summary">
          <div className="proxy-detail-head">
            <div>
              <strong className="proxy-detail-model-title">
                {proxyLogDisplayModel(entry) || "未知模型"}
                {upstreamResponseModelMismatch(entry) ? (
                  <span className="proxy-upstream-model-mismatch">（上游响应模型：{entry.upstreamResponseModel}）</span>
                ) : null}
                {formatIndependentCompactionDescription(entry) ? (
                  <span className="proxy-compaction-model-description">
                    {formatIndependentCompactionDescription(entry)}
                  </span>
                ) : null}
              </strong>
              <span>{`${formatTime(entry.timestampMs)} · ${formatProtocolRoute(entry)}`}</span>
            </div>
          </div>
          <div className="proxy-detail-meta">
            <span>请求 {formatBytes(entry.requestBytes)}</span>
            <span>返回 {formatOptionalBytes(entry.responseBytes)}</span>
            <span>{entry.stream ? "流式" : "非流式"}</span>
            {isClaudeModel(entry.upstreamRequestModel || entry.upstreamResponseModel || proxyLogDisplayModel(entry)) ? (
              <span data-tooltip={cacheTtlHint}>
                缓存 {cacheTtlText}{!tokenUsage.cacheTtl && requestCacheTtl ? "（请求）" : ""}
              </span>
            ) : null}
            {entry.responseTruncated ? <span>返回内容已截断</span> : null}
          </div>
          <div className="proxy-detail-audit" aria-label="请求结果记录">
            <span>上游传输：{formatRequestTransport(entry)}</span>
            {formatRequestTransport(entry) === "WS" ? (
              <span>上游结果：{entry.state === "pending" ? "进行中" : formatRequestOutcome(localProxyOutcome(entry))}</span>
            ) : (
              <span>上游原始 HTTP 状态：{formatUpstreamStatusCode(entry)}</span>
            )}
            <span>本地代理结果码：{entry.statusCode ?? "未记录"}</span>
            <span>实际请求模型：{entry.upstreamRequestModel || "未记录"}</span>
            <span>上游响应模型：{entry.upstreamResponseModel || "未记录"}</span>
            <span>会话：{entry.threadId || "未记录"}</span>
            <span>轮次：{entry.turnId || "未记录"}</span>
            {localProxyFailureReason(entry) ? (
              <span className="proxy-detail-failure" title={entry.error || undefined}>
                原因：{localProxyFailureReason(entry)}{entry.errorCode ? `（${entry.errorCode}）` : ""}
              </span>
            ) : null}
          </div>
          <details className="proxy-detail-attempts">
            <summary>上游尝试（{entry.upstreamAttempts?.length ? `${entry.upstreamAttempts.length} 次` : "未记录"}）</summary>
            {entry.upstreamAttempts?.length ? entry.upstreamAttempts.map((attempt) => (
              <div className="proxy-detail-attempt" key={attempt.attempt}>
                <strong>第 {attempt.attempt} 次 · {formatRequestOutcome(attempt.outcome)}</strong>
                <span>请求模型：{attempt.model || "未记录"} · 响应模型：{attempt.upstreamResponseModel || "未记录"}</span>
                <span>{attempt.transport === "ws" ? "上游传输：WS" : `上游 HTTP 状态：${attempt.statusCode ?? "未记录"}`}</span>
                {attempt.error || attempt.errorCode ? (
                  <span className="proxy-detail-failure">
                    原因：{proxyFailureReason(attempt) || "未记录"}{attempt.errorCode ? `（${attempt.errorCode}）` : ""}
                  </span>
                ) : null}
              </div>
            )) : <p>旧日志或未发送上游的请求可能没有尝试记录，不据代理状态反推上游状态。</p>}
          </details>
          {entry.remoteCompactionTriggered ? (
            <div className="proxy-detail-meta proxy-layered-compaction-meta proxy-remote-compaction-meta">
              <span>
                <Cloud className="h-4 w-4" />
                Remote Compaction V2
              </span>
              {entry.layeredCompactionTriggered && typeof entry.layeredCompactionRetainedItems === "number" ? (
                <span>保留 {entry.layeredCompactionRetainedItems} 条原始记录</span>
              ) : null}
              {entry.layeredCompactionTriggered && typeof entry.layeredCompactionRetainedChars === "number" ? (
                <span>约 {Math.round(entry.layeredCompactionRetainedChars / 4).toLocaleString()} token</span>
              ) : null}
              {entry.layeredCompactionTriggered && typeof entry.layeredCompactionRetainTokens === "number" ? (
                <span>裁剪目标 {entry.layeredCompactionRetainTokens.toLocaleString()} token</span>
              ) : null}
            </div>
          ) : entry.compactionRequested || entry.layeredCompactionTriggered ? (
            <div className="proxy-detail-meta proxy-layered-compaction-meta">
              <span>
                <Shrink className="h-4 w-4" />
                已触发上下文压缩
              </span>
              {typeof entry.layeredCompactionRetainedItems === "number" ? (
                <span>保留 {entry.layeredCompactionRetainedItems} 条原始记录</span>
              ) : null}
              {typeof entry.layeredCompactionRetainedChars === "number" ? (
                <span>约 {Math.round(entry.layeredCompactionRetainedChars / 4).toLocaleString()} token</span>
              ) : null}
              {typeof entry.layeredCompactionRetainTokens === "number" ? (
                <span>裁剪目标 {entry.layeredCompactionRetainTokens.toLocaleString()} token</span>
              ) : null}
            </div>
          ) : null}
        </div>
        <section className="proxy-detail-usage" aria-label="Token 用量">
          <strong title="当前返回的 Token 用量；缓存计入输入，推理计入输出">Token</strong>
          {responseView !== "full" ? (
            <span className="proxy-detail-usage-scope">
              {responseView === "before" ? "续接前" : responseView === "after" ? "续接后" : "压缩前"}
            </span>
          ) : null}
          <dl className="proxy-detail-usage-list">
            {tokenMetrics.map(({ label, value, hint }) => (
              <div className="proxy-detail-usage-item" key={label} title={hint}>
                <dt>{label}</dt>
                <dd data-missing={value === null || undefined}>
                  {value === null ? "未提供" : value.toLocaleString("zh-CN")}
                </dd>
                {label === "缓存命中" && tokenUsage.cacheHitRate !== null ? (
                  <small title="缓存命中率：缓存读取 Token ÷ 输入 Token">
                    （{(tokenUsage.cacheHitRate * 100).toFixed(2)}%）
                  </small>
                ) : null}
              </div>
            ))}
          </dl>
          {entry.responseTruncated ? (
            <p className="proxy-detail-usage-note">返回内容已截断，用量可能不完整。</p>
          ) : null}
        </section>
        <div className="proxy-log-detail">
          <div className="proxy-detail-grid">
            <div className="proxy-detail-pane">
              <div className="proxy-detail-pane-head">
                <span>完整请求</span>
                {hasContinueThinkingRequest ? (
                  <span className="proxy-detail-response-tabs" aria-label="续接请求视图">
                    <Button
                      aria-pressed={requestView === "full"}
                      className={`proxy-detail-copy-button proxy-detail-view-button ${requestView === "full" ? "active" : ""}`}
                      onClick={() => setRequestView("full")}
                      size="sm"
                      title="查看续接前请求"
                      type="button"
                      variant="secondary"
                    >
                      续接前
                    </Button>
                    <Button
                      aria-pressed={requestView === "continue"}
                      className={`proxy-detail-copy-button proxy-detail-view-button ${requestView === "continue" ? "active" : ""}`}
                      onClick={() => setRequestView("continue")}
                      size="sm"
                      title="查看续接后请求"
                      type="button"
                      variant="secondary"
                    >
                      续接后
                    </Button>
                  </span>
                ) : null}
                <Button
                  aria-label="复制请求"
                  className="proxy-detail-copy-button"
                  size="sm"
                  title="复制请求"
                  variant="secondary"
                  onClick={() => void onCopyRequest(formattedRequestViewBody)}
                >
                  <Copy className="h-4 w-4" />
                  复制
                </Button>
              </div>
              <JsonViewer text={formattedRequestViewBody} label="请求 JSON 查看器" onCopy={onCopyRequest} />
            </div>
            <div className="proxy-detail-pane">
              <div className="proxy-detail-pane-head">
                <span>完整返回</span>
                {responseBodyView.summary ? (
                  <span className="proxy-detail-response-tabs" role="group" aria-label="返回展示方式">
                    <Button
                      aria-pressed={responseFormat === "summary"}
                      className={`proxy-detail-copy-button proxy-detail-view-button ${responseFormat === "summary" ? "active" : ""}`}
                      onClick={() => setResponseFormat("summary")}
                      size="sm"
                      variant="secondary"
                    >
                      汇总
                    </Button>
                    <Button
                      aria-pressed={responseFormat === "raw"}
                      className={`proxy-detail-copy-button proxy-detail-view-button ${responseFormat === "raw" ? "active" : ""}`}
                      onClick={() => setResponseFormat("raw")}
                      size="sm"
                      variant="secondary"
                    >
                      原始事件
                    </Button>
                  </span>
                ) : null}
                {hasContinueThinkingViews ? (
                  <span className="proxy-detail-response-tabs" aria-label="续接返回视图">
                    <Button
                      aria-pressed={responseView === "before"}
                      className={`proxy-detail-copy-button proxy-detail-view-button ${responseView === "before" ? "active" : ""}`}
                      disabled={!continueBeforeBody}
                      onClick={() => setResponseView("before")}
                      size="sm"
                      title={continueBeforeBody ? "查看续接前汇总返回" : "当前日志未保存续接前汇总返回"}
                      type="button"
                      variant="secondary"
                    >
                      续接前
                    </Button>
                    <Button
                      aria-pressed={responseView === "after"}
                      className={`proxy-detail-copy-button proxy-detail-view-button ${responseView === "after" ? "active" : ""}`}
                      disabled={!continueAfterBody}
                      onClick={() => setResponseView("after")}
                      size="sm"
                      title={continueAfterBody ? "查看续接后汇总返回" : "当前日志未保存续接后汇总返回"}
                      type="button"
                      variant="secondary"
                    >
                      续接后
                    </Button>
                  </span>
                ) : null}
                {hasLayeredCompactionView ? (
                  <span className="proxy-detail-response-tabs" aria-label="压缩返回视图">
                    <Button
                      aria-pressed={responseView === "full"}
                      className={`proxy-detail-copy-button proxy-detail-view-button ${responseView === "full" ? "active" : ""}`}
                      onClick={() => setResponseView("full")}
                      size="sm"
                      title="查看上下文压缩处理后的最终返回"
                      type="button"
                      variant="secondary"
                    >
                      压缩后
                    </Button>
                    <Button
                      aria-pressed={responseView === "precompaction"}
                      className={`proxy-detail-copy-button proxy-detail-view-button ${responseView === "precompaction" ? "active" : ""}`}
                      onClick={() => setResponseView("precompaction")}
                      size="sm"
                      title="查看上游返回的原始纯摘要（上下文压缩处理前）"
                      type="button"
                      variant="secondary"
                    >
                      压缩前（纯摘要）
                    </Button>
                  </span>
                ) : null}
                <Button
                  aria-label="复制返回"
                  className="proxy-detail-copy-button"
                  size="sm"
                  title="复制返回"
                  variant="secondary"
                  onClick={() => void onCopyResponse(displayedResponseBody)}
                >
                  <Copy className="h-4 w-4" />
                  复制
                </Button>
              </div>
              <JsonViewer text={displayedResponseBody} label="返回 JSON 查看器" onCopy={onCopyResponse} />
            </div>
          </div>
        </div>
      </div>
    </div>,
    document.body,
  );
}

export function RelayScreen({
  settings: _settings,
  relayFiles,
  envConflicts,
  ccsProviders,
  form,
  onFormChange,
  actions,
}: {
  settings: SettingsResult | null;
  relayFiles: RelayFilesResult | null;
  envConflicts: EnvConflictsResult | null;
  ccsProviders: CcsProvidersResult | null;
  form: BackendSettings;
  onFormChange: (value: BackendSettings) => void;
  actions: Actions;
}) {
  const normalized = normalizeSettings(form);
  const [detailProfileId, setDetailProfileId] = useState<string | null>(() => (isBrowserPreview() ? normalized.activeRelayId : null));
  const [newProfileDraft, setNewProfileDraft] = useState<RelayProfile | null>(null);
  const [thirdPartyImportOpen, setThirdPartyImportOpen] = useState(false);
  const [testProfileId, setTestProfileId] = useState<string | null>(null);
  const [speedTestProfileId, setSpeedTestProfileId] = useState<string | null>(null);
  const browserPreviewDetailInitializedRef = useRef(isBrowserPreview());
  const detailProfile = newProfileDraft || (detailProfileId
    ? normalized.relayProfiles.find((profile) => profile.id === detailProfileId) || null
    : null);
  const testProfile = testProfileId
    ? normalized.relayProfiles.find((profile) => profile.id === testProfileId) || null
    : null;
  const speedTestProfile = speedTestProfileId
    ? normalized.relayProfiles.find((profile) => profile.id === speedTestProfileId) || null
    : null;
  const isNewProfile = !!newProfileDraft;
  const saveRelaySettings = async (next: BackendSettings) => {
    onFormChange(next);
    await actions.saveSettingsValue(next, true);
    await actions.refreshRelayFiles();
  };
  const createNewAggregateProfile = () => {
    const draft = createAggregateRelayProfile(normalized);
    setDetailProfileId(null);
    setNewProfileDraft(draft);
    if (!normalizeAggregateConfig(draft.aggregate, aggregateMemberCandidates(normalized, draft.id)).members.length) {
      void actions.showMessage(
        "添加聚合供应商",
        "已打开聚合供应商详情；请先添加或完善至少 1 个普通 API 供应商的 Base URL / Key，再勾选为成员。",
        "failed",
      );
    }
  };
  const editRelayProfile = async (profileId: string) => {
    setNewProfileDraft(null);
    setDetailProfileId(
      normalized.relayProfiles.some((item) => item.id === profileId) ? profileId : null,
    );
  };
  useEffect(() => {
    if (!newProfileDraft && detailProfileId && !normalized.relayProfiles.some((profile) => profile.id === detailProfileId)) {
      setDetailProfileId(null);
    }
  }, [detailProfileId, newProfileDraft, normalized.relayProfiles]);
  useEffect(() => {
    if (isBrowserPreview() && !browserPreviewDetailInitializedRef.current && !newProfileDraft && !detailProfileId) {
      browserPreviewDetailInitializedRef.current = true;
      setDetailProfileId(normalized.activeRelayId);
    }
  }, [detailProfileId, newProfileDraft, normalized.activeRelayId]);
  useEffect(() => {
    if (!newProfileDraft && detailProfileId === normalized.activeRelayId) {
      void actions.refreshRelayFiles();
    }
  }, [detailProfileId, newProfileDraft, normalized.activeRelayId]);
  const openThirdPartyImport = () => {
    setThirdPartyImportOpen((open) => !open);
    if (!ccsProviders) void actions.refreshCcsProviders(true);
  };

  if (detailProfile) {
    return (
      <RelayProfileDetail
        profile={detailProfile}
        relayFiles={!isNewProfile && detailProfile.id === normalized.activeRelayId ? relayFiles : null}
        form={normalized}
        isNew={isNewProfile}
        onBack={() => {
          setNewProfileDraft(null);
          setDetailProfileId(null);
        }}
        onFormChange={saveRelaySettings}
        onSaved={() => {
          setNewProfileDraft(null);
          setDetailProfileId(null);
        }}
        actions={actions}
      />
    );
  }

  return (
    <>
      <Panel className="relay-provider-panel">
        <CardHead
          title="供应商列表"
          detail={`${normalized.relayProfiles.length} 个供应商配置；可拖动排序，点编辑进入详情`}
          actions={(
            <>
              <div className="relay-add-row">
                <Button
                  variant="secondary"
                  onClick={() => {
                    setNewProfileDraft(createRelayProfile(normalized));
                    setDetailProfileId(null);
                  }}
                >
                  <Plus className="h-4 w-4" />
                  添加供应商
                </Button>
                <Button
                  variant="secondary"
                  onClick={createNewAggregateProfile}
                >
                  <Plus className="h-4 w-4" />
                  添加聚合供应商
                </Button>
                <div className="third-party-import">
                  <Button
                    onClick={openThirdPartyImport}
                    variant="secondary"
                  >
                    <Download className="h-4 w-4" />
                    从第三方导入
                  </Button>
                  {thirdPartyImportOpen ? (
                    <div className="third-party-import-menu">
                      <button
                        disabled={!ccsProviders?.providers.length}
                        onClick={() => {
                          setThirdPartyImportOpen(false);
                          void actions.importCcsProviders();
                        }}
                        type="button"
                      >
                        <strong>ccswitch</strong>
                        <span>{ccsProviderSummary(ccsProviders)}</span>
                      </button>
                      <button
                        onClick={() => void actions.refreshCcsProviders()}
                        type="button"
                      >
                        <RefreshCw className="h-4 w-4" />
                        刷新列表
                      </button>
                    </div>
                  ) : null}
                </div>
              </div>
              <button
                aria-checked={normalized.relayProfilesEnabled}
                aria-label="启用供应商功能"
                className={`context-enabled-switch compact-enabled-switch ${normalized.relayProfilesEnabled ? "active" : ""}`}
                data-tooltip="开启后允许切换供应商，并在启动 ChatGPT/Codex 时按当前供应商同步 config.toml / auth.json；需要本地代理或聚合供应商时也会随启动启用。关闭后只保存供应商列表，不写入 Codex live 配置、不启动本地代理。"
                disabled={actions.relaySwitching}
                onClick={() => {
                  const next = { ...normalized, relayProfilesEnabled: !normalized.relayProfilesEnabled };
                  void saveRelaySettings(next);
                }}
                role="switch"
                type="button"
              >
                <span className="context-switch-track" aria-hidden="true"><span className="context-switch-thumb" /></span>
                <span>{normalized.relayProfilesEnabled ? "已启用" : "已停用"}</span>
              </button>
            </>
          )}
        />
        <CardContent>
          <EnvConflictNotice envConflicts={envConflicts} actions={actions} />
          <RelayProfileList
            form={normalized}
            onEdit={(profileId) => void editRelayProfile(profileId)}
            onFormChange={saveRelaySettings}
            onTest={(profileId) => setTestProfileId(profileId)}
            onSpeedTest={(profileId) => setSpeedTestProfileId(profileId)}
            disabled={!normalized.relayProfilesEnabled || actions.relaySwitching}
            actions={actions}
          />
        </CardContent>
      </Panel>
      {testProfile ? (
        <RelayProfileTestDialog
          form={normalized}
          profile={testProfile}
          onClose={() => setTestProfileId(null)}
          actions={actions}
        />
      ) : null}
      {speedTestProfile ? (
        <RelayProfileSpeedTestDialog
          profile={speedTestProfile}
          onClose={() => setSpeedTestProfileId(null)}
          actions={actions}
        />
      ) : null}
    </>
  );
}

export function RelayProfileTestDialog({
  profile,
  form,
  onClose,
  actions,
}: {
  profile: RelayProfile;
  form: BackendSettings;
  onClose: () => void;
  actions: Actions;
}) {
  const fallbackModel = relayProfileDefaultTestModel(profile, form);
  const [model, setModel] = useState("");
  const [choices, setChoices] = useState<string[]>(() => relayProfileKnownModels(profile, fallbackModel));
  const [fetchingModels, setFetchingModels] = useState(false);
  const [sending, setSending] = useState(false);
  const [result, setResult] = useState<RelayProfileTestResult | null>(null);

  useEffect(() => {
    setModel("");
    setChoices(relayProfileKnownModels(profile, fallbackModel));
    setResult(null);
  }, [profile.id, fallbackModel]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !sending) onClose();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [onClose, sending]);

  useEffect(() => {
    let cancelled = false;
    const loadModels = async () => {
      setFetchingModels(true);
      try {
        const models = await actions.fetchRelayProfileModels(profile, true);
        if (!cancelled && models?.length) {
          setChoices(uniqueStrings([...relayProfileKnownModels(profile, fallbackModel), ...models]));
        }
      } finally {
        if (!cancelled) setFetchingModels(false);
      }
    };
    void loadModels();
    return () => {
      cancelled = true;
    };
  }, [actions, profile.id, fallbackModel]);

  const sendTest = async () => {
    const selectedModel = model.trim();
    if (sending) return;
    setSending(true);
    setResult(null);
    try {
      const nextResult = await actions.testRelayProfile(profile, selectedModel || undefined);
      setResult(nextResult);
    } finally {
      setSending(false);
    }
  };

  return createPortal(
    <div className="modal-backdrop" role="dialog" aria-modal="true" aria-labelledby="relay-test-title" onClick={sending ? undefined : onClose}>
      <div className="modal-card relay-test-modal" onClick={(event) => event.stopPropagation()}>
        <div className="modal-head">
          <div>
            <h2 id="relay-test-title">发送测试</h2>
            <p>{profile.name || "未命名供应商"} · 发送 hi 到当前供应商，返回结果会显示在这里。</p>
          </div>
          <Button disabled={sending} onClick={onClose} size="icon" title="关闭" variant="ghost">
            <X className="h-4 w-4" />
          </Button>
        </div>
        <Field label="发送模型（可选）">
          <ModelChoiceInput
            choices={choices}
            onChange={(value) => {
              setModel(value);
              setResult(null);
            }}
            placeholder={fetchingModels ? "正在读取模型列表" : "点击选择模型，留空使用默认模型"}
            value={model}
          />
        </Field>
        {result ? (
          <div className={`relay-test-result ${isSuccessStatus(result.status) ? "ok" : "bad"}`}>
            <strong>{result.message}</strong>
            {result.endpoint ? <span>Endpoint：{result.endpoint}</span> : null}
            <pre>{result.responsePreview || "响应内容为空"}</pre>
          </div>
        ) : null}
        <div className="relay-test-actions">
          <Toolbar>
            <Button disabled={sending} onClick={() => void sendTest()}>
              <TestTube className="h-4 w-4" />
              {sending ? "发送中" : "发送"}
            </Button>
            <Button disabled={sending} onClick={onClose} variant="secondary">关闭</Button>
          </Toolbar>
        </div>
      </div>
    </div>,
    document.body,
  );
}

export const SPEED_TEST_CONCURRENCY = 5;

export function formatSpeedTestDurationMs(value: number): string {
  if (!Number.isFinite(value)) return "-";
  const elapsedMs = Math.max(0, value);
  if (elapsedMs < 1000) return `${Math.round(elapsedMs)} ms`;
  const seconds = elapsedMs / 1000;
  const digits = seconds < 10 ? 2 : 1;
  return `${seconds.toFixed(digits).replace(/\.?0+$/, "")} s`;
}

export function formatOptionalSpeedTestDurationMs(value?: number | null): string {
  return typeof value === "number" ? formatSpeedTestDurationMs(value) : "-";
}

export function speedTestTokenRate(outputTokens: number | null | undefined, elapsedMs: number | null | undefined): number {
  if (
    typeof outputTokens !== "number"
    || !Number.isFinite(outputTokens)
    || outputTokens <= 0
    || typeof elapsedMs !== "number"
    || !Number.isFinite(elapsedMs)
    || elapsedMs <= 0
  ) return Number.NaN;
  return outputTokens * 1000 / elapsedMs;
}

export function formatSpeedTestTokenRate(outputTokens: number | null | undefined, elapsedMs: number | null | undefined): string {
  const rate = speedTestTokenRate(outputTokens, elapsedMs);
  return formatProxyTokenRate(rate);
}

export function speedTestItemToProxyLogDetail(
  profile: RelayProfile,
  item: RelayProfileSpeedTestItem,
): LocalProxyLogDetail {
  const requestBodyStr = item.requestBody || "";
  const responseBodyStr = item.responseBody || "";
  const reqBytes = new TextEncoder().encode(requestBodyStr).length;
  const resBytes = new TextEncoder().encode(responseBodyStr).length;
  const protocol = profile.protocol || "chat_completions";
  const protocolLabel =
    protocol === "responses"
      ? "Responses API"
      : protocol === "anthropic"
        ? "Anthropic"
        : "Chat Completions";
  const defaultPath =
    protocol === "responses"
      ? "/responses"
      : protocol === "anthropic"
        ? "/messages"
        : "/chat/completions";
  let pathname = defaultPath;
  if (item.endpoint) {
    try {
      pathname = new URL(item.endpoint).pathname;
    } catch {
      pathname = item.endpoint;
    }
  }
  return {
    id: `speed-test-${item.model}-${item.elapsedMs}`,
    transport: "http",
    timestampMs: Date.now(),
    method: "POST",
    path: pathname,
    remoteAddr: "127.0.0.1",
    model: item.model,
    upstreamRequestModel: item.model,
    upstreamResponseModel: item.model,
    outputTokens: item.outputTokens,
    relayId: profile.id,
    relayName: profile.name || "当前供应商",
    endpoint: item.endpoint || profile.baseUrl,
    responseProtocol: protocolLabel,
    statusCode: item.httpStatus,
    firstTokenMs: item.firstTokenMs,
    durationMs: item.elapsedMs,
    stream: true,
    requestBytes: reqBytes,
    responseBytes: resBytes,
    responseCapturedBytes: resBytes,
    responseTruncated: false,
    error: item.status === "failed" ? item.message : null,
    requestBody: requestBodyStr,
    responseBody: responseBodyStr,
  };
}

export function RelayProfileSpeedTestDialog({
  profile,
  onClose,
  actions,
}: {
  profile: RelayProfile;
  onClose: () => void;
  actions: Actions;
}) {
  const configuredModels = useMemo(
    () => relayProfileConfiguredModels(profile),
    [
      profile.id,
      profile.modelMappings,
      profile.responsesModelList,
      profile.chatCompletionsModelList,
      profile.anthropicModelList,
      profile.modelList,
    ],
  );
  const [mode, setMode] = useState<"configured" | "selected">("configured");
  const [availableModels, setAvailableModels] = useState<string[]>(configuredModels);
  // 用户要求右侧按所选模型测速；未选择时保持为空，不能自动测试供应商的全部模型。
  const [selectedModels, setSelectedModels] = useState<string[]>([]);
  const [fetchingModels, setFetchingModels] = useState(false);
  const [running, setRunning] = useState(false);
  const [sortBy, setSortBy] = useState<"firstToken" | "tokenRate">("tokenRate");
  const [selectedDetailItem, setSelectedDetailItem] = useState<RelayProfileSpeedTestItem | null>(null);
  const [resultsByMode, setResultsByMode] = useState<Record<"configured" | "selected", RelayProfileSpeedTestItem[]>>({
    configured: [],
    selected: [],
  });
  const results = resultsByMode[mode];
  const configuredModelsKey = configuredModels.join("\n");

  useEffect(() => {
    setMode("configured");
    setAvailableModels(configuredModels);
    setSelectedModels([]);
    setSortBy("tokenRate");
    setResultsByMode({ configured: [], selected: [] });
    setSelectedDetailItem(null);
  }, [profile.id, configuredModelsKey]);

  useEffect(() => {
    let cancelled = false;
    const loadModels = async () => {
      setFetchingModels(true);
      try {
        const fetched = await actions.fetchRelayProfileModels(profile, true);
        if (!cancelled && fetched?.length) {
          setAvailableModels(uniqueStrings([...configuredModels, ...fetched]));
        }
      } finally {
        if (!cancelled) setFetchingModels(false);
      }
    };
    void loadModels();
    return () => {
      cancelled = true;
    };
  }, [actions, configuredModelsKey, profile.id]);
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !running) {
        if (selectedDetailItem) {
          setSelectedDetailItem(null);
        } else {
          onClose();
        }
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [onClose, running, selectedDetailItem]);

  const targetModels = mode === "selected" ? selectedModels : configuredModels;

  const runSpeedTest = async () => {
    if (running || !targetModels.length) return;
    const testMode = mode;
    setRunning(true);
    setResultsByMode((current) => ({ ...current, [testMode]: [] }));
    try {
      let nextIndex = 0;
      const testNext = async () => {
        while (nextIndex < targetModels.length) {
          const model = targetModels[nextIndex];
          nextIndex += 1;
          const startedAt = performance.now();
          let item: RelayProfileSpeedTestItem;
          try {
            // 用户要求只发送完整糖果题，不再用重复内容补齐 5 万 token；普通连通性测试仍单独使用 hi。
            const result = await actions.testRelayProfile(profile, model, true);
            const measuredElapsedMs = Math.max(1, Math.round(performance.now() - startedAt));
            item = {
              model,
              elapsedMs: typeof result?.durationMs === "number" && result.durationMs > 0
                ? result.durationMs
                : measuredElapsedMs,
              firstTokenMs: result?.firstTokenMs ?? null,
              outputTokens: result?.outputTokens ?? null,
              status: result && isSuccessStatus(result.status) ? "ok" : "failed",
              httpStatus: result?.httpStatus ?? null,
              message: result?.message || "请求失败",
              endpoint: result?.endpoint ?? null,
              requestBody: result?.requestBody ?? null,
              responseBody: result?.responseBody ?? null,
            };
          } catch (error) {
            item = {
              model,
              elapsedMs: Math.max(1, Math.round(performance.now() - startedAt)),
              firstTokenMs: null,
              outputTokens: null,
              status: "failed",
              httpStatus: null,
              message: error instanceof Error ? error.message : "请求失败",
              endpoint: null,
              requestBody: null,
              responseBody: null,
            };
          }
          setResultsByMode((current) => ({
            ...current,
            [testMode]: [...current[testMode], item],
          }));
        }
      };
      await Promise.all(
        Array.from({ length: Math.min(SPEED_TEST_CONCURRENCY, targetModels.length) }, () => testNext()),
      );
    } finally {
      setRunning(false);
    }
  };

  const fastest = results.length ? Math.min(...results.map((item) => item.elapsedMs)) : 0;
  const slowest = results.length ? Math.max(...results.map((item) => item.elapsedMs)) : 0;
  const average = results.length
    ? Math.round(results.reduce((total, item) => total + item.elapsedMs, 0) / results.length)
    : 0;
  const sortedResults = useMemo(
    () => [...results].sort((left, right) => {
      // 用户要求以首字耗时降序替换 tk/s 升序：首字越慢越靠前，保留 tk/s 降序为默认；失败或缺失指标仍排末尾。
      const metric = (item: RelayProfileSpeedTestItem) => item.status !== "ok"
        ? Number.NaN
        : sortBy === "firstToken"
          ? item.firstTokenMs ?? Number.NaN
          : speedTestTokenRate(item.outputTokens, item.elapsedMs);
      const leftValue = metric(left);
      const rightValue = metric(right);
      const leftMissing = !Number.isFinite(leftValue);
      const rightMissing = !Number.isFinite(rightValue);
      if (leftMissing !== rightMissing) return leftMissing ? 1 : -1;
      if (!leftMissing && !rightMissing && leftValue !== rightValue) {
        return rightValue - leftValue;
      }
      return left.elapsedMs - right.elapsedMs;
    }),
    [results, sortBy],
  );

  return createPortal(
    <div
      className="modal-backdrop"
      role="dialog"
      aria-modal="true"
      aria-labelledby="relay-speed-test-title"
      onClick={running ? undefined : onClose}
    >
      <div
        className={`modal-card relay-speed-test-modal ${running || results.length ? "has-results" : ""}`}
        onClick={(event) => event.stopPropagation()}
      >
        <div className="modal-head">
          <div>
            <h2 id="relay-speed-test-title">模型速度测试</h2>
            <p>{profile.name || "未命名供应商"} · 每个模型回答同一道完整糖果题，按实际输出 token / 完整请求耗时计算 tok/s，最多 5 个并行测速。</p>
          </div>
          <Button disabled={running} onClick={onClose} size="icon" title="关闭" variant="ghost">
            <X className="h-4 w-4" />
          </Button>
        </div>

        <div className="speed-test-mode-grid">
          <button
            className={`speed-test-mode-card ${mode === "configured" ? "active" : ""}`}
            disabled={running}
            onClick={() => {
              setMode("configured");
            }}
            type="button"
          >
            <strong>模型列表</strong>
            <span>测试当前供应商已配置的 {configuredModels.length} 个模型</span>
          </button>
          <button
            className={`speed-test-mode-card ${mode === "selected" ? "active" : ""}`}
            disabled={running}
            onClick={() => {
              setMode("selected");
            }}
            type="button"
          >
            <strong>选择模型</strong>
            <span>{fetchingModels ? "正在获取供应商模型…" : `从 ${availableModels.length} 个可用模型中单选或多选测速`}</span>
          </button>
        </div>

        {mode === "selected" ? (
          <Field as="div" className="speed-test-model-picker" label="测速模型">
            <SelectMenu
              ariaLabel="选择测速模型"
              disabled={running || !availableModels.length}
              menuClassName="speed-test-model-menu"
              multiple
              onChange={(models) => {
                setSelectedModels(models);
                setResultsByMode((current) => ({ ...current, selected: [] }));
                setSelectedDetailItem(null);
              }}
              options={availableModels.map((model) => ({ value: model, label: model }))}
              placeholder={availableModels.length
                ? "请选择一个或多个模型"
                : fetchingModels ? "正在获取供应商模型…" : "暂无可选模型"}
              value={selectedModels}
            />
          </Field>
        ) : null}

        {running ? (
          <div className="speed-test-running">
            <span className="speed-test-spinner" />
            <span>正在测速（最多 5 个并行）… 已完成 {results.length}/{targetModels.length}</span>
          </div>
        ) : null}

        {results.length ? (
          <div className="speed-test-result-panel">
            <div className="speed-test-result-toolbar">
              <div className="speed-test-result-summary" aria-label="测速统计">
                <span>最快 <strong>{formatSpeedTestDurationMs(fastest)}</strong></span>
                <span>最慢 <strong>{formatSpeedTestDurationMs(slowest)}</strong></span>
                <span>平均 <strong>{formatSpeedTestDurationMs(average)}</strong></span>
                <span>已完成 <strong>{results.length}/{targetModels.length}</strong></span>
              </div>
              <div className="speed-test-sort-controls" role="group" aria-label="测速结果排序">
                <button
                  aria-pressed={sortBy === "firstToken"}
                  className={sortBy === "firstToken" ? "active" : ""}
                  onClick={() => setSortBy("firstToken")}
                  type="button"
                >
                  <ArrowDownWideNarrow className="h-3.5 w-3.5" />
                  首字降序
                </button>
                <button
                  aria-pressed={sortBy === "tokenRate"}
                  className={sortBy === "tokenRate" ? "active" : ""}
                  onClick={() => setSortBy("tokenRate")}
                  type="button"
                >
                  <ArrowUpNarrowWide className="h-3.5 w-3.5" />
                  tk/s 降序
                </button>
              </div>
            </div>
            <div className="speed-test-result-list">
              {sortedResults.map((item, index) => {
                return (
                  <div
                    className={`speed-test-result-row ${item.status === "failed" ? "failed" : ""}`}
                    key={item.model}
                    onClick={() => setSelectedDetailItem(item)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter" || event.key === " ") {
                        event.preventDefault();
                        setSelectedDetailItem(item);
                      }
                    }}
                    role="button"
                    tabIndex={0}
                    title="点击查看请求详情"
                  >
                    <span className="speed-test-rank">{index + 1}</span>
                    <span className="speed-test-result-model">{item.model}</span>
                    <span className="speed-test-result-metric">
                      <span>{formatOptionalSpeedTestDurationMs(item.firstTokenMs)}</span>
                      <span className="speed-test-result-metric-label">首字</span>
                    </span>
                    <span className="speed-test-result-metric">
                      <span>{formatSpeedTestDurationMs(item.elapsedMs)}</span>
                      <span className="speed-test-result-metric-label">耗时</span>
                    </span>
                    <span
                      className="speed-test-result-metric speed-test-result-token-rate"
                      title="按上游 usage.output_tokens ÷ 完整请求耗时计算，与本地代理模型速率一致"
                    >
                      <span>{formatSpeedTestTokenRate(
                        item.status === "ok" ? item.outputTokens : null,
                        item.elapsedMs,
                      )}</span>
                      <span className="speed-test-result-metric-label">tk/s</span>
                    </span>
                    <span className="speed-test-result-status">{item.status === "ok" ? "完成" : "失败"}</span>
                  </div>
                );
              })}
            </div>
          </div>
        ) : null}

        <div className="relay-test-actions">
          <Toolbar>
            <Button disabled={running || !targetModels.length} onClick={() => void runSpeedTest()}>
              <Zap className="h-4 w-4" />
              {running ? "测速中…" : "开始测速"}
            </Button>
            <Button disabled={running} onClick={onClose} variant="secondary">关闭</Button>
          </Toolbar>
        </div>
      </div>
      {selectedDetailItem ? (
        <LocalProxyLogDetailDialog
          entry={speedTestItemToProxyLogDetail(profile, selectedDetailItem)}
          onClose={() => setSelectedDetailItem(null)}
          onCopyRequest={(text) => actions.copyLocalProxyRequest(text || selectedDetailItem.requestBody || "")}
          onCopyResponse={(text) => actions.copyLocalProxyResponse(text || selectedDetailItem.responseBody || "")}
        />
      ) : null}
    </div>,
    document.body,
  );
}

export function EnvConflictNotice({
  envConflicts,
  actions,
}: {
  envConflicts: EnvConflictsResult | null;
  actions: Actions;
}) {
  const conflicts = envConflicts?.conflicts ?? [];
  if (!conflicts.length) return null;
  const names = Array.from(new Set(conflicts.map((conflict) => conflict.name))).sort();
  return (
    <div className="env-conflict-notice">
      <div className="env-conflict-icon">
        <ShieldAlert className="h-4 w-4" />
      </div>
      <div className="env-conflict-body">
        <strong>检测到 OPENAI 环境变量</strong>
        <p>这些变量可能覆盖当前供应商写入的 config.toml / auth.json；CODEX_HOME 不会被清理。</p>
        <div className="env-conflict-tags">
          {conflicts.map((conflict) => (
            <span key={`${conflict.source}-${conflict.name}`}>
              {conflict.name}
              <small>{envConflictSourceLabel(conflict.source)}</small>
            </span>
          ))}
        </div>
      </div>
      <div className="env-conflict-actions">
        <Button onClick={() => void actions.removeEnvConflicts(names)} size="sm">
          <Trash2 className="h-4 w-4" />
          删除
        </Button>
        <Button onClick={() => void actions.refreshEnvConflicts(false)} size="sm" variant="secondary">
          <RefreshCw className="h-4 w-4" />
          检测
        </Button>
      </div>
    </div>
  );
}

export function envConflictSourceLabel(source: string): string {
  if (source === "process") return "当前进程";
  if (source === "user") return "用户环境";
  return source || "环境变量";
}

export function SessionsScreen({ sessions, actions }: {
  sessions: LocalSessionsResult | null; actions: Actions;
}) {

  const items = sessions?.sessions ?? [];

  const projectlessWorkspaceRoot = sessions?.projectlessWorkspaceRoot ?? "";

  const activeCount = items.filter((item) => !item.archived).length;

  const archivedCount = items.length - activeCount;

  const [projectFilter, setProjectFilter] = useState<string>("");

  // 时间范围筛选：start/end 为当天 00:00 的毫秒时间戳，null 表示不限
  const [startMs, setStartMs] = useState<number | null>(null);

  const [endMs, setEndMs] = useState<number | null>(null);

  const [batchDeleting, setBatchDeleting] = useState(false);

  const [localSessionPage, setLocalSessionPage] = useState(1);


  // 项目（cwd）筛选选项：去重后的项目路径列表，附带会话数量
  const projectOptions = useMemo(() => {
    const counts = new Map<string, number>();
    for (const item of items) {
      const projectKey = sessionProjectKey(item.cwd, projectlessWorkspaceRoot);
      if (projectKey) counts.set(projectKey, (counts.get(projectKey) ?? 0) + 1);
    }
    return Array.from(counts.entries())
      .map(([value, count]) => ({ value, count }))
      .sort((a, b) => sessionProjectLabel(a.value).localeCompare(sessionProjectLabel(b.value)));
  }, [items, projectlessWorkspaceRoot]);


  // 结束时间含当天：转换为当天 23:59:59.999
  const endCutoffMs = endMs === null ? null : endMs + 24 * 60 * 60 * 1000 - 1;

  const hasTimeFilter = startMs !== null || endMs !== null;


  const filteredItems = useMemo(() => {
    return items.filter((item) => {
      if (projectFilter && sessionProjectKey(item.cwd, projectlessWorkspaceRoot) !== projectFilter) return false;
      const updated = item.updatedAtMs ?? 0;
      if (startMs !== null && !(updated >= startMs)) return false;
      if (endCutoffMs !== null && !(updated > 0 && updated <= endCutoffMs)) return false;
      return true;
    });
  }, [items, projectFilter, startMs, endCutoffMs, projectlessWorkspaceRoot]);


  const localSessionPageSize = 100;

  const localSessionTotalPages = Math.max(1, Math.ceil(filteredItems.length / localSessionPageSize));

  const currentLocalSessionPage = Math.min(localSessionPage, localSessionTotalPages);

  const visibleSessionItems = useMemo(() => {
    const start = (currentLocalSessionPage - 1) * localSessionPageSize;
    return filteredItems.slice(start, start + localSessionPageSize);
  }, [currentLocalSessionPage, filteredItems]);


  const hasFilter = Boolean(projectFilter) || hasTimeFilter;

  const batchDeleteDisabled = !hasFilter || batchDeleting || filteredItems.length === 0;


  const onBatchDelete = async () => {
    if (batchDeleteDisabled) return;
    setBatchDeleting(true);
    try {
      await actions.deleteLocalSessionsBatch(filteredItems);
      setProjectFilter("");
      setStartMs(null);
      setEndMs(null);
      setLocalSessionPage(1);
    } finally {
      setBatchDeleting(false);
    }
  };
  return (<>
  {/* 用户要求仅保留会话浏览、筛选及删除，移除历史会话修复和上下文压缩配置。 */}
  <Panel>
    <CardHead title="会话管理" detail="浏览当前服务的会话，按项目和时间筛选后删除" />
    <CardContent>
      <div className="metric-list session-summary-grid">
        <Metric label="会话总数" value={`${items.length} 个`} />
        <Metric label="未归档" value={`${activeCount} 个`} />
        <Metric label="已归档" value={`${archivedCount} 个`} />
      </div>
      <Button onClick={() => void actions.refreshLocalSessions()}><RefreshCw className="h-4 w-4" />刷新会话</Button>
    </CardContent>
  </Panel>
  <Panel>
        <CardHead title="会话列表" detail={items.length ? "按更新时间倒序显示；可按项目和时间筛选后批量删除" : "点击刷新会话读取服务端 Codex 会话"} />
        <CardContent>
          {items.length ? (
            <>
              <div className="session-filter-bar">
                <div className="session-filter-field">
                  <span className="session-filter-label">项目</span>
                  <SessionProjectSelect
                    value={projectFilter}
                    options={projectOptions}
                    projectlessWorkspaceRoot={projectlessWorkspaceRoot}
                    onChange={(next) => {
                      setProjectFilter(next);
                      setLocalSessionPage(1);
                    }}
                  />
                </div>
                <div className="session-filter-field">
                  <span className="session-filter-label">时间范围（按更新时间）</span>
                  <SessionTimeRangePicker
                    startMs={startMs}
                    endMs={endMs}
                    onChange={(nextStart, nextEnd) => {
                      setStartMs(nextStart);
                      setEndMs(nextEnd);
                      setLocalSessionPage(1);
                    }}
                  />
                </div>
                <Button
                  className="session-filter-delete"
                  variant="destructive"
                  disabled={batchDeleteDisabled}
                  onClick={() => void onBatchDelete()}
                >
                  <Trash2 className="h-4 w-4" />
                  {batchDeleting ? "正在批量删除…" : `批量删除${hasFilter ? `（${filteredItems.length}）` : ""}`}
                </Button>
              </div>
              <div className="session-filter-summary">
                {hasFilter
                  ? `匹配 ${filteredItems.length} 个会话（共 ${items.length} 个）`
                  : `共 ${items.length} 个会话；选择项目或时间范围后可批量删除`}
              </div>
              {filteredItems.length ? (
                <>
                  <div className="session-list">
                    {visibleSessionItems.map((session) => (
                      <div className="session-row" key={session.id}>
                        <div className="session-main">
                          <strong>{session.title || "未命名会话"}</strong>
                          <span>{session.id}</span>
                          <small data-tooltip={session.cwd || undefined}>{sessionProjectLabel(sessionProjectKey(session.cwd, projectlessWorkspaceRoot)) || "未记录项目路径"}</small>
                        </div>
                        <div className="session-meta">
                          <Badge status={session.archived ? "archived" : "ok"} />
                          <span>{session.modelProvider || "provider 未记录"}</span>
                          <span>{formatTime(session.updatedAtMs ?? 0)}</span>
                        </div>
                        <Button variant="outline" onClick={() => void actions.deleteLocalSession(session)}>
                          <Trash2 className="h-4 w-4" />
                          删除
                        </Button>
                      </div>
                    ))}
                  </div>
                  {localSessionTotalPages > 1 ? (
                    <div className="session-pagination" aria-label="本地会话分页">
                      <span>{`每页 ${localSessionPageSize} 条 · 第 ${currentLocalSessionPage} / ${localSessionTotalPages} 页`}</span>
                      <Button
                        disabled={currentLocalSessionPage <= 1}
                        onClick={() => setLocalSessionPage(1)}
                        size="sm"
                        variant="outline"
                      >
                        首页
                      </Button>
                      <Button
                        disabled={currentLocalSessionPage <= 1}
                        onClick={() => setLocalSessionPage((current) => Math.max(1, current - 1))}
                        size="sm"
                        variant="outline"
                      >
                        上一页
                      </Button>
                      <Button
                        disabled={currentLocalSessionPage >= localSessionTotalPages}
                        onClick={() => setLocalSessionPage((current) => Math.min(localSessionTotalPages, current + 1))}
                        size="sm"
                        variant="outline"
                      >
                        下一页
                      </Button>
                    </div>
                  ) : null}
                </>
              ) : (
                <div className="empty">没有符合筛选条件的会话。</div>
              )}
            </>
          ) : (
            <div className="empty">未读取到本地会话，或当前 SQLite 会话库不存在。</div>
          )}
        </CardContent>
      </Panel>
</>);
}

export function RelayProfileList({
  form,
  onFormChange,
  onEdit,
  onTest,
  onSpeedTest,
  disabled = false,
  actions,
}: {
  form: BackendSettings;
  onFormChange: (value: BackendSettings) => void;
  onEdit: (id: string) => void;
  onTest: (id: string) => void;
  onSpeedTest: (id: string) => void;
  disabled?: boolean;
  actions: Actions;
}) {
  const sensors = useSensors(
    useSensor(PointerSensor, {
      activationConstraint: { distance: 8 },
    }),
    useSensor(KeyboardSensor, {
      coordinateGetter: sortableKeyboardCoordinates,
    }),
  );
  const handleDragEnd = (event: DragEndEvent) => {
    const { active, over } = event;
    if (!over || active.id === over.id) return;
    const next = reorderRelayProfiles(form, String(active.id), String(over.id));
    if (next !== form) onFormChange(next);
  };
  return (
    <DndContext sensors={sensors} collisionDetection={closestCenter} onDragEnd={handleDragEnd}>
      <SortableContext items={form.relayProfiles.map((profile) => profile.id)} strategy={verticalListSortingStrategy}>
        <div className="relay-profile-list">
          {form.relayProfiles.map((profile) => (
            <SortableRelayProfileCard
              actions={actions}
              form={form}
              key={profile.id}
              onEdit={onEdit}
              onFormChange={onFormChange}
              onTest={onTest}
              onSpeedTest={onSpeedTest}
              disabled={disabled}
              profile={profile}
            />
          ))}
        </div>
      </SortableContext>
    </DndContext>
  );
}

export function SortableRelayProfileCard({
  form,
  profile,
  onFormChange,
  onEdit,
  onTest,
  onSpeedTest,
  disabled = false,
  actions,
}: {
  form: BackendSettings;
  profile: RelayProfile;
  onFormChange: (value: BackendSettings) => void;
  onEdit: (id: string) => void;
  onTest: (id: string) => void;
  onSpeedTest: (id: string) => void;
  disabled?: boolean;
  actions: Actions;
}) {
  const { attributes, listeners, setNodeRef, transform, transition, isDragging } = useSortable({ id: profile.id });
  const active = profile.id === form.activeRelayId;
  const style: CSSProperties = {
    transform: CSS.Transform.toString(transform),
    transition,
  };

  return (
    <div
      className={`relay-profile-card ${active ? "active" : ""} ${isDragging ? "dragging" : ""}`}
      data-relay-profile-id={profile.id}
      key={profile.id}
      onKeyDown={(event) => {
        if (event.key === "Enter") onEdit(profile.id);
      }}
      ref={setNodeRef}
      style={style}
      tabIndex={0}
    >
      <button
        aria-label="拖动排序"
        className="relay-drag"
        data-tooltip="拖动排序"
        type="button"
        {...attributes}
        {...listeners}
      >
        <GripVertical className="h-4 w-4" />
      </button>
      <span className="relay-index" data-tooltip={profile.name || "未命名供应商"}>
        {providerInitial(profile.name)}
      </span>
      <span className="relay-summary">
        <strong>{profile.name || "未命名供应商"}</strong>
        <small>{relayModeLabel(profile.relayMode)} · {relayProfileConfigBrief(profile)}</small>
      </span>
      <span className="relay-card-actions">
        <Button
          className={`relay-use-button ${active ? "active" : ""}`}
          disabled={disabled}
          onClick={(event) => {
            event.stopPropagation();
            if (disabled) return;
            const previousActiveRelayId = form.activeRelayId;
            const next = syncLegacyRelayFields({ ...form, activeRelayId: profile.id });
            void actions.switchRelayProfile(next, previousActiveRelayId);
          }}
          size="sm"
          title={disabled ? "供应商切换不可用" : active ? "当前正在使用" : "设为当前"}
          variant={active ? "secondary" : "outline"}
        >
          <CheckCircle2 className="h-4 w-4" />
          {active ? "使用中" : "使用"}
        </Button>
        <span className="relay-card-extra">
          <Button
            disabled={isAggregateRelayProfile(profile)}
            onClick={(event) => {
              event.stopPropagation();
              if (isAggregateRelayProfile(profile)) return;
              onTest(profile.id);
            }}
            size="icon"
            title={isAggregateRelayProfile(profile) ? "聚合供应商会在真实对话中轮转成员，请测试成员供应商" : "发送 hi 测试"}
            variant="ghost"
          >
            <TestTube className="h-4 w-4" />
          </Button>
          <Button
            disabled={isAggregateRelayProfile(profile)}
            onClick={(event) => {
              event.stopPropagation();
              if (isAggregateRelayProfile(profile)) return;
              onSpeedTest(profile.id);
            }}
            size="icon"
            title={isAggregateRelayProfile(profile) ? "聚合供应商不支持单独测速" : "模型速度测试"}
            variant="ghost"
          >
            <Zap className="h-4 w-4" />
          </Button>
          <Button
            onClick={(event) => {
              event.stopPropagation();
              onEdit(profile.id);
            }}
            size="icon"
            title="编辑"
            variant="ghost"
          >
            <Edit3 className="h-4 w-4" />
          </Button>
          <Button
            onClick={(event) => {
              event.stopPropagation();
              onFormChange(duplicateRelayProfile(form, profile.id));
            }}
            size="icon"
            title="复制"
            variant="ghost"
          >
            <Copy className="h-4 w-4" />
          </Button>
          <Button
            disabled={form.relayProfiles.length <= 1}
            onClick={(event) => {
              event.stopPropagation();
              onFormChange(removeRelayProfile(form, profile.id));
            }}
            size="icon"
            title="删除供应商"
            variant="ghost"
          >
            <Trash2 className="h-4 w-4" />
          </Button>
        </span>
      </span>
    </div>
  );
}

export function RelayProfileDetail({
  profile,
  relayFiles,
  form,
  isNew = false,
  onBack,
  onFormChange,
  onSaved,
  actions,
}: {
  profile: RelayProfile;
  relayFiles: RelayFilesResult | null;
  form: BackendSettings;
  isNew?: boolean;
  onBack: () => void;
  onFormChange: (value: BackendSettings) => void | Promise<void>;
  onSaved?: () => void;
  actions: Actions;
}) {
  const [draft, setDraft] = useState<RelayProfile>(profile);
  const isActive = !isNew && profile.id === form.activeRelayId;
  const profileUsesLiveFiles = relayProfileUsesLiveFiles(profile);
  useEffect(() => {
    setDraft(
      isAggregateRelayProfile(profile)
        ? normalizeAggregateRelayProfile(profile, form)
        : deriveRelayProfileFromFiles(
            isActive && profileUsesLiveFiles && relayFiles
              ? {
                ...profile,
                configContents: relayFiles.configContents,
                authContents: relayFiles.authContents,
              }
              : profile,
          ),
    );
  }, [profile.id, profileUsesLiveFiles, isActive, isNew, relayFiles?.configContents, relayFiles?.authContents]);
  const validationError = isAggregateRelayProfile(draft) ? aggregateRelayProfileValidation(draft, form) : null;
  const saveDraft = async () => {
    if (validationError) return;
    const normalizedDraft = isAggregateRelayProfile(draft) ? normalizeAggregateRelayProfile(draft, form) : deriveRelayProfileFromFiles(draft);
    const next = isNew
      ? addRelayProfile(form, normalizedDraft)
      : updateRelayProfile(form, profile.id, normalizedDraft);
    await onFormChange(next);
    if (isActive && !isAggregateRelayProfile(normalizedDraft) && relayProfileUsesLiveFiles(normalizedDraft)) {
      await actions.saveRelayAuthFile(normalizedDraft.authContents, true);
    }
    onSaved?.();
  };
  const switchDraft = () => {
    if (isNew || !form.relayProfilesEnabled) return;
    const normalizedDraft = isAggregateRelayProfile(draft) ? normalizeAggregateRelayProfile(draft, form) : deriveRelayProfileFromFiles(draft);
    const previousActiveRelayId = form.activeRelayId;
    const next = syncLegacyRelayFields({
      ...form,
      relayProfiles: form.relayProfiles.map((item) => (item.id === profile.id ? normalizedDraft : item)),
      activeRelayId: profile.id,
    });
    void actions.switchRelayProfile(next, previousActiveRelayId);
  };
  return (
    <div className="relay-detail-page" key={profile.id}>
      <div className="relay-detail-sticky">
        <Toolbar>
          <Button onClick={onBack} variant="secondary">
            <ArrowLeft className="h-4 w-4" />
            返回列表
          </Button>
          <Button disabled={!!validationError} onClick={() => void saveDraft()}>
            <Save className="h-4 w-4" />
            保存
          </Button>
        </Toolbar>
      </div>
      <RelayProfileEditor profile={draft} form={form} isNew={isNew} onProfileChange={setDraft} onSwitch={switchDraft} actions={actions} />
      {isAggregateRelayProfile(draft) ? (
        <AggregateRelayImpactPanel profile={draft} form={form} isNew={isNew} />
      ) : (
        <RelayActivationPanel
          profile={draft}
          isActive={isActive}
          onProfileChange={setDraft}
        />
      )}
    </div>
  );
}

export function RelayProfileEditor({
  profile,
  form,
  isNew = false,
  onProfileChange,
  onSwitch,
  actions,
}: {
  profile: RelayProfile;
  form: BackendSettings;
  isNew?: boolean;
  onProfileChange: (value: RelayProfile) => void;
  onSwitch: () => void;
  actions: Actions;
}) {
  const [modelChoices, setModelChoices] = useState<Record<RelayProtocol, string[]>>({
    responses: [],
    chatCompletions: [],
    anthropic: [],
  });
  const [fetchingModelChoices, setFetchingModelChoices] = useState(false);
  const [probingResponsesWebsocket, setProbingResponsesWebsocket] = useState(false);
  const [systemPromptOpen, setSystemPromptOpen] = useState(false);
  const [apiKeyVisible, setApiKeyVisible] = useState(false);
  if (isAggregateRelayProfile(profile)) {
    return (
      <AggregateRelayProfileEditor
        profile={profile}
        form={form}
        isNew={isNew}
        onProfileChange={onProfileChange}
      />
    );
  }

  const showApiFields = profile.relayMode !== "official" || profile.officialMixApiKey;
  const responsesWebsocketApplicable = relayCanProbeNativeResponsesWebsocket(profile);
  const responsesWebsocketState: ResponsesWebsocketCapabilityState =
    responsesWebsocketApplicable
      ? profile.responsesWebsocket.state
      : "unsupported";
  const responsesWebsocketToggleEnabled =
    responsesWebsocketApplicable && responsesWebsocketState === "supported";
  const responsesWebsocketToggleChecked =
    responsesWebsocketToggleEnabled && profile.responsesWebsocketEnabled;
  const remoteCompactionV2Enabled = relayRemoteCompactionV2Enabled(profile);
  const multiAgentV2Enabled = relayMultiAgentV2Enabled(profile);
  const subagentCount = relaySubagentCount(profile);
  const responsesWebsocketLabel =
    probingResponsesWebsocket
      ? "探测中"
      : !responsesWebsocketApplicable
      ? "当前配置不适用"
      : responsesWebsocketState === "supported"
      ? "已支持"
      : responsesWebsocketState === "unsupported"
        ? "不支持"
        : "待探测";
  const responsesWebsocketBaseMessage =
    probingResponsesWebsocket
      ? "正在连接供应商的 Responses WebSocket 端点并执行真实握手。"
      : !responsesWebsocketApplicable
      ? "当前供应商没有配置原生 Responses 模型，或处于未混用 API Key 的官方登录模式。"
      : profile.responsesWebsocket.message
    || (responsesWebsocketState === "unsupported"
      ? "上次探测确认当前端点不支持 Responses WebSocket。"
      : responsesWebsocketState === "supported"
        ? "上游已支持。"
        : "点击“重新探测”立即执行真实 WebSocket 握手。");
  const responsesWebsocketMessage =
    !probingResponsesWebsocket
    && responsesWebsocketApplicable
    && responsesWebsocketState === "supported"
      ? `${responsesWebsocketBaseMessage} ${
          !profile.responsesWebsocketEnabled
            ? "当前已关闭，Responses 模型会使用 HTTP/SSE。"
            : form.gptReasoningContinuation
              ? "Responses 模型会使用 WebSocket；触发 GPT 推理续接时会复用同一连接。"
              : "Responses 模型会使用 WebSocket。"
        }`
      : responsesWebsocketBaseMessage;
  const responsesWebsocketCheckedAt =
    responsesWebsocketApplicable
    && profile.responsesWebsocket.checkedAtMs
    && profile.responsesWebsocket.checkedAtMs > 0
      ? new Date(profile.responsesWebsocket.checkedAtMs).toLocaleString("zh-CN")
      : "";
  const updateDraft = (patch: Partial<RelayProfile>) => {
    onProfileChange(applyRelayProfilePatchToFiles(profile, patch, { allowGenerateFiles: isNew }));
  };
  const updateModelMappings = (mappings: RelayModelMapping[]) => {
    updateDraft({
      modelMappings: normalizeRelayModelMappings(mappings),
      responsesModelList: "",
      chatCompletionsModelList: "",
      anthropicModelList: "",
      modelList: "",
    });
  };
  const applyFetchedModelChoices = (models: string[]) => {
    const normalized = uniqueStrings(models.map((item) => item.trim()).filter(Boolean)).sort((a, b) =>
      a.localeCompare(b),
    );
    setModelChoices({
      responses: normalized,
      chatCompletions: normalized,
      anthropic: normalized,
    });
    return normalized;
  };
  const fetchModelChoices = async () => {
    if (fetchingModelChoices) return;
    setFetchingModelChoices(true);
    try {
      const models = await actions.fetchRelayProfileModels(profile);
      if (models?.length) applyFetchedModelChoices(models);
    } finally {
      setFetchingModelChoices(false);
    }
  };
  const ensureModelChoices = () => {
    if (fetchingModelChoices) return;
    if (modelChoices.responses.length) return;
    void fetchModelChoices();
  };
  const autoAddModels = async () => {
    if (fetchingModelChoices) return;
    setFetchingModelChoices(true);
    try {
      const models = await actions.fetchRelayProfileModels(profile);
      if (!models?.length) return;
      const fetchedModels = applyFetchedModelChoices(models);
      const existingModels = new Set(
        profile.modelMappings.map((mapping) => mapping.requestModel.trim()).filter(Boolean),
      );
      const additions = fetchedModels
        .filter((model) => !existingModels.has(model))
        .map((requestModel) => ({
          requestModel,
          alias: "",
          protocol: defaultProtocolForModel(requestModel),
          contextWindow: defaultModelContextWindow(requestModel),
        }));
      if (additions.length) updateModelMappings([...profile.modelMappings, ...additions]);
    } finally {
      setFetchingModelChoices(false);
    }
  };
  const probeResponsesWebsocket = async () => {
    if (probingResponsesWebsocket || !responsesWebsocketApplicable) return;
    setProbingResponsesWebsocket(true);
    try {
      const capability = await actions.probeRelayProfileResponsesWebsocket({
        ...profile,
        responsesWebsocket: emptyResponsesWebsocketCapability(),
      });
      if (capability) updateDraft({ responsesWebsocket: capability });
    } finally {
      setProbingResponsesWebsocket(false);
    }
  };
  return (
    <div className="relay-profile-editor">
      <div className="relay-editor-head">
        <div className="relay-editor-title">
          <strong>{profile.name || "未命名供应商"}</strong>
          <span>{relayProfileEditorStatus(profile, form, isNew)}</span>
        </div>
        <div className="relay-editor-actions">
          {showApiFields ? (
            <label
              className="inline-check local-proxy-toggle"
              data-tooltip="启动 ChatGPT/Codex 后会拉起本地协议代理；配合「GPT 推理续接」可解决 GPT 516 降智问题：推理被截断时自动续接，减少长任务中途降智。"
            >
              <input
                checked={profile.localProxyEnabled}
                onChange={(event) => updateDraft({ localProxyEnabled: event.currentTarget.checked })}
                type="checkbox"
              />
              <span>启用本地代理</span>
            </label>
          ) : null}
          {showApiFields ? (
            <Button
              onClick={() => setSystemPromptOpen(true)}
              size="sm"
              title="设置当前供应商的系统提示词"
              variant="secondary"
            >
              <MessageSquareText className="h-4 w-4" />
              替换系统提示词
            </Button>
          ) : null}
          {isNew ? null : (
            <Button
              disabled={!form.relayProfilesEnabled || actions.relaySwitching}
              onClick={onSwitch}
              title={!form.relayProfilesEnabled ? "供应商功能已关闭" : actions.relaySwitching ? "供应商切换中" : undefined}
              variant={profile.id === form.activeRelayId ? "secondary" : "default"}
            >
              {actions.relaySwitching ? "切换中" : profile.id === form.activeRelayId ? "使用中" : "设为当前"}
            </Button>
          )}
        </div>
      </div>
      {isNew ? (
        <ProviderPresetSelector
          onSelect={(patch: PresetPatch) => {
            updateDraft(patch);
          }}
        />
      ) : null}
      <div className="relay-fields">
        <Field className="relay-field-name" label="名称">
          <Input
            value={profile.name}
            onChange={(event) => updateDraft({ name: event.currentTarget.value })}
          />
        </Field>
        <Field className="relay-field-mode" label="接入模式">
          <SelectMenu<RelayMode>
            ariaLabel="接入模式"
            value={profile.relayMode}
            options={[
              { value: "official", label: "官方登录" },
              { value: "pureApi", label: "纯 API" },
            ]}
            onChange={(relayMode) => {
              updateDraft(relayMode === "official" ? { relayMode, officialMixApiKey: false } : { relayMode });
            }}
          />
        </Field>
        <Field className="relay-field-config-model" label="配置模型">
          <Input
            value={profile.model}
            onChange={(event) => updateDraft({ model: event.currentTarget.value })}
            placeholder="写入 config.toml 的 model 字段，例如 gpt-5"
          />
        </Field>
        {profile.relayMode === "official" ? (
          <Field className="relay-field-official-key" label="API Key">
            <label className="inline-check">
              <input
                checked={profile.officialMixApiKey}
                onChange={(event) => updateDraft({ officialMixApiKey: event.currentTarget.checked })}
                type="checkbox"
              />
              <span>混入 API KEY</span>
            </label>
          </Field>
        ) : null}
        {showApiFields ? (
          <div className="relay-api-fields">
            <Field className="relay-field-base-url" label="Base URL">
              <Input
                value={profile.baseUrl}
                onChange={(event) => updateDraft({ baseUrl: event.currentTarget.value })}
                placeholder="填写中转服务 Base URL"
              />
            </Field>
            <Field className="relay-field-key" label="Key">
              <div className="relay-secret-input">
                <Input
                  className="relay-secret-input-control"
                  type={apiKeyVisible ? "text" : "password"}
                  value={profile.apiKey}
                  onChange={(event) => updateDraft({ apiKey: event.currentTarget.value })}
                  placeholder="输入中转服务的 API Key"
                />
                <button
                  aria-label={apiKeyVisible ? "隐藏 API Key" : "显示 API Key"}
                  className="relay-secret-toggle"
                  onClick={() => setApiKeyVisible((current) => !current)}
                  title={apiKeyVisible ? "隐藏 API Key" : "显示 API Key"}
                  type="button"
                >
                  {apiKeyVisible ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
                </button>
              </div>
            </Field>
          </div>
        ) : null}
        <div className="relay-advanced-fields">
          <Field className="relay-field-auto-compact" label="压缩上下文大小">
            <Input
              inputMode="numeric"
              value={profile.autoCompactLimit}
              onChange={(event) => updateDraft({ autoCompactLimit: event.currentTarget.value.replace(/[^\d]/g, "") })}
              placeholder="留空不改写，例如 160000"
            />
          </Field>
          {showApiFields ? (
            <Field className="relay-field-user-agent" label="User-Agent">
              <Input
                value={profile.userAgent}
                onChange={(event) => updateDraft({ userAgent: event.currentTarget.value })}
                placeholder="留空使用默认值"
              />
            </Field>
          ) : null}
        </div>
        {showApiFields ? (
          <Field
            as="div"
            className="relay-field-model-list"
            label="模型列表"
            actions={
              <>
                <Button
                  disabled={fetchingModelChoices}
                  onClick={autoAddModels}
                  size="sm"
                  title="从供应商的模型接口拉取模型，并按模型名称自动选择协议；已有模型不会重复添加。"
                  type="button"
                  variant="secondary"
                >
                  <Download className="h-4 w-4" />
                  {fetchingModelChoices ? "自动添加中" : "自动添加模型"}
                </Button>
                <Button
                  onClick={() =>
                    updateModelMappings([
                      ...profile.modelMappings,
                      { requestModel: "", alias: "", protocol: defaultProtocolForModel(""), contextWindow: defaultModelContextWindow("") },
                    ])
                  }
                  size="sm"
                  type="button"
                  variant="secondary"
                >
                  <Plus className="h-4 w-4" />
                  手动添加模型
                </Button>
              </>
            }
          >
            <RelayModelMappingTable
              choices={modelChoices}
              fetchingChoices={fetchingModelChoices}
              mappings={profile.modelMappings}
              onChange={updateModelMappings}
              onEnsureChoices={ensureModelChoices}
            />
          </Field>
        ) : null}
      </div>
      {showApiFields && profile.localProxyEnabled ? (
        <div className="hint-line relay-protocol-hint">
          <MessageCircle className="h-4 w-4" />
          <span>本地代理优先使用模型列表中的显式协议；未列入时 GPT/OpenAI 系列用 Responses API，其他模型默认用 Anthropic。</span>
        </div>
      ) : null}
      {showApiFields ? (
        <div className="relay-remote-compaction-panel">
          <div className="relay-remote-compaction-copy">
            <strong>Remote Compaction V2</strong>
            <span>
              开启后，保存供应商配置会将
              {" "}
              <code>[model_providers.{relayProfileProviderId(profile)}].name</code>
              {" "}
              写为 OpenAI，以启用 Codex 远程上下文压缩 V2。
            </span>
          </div>
          <label className="relay-remote-compaction-toggle">
            <input
              checked={remoteCompactionV2Enabled}
              onChange={(event) =>
                updateDraft({
                  configContents: setRelayRemoteCompactionV2Enabled(
                    profile.configContents,
                    event.currentTarget.checked,
                  ),
                })
              }
              type="checkbox"
            />
            <span>启用压缩</span>
          </label>
        </div>
      ) : null}
      {showApiFields ? (
        <div className="relay-subagent-panel">
          <div className="relay-remote-compaction-copy">
            <strong>子代理配置</strong>
            <span>V2 模式和子代理数量是两个独立设置，可分别配置。</span>
          </div>
          <div className="relay-subagent-options">
            <div className="relay-subagent-option">
              <div className="relay-subagent-option-copy">
                <strong>V2 模式</strong>
                <span title={`控制 [${MULTI_AGENT_V2_SECTION}].${MULTI_AGENT_V2_ENABLED_KEY}。`}>
                  控制
                  {" "}
                  <code>[{MULTI_AGENT_V2_SECTION}].{MULTI_AGENT_V2_ENABLED_KEY}</code>。
                </span>
              </div>
              <label className="relay-remote-compaction-toggle">
                <input
                  checked={multiAgentV2Enabled}
                  onChange={(event) =>
                    updateDraft({
                      configContents: setRelayMultiAgentV2Enabled(
                        profile.configContents,
                        event.currentTarget.checked,
                      ),
                    })
                  }
                  type="checkbox"
                />
                <span>启动</span>
              </label>
            </div>
            <div className="relay-subagent-option">
              <div className="relay-subagent-option-copy">
                <strong>子代理数量</strong>
                <span
                  title={`写入 [${AGENTS_SECTION}].${AGENTS_MAX_CONCURRENT_THREADS_KEY}，与 V2 模式无关。`}
                >
                  写入
                  {" "}
                  <code>[{AGENTS_SECTION}].{AGENTS_MAX_CONCURRENT_THREADS_KEY}</code>，
                  与 V2 模式无关。
                </span>
              </div>
              <label className="relay-subagent-count">
                <span>数量</span>
                <Input
                  inputMode="numeric"
                  min={1}
                  onChange={(event) =>
                    updateDraft({
                      configContents: setRelaySubagentCount(
                        profile.configContents,
                        Number(event.currentTarget.value),
                      ),
                    })
                  }
                  title="允许同时运行的子代理数量，与 V2 模式开关无关"
                  type="number"
                  value={subagentCount}
                />
              </label>
            </div>
          </div>
        </div>
      ) : null}
      {showApiFields ? (
        <div className="relay-websocket-panel">
          <div className="relay-websocket-status">
            <Network className="h-4 w-4" />
            <div>
              <strong>Responses WebSocket：{responsesWebsocketLabel}</strong>
              <span>{responsesWebsocketMessage}</span>
              {responsesWebsocketCheckedAt ? (
                <small>最近探测：{responsesWebsocketCheckedAt}</small>
              ) : null}
            </div>
          </div>
          <div className="relay-websocket-actions">
            <Button
              disabled={probingResponsesWebsocket || !responsesWebsocketApplicable}
              onClick={() => void probeResponsesWebsocket()}
              size="sm"
              title={
                responsesWebsocketApplicable
                  ? "立即连接上游 Responses WebSocket 端点执行真实握手"
                  : "当前供应商没有可探测的原生 Responses 模型"
              }
              type="button"
              variant="secondary"
            >
              <RefreshCw className="h-4 w-4" />
              {probingResponsesWebsocket ? "探测中" : "重新探测"}
            </Button>
            <label
              className={`relay-websocket-toggle${responsesWebsocketToggleEnabled ? "" : " is-disabled"}`}
              title={
                responsesWebsocketToggleEnabled
                  ? "控制当前供应商是否实际使用 Responses WebSocket"
                  : "仅在 Responses WebSocket 探测为已支持时可启用"
              }
            >
              <input
                checked={responsesWebsocketToggleChecked}
                disabled={!responsesWebsocketToggleEnabled}
                onChange={(event) =>
                  updateDraft({ responsesWebsocketEnabled: event.currentTarget.checked })
                }
                type="checkbox"
              />
              <span>启用 WebSocket</span>
            </label>
          </div>
        </div>
      ) : null}
      <div className="hint-line relay-protocol-hint">
        <ShieldCheck className="h-4 w-4" />
        <span>{relayProfileModeHelp(profile)}</span>
      </div>
      {systemPromptOpen ? (
        <SystemPromptOverrideModal
          value={profile.systemPromptOverride}
          onClose={() => setSystemPromptOpen(false)}
          onSave={(value) => {
            updateDraft({ systemPromptOverride: value });
            setSystemPromptOpen(false);
          }}
        />
      ) : null}
    </div>
  );
}

export function SystemPromptOverrideModal({
  value,
  modelName,
  onClose,
  onSave,
}: {
  value: string;
  modelName?: string;
  onClose: () => void;
  onSave: (value: string) => void;
}) {
  const [draft, setDraft] = useState(value);
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [onClose]);

  return createPortal(
    <div className="modal-backdrop" role="dialog" aria-modal="true" aria-labelledby="system-prompt-title" onClick={onClose}>
      <div className="modal-card system-prompt-modal" onClick={(event) => event.stopPropagation()}>
        <div className="modal-head">
          <div>
            <h2 id="system-prompt-title">{modelName ? `${modelName} · 系统提示词` : "替换系统提示词"}</h2>
            <p>{modelName
              ? "仅对当前模型生效；留空继承供应商配置，供应商也为空时恢复模型默认提示词。保存后请保存供应商配置。本地代理的后续请求立即生效。"
              : "保存后请保存供应商配置。本地代理的后续请求立即生效，留空恢复模型默认提示词；单模型配置优先。直连配置在重启 Elves 后用于新会话。"}</p>
          </div>
          <Button onClick={onClose} size="icon" title="关闭" variant="ghost">
            <X className="h-4 w-4" />
          </Button>
        </div>
        <Textarea
          autoFocus
          className="system-prompt-textarea"
          onChange={(event) => setDraft(event.currentTarget.value)}
          aria-label={modelName ? "模型系统提示词" : "供应商系统提示词"}
          placeholder={modelName ? "输入当前模型的系统提示词；留空继承供应商或模型默认值。" : "输入新的系统提示词；留空恢复模型默认提示词。"}
          value={draft}
        />
        <Toolbar>
          <Button onClick={() => onSave(draft)}>
            <Save className="h-4 w-4" />
            保存
          </Button>
          <Button onClick={() => onSave("")} variant="secondary">清空</Button>
          <Button onClick={onClose} variant="secondary">取消</Button>
        </Toolbar>
      </div>
    </div>,
    document.body,
  );
}

export function RelayModelMappingTable({
  mappings,
  choices,
  fetchingChoices,
  onChange,
  onEnsureChoices,
}: {
  mappings: RelayModelMapping[];
  choices: Record<RelayProtocol, string[]>;
  fetchingChoices: boolean;
  onChange: (mappings: RelayModelMapping[]) => void;
  onEnsureChoices: () => void;
}) {
  const displayRows = mappings.length
    ? mappings
    : [{ requestModel: "", alias: "", protocol: defaultProtocolForModel("") as RelayProtocol, contextWindow: defaultModelContextWindow("") }];
  const canSort = mappings.length > 1;
  const sensors = useSensors(
    useSensor(PointerSensor, {
      activationConstraint: { distance: 8 },
    }),
    useSensor(KeyboardSensor, {
      coordinateGetter: sortableKeyboardCoordinates,
    }),
  );
  const updateRow = (index: number, patch: Partial<RelayModelMapping>) => {
    const next = mappings.length
      ? [...mappings]
      : [{ requestModel: "", alias: "", protocol: defaultProtocolForModel("") as RelayProtocol, contextWindow: defaultModelContextWindow("") }];
    next[index] = {
      ...next[index],
      ...patch,
    };
    onChange(next);
  };
  const updateRowModel = (index: number, row: RelayModelMapping, requestModel: string) => {
    const nextContextWindow = defaultModelContextWindow(requestModel);
    const currentContextWindow = row.contextWindow.trim();
    const previousContextWindow = defaultModelContextWindow(row.requestModel);
    const shouldFillContextWindow =
      !currentContextWindow || currentContextWindow === previousContextWindow;
    // 模型名为空时行上协议还是默认值，选定模型后按模型归属自动纠正；
    // 已手动改过协议的行不覆盖，避免覆盖用户选择。
    const shouldFillProtocol =
      !row.requestModel.trim() || row.protocol === defaultProtocolForModel(row.requestModel);
    updateRow(index, {
      requestModel,
      ...(shouldFillContextWindow ? { contextWindow: nextContextWindow } : {}),
      ...(shouldFillProtocol ? { protocol: defaultProtocolForModel(requestModel) } : {}),
    });
  };
  const removeRow = (index: number) => {
    onChange(mappings.filter((_, itemIndex) => itemIndex !== index));
  };
  const handleDragEnd = (event: DragEndEvent) => {
    const { active, over } = event;
    if (!over || active.id === over.id) return;
    const next = reorderRelayModelMappings(mappings, String(active.id), String(over.id));
    if (next !== mappings) onChange(next);
  };

  return (
    <div className="relay-model-table-wrap">
      <div className="relay-model-table" role="table" aria-label="模型列表">
        <div className="relay-model-table-head" role="row">
          <span aria-hidden="true" />
          <span role="columnheader">请求模型</span>
          <span role="columnheader">别名</span>
          <span role="columnheader">协议</span>
          <span role="columnheader">上下文大小</span>
          <span role="columnheader" aria-label="模型操作" />
        </div>
        <DndContext sensors={sensors} collisionDetection={closestCenter} onDragEnd={handleDragEnd}>
          <SortableContext
            items={displayRows.map((_, index) => relayModelMappingRowId(index))}
            strategy={verticalListSortingStrategy}
          >
            {displayRows.map((row, index) => (
              <SortableRelayModelMappingRow
                canSort={canSort}
                choices={choices}
                fetchingChoices={fetchingChoices}
                index={index}
                key={relayModelMappingRowId(index)}
                mappingsLength={mappings.length}
                onEnsureChoices={onEnsureChoices}
                onRemove={removeRow}
                onUpdateModel={updateRowModel}
                onUpdate={updateRow}
                row={row}
              />
            ))}
          </SortableContext>
        </DndContext>
      </div>
    </div>
  );
}

export function SortableRelayModelMappingRow({
  row,
  index,
  choices,
  fetchingChoices,
  canSort,
  mappingsLength,
  onEnsureChoices,
  onUpdate,
  onUpdateModel,
  onRemove,
}: {
  row: RelayModelMapping;
  index: number;
  choices: Record<RelayProtocol, string[]>;
  fetchingChoices: boolean;
  canSort: boolean;
  mappingsLength: number;
  onEnsureChoices: () => void;
  onUpdate: (index: number, patch: Partial<RelayModelMapping>) => void;
  onUpdateModel: (index: number, row: RelayModelMapping, requestModel: string) => void;
  onRemove: (index: number) => void;
}) {
  const [systemPromptOpen, setSystemPromptOpen] = useState(false);
  const hasSystemPrompt = !!row.systemPromptOverride?.trim();
  const { attributes, listeners, setNodeRef, transform, transition, isDragging } = useSortable({
    id: relayModelMappingRowId(index),
    disabled: !canSort,
  });
  const style: CSSProperties = {
    transform: CSS.Transform.toString(transform),
    transition,
  };

  return (
    <div className={`relay-model-table-row ${isDragging ? "dragging" : ""}`} role="row" ref={setNodeRef} style={style}>
      <div className="relay-model-drag-cell" role="cell">
        <button
          aria-label="拖动模型排序"
          className="relay-model-drag-button"
          data-tooltip={canSort ? "拖动模型排序" : "至少两个模型才能排序"}
          disabled={!canSort}
          type="button"
          {...attributes}
          {...listeners}
        >
          <GripVertical className="h-4 w-4" />
        </button>
      </div>
      <div className="relay-model-request-cell" role="cell">
        <ModelChoiceInput
          choices={choices[row.protocol]}
          loadingChoices={fetchingChoices}
          value={row.requestModel}
          onChange={(value) => onUpdateModel(index, row, value)}
          onEnsureChoices={onEnsureChoices}
          placeholder="点击选择或输入模型"
        />
      </div>
      <div className="relay-model-alias-cell" role="cell">
        <Input
          aria-label="模型别名"
          value={row.alias}
          onChange={(event) => onUpdate(index, { alias: event.currentTarget.value })}
          placeholder="留空显示请求模型"
        />
      </div>
      <div role="cell">
        <SelectMenu
          ariaLabel="协议"
          value={row.protocol}
          options={protocolSelectOptions.map((protocol) => ({
            value: protocol,
            label: relayProtocolLabel(protocol),
          }))}
          onChange={(protocol) => onUpdate(index, { protocol })}
        />
      </div>
      <div role="cell">
        <Input
          inputMode="numeric"
          value={row.contextWindow}
          onChange={(event) => onUpdate(index, { contextWindow: event.currentTarget.value.replace(/[^\d]/g, "") })}
          placeholder="默认 1000000"
        />
      </div>
      <div className="relay-model-delete-cell" role="cell">
        <button
          aria-label="删除模型"
          className="relay-model-delete-button"
          data-tooltip="删除模型"
          disabled={!mappingsLength}
          onClick={() => onRemove(index)}
          type="button"
        >
          <Trash2 className="h-4 w-4" />
        </button>
        <button
          aria-label={`设置${row.alias.trim() || row.requestModel.trim() || "模型"}的系统提示词`}
          className={`relay-model-prompt-button${hasSystemPrompt ? " is-configured" : ""}`}
          data-tooltip={hasSystemPrompt ? "编辑系统提示词（已配置）" : "设置系统提示词"}
          disabled={!row.requestModel.trim()}
          onClick={() => setSystemPromptOpen(true)}
          type="button"
        >
          <MessageSquareText className="h-4 w-4" />
        </button>
      </div>
      {systemPromptOpen ? (
        <SystemPromptOverrideModal
          modelName={row.alias.trim() || row.requestModel.trim()}
          value={row.systemPromptOverride || ""}
          onClose={() => setSystemPromptOpen(false)}
          onSave={(value) => {
            onUpdate(index, { systemPromptOverride: value });
            setSystemPromptOpen(false);
          }}
        />
      ) : null}
    </div>
  );
}

export const protocolSelectOptions: RelayProtocol[] = ["responses", "chatCompletions", "anthropic"];

export type SelectMenuOption<T extends string> = {
  value: T;
  label: React.ReactNode;
};

export type SelectMenuProps<T extends string> = {
  options: SelectMenuOption<T>[];
  disabled?: boolean;
  className?: string;
  menuClassName?: string;
  menuFitContent?: boolean;
  menuMaxWidth?: number;
  menuMinWidth?: number;
  ariaLabel?: string;
  placeholder?: string;
} & ({
  multiple?: false;
  value: T;
  onChange: (value: T) => void;
} | {
  multiple: true;
  value: T[];
  onChange: (value: T[]) => void;
});

export function SelectMenu<T extends string>(props: SelectMenuProps<T>) {
  const {
    options,
    disabled = false,
    className,
    menuClassName,
    menuFitContent = false,
    menuMaxWidth,
    menuMinWidth,
    ariaLabel,
    placeholder,
  } = props;
  const [open, setOpen] = useState(false);
  const [menuStyle, setMenuStyle] = useState<CSSProperties | null>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const isSelected = (value: T) => props.multiple ? props.value.includes(value) : props.value === value;
  const selectedOptions = options.filter((option) => isSelected(option.value));
  const selectionLabel = props.multiple && selectedOptions.length > 1
    ? `已选择 ${selectedOptions.length} 项`
    : selectedOptions[0]?.label ?? placeholder ?? "";

  const updateMenuPosition = () => {
    const button = buttonRef.current;
    if (!button) return;
    const rect = button.getBoundingClientRect();
    const gap = 4;
    const viewportPadding = 8;
    const availableWidth = Math.max(0, window.innerWidth - viewportPadding * 2);
    const contentWidth = menuFitContent
      ? Math.max(
          0,
          ...Array.from(menuRef.current?.querySelectorAll<HTMLElement>(".app-select-option") ?? [])
            .map((option) => {
              const range = document.createRange();
              range.selectNodeContents(option);
              const contentRect = range.getBoundingClientRect();
              const styles = getComputedStyle(option);
              return contentRect.width
                + Number.parseFloat(styles.paddingLeft || "0")
                + Number.parseFloat(styles.paddingRight || "0");
            }),
        ) + 22
      : 0;
    const widthLimit = Math.min(menuMaxWidth ?? availableWidth, availableWidth);
    const menuWidth = Math.min(
      Math.max(rect.width, menuMinWidth ?? rect.width, contentWidth),
      widthLimit,
    );
    const menuLeft = Math.min(
      Math.max(rect.left, viewportPadding),
      window.innerWidth - viewportPadding - menuWidth,
    );
    const desiredHeight = Math.min(options.length * 36 + 8, 280);
    const spaceBelow = window.innerHeight - rect.bottom - viewportPadding;
    const spaceAbove = rect.top - viewportPadding;
    const openAbove = spaceBelow < desiredHeight && spaceAbove > spaceBelow;
    const maxHeight = Math.max(96, Math.min(desiredHeight, openAbove ? spaceAbove - gap : spaceBelow - gap));
    setMenuStyle({
      left: menuLeft,
      maxHeight,
      top: openAbove ? rect.top - gap - maxHeight : rect.bottom + gap,
      width: menuWidth,
    });
  };

  useLayoutEffect(() => {
    if (!open) return;
    updateMenuPosition();
    // 多选变化会清空测速结果并使弹窗重新居中，下拉位置需随选择同步更新。
  }, [open, props.value, options.length]);

  useEffect(() => {
    if (!open) return;
    const handleLayoutChange = () => updateMenuPosition();
    window.addEventListener("resize", handleLayoutChange);
    window.addEventListener("scroll", handleLayoutChange, true);
    return () => {
      window.removeEventListener("resize", handleLayoutChange);
      window.removeEventListener("scroll", handleLayoutChange, true);
    };
  }, [open]);

  const containsSelectTarget = (target: EventTarget | null) =>
    target instanceof Node && (!!rootRef.current?.contains(target) || !!menuRef.current?.contains(target));

  const focusOption = (direction: 1 | -1, current?: HTMLButtonElement | null) => {
    const optionEls = Array.from(menuRef.current?.querySelectorAll<HTMLButtonElement>(".app-select-option") ?? []);
    if (!optionEls.length) return;
    const currentIndex = current ? optionEls.indexOf(current) : -1;
    const fallbackIndex = Math.max(0, options.findIndex((option) => isSelected(option.value)));
    const nextIndex = currentIndex < 0 ? fallbackIndex : (currentIndex + direction + optionEls.length) % optionEls.length;
    optionEls[nextIndex]?.focus();
  };

  const selectValue = (next: T) => {
    // 测速多选需连续勾选或取消，保留下拉框；现有单选仍在选中后关闭。
    if (props.multiple) {
      props.onChange(isSelected(next) ? props.value.filter((value) => value !== next) : [...props.value, next]);
      return;
    }
    props.onChange(next);
    setOpen(false);
    buttonRef.current?.focus();
  };

  const menu = open ? (
    <div
      className={`app-select-menu${menuClassName ? ` ${menuClassName}` : ""}`}
      onBlur={(event) => {
        if (containsSelectTarget(event.relatedTarget)) return;
        setOpen(false);
      }}
      ref={menuRef}
      aria-multiselectable={props.multiple || undefined}
      role="listbox"
      style={menuStyle ?? undefined}
    >
      {options.map((option) => (
        <button
          aria-selected={isSelected(option.value)}
          className={`app-select-option${props.multiple ? " app-select-option-multiple" : ""}`}
          key={option.value}
          onClick={() => selectValue(option.value)}
          onKeyDown={(event) => {
            if (event.key === "Escape") {
              if (props.multiple) event.stopPropagation();
              setOpen(false);
              buttonRef.current?.focus();
            }
            if (event.key === "ArrowDown") {
              event.preventDefault();
              focusOption(1, event.currentTarget);
            }
            if (event.key === "ArrowUp") {
              event.preventDefault();
              focusOption(-1, event.currentTarget);
            }
            if (event.key === "Enter" || event.key === " ") {
              event.preventDefault();
              selectValue(option.value);
            }
          }}
          onMouseDown={(event) => event.preventDefault()}
          role="option"
          type="button"
        >
          {props.multiple ? (
            <Check aria-hidden="true" className="h-4 w-4" visibility={isSelected(option.value) ? "visible" : "hidden"} />
          ) : null}
          {option.label}
        </button>
      ))}
    </div>
  ) : null;

  return (
    <div
      className={`app-select${className ? ` ${className}` : ""}`}
      onBlur={(event) => {
        if (containsSelectTarget(event.relatedTarget)) return;
        setOpen(false);
      }}
      ref={rootRef}
    >
      <button
        aria-expanded={open}
        aria-haspopup="listbox"
        aria-label={ariaLabel}
        className="app-select-trigger"
        disabled={disabled}
        onClick={() => setOpen((current) => !current)}
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            if (props.multiple && open) event.stopPropagation();
            setOpen(false);
            return;
          }
          if (event.key === "ArrowDown") {
            event.preventDefault();
            setOpen(true);
            requestAnimationFrame(() => focusOption(1));
          }
          if (event.key === "ArrowUp") {
            event.preventDefault();
            setOpen(true);
            requestAnimationFrame(() => focusOption(-1));
          }
        }}
        ref={buttonRef}
        type="button"
      >
        <span>{selectionLabel}</span>
        <ChevronDown className="h-4 w-4" aria-hidden="true" />
      </button>
      {menu ? createPortal(menu, document.body) : null}
    </div>
  );
}

export function ModelChoiceInput({
  value,
  choices,
  loadingChoices = false,
  placeholder,
  onChange,
  onEnsureChoices,
}: {
  value: string;
  choices: string[];
  loadingChoices?: boolean;
  placeholder?: string;
  onChange: (value: string, source: ModelChoiceChangeSource) => void;
  onEnsureChoices?: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [menuStyle, setMenuStyle] = useState<CSSProperties | null>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const normalizedChoices = useMemo(
    () => uniqueStrings(choices.map((item) => item.trim()).filter(Boolean)).sort((a, b) => a.localeCompare(b)),
    [choices],
  );
  const filteredChoices = useMemo(() => {
    const keyword = value.trim().toLowerCase();
    const source = keyword
      ? normalizedChoices.filter((model) => model.toLowerCase().includes(keyword))
      : normalizedChoices;
    return source.slice(0, 80);
  }, [normalizedChoices, value]);

  const updateMenuPosition = () => {
    const input = inputRef.current;
    if (!input) return;
    const rect = input.getBoundingClientRect();
    const gap = 4;
    const viewportPadding = 8;
    const spaceBelow = window.innerHeight - rect.bottom - viewportPadding;
    const spaceAbove = rect.top - viewportPadding;
    const openAbove = spaceBelow < 140 && spaceAbove > spaceBelow;
    const maxHeight = Math.max(120, Math.min(220, openAbove ? spaceAbove - gap : spaceBelow - gap));
    setMenuStyle({
      bottom: openAbove ? window.innerHeight - rect.top + gap : "auto",
      left: rect.left,
      maxHeight,
      top: openAbove ? "auto" : rect.bottom + gap,
      width: rect.width,
    });
  };

  useLayoutEffect(() => {
    if (!open) return;
    updateMenuPosition();
  }, [open, filteredChoices.length, normalizedChoices.length]);

  useEffect(() => {
    if (!open) return;
    const handleLayoutChange = () => updateMenuPosition();
    window.addEventListener("resize", handleLayoutChange);
    window.addEventListener("scroll", handleLayoutChange, true);
    return () => {
      window.removeEventListener("resize", handleLayoutChange);
      window.removeEventListener("scroll", handleLayoutChange, true);
    };
  }, [open]);

  useEffect(() => {
    if (open && !normalizedChoices.length) onEnsureChoices?.();
  }, [open, normalizedChoices.length, onEnsureChoices]);

  const containsModelChoiceTarget = (target: EventTarget | null) =>
    target instanceof Node && (!!rootRef.current?.contains(target) || !!menuRef.current?.contains(target));

  const focusOption = (direction: 1 | -1, current?: HTMLButtonElement | null) => {
    const options = Array.from(menuRef.current?.querySelectorAll<HTMLButtonElement>(".relay-model-choice-option") ?? []);
    if (!options.length) return;
    const currentIndex = current ? options.indexOf(current) : -1;
    const nextIndex = currentIndex < 0 ? 0 : (currentIndex + direction + options.length) % options.length;
    options[nextIndex]?.focus();
  };

  const menu = open ? (
    <div
      className="relay-model-choice-menu"
      onBlur={(event) => {
        if (containsModelChoiceTarget(event.relatedTarget)) return;
        setOpen(false);
      }}
      ref={menuRef}
      role="listbox"
      style={menuStyle ?? undefined}
    >
      {normalizedChoices.length ? (
        filteredChoices.length ? (
          filteredChoices.map((model) => (
            <button
              aria-selected={model === value}
              className="relay-model-choice-option"
              key={model}
              onClick={() => {
                onChange(model, "select");
                setOpen(false);
                inputRef.current?.focus();
              }}
              onKeyDown={(event) => {
                if (event.key === "Escape") {
                  setOpen(false);
                  inputRef.current?.focus();
                }
                if (event.key === "ArrowDown") {
                  event.preventDefault();
                  focusOption(1, event.currentTarget);
                }
                if (event.key === "ArrowUp") {
                  event.preventDefault();
                  focusOption(-1, event.currentTarget);
                }
              }}
              onMouseDown={(event) => event.preventDefault()}
              role="option"
              type="button"
            >
              {model}
            </button>
          ))
        ) : (
          <div className="relay-model-choice-empty">无匹配模型</div>
        )
      ) : (
        <div className="relay-model-choice-empty">
          {loadingChoices ? "正在获取模型列表…" : "未获取到模型，可直接输入"}
        </div>
      )}
    </div>
  ) : null;

  return (
    <div
      className="relay-model-choice"
      onBlur={(event) => {
        if (containsModelChoiceTarget(event.relatedTarget)) return;
        setOpen(false);
      }}
      ref={rootRef}
    >
      <Input
        aria-autocomplete="list"
        aria-expanded={open}
        aria-haspopup="listbox"
        onChange={(event) => {
          onChange(event.currentTarget.value, "input");
          setOpen(true);
        }}
        onClick={() => setOpen(true)}
        onFocus={() => setOpen(true)}
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            setOpen(false);
            return;
          }
          if (event.key === "ArrowDown") {
            event.preventDefault();
            setOpen(true);
            requestAnimationFrame(() => focusOption(1));
          }
        }}
        placeholder={placeholder}
        ref={inputRef}
        value={value}
      />
       {menu ? createPortal(menu, document.body) : null}
     </div>
   );
 }

export const CONVERSATION_PROJECT_KEY = "__codex_conversation_project__";

export const CONVERSATION_PROJECT_LABEL = "无项目任务";

export function sessionProjectKey(path: string, projectlessWorkspaceRoot: string): string {
  const trimmed = path.trim();
  if (!trimmed) return "";
  return isCodexConversationProjectPath(trimmed, projectlessWorkspaceRoot) ? CONVERSATION_PROJECT_KEY : trimmed;
}

export function sessionProjectLabel(pathOrKey: string): string {
  if (pathOrKey === CONVERSATION_PROJECT_KEY) {
    return CONVERSATION_PROJECT_LABEL;
  }
  return projectLabel(pathOrKey);
}

export function isCodexConversationProjectPath(path: string, workspaceRoot: string): boolean {
  const normalize = (value: string) => {
    const slashed = value.trim().replace(/\\/g, "/")
      .replace(/^\/\/\?\/UNC\//i, "//").replace(/^\/\/\?\//, "");
    const prefix = slashed.startsWith("//") ? "//" : slashed.startsWith("/") ? "/" : "";
    const parts: string[] = [];
    for (const part of slashed.split("/")) {
      if (!part || part === ".") continue;
      if (part === "..") parts.pop();
      else parts.push(part);
    }
    return prefix + parts.join("/");
  };
  let root = normalize(workspaceRoot);
  let cwd = normalize(path);
  if (!root || !cwd) return false;
  if (/^(?:[a-z]:|\/\/)/i.test(root)) {
    root = root.toLowerCase();
    cwd = cwd.toLowerCase();
  }
  return cwd === root || cwd.startsWith(root.endsWith("/") ? root : `${root}/`);
}

export function projectLabel(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, "");
  const parts = trimmed.split(/[\\/]+/).filter(Boolean);
  return parts.length ? parts[parts.length - 1] : path;
}

export function SessionProjectSelect({
  value,
  options,
  projectlessWorkspaceRoot,
  onChange,
}: {
  value: string;
  options: Array<{ value: string; count: number }>;
  projectlessWorkspaceRoot: string;
  onChange: (value: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [keyword, setKeyword] = useState("");
  const [menuStyle, setMenuStyle] = useState<CSSProperties | null>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  const totalCount = useMemo(() => options.reduce((sum, item) => sum + item.count, 0), [options]);
  const filtered = useMemo(() => {
    const kw = keyword.trim().toLowerCase();
    if (!kw) return options;
    return options.filter((item) => {
      const label = sessionProjectLabel(item.value).toLowerCase();
      const path = item.value === CONVERSATION_PROJECT_KEY ? projectlessWorkspaceRoot : item.value;
      return label.includes(kw) || path.toLowerCase().includes(kw);
    });
  }, [options, keyword, projectlessWorkspaceRoot]);

  const updatePosition = () => {
    const root = rootRef.current;
    if (!root) return;
    const rect = root.getBoundingClientRect();
    const gap = 4;
    const viewportPadding = 8;
    const spaceBelow = window.innerHeight - rect.bottom - viewportPadding;
    const spaceAbove = rect.top - viewportPadding;
    const openAbove = spaceBelow < 220 && spaceAbove > spaceBelow;
    const maxHeight = Math.max(160, Math.min(320, openAbove ? spaceAbove - gap : spaceBelow - gap));
    setMenuStyle({
      left: rect.left,
      maxHeight,
      top: openAbove ? rect.top - gap - maxHeight : rect.bottom + gap,
      width: rect.width,
    });
  };

  useLayoutEffect(() => {
    if (!open) return;
    updatePosition();
  }, [open, filtered.length]);

  useEffect(() => {
    if (!open) return;
    const handleLayoutChange = () => updatePosition();
    window.addEventListener("resize", handleLayoutChange);
    window.addEventListener("scroll", handleLayoutChange, true);
    return () => {
      window.removeEventListener("resize", handleLayoutChange);
      window.removeEventListener("scroll", handleLayoutChange, true);
    };
  }, [open]);

  useEffect(() => {
    if (open) {
      setKeyword("");
      requestAnimationFrame(() => inputRef.current?.focus());
    }
  }, [open]);

  const containsTarget = (target: EventTarget | null) =>
    target instanceof Node && (!!rootRef.current?.contains(target) || !!menuRef.current?.contains(target));

  const choose = (next: string) => {
    onChange(next);
    setOpen(false);
  };

  const triggerText = value ? sessionProjectLabel(value) : "全部项目";

  const menu = open ? (
    <div className="session-project-menu" ref={menuRef} style={menuStyle ?? undefined}>
      <div className="session-project-search">
        <Search className="h-4 w-4" />
        <input
          ref={inputRef}
          value={keyword}
          placeholder="搜索项目路径…"
          onChange={(event) => setKeyword(event.currentTarget.value)}
          onKeyDown={(event) => {
            if (event.key === "Escape") setOpen(false);
          }}
        />
      </div>
      <div className="session-project-options">
        <button
          type="button"
          className={"session-project-option" + (value === "" ? " active" : "")}
          onMouseDown={(event) => event.preventDefault()}
          onClick={() => choose("")}
        >
          <span className="session-project-option-name">全部项目</span>
          <span className="session-project-option-count">{totalCount}</span>
        </button>
        {filtered.length ? (
          filtered.map((item) => (
            <button
              key={item.value}
              type="button"
              data-tooltip={sessionProjectLabel(item.value)}
              className={"session-project-option" + (value === item.value ? " active" : "")}
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => choose(item.value)}
            >
              <span className="session-project-option-name">
                <strong>{sessionProjectLabel(item.value)}</strong>
                <small>{item.value === CONVERSATION_PROJECT_KEY ? projectlessWorkspaceRoot : item.value}</small>
              </span>
              <span className="session-project-option-count">{item.count}</span>
            </button>
          ))
        ) : (
          <div className="session-project-empty">无匹配项目</div>
        )}
      </div>
    </div>
  ) : null;

  return (
    <div
      className="session-project-select"
      ref={rootRef}
      onBlur={(event) => {
        if (containsTarget(event.relatedTarget)) return;
        setOpen(false);
      }}
    >
      <button type="button" className="session-trigger" onClick={() => setOpen((prev) => !prev)}>
        <span className="session-trigger-text" data-tooltip={value ? sessionProjectLabel(value) : "全部项目"}>
          {triggerText}
        </span>
        <ChevronDown className="h-4 w-4" />
      </button>
      {menu ? createPortal(menu, document.body) : null}
    </div>
  );
}

export function startOfDayMs(date: Date): number {
  return new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
}

export function daysAgoStartMs(days: number): number {
  const d = new Date();
  d.setDate(d.getDate() - days);
  return startOfDayMs(d);
}

export function formatDateMs(ms: number | null): string {
  if (ms === null) return "";
  const d = new Date(ms);
  const mm = String(d.getMonth() + 1).padStart(2, "0");
  const dd = String(d.getDate()).padStart(2, "0");
  return `${d.getFullYear()}-${mm}-${dd}`;
}

export function SessionTimeRangePicker({
  startMs,
  endMs,
  onChange,
}: {
  startMs: number | null;
  endMs: number | null;
  onChange: (start: number | null, end: number | null) => void;
}) {
  const [open, setOpen] = useState(false);
  const [menuStyle, setMenuStyle] = useState<CSSProperties | null>(null);
  const [viewMonth, setViewMonth] = useState(() => {
    const base = endMs ?? startMs ?? Date.now();
    const d = new Date(base);
    return new Date(d.getFullYear(), d.getMonth(), 1);
  });
  const rootRef = useRef<HTMLDivElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  const updatePosition = () => {
    const root = rootRef.current;
    if (!root) return;
    const rect = root.getBoundingClientRect();
    const gap = 4;
    const viewportPadding = 8;
    const menuWidth = 420;
    const spaceBelow = window.innerHeight - rect.bottom - viewportPadding;
    const spaceAbove = rect.top - viewportPadding;
    const openAbove = spaceBelow < 320 && spaceAbove > spaceBelow;
    // 优先与触发器右缘对齐（弹窗向左展开），避免右侧贴边被遮
    const maxLeft = window.innerWidth - menuWidth - viewportPadding;
    const left = Math.max(viewportPadding, Math.min(rect.right - menuWidth, maxLeft));
    setMenuStyle({
      left,
      top: openAbove ? undefined : rect.bottom + gap,
      bottom: openAbove ? window.innerHeight - rect.top + gap : undefined,
      width: menuWidth,
    });
  };

  useLayoutEffect(() => {
    if (!open) return;
    updatePosition();
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const handleLayoutChange = () => updatePosition();
    window.addEventListener("resize", handleLayoutChange);
    window.addEventListener("scroll", handleLayoutChange, true);
    return () => {
      window.removeEventListener("resize", handleLayoutChange);
      window.removeEventListener("scroll", handleLayoutChange, true);
    };
  }, [open]);

  const containsTarget = (target: EventTarget | null) =>
    target instanceof Node && (!!rootRef.current?.contains(target) || !!menuRef.current?.contains(target));

  const quickPresets: Array<{ label: string; apply: () => void }> = [
    { label: "最近 1 周", apply: () => onChange(daysAgoStartMs(7), startOfDayMs(new Date())) },
    { label: "最近 2 周", apply: () => onChange(daysAgoStartMs(14), startOfDayMs(new Date())) },
    { label: "最近 3 周", apply: () => onChange(daysAgoStartMs(21), startOfDayMs(new Date())) },
    { label: "最近 1 月", apply: () => onChange(daysAgoStartMs(30), startOfDayMs(new Date())) },
    { label: "1 周前（更早）", apply: () => onChange(null, daysAgoStartMs(7)) },
    { label: "2 周前（更早）", apply: () => onChange(null, daysAgoStartMs(14)) },
    { label: "3 周前（更早）", apply: () => onChange(null, daysAgoStartMs(21)) },
    { label: "1 月前（更早）", apply: () => onChange(null, daysAgoStartMs(30)) },
  ];

  const onPickDay = (dayMs: number) => {
    if (startMs === null || endMs !== null) {
      onChange(dayMs, null);
      return;
    }
    if (dayMs < startMs) {
      onChange(dayMs, startMs);
    } else {
      onChange(startMs, dayMs);
    }
  };

  const monthGrid = useMemo(() => {
    const year = viewMonth.getFullYear();
    const month = viewMonth.getMonth();
    const firstWeekday = new Date(year, month, 1).getDay();
    const daysInMonth = new Date(year, month + 1, 0).getDate();
    const cells: Array<number | null> = [];
    for (let i = 0; i < firstWeekday; i += 1) cells.push(null);
    for (let d = 1; d <= daysInMonth; d += 1) cells.push(new Date(year, month, d).getTime());
    return cells;
  }, [viewMonth]);

  const triggerText =
    startMs === null && endMs === null
      ? "全部时间"
      : `${formatDateMs(startMs) || "不限"} ~ ${formatDateMs(endMs) || "不限"}`;

  const menu = open ? (
    <div className="session-date-menu" ref={menuRef} style={menuStyle ?? undefined}>
      <div className="session-date-quick">
        {quickPresets.map((preset) => (
          <button
            key={preset.label}
            type="button"
            className="session-date-quick-item"
            onMouseDown={(event) => event.preventDefault()}
            onClick={preset.apply}
          >
            {preset.label}
          </button>
        ))}
      </div>
      <div className="session-date-calendar">
        <div className="session-date-header">
          <button
            type="button"
            className="session-date-nav"
            onMouseDown={(event) => event.preventDefault()}
            onClick={() => setViewMonth((prev) => new Date(prev.getFullYear(), prev.getMonth() - 1, 1))}
          >
            ‹
          </button>
          <span>
            {viewMonth.getFullYear()} 年 {viewMonth.getMonth() + 1} 月
          </span>
          <button
            type="button"
            className="session-date-nav"
            onMouseDown={(event) => event.preventDefault()}
            onClick={() => setViewMonth((prev) => new Date(prev.getFullYear(), prev.getMonth() + 1, 1))}
          >
            ›
          </button>
        </div>
        <div className="session-date-weekdays">
          {["日", "一", "二", "三", "四", "五", "六"].map((w) => (
            <span key={w}>{w}</span>
          ))}
        </div>
        <div className="session-date-grid">
          {monthGrid.map((dayMs, index) => {
            if (dayMs === null) return <span key={`empty-${index}`} className="session-date-cell empty" />;
            const isStart = startMs !== null && dayMs === startMs;
            const isEnd = endMs !== null && dayMs === endMs;
            const inRange = startMs !== null && endMs !== null && dayMs >= startMs && dayMs <= endMs;
            return (
              <button
                key={dayMs}
                type="button"
                className={
                  "session-date-cell" +
                  (inRange ? " in-range" : "") +
                  (isStart || isEnd ? " selected" : "")
                }
                onMouseDown={(event) => event.preventDefault()}
                onClick={() => onPickDay(dayMs)}
              >
                {new Date(dayMs).getDate()}
              </button>
            );
          })}
        </div>
        <div className="session-date-footer">
          <span>{triggerText}</span>
          <div className="session-date-footer-actions">
            <button
              type="button"
              className="session-date-clear"
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => onChange(null, null)}
            >
              清空
            </button>
            <button
              type="button"
              className="session-date-done"
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => setOpen(false)}
            >
              完成
            </button>
          </div>
        </div>
      </div>
    </div>
  ) : null;

  return (
    <div
      className="session-date-select"
      ref={rootRef}
      onBlur={(event) => {
        if (containsTarget(event.relatedTarget)) return;
        setOpen(false);
      }}
    >
      <button type="button" className="session-trigger" onClick={() => setOpen((prev) => !prev)}>
        <Calendar className="h-4 w-4" />
        <span className="session-trigger-text">{triggerText}</span>
        <ChevronDown className="h-4 w-4" />
      </button>
      {menu ? createPortal(menu, document.body) : null}
    </div>
  );
}

export function AggregateRelayProfileEditor({
  profile,
  form,
  isNew = false,
  onProfileChange,
}: {
  profile: RelayProfile;
  form: BackendSettings;
  isNew?: boolean;
  onProfileChange: (value: RelayProfile) => void;
}) {
  const candidates = aggregateMemberCandidates(form, profile.id);
  const aggregate = normalizeAggregateConfig(profile.aggregate, candidates);
  const memberIds = new Set(aggregate.members.map((member) => member.profileId));
  const updateAggregate = (nextAggregate: RelayAggregateConfig) => {
    onProfileChange(normalizeAggregateRelayProfile({ ...profile, aggregate: nextAggregate }, form));
  };
  const toggleMember = (profileId: string, checked: boolean) => {
    const members = checked
      ? [...aggregate.members, { profileId, weight: 1 }]
      : aggregate.members.filter((member) => member.profileId !== profileId);
    updateAggregate({ ...aggregate, members });
  };
  const updateWeight = (profileId: string, weight: number) => {
    updateAggregate({
      ...aggregate,
      members: aggregate.members.map((member) =>
        member.profileId === profileId ? { ...member, weight: clampAggregateWeight(weight) } : member,
      ),
    });
  };
  return (
    <div className="relay-profile-editor aggregate-editor">
      <div className="relay-editor-head">
        <div>
          <strong>{profile.name || "未命名聚合供应商"}</strong>
          <span>{isNew ? "选择已有供应商作为成员，并按策略分配请求" : "聚合配置只引用已有供应商，不复制 Key 和配置文件"}</span>
        </div>
        <UiBadge variant="secondary">聚合</UiBadge>
      </div>
      <div className="relay-fields aggregate-fields">
        <Field className="relay-field-name" label="名称">
          <Input
            value={profile.name}
            onChange={(event) => onProfileChange({ ...profile, name: event.currentTarget.value })}
            placeholder="例如 主力聚合池"
          />
        </Field>
        <Field className="aggregate-strategy-field" label="聚合策略">
          <SelectMenu<RelayAggregateStrategy>
            ariaLabel="聚合策略"
            value={aggregate.strategy}
            options={aggregateStrategyOptions.map((option) => ({ value: option.value, label: option.label }))}
            onChange={(strategy) => updateAggregate({ ...aggregate, strategy })}
          />
        </Field>
      </div>
      <div className="aggregate-strategy-grid">
        {aggregateStrategyOptions.map((option) => (
          <button
            className={`mode-option aggregate-strategy-option ${aggregate.strategy === option.value ? "active" : ""}`}
            key={option.value}
            onClick={() => updateAggregate({ ...aggregate, strategy: option.value })}
            type="button"
          >
            <strong>{option.label}</strong>
            <span>{option.description}</span>
          </button>
        ))}
      </div>
      <div className="aggregate-members">
        <div className="aggregate-members-head">
          <div>
            <strong>成员供应商</strong>
            <span>只能勾选已填写 Base URL / Key 的 API 供应商，聚合供应商不会作为成员。</span>
          </div>
          <UiBadge variant="outline">{aggregate.members.length} / {candidates.length}</UiBadge>
        </div>
        {candidates.length ? (
          <div className="aggregate-member-list">
            {candidates.map((candidate) => {
              const member = aggregate.members.find((item) => item.profileId === candidate.id);
              const checked = memberIds.has(candidate.id);
              return (
                <label className={`aggregate-member-row ${checked ? "selected" : ""}`} key={candidate.id}>
                  <input
                    checked={checked}
                    onChange={(event) => toggleMember(candidate.id, event.currentTarget.checked)}
                    type="checkbox"
                  />
                  <span className="aggregate-member-summary">
                    <strong>{candidate.name || "未命名供应商"}</strong>
                    <small>{relayModeLabel(candidate.relayMode)} · {relayProtocolLabel(candidate.protocol)} · {relayProfileConfigBrief(candidate)}</small>
                  </span>
                  <span className="aggregate-weight-box">
                    <span>权重</span>
                    <Input
                      disabled={!checked}
                      min={1}
                      onChange={(event) => updateWeight(candidate.id, Number.parseInt(event.currentTarget.value, 10))}
                      type="number"
                      value={String(member?.weight ?? 1)}
                    />
                  </span>
                </label>
              );
            })}
          </div>
        ) : (
          <div className="empty">先添加至少 1 个已填写 Base URL / Key 的 API 供应商，再创建聚合供应商。</div>
        )}
      </div>
    </div>
  );
}

export function AggregateRelayImpactPanel({ profile, form, isNew = false }: { profile: RelayProfile; form: BackendSettings; isNew?: boolean }) {
  const candidates = aggregateMemberCandidates(form, profile.id);
  const aggregate = normalizeAggregateConfig(profile.aggregate, candidates);
  const memberById = new Map(candidates.map((candidate) => [candidate.id, candidate]));
  const totalWeight = aggregate.members.reduce((total, member) => total + clampAggregateWeight(member.weight), 0);
  const isActive = !isNew && profile.id === form.activeRelayId;
  const canSwitchToThis = !isNew && !isActive;
  const memberDetails = aggregate.members.length
    ? aggregate.members
        .map((member) => {
          const candidate = memberById.get(member.profileId);
          const name = candidate?.name || member.profileId;
          return `${name} / 权重 ${clampAggregateWeight(member.weight)}`;
        })
        .join("；")
    : "未选择成员，保存按钮会保持禁用。";
  const rows: RelayActivationImpactRow[] = [
    {
      file: "settings payload",
      field: "relayProfiles[].aggregate.strategy",
      value: aggregateStrategyLabel(aggregate.strategy),
      detail: `保存聚合策略为 ${aggregate.strategy}。${aggregateStrategyHelp(aggregate.strategy)}`,
      tone: "write",
    },
    {
      file: "settings payload",
      field: "relayProfiles[].aggregate.members",
      value: `${aggregate.members.length} 个`,
      detail: memberDetails,
      tone: aggregate.members.length ? "write" : "remove",
    },
    {
      file: "settings payload",
      field: "aggregateRelayProfiles[]",
      value: "同步当前聚合供应商",
      detail: `由 relayProfiles[].aggregate 同步生成；当前聚合包含 ${aggregate.members.length} 个成员，供本地代理按聚合策略读取。`,
      tone: aggregate.members.length ? "write" : "remove",
    },
    {
      file: "settings payload",
      field: "aggregate.members[].weight",
      value: String(totalWeight),
      detail: "保存成员权重；仅权重轮转策略会按权重分配更多请求，其它策略仍保留权重数据。",
      tone: aggregate.strategy === "weightedRoundRobin" ? "write" : "file",
    },
  ];
  const switchRows: RelayActivationImpactRow[] = [
    {
      file: "设为当前后",
      field: "activeRelayId / activeAggregateRelayId",
      value: profile.id,
      detail: "回列表点击“使用”后会一并保存当前编辑内容，切换 activeRelayId，并把当前聚合 ID 写入 settings payload。",
      tone: "write",
    },
    {
      file: "设为当前后",
      field: "config.toml",
      value: "本地协议代理",
      detail: "回列表点击“使用”后会把当前 provider 的 base_url 写成本地协议代理地址，由代理按聚合策略转发。",
      tone: "file",
    },
    {
      file: "设为当前后",
      field: "[model_providers.custom].supports_websockets",
      value: "false",
      detail: "聚合供应商固定不启用原生 Responses WebSocket，避免沿用上一供应商的 true。",
      tone: "write",
    },
    {
      file: "设为当前后",
      field: "auth.json",
      value: "codex-elves-aggregate",
      detail: "回列表点击“使用”后会写入聚合代理占位 Key；不会复制任何成员供应商的真实 Key。",
      tone: "file",
    },
  ];
  const groupedRows = relayActivationImpactGroups(canSwitchToThis ? [...rows, ...switchRows] : rows);

  return (
    <div className="relay-file-grid aggregate-impact-grid">
      <div className="relay-file-panel relay-impact-panel">
        <div className="relay-file-head">
          <div>
            <strong>{canSwitchToThis ? "保存 / 设为当前影响" : "保存影响"}</strong>
            <span>
              {canSwitchToThis
                ? "保存只写 settings payload；回列表点击“使用”才会更新 Codex config.toml / auth.json。"
                : isActive
                  ? "保存只更新聚合 settings payload；当前 Codex config.toml / auth.json 不会因保存动作重新写入。"
                  : "保存只写 settings payload，不复制成员 Key，也不直接写 Codex config.toml / auth.json。"}
            </span>
          </div>
        </div>
        <div className="relay-impact-summary">
          <span>聚合供应商</span>
          <strong>{aggregateStrategyLabel(aggregate.strategy)} · {aggregate.members.length} 个成员</strong>
        </div>
        <div className="relay-impact-groups">
          {groupedRows.map((group) => (
            <div className="relay-impact-group" key={group.file}>
              <div className="relay-impact-group-head">
                <strong>{group.file}</strong>
                <span>{group.rows.length} 项</span>
              </div>
              <div className="relay-impact-table" role="table" aria-label={`${group.file} 操作影响清单`}>
                <div className="relay-impact-table-head" role="row">
                  <span>字段</span>
                  <span>值 / 动作</span>
                </div>
                {group.rows.map((row) => (
                  <div className="relay-impact-row" data-tone={row.tone} key={`${row.file}-${row.field}`} role="row">
                    <span className="relay-impact-field" role="cell">
                      <code>{row.field}</code>
                      <small>{row.detail}</small>
                    </span>
                    <strong className="relay-impact-value" role="cell">{row.value}</strong>
                  </div>
                ))}
              </div>
            </div>
          ))}
        </div>
        <div className="hint-line relay-protocol-hint">
          <ShieldCheck className="h-4 w-4" />
          <span>聚合供应商不会复制成员 Key；真实对话只有在设为当前后才会走本地协议代理按策略轮转成员。</span>
        </div>
      </div>
    </div>
  );
}

export function SyncedTextarea({
  value,
  onValueChange,
  className,
}: {
  value: string;
  onValueChange: (value: string) => void;
  className?: string;
}) {
  const [localValue, setLocalValue] = useState(value);
  const isFocusedRef = useRef(false);
  const latestExternalValueRef = useRef(value);

  useEffect(() => {
    latestExternalValueRef.current = value;
    if (!isFocusedRef.current) {
      setLocalValue(value);
    }
  }, [value]);

  return (
    <Textarea
      className={className}
      value={localValue}
      onBlur={() => {
        isFocusedRef.current = false;
        setLocalValue(latestExternalValueRef.current);
      }}
      onChange={(event) => {
        const next = event.currentTarget.value;
        setLocalValue(next);
        onValueChange(next);
      }}
      onFocus={() => {
        isFocusedRef.current = true;
      }}
      spellCheck={false}
    />
  );
}

export type RelayActivationImpactTone = "write" | "skip" | "remove" | "file";

export type RelayActivationImpactRow = {
  file: string;
  field: string;
  value: string;
  detail: string;
  tone: RelayActivationImpactTone;
};

export function RelayActivationPanel({
  profile,
  isActive,
  onProfileChange,
}: {
  profile: RelayProfile;
  isActive: boolean;
  onProfileChange: (value: RelayProfile) => void;
}) {
  const rows = relayActivationImpactRows(profile);
  const groupedRows = relayActivationImpactGroups(rows.filter((row) => row.file !== "auth.json" && row.tone !== "skip"));
  const apiKey = relayProfileEffectiveApiKey(profile);
  const authApiKeyState = profile.relayMode === "pureApi" ? sensitiveStatus(apiKey) : "不写入";
  const authWriteTarget =
    profile.relayMode === "pureApi"
      ? "~/.codex/auth.json"
      : "不写入 auth.json";
  const authWriteDetail =
    profile.relayMode === "pureApi"
      ? ""
      : profile.relayMode === "official" && !profile.officialMixApiKey
        ? "保留登录态。"
        : "Key 写入 bearer token。";
  const authApiKeyTone = authApiKeyState === "已配置" ? "ready" : authApiKeyState === "不写入" ? "muted" : "missing";
  const authStatusLabel = authApiKeyState === "未配置" ? "缺少 Key" : authApiKeyState;
  return (
    <div className="relay-file-grid relay-activation-grid">
      <div className="relay-file-panel relay-impact-panel">
        <div className="relay-file-head">
          <div>
            <strong>启用后会修改</strong>
            <span>仅更新必要的 Codex 配置字段。</span>
          </div>
        </div>
        <div className="relay-impact-summary">
          <span>{relayModeLabel(profile.relayMode)}</span>
          <strong>{relayProfileConfigBrief(profile)}</strong>
        </div>
        <div className="relay-impact-groups">
          {groupedRows.map((group) => (
            <div className="relay-impact-group" key={group.file}>
              <div className="relay-impact-group-head">
                <strong>{group.file}</strong>
                <span>{group.rows.length} 项</span>
              </div>
              <div className="relay-impact-table" role="table" aria-label={`${group.file} 启用修改清单`}>
                <div className="relay-impact-table-head" role="row">
                  <span>字段</span>
                  <span>值 / 动作</span>
                </div>
                {group.rows.map((row) => (
                  <div className="relay-impact-row" data-tone={row.tone} key={`${row.file}-${row.field}`} role="row">
                    <span className="relay-impact-field" role="cell">
                      <code>{row.field}</code>
                      <small>{row.detail}</small>
                    </span>
                    <strong className="relay-impact-value" role="cell">{row.value}</strong>
                  </div>
                ))}
              </div>
            </div>
          ))}
        </div>
      </div>
      <div className="relay-file-panel relay-auth-archive-panel">
        <div className="relay-auth-overview">
          <div className="relay-auth-overview-head">
            <div className="relay-auth-title-row">
              <strong>auth.json 存档</strong>
              <span className="relay-auth-status" data-tone={authApiKeyTone}>
                {authStatusLabel}
              </span>
            </div>
            <span className="relay-auth-summary-text">
              {relayAuthArchiveSummary(profile, isActive)}
            </span>
          </div>
          <div className="relay-auth-target">
            <span>写入目标</span>
            <code>{authWriteTarget}</code>
            {authWriteDetail ? <small>{authWriteDetail}</small> : null}
          </div>
        </div>
        <div className="relay-auth-preview-head">
          <strong>JSON 预览</strong>
          <span>{profile.relayMode === "pureApi" ? "启用时同步" : "独立存档"}</span>
        </div>
        <SyncedTextarea
          className="relay-file-textarea relay-auth-textarea"
          value={profile.authContents}
          onValueChange={(value) => onProfileChange(deriveRelayProfileFromFiles({ ...profile, authContents: value }))}
        />
      </div>
    </div>
  );
}

export function relayActivationImpactGroups(rows: RelayActivationImpactRow[]): Array<{ file: string; rows: RelayActivationImpactRow[] }> {
  const groups: Array<{ file: string; rows: RelayActivationImpactRow[] }> = [];
  for (const row of rows) {
    const existing = groups.find((group) => group.file === row.file);
    if (existing) {
      existing.rows.push(row);
    } else {
      groups.push({ file: row.file, rows: [row] });
    }
  }
  return groups;
}

export function relayActivationImpactRows(profile: RelayProfile): RelayActivationImpactRow[] {
  if (profile.relayMode === "official" && !profile.officialMixApiKey) {
    return [
      {
        file: "config.toml",
        field: "Codex provider 配置",
        value: "清理",
        detail: "切回官方登录时移除自定义供应商配置；工具、插件等通用配置仍按独立设置保留。",
        tone: "remove",
      },
      {
        file: "auth.json",
        field: "OPENAI_API_KEY",
        value: "不写入",
        detail: "官方登录模式只保留 ChatGPT 登录态，不保存 API Key。",
        tone: "skip",
      },
    ];
  }

  const providerId = relayProfileProviderId(profile);
  const providerName = relayProfileProviderName(profile);
  const providerTable = `[model_providers.${providerId}]`;
  const baseUrl = profile.localProxyEnabled ? PROTOCOL_PROXY_BASE_URL : relayProfileEffectiveBaseUrl(profile);
  const apiKey = relayProfileEffectiveApiKey(profile);
  const modelCatalogCount = relayProfileCatalogModelCount(profile);
  const contextWindow = relayProfileContextWindowForActiveModel(profile).trim();
  const multiAgentV2Enabled = relayMultiAgentV2Enabled(profile);
  const subagentCount = relaySubagentCount(profile);
  const rows: RelayActivationImpactRow[] = [
    {
      file: "config.toml",
      field: "model_provider",
      value: providerId,
      detail: "选择当前 provider 表。",
      tone: "write",
    },
    {
      file: "config.toml",
      field: "model",
      value: profile.model.trim() || "不写入",
      detail: profile.model.trim() ? "作为默认请求模型。" : "未填写配置模型时不覆盖现有 model 字段。",
      tone: profile.model.trim() ? "write" : "skip",
    },
    {
      file: "config.toml",
      field: `${providerTable}.name`,
      value: providerName,
      detail: relayRemoteCompactionV2Enabled(profile)
        ? "写入 OpenAI，以启用 Codex Remote Compaction V2。"
        : "保证 provider 表可被 Codex 识别。",
      tone: "write",
    },
    {
      file: "config.toml",
      field: `${providerTable}.wire_api`,
      value: "responses",
      detail: "Elves 固定以 Responses API 入口承接请求。",
      tone: "write",
    },
    {
      file: "config.toml",
      field: `${providerTable}.requires_openai_auth`,
      value: "true",
      detail: "沿用 Codex 对 provider auth 的要求。",
      tone: "write",
    },
    {
      file: "config.toml",
      field: `${providerTable}.supports_websockets`,
      value: relayPrefersNativeResponsesWebsocket(profile) ? "true" : "false",
      detail: "仅在探测已支持、开关已启用且配置满足原生 Responses WebSocket 条件时写入 true。",
      tone: "write",
    },
    {
      file: "config.toml",
      field: `${providerTable}.base_url`,
      value: baseUrl || "未配置",
      detail: profile.localProxyEnabled ? "写入本地协议代理地址；真实上游来自此供应商 Base URL。" : "写入此供应商 Base URL。",
      tone: baseUrl ? "write" : "skip",
    },
    {
      file: "config.toml",
      field: `${providerTable}.experimental_bearer_token`,
      value: profile.relayMode === "pureApi" ? "不写入" : sensitiveStatus(apiKey),
      detail: profile.relayMode === "pureApi" ? "纯 API 的 Key 写入 auth.json。" : "官方混入 API Key 时写入 bearer token。",
      tone: profile.relayMode === "pureApi" ? "skip" : apiKey ? "write" : "skip",
    },
    ...(!modelCatalogCount
      ? [{
          file: "config.toml",
          field: "model_context_window",
          value: contextWindow || "不写入",
          detail: "没有模型目录时才写入顶层上下文大小。",
          tone: contextWindow ? "write" : "skip",
        } satisfies RelayActivationImpactRow]
      : []),
    {
      file: "config.toml",
      field: "model_auto_compact_token_limit",
      value: profile.autoCompactLimit.trim() || "不写入",
      detail: "仅在填写压缩上下文大小时写入。",
      tone: profile.autoCompactLimit.trim() ? "write" : "skip",
    },
    {
      file: "config.toml",
      field: `${MULTI_AGENT_V2_SECTION}.${MULTI_AGENT_V2_ENABLED_KEY}`,
      value: multiAgentV2Enabled ? "true" : "不写入",
      detail: multiAgentV2Enabled
        ? "启动 Codex 子代理 V2 模式。"
        : "当前供应商不启动子代理 V2 模式。",
      tone: multiAgentV2Enabled ? "write" : "skip",
    },
    {
      file: "config.toml",
      field: `${AGENTS_SECTION}.${AGENTS_MAX_CONCURRENT_THREADS_KEY}`,
      value: String(subagentCount),
      detail: "允许同时运行的子代理数量，与 V2 模式开关无关。",
      tone: "write",
    },
    {
      file: "config.toml",
      field: "model_catalog_json",
      value: modelCatalogCount ? "codex-elves-model-catalog.json" : "不写入",
      detail: modelCatalogCount ? "模型列表会生成独立目录文件供 Codex 读取。" : "没有模型映射时不生成模型目录。",
      tone: modelCatalogCount ? "write" : "skip",
    },
  ];

  if (modelCatalogCount) {
    rows.push({
      file: "codex-elves-model-catalog.json",
      field: "models",
      value: `${modelCatalogCount} 个模型`,
      detail: multiAgentV2Enabled
        ? "由模型列表生成，包含协议、上下文大小和子代理 V2 能力。"
        : "由模型列表生成，包含协议和上下文大小信息。",
      tone: "file",
    });
    if (multiAgentV2Enabled) {
      rows.push({
        file: "codex-elves-model-catalog.json",
        field: "models[].multi_agent_version",
        value: "v2",
        detail: "当前供应商的所有生成模型条目使用子代理 V2。",
        tone: "write",
      });
    }
  }

  rows.push({
    file: "auth.json",
    field: "OPENAI_API_KEY",
    value: profile.relayMode === "pureApi" ? sensitiveStatus(apiKey) : "不写入",
    detail: profile.relayMode === "pureApi" ? "切换到此供应商时写入 API Key。" : "官方混入模式不把 API Key 放进 auth.json。",
    tone: profile.relayMode === "pureApi" && apiKey ? "write" : "skip",
  });

  return rows;
}

export function relayAuthArchiveSummary(profile: RelayProfile, isActive: boolean): string {
  if (profile.relayMode === "official" && !profile.officialMixApiKey) {
    return isActive ? "当前使用官方登录态，不会写入 API Key。" : "仅保留此供应商的官方登录态存档。";
  }
  if (profile.relayMode === "pureApi") {
    return isActive ? "当前 auth.json 会回填到此供应商存档。" : "保存此供应商的 API Key，启用时写入本地 auth 文件。";
  }
  return "auth.json 保持登录态，API Key 由 config.toml 的 bearer token 承接。";
}

export function relayProfileProviderId(profile: RelayProfile): string {
  return rootTomlStringValue(profile.configContents, "model_provider").trim() || "custom";
}

export function relayProfileProviderName(profile: RelayProfile): string {
  const provider = relayProfileProviderId(profile);
  return codexProviderStringFromConfig(profile.configContents, "name").trim()
    === REMOTE_COMPACTION_V2_PROVIDER_NAME
    ? REMOTE_COMPACTION_V2_PROVIDER_NAME
    : provider;
}

export function relayRemoteCompactionV2Enabled(profile: RelayProfile): boolean {
  return relayProfileProviderName(profile) === REMOTE_COMPACTION_V2_PROVIDER_NAME;
}

export function setRelayRemoteCompactionV2Enabled(contents: string, enabled: boolean): string {
  const provider = rootTomlStringValue(contents, "model_provider").trim() || "custom";
  return setCodexProviderStringKey(
    contents,
    "name",
    enabled ? REMOTE_COMPACTION_V2_PROVIDER_NAME : provider,
  );
}

export function relayMultiAgentV2Enabled(profile: RelayProfile): boolean {
  const enabled = tomlSectionOptionalBoolValue(
    profile.configContents,
    MULTI_AGENT_V2_SECTION,
    MULTI_AGENT_V2_ENABLED_KEY,
  );
  if (enabled !== null) return enabled;
  return tomlSectionBoolValue(profile.configContents, "features", MULTI_AGENT_V2_FEATURE_KEY);
}

export function relaySubagentCount(profile: RelayProfile): number {
  const configured = tomlSectionIntValue(
    profile.configContents,
    AGENTS_SECTION,
    AGENTS_MAX_CONCURRENT_THREADS_KEY,
  );
  if (configured !== null) {
    return clampNumber(configured, 1, Number.MAX_SAFE_INTEGER);
  }
  const legacyMaxThreads = tomlSectionIntValue(
    profile.configContents,
    MULTI_AGENT_V2_SECTION,
    LEGACY_MULTI_AGENT_V2_MAX_THREADS_KEY,
  );
  return clampNumber(
    legacyMaxThreads === null ? DEFAULT_SUBAGENT_COUNT : legacyMaxThreads - 1,
    1,
    Number.MAX_SAFE_INTEGER,
  );
}

export function migrateLegacyRelaySubagentCount(contents: string): string {
  if (
    tomlSectionIntValue(contents, AGENTS_SECTION, AGENTS_MAX_CONCURRENT_THREADS_KEY)
    !== null
  ) {
    return contents;
  }
  const legacyMaxThreads = tomlSectionIntValue(
    contents,
    MULTI_AGENT_V2_SECTION,
    LEGACY_MULTI_AGENT_V2_MAX_THREADS_KEY,
  );
  if (legacyMaxThreads === null) return contents;
  return setTomlSectionRawKey(
    contents,
    AGENTS_SECTION,
    AGENTS_MAX_CONCURRENT_THREADS_KEY,
    String(clampNumber(legacyMaxThreads - 1, 1, Number.MAX_SAFE_INTEGER)),
  );
}

export function setRelayMultiAgentV2Enabled(contents: string, enabled: boolean): string {
  let next = migrateLegacyRelaySubagentCount(contents);
  next = removeTomlSectionKey(next, "features", MULTI_AGENT_V2_FEATURE_KEY);
  next = removeTomlSection(next, MULTI_AGENT_V2_SECTION);
  if (!enabled) return next;
  return setTomlSectionBoolKey(
    next,
    MULTI_AGENT_V2_SECTION,
    MULTI_AGENT_V2_ENABLED_KEY,
    true,
  );
}

export function setRelaySubagentCount(contents: string, subagentCount: number): string {
  const next = removeTomlSectionKey(
    contents,
    MULTI_AGENT_V2_SECTION,
    LEGACY_MULTI_AGENT_V2_MAX_THREADS_KEY,
  );
  const normalizedSubagentCount = clampNumber(
    subagentCount,
    1,
    Number.MAX_SAFE_INTEGER,
  );
  return setTomlSectionRawKey(
    next,
    AGENTS_SECTION,
    AGENTS_MAX_CONCURRENT_THREADS_KEY,
    String(normalizedSubagentCount),
  );
}

export function relayProfileEffectiveBaseUrl(profile: RelayProfile): string {
  return (
    profile.upstreamBaseUrl.trim()
    || profile.baseUrl.trim()
    || codexBaseUrlFromConfig(profile.configContents).trim()
  );
}

export function relayProfileEffectiveApiKey(profile: RelayProfile): string {
  if (profile.relayMode === "official") {
    return codexExperimentalBearerTokenFromConfig(profile.configContents).trim() || profile.apiKey.trim();
  }
  return (
    codexApiKeyFromAuth(profile.authContents).trim()
    || codexExperimentalBearerTokenFromConfig(profile.configContents).trim()
    || profile.apiKey.trim()
  );
}

export function sensitiveStatus(value: string): string {
  return value.trim() ? "已配置" : "未配置";
}

export function relayProfileCatalogModelCount(profile: RelayProfile): number {
  const models = new Set<string>();
  for (const mapping of normalizeRelayModelMappings(profile.modelMappings)) {
    const model = mapping.requestModel.trim();
    if (model) models.add(model);
  }
  if (models.size) return models.size;
  for (const list of [
    profile.responsesModelList,
    profile.chatCompletionsModelList,
    profile.anthropicModelList,
    profile.modelList,
  ]) {
    for (const model of splitRelayModelList(list)) {
      models.add(model);
    }
  }
  return models.size;
}

export function splitRelayModelList(value: string): string[] {
  return value
    .split(/[\r\n,]+/)
    .map((item) => item.trim())
    .filter(Boolean);
}

export function formatBytes(bytes: number) {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  let value = bytes;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return `${value >= 10 || index === 0 ? value.toFixed(0) : value.toFixed(1)} ${units[index]}`;
}

export function formatOptionalBytes(bytes?: number | null) {
  return typeof bytes === "number" ? formatBytes(bytes) : "-";
}

export function Panel({ children, fill = false, className = "" }: { children: React.ReactNode; fill?: boolean; className?: string }) {
  return (
    <Card className={`panel ${fill ? "fill" : ""} ${className}`}>
      {children}
    </Card>
  );
}

export function CardHead({ title, detail, actions }: { title: string; detail: string; actions?: React.ReactNode }) {
  return (
    <CardHeader className="panel-head">
      <div className="panel-head-copy">
        <CardTitle>{title}</CardTitle>
        {detail ? <CardDescription>{detail}</CardDescription> : null}
      </div>
      {actions ? <div className="panel-head-actions">{actions}</div> : null}
    </CardHeader>
  );
}

export function Toolbar({ children }: { children: React.ReactNode }) {
  return <div className="toolbar">{children}</div>;
}

export function Field({
  label,
  children,
  className = "",
  as = "label",
  actions,
}: {
  label: string;
  children: React.ReactNode;
  className?: string;
  as?: "label" | "div";
  actions?: React.ReactNode;
}) {
  if (as === "div") {
    return (
      <div className={`field ${className}`}>
        {actions ? (
          <div className="field-label-row">
            <span>{label}</span>
            <div className="field-label-actions">{actions}</div>
          </div>
        ) : (
          <span>{label}</span>
        )}
        {children}
      </div>
    );
  }

  return (
    <Label className={`field ${className}`}>
      <span>{label}</span>
      {children}
    </Label>
  );
}

export function Badge({ status }: { status: string }) {
  return <UiBadge className={statusClass(status)} variant="secondary">{statusLabel(status)}</UiBadge>;
}

export function Metric({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <span>{label}</span>
      <strong>{value}</strong>
    </div>
  );
}

export const contextKindOptions: Array<{ kind: ContextKind; label: string; tableName: string }> = [
  { kind: "mcp", label: "MCP", tableName: "mcp_servers" },
  { kind: "skill", label: "Skills", tableName: "skills" },
  { kind: "plugin", label: "插件", tableName: "plugins" },
];

export function contextEntriesFromSettings(settings: BackendSettings): CodexContextEntries {
  const commonConfig = normalizeDuplicateTomlTables(settings.relayContextConfigContents || "");
  return {
    mcpServers: parseContextEntries(commonConfig, "mcp", "mcp_servers"),
    skills: parseContextEntries(commonConfig, "skill", "skills"),
    plugins: parseContextEntries(commonConfig, "plugin", "plugins"),
  };
}

export function contextEntriesFromConfig(configContents: string): CodexContextEntries {
  return {
    mcpServers: parseContextEntries(configContents, "mcp", "mcp_servers"),
    skills: parseContextEntries(configContents, "skill", "skills"),
    plugins: parseContextEntries(configContents, "plugin", "plugins"),
  };
}

export function parseContextEntries(commonConfig: string, kind: ContextKind, tableName: string): CodexContextEntry[] {
  const entries = new Map<string, CodexContextEntry>();
  let currentId: string | null = null;
  let body: string[] = [];

  const flush = () => {
    if (!currentId) return;
    const tomlBody = ensureTrailingNewline(body.join("\n").trimEnd());
    entries.set(currentId, {
      id: currentId,
      kind,
      title: currentId,
      summary: contextEntrySummary(tomlBody),
      tomlBody,
      enabled: contextEntryEnabled(tomlBody),
    });
  };

  for (const line of commonConfig.split(/\r?\n/)) {
    const tablePath = tomlTablePathFromLine(line);
    const arrayPath = tomlArrayTablePathFromLine(line);
    const path = tablePath ?? arrayPath;
    if (tablePath?.[0] === tableName && tablePath.length === 2) {
      const id = tablePath[1];
      flush();
      currentId = id;
      body = [];
      continue;
    }
    if (currentId && path?.[0] === tableName && path[1] === currentId && path.length > 2) {
      const subtable = path.slice(2).map(tomlKey).join(".");
      body.push(arrayPath ? `[[${subtable}]]` : `[${subtable}]`);
      continue;
    }
    if (currentId && path) {
      flush();
      currentId = null;
      body = [];
      continue;
    }
    if (currentId) body.push(line);
  }
  flush();

  return Array.from(entries.values());
}

export function tomlTablePathFromLine(line: string): string[] | null {
  const match = /^\s*\[([^\]]+)\]\s*$/.exec(line);
  if (!match) return null;
  return parseTomlDottedPath(match[1].trim());
}

export function tomlArrayTablePathFromLine(line: string): string[] | null {
  const match = /^\s*\[\[([^\]]+)\]\]\s*$/.exec(line);
  if (!match) return null;
  return parseTomlDottedPath(match[1].trim());
}

export function tomlHeaderPathFromLine(line: string): string[] | null {
  return tomlTablePathFromLine(line) ?? tomlArrayTablePathFromLine(line);
}

export function parseTomlDottedPath(path: string): string[] | null {
  const parts: string[] = [];
  let current = "";
  let quote: '"' | "'" | null = null;
  let escaping = false;

  for (const char of path) {
    if (quote) {
      if (quote === '"' && escaping) {
        current += char;
        escaping = false;
      } else if (quote === '"' && char === "\\") {
        escaping = true;
      } else if (char === quote) {
        quote = null;
      } else {
        current += char;
      }
      continue;
    }

    if (char === '"' || char === "'") {
      quote = char;
      continue;
    }
    if (char === ".") {
      if (!current.trim()) return null;
      parts.push(current.trim());
      current = "";
      continue;
    }
    current += char;
  }

  if (quote || escaping || !current.trim()) return null;
  parts.push(current.trim());
  return parts;
}

export function contextEntrySummary(tomlBody: string) {
  return tomlBody
    .split(/\r?\n/)
    .map((line) => line.trim())
    .find((line) => line && !line.startsWith("#") && !/^enabled\s*=/.test(line))
    ?.slice(0, 96) ?? "";
}

export function contextEntryEnabled(tomlBody: string) {
  return !tomlBody.split(/\r?\n/).some((line) => /^\s*enabled\s*=\s*false\s*(#.*)?$/i.test(line));
}

export function ensureTrailingNewline(value: string) {
  return value.trim() ? `${value}\n` : "";
}

export function splitContextConfigText(configContents: string): { common: string; context: string } {
  const entries = contextEntriesFromConfig(configContents);
  return {
    common: stripContextEntriesFromConfig(configContents, entries),
    context: contextConfigTextFromConfig(configContents, entries),
  };
}

export function contextConfigTextFromConfig(configContents: string, entries: CodexContextEntries): string {
  const knownIds: Record<ContextKind, Set<string>> = {
    mcp: new Set(entries.mcpServers.map((entry) => entry.id)),
    skill: new Set(entries.skills.map((entry) => entry.id)),
    plugin: new Set(entries.plugins.map((entry) => entry.id)),
  };
  const collected: string[] = [];
  let collecting = false;

  for (const line of configContents.split(/\r?\n/)) {
    const contextHeader = contextHeaderFromLine(line);
    if (contextHeader) {
      collecting = knownIds[contextHeader.kind].has(contextHeader.id);
    } else if (tomlHeaderPathFromLine(line)) {
      collecting = false;
    }
    if (collecting) collected.push(line);
  }

  return ensureTrailingNewline(collected.join("\n").trimEnd());
}

export function stripContextEntriesFromConfig(configContents: string, entries: CodexContextEntries): string {
  const knownIds: Record<ContextKind, Set<string>> = {
    mcp: new Set(entries.mcpServers.map((entry) => entry.id)),
    skill: new Set(entries.skills.map((entry) => entry.id)),
    plugin: new Set(entries.plugins.map((entry) => entry.id)),
  };
  const lines = configContents.split(/\r?\n/);
  const kept: string[] = [];
  let skipping = false;

  for (const line of lines) {
    const contextHeader = contextHeaderFromLine(line);
    if (contextHeader) {
      skipping = knownIds[contextHeader.kind].has(contextHeader.id);
    } else if (tomlHeaderPathFromLine(line)) {
      skipping = false;
    }
    if (!skipping) kept.push(line);
  }

  return ensureTrailingNewline(kept.join("\n").trimEnd());
}

export function tomlRootKeyFromLine(line: string): string | null {
  if (!line || line.startsWith("#")) return null;
  const index = line.indexOf("=");
  if (index < 0) return null;
  const key = line.slice(0, index).trim();
  return key || null;
}

export function contextHeaderFromLine(line: string): { kind: ContextKind; id: string } | null {
  const path = tomlHeaderPathFromLine(line);
  if (!path || path.length < 2) return null;
  const option = contextKindOptions.find((item) => item.tableName === path[0]);
  return option ? { kind: option.kind, id: path[1] } : null;
}

export function removeRootTomlKey(contents: string, key: string): string {
  const lines: string[] = [];
  let inRoot = true;
  for (const line of contents.split(/\r?\n/)) {
    if (/^\s*\[[^\]]+\]\s*$/.test(line)) inRoot = false;
    if (inRoot && new RegExp(`^\\s*${key}\\s*=`).test(line)) continue;
    lines.push(line);
  }
  return ensureTrailingNewline(lines.join("\n").trimEnd());
}

export function joinTomlSections(sections: string[]): string {
  return ensureTrailingNewline(
    sections
      .map((section) => section.trim())
      .filter(Boolean)
      .join("\n\n"),
  );
}

export function joinTomlSectionsRootFirst(sections: string[]): string {
  const rootParts: string[] = [];
  const tableParts: string[] = [];

  for (const section of sections) {
    const { root, tables } = splitTomlRootAndTables(section);
    if (root.trim()) rootParts.push(root.trim());
    if (tables.trim()) tableParts.push(tables.trim());
  }

  return normalizeDuplicateTomlTables(joinTomlSections([...dedupeTomlRootLines(rootParts), ...tableParts]));
}

export function normalizeDuplicateTomlTables(contents: string): string {
  const seenHeaders = new Set<string>();
  const kept: string[] = [];
  let skipping = false;

  for (const line of contents.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (/^\[[^\]]+\]$/.test(trimmed)) {
      skipping = seenHeaders.has(trimmed);
      seenHeaders.add(trimmed);
      if (skipping) continue;
    }
    if (!skipping) kept.push(line);
  }

  return ensureTrailingNewline(kept.join("\n").trimEnd());
}

export function dedupeTomlRootLines(rootParts: string[]): string[] {
  const rootLines = rootParts
    .join("\n")
    .split(/\r?\n/)
    .map((line) => line.trimEnd());
  const rootSeen = new Set<string>();
  const kept: string[] = [];

  for (let index = rootLines.length - 1; index >= 0; index -= 1) {
    const line = rootLines[index];
    const key = tomlRootKeyFromLine(line.trim());
    if (key) {
      if (rootSeen.has(key)) continue;
      rootSeen.add(key);
    }
    kept.push(line);
  }

  const normalized = kept.reverse().join("\n").trim();
  return normalized ? [normalized] : [];
}

export function splitTomlRootAndTables(section: string): { root: string; tables: string } {
  const lines = section.trim().split(/\r?\n/);
  const firstTable = lines.findIndex((line) => /^\s*\[[^\]]+\]\s*$/.test(line));
  if (firstTable < 0) return { root: lines.join("\n"), tables: "" };
  return {
    root: lines.slice(0, firstTable).join("\n"),
    tables: lines.slice(firstTable).join("\n"),
  };
}

export function tomlKey(key: string): string {
  return /^[A-Za-z0-9_-]+$/.test(key) ? key : `"${tomlString(key)}"`;
}

export function contextSelectionForAllEntries(settings: BackendSettings): RelayContextSelection {
  const entries = contextEntriesFromSettings(settings);
  return {
    mcpServers: entries.mcpServers.map((entry) => entry.id),
    skills: entries.skills.map((entry) => entry.id),
    plugins: entries.plugins.map((entry) => entry.id),
  };
}

export function relayProfileEditorStatus(profile: RelayProfile, form: BackendSettings, isNew: boolean) {
  if (isNew) return "新建供应商需要先保存到列表";
  if (!form.relayProfilesEnabled) return "供应商功能已关闭；当前只保存供应商列表，不写入 Codex live 配置";
  return profile.id === form.activeRelayId ? "当前正在使用" : "编辑后保存列表，再切换模式时会使用新配置";
}

export function providerInitial(name: string) {
  const trimmed = (name || "供应商").trim();
  return Array.from(trimmed)[0]?.toUpperCase() || "供";
}

export function statusLabel(status: string) {
  const labels: Record<string, string> = {
    found: "已找到",
    missing: "缺失",
    installed: "已安装",
    ok: "正常",
    running: "运行中",
    failed: "失败",
    archived: "已归档",
    accepted: "已受理",
    not_checked: "未检查",
    not_implemented: "未实现",
    disabled: "已禁用",
    unknown: "未知",
  };
  return labels[status] ?? status;
}

export function localProxyState(status: LocalProxyStatusResult | null) {
  if (!status) return "unknown";
  if (!status.enabled) return "disabled";
  return status.listening ? "running" : "failed";
}

export function localProxyStateLabel(status: LocalProxyStatusResult | null) {
  const state = localProxyState(status);
  if (state === "running") return "代理运行中";
  if (state === "disabled") return "代理未启用";
  if (state === "failed") return "代理未启动";
  return "代理未检查";
}

export function calculateRequestRatio(entries: LocalProxyLogEntry[]) {
  let high = 0;
  let highTotal = 0;
  let continuation = 0;
  let continuationTotal = 0;

  entries.forEach((entry) => {
    highTotal += 1;
    if (classifyHighReasoningRequest(entry) === "high") {
      high += 1;
    }
    if (isGptModel(proxyLogDisplayModel(entry))) {
      continuationTotal += 1;
      if (isContinueThinkingEntry(entry)) {
        continuation += 1;
      }
    }
  });

  return {
    high,
    continuation,
    highTotal,
    continuationTotal,
    highPercent: highTotal ? Math.round((high / highTotal) * 100) : 0,
    continuationPercent: continuationTotal
      ? Math.round((continuation / continuationTotal) * 100)
      : 0,
  };
}

export function requestOutputRate(entry: LocalProxyLogEntry) {
  const tokens = entry.outputTokens;
  const duration = entry.durationMs;
  if (
    entry.state === "pending" ||
    entry.outcome === "failed" ||
    entry.outcome === "incomplete" ||
    entry.error ||
    typeof entry.statusCode !== "number" ||
    entry.statusCode < 200 ||
    entry.statusCode >= 300 ||
    typeof tokens !== "number" ||
    !Number.isFinite(tokens) ||
    tokens <= 0 ||
    typeof duration !== "number" ||
    !Number.isFinite(duration) ||
    duration <= 0
  ) return null;
  return { tokens, generationMs: duration };
}

export function calculateProxyModelSpeeds(entries: LocalProxyLogEntry[]) {
  const models = new Map<string, {
    requestCount: number;
    latestAtMs: number;
    latestRateAtMs: number;
    latestRate: number | null;
    tokens: number;
    generationMs: number;
  }>();
  for (const entry of entries) {
    const model = entry.upstreamRequestModel?.trim();
    if (!model) continue;
    if (!models.has(model)) {
      models.set(model, {
        requestCount: 0,
        latestAtMs: 0,
        latestRateAtMs: -1,
        latestRate: null,
        tokens: 0,
        generationMs: 0,
      });
    }
    const summary = models.get(model)!;
    summary.requestCount += 1;
    summary.latestAtMs = Math.max(summary.latestAtMs, entry.timestampMs);
    const sample = requestOutputRate(entry);
    if (!sample) continue;
    if (entry.timestampMs > summary.latestRateAtMs) {
      summary.latestRateAtMs = entry.timestampMs;
      summary.latestRate = sample.tokens * 1000 / sample.generationMs;
    }
    summary.tokens += sample.tokens;
    summary.generationMs += sample.generationMs;
  }
  const result: Array<{ model: string; latestRate: number | null; averageRate: number | null } | null> =
    Array.from(models)
      .sort(([leftModel, left], [rightModel, right]) =>
        right.requestCount - left.requestCount ||
        right.latestAtMs - left.latestAtMs ||
        leftModel.localeCompare(rightModel),
      )
      .slice(0, 4)
      .map(([model, summary]) => ({
        model,
        latestRate: summary.latestRate,
        averageRate: summary.generationMs > 0 ? summary.tokens * 1000 / summary.generationMs : null,
      }));
  while (result.length < 2) result.push(null);
  return result;
}

export function formatProxyTokenRate(rate?: number | null) {
  return typeof rate === "number" && Number.isFinite(rate)
    ? rate.toLocaleString("en-US", { minimumFractionDigits: 1, maximumFractionDigits: 1 })
    : "-";
}

export function classifyHighReasoningRequest(entry: Pick<LocalProxyLogEntry, "reasoningTokens">) {
  if (entry.reasoningTokens === 516) return "low";
  if (typeof entry.reasoningTokens === "number" && entry.reasoningTokens > 516) return "high";
  return null;
}

export function isContinueThinkingEntry(entry: LocalProxyLogEntry) {
  return isGptModel(proxyLogDisplayModel(entry)) && entry.continueThinkingTriggered === true;
}

export function isGptModel(model?: string | null) {
  return Boolean(model?.toLowerCase().includes("gpt"));
}

export function isClaudeModel(model?: string | null) {
  const slug = model?.trim().toLowerCase().split("/").filter(Boolean).pop() || "";
  return slug === "claude" || slug.startsWith("claude-") || slug.startsWith("anthropic.claude");
}

export function formatReasoningTokens(value?: number | null) {
  return typeof value === "number" ? value.toLocaleString() : "-";
}

export function formatContinueThinkingTitle(entry: Pick<LocalProxyLogEntry, "continueThinkingRounds">) {
  return typeof entry.continueThinkingRounds === "number" && entry.continueThinkingRounds > 0
    ? `已触发 GPT 推理续接 ${entry.continueThinkingRounds} 轮`
    : "已触发 GPT 推理续接";
}

export function formatLayeredCompactionTitle(
  entry: Pick<
    LocalProxyLogEntry,
    "layeredCompactionRetainTokens" | "layeredCompactionRetainedItems" | "layeredCompactionRetainedChars"
  >,
) {
  const parts = ["已触发上下文压缩"];
  if (typeof entry.layeredCompactionRetainedItems === "number") {
    parts.push(`保留 ${entry.layeredCompactionRetainedItems} 条原始记录`);
  }
  if (typeof entry.layeredCompactionRetainedChars === "number") {
    parts.push(`约 ${Math.round(entry.layeredCompactionRetainedChars / 4).toLocaleString()} token`);
  }
  if (typeof entry.layeredCompactionRetainTokens === "number") {
    parts.push(`裁剪目标 ${entry.layeredCompactionRetainTokens.toLocaleString()} token`);
  }
  return parts.join(" · ");
}

export function formatIndependentCompactionDescription(
  entry: Pick<
    LocalProxyLogEntry,
    | "transport"
    | "model"
    | "upstreamResponseModel"
    | "independentCompactionModel"
    | "independentCompactionUsage"
    | "cacheMissCompactionModel"
  >,
) {
  const legacyCompactionModel = entry.cacheMissCompactionModel?.trim();
  const compactionModel = independentCompactionTarget(entry);
  const usage = entry.independentCompactionUsage || (legacyCompactionModel ? "cacheMiss" : null);
  if (!compactionModel || !usage) return "";
  const description = usage === "default" ? "默认压缩" : "缓存失效压缩";
  if (compactionModel.toLowerCase() === proxyLogDisplayModel(entry)?.toLowerCase()) {
    return `（${description}）`;
  }
  return `（${description}：${compactionModel}）`;
}

export function reasoningTokenTone(value?: number | null) {
  if (value === 516) return "low";
  if (typeof value === "number" && value > 516) return "high";
  return "normal";
}

export function formatRequestDurationMs(value: number) {
  if (!Number.isFinite(value)) return "-";
  const seconds = Math.max(0, value) / 1000;
  const digits = seconds < 10 ? 2 : 1;
  return `${seconds.toFixed(digits).replace(/\.?0+$/, "")}s`;
}

export function formatOptionalRequestDurationMs(value?: number | null) {
  return typeof value === "number" ? formatRequestDurationMs(value) : "-";
}

export function isSlowRequestDuration(value?: number | null) {
  return typeof value === "number" && value >= 60000;
}

export function localProxyOutcome(entry: LocalProxyLogEntry): LocalProxyRequestOutcome | null {
  if (entry.state === "pending") return null;
  if (entry.statusCode === 499) return "incomplete";
  if (entry.error || (typeof entry.statusCode === "number" && (entry.statusCode < 200 || entry.statusCode >= 300))) return "failed";
  return entry.outcome ?? null;
}

export function formatRequestOutcome(outcome?: LocalProxyRequestOutcome | null) {
  return outcome === "succeeded" ? "成功" : outcome === "failed" ? "失败" : outcome === "incomplete" ? "未完成" : "结果未记录";
}

export function localProxyStatusCodeClass(entry: LocalProxyLogEntry) {
  const outcome = localProxyOutcome(entry);
  return outcome === "succeeded" ? "proxy-code ok" : outcome === "failed" ? "proxy-code bad" : "proxy-code pending";
}

export function formatLocalProxyStatusCode(entry: LocalProxyLogEntry) {
  const outcome = localProxyOutcome(entry);
  const result = entry.state === "pending" ? "进行中"
    : outcome === "failed" && (entry.compactionRequested || entry.remoteCompactionTriggered || entry.layeredCompactionTriggered)
      ? "压缩失败" : formatRequestOutcome(outcome);
  const transport = formatRequestTransport(entry);
  const status = transport === "WS" ? "WS" : `HTTP ${entry.statusCode ?? "未记录"}`;
  const fallback = transport === "HTTP" && entry.upstreamAttempts?.slice(0, -1).some(
    (attempt) => attempt.transport === "ws" && attempt.outcome === "failed",
  ) ? "（WS 回退）" : "";
  return `${result} · ${status}${fallback}`;
}

export function formatUpstreamStatusCode(entry: LocalProxyLogEntry) {
  if (formatRequestTransport(entry) === "WS") return "不适用（WS）";
  if (typeof entry.upstreamStatusCode === "number") return String(entry.upstreamStatusCode);
  return "未记录";
}

export function localProxyFailureReason(entry: LocalProxyLogEntry) {
  const outcome = localProxyOutcome(entry);
  if (outcome !== "failed" && outcome !== "incomplete") return "";
  return proxyFailureReason(entry) || (outcome === "incomplete" ? "响应未完成" : "失败原因未记录");
}

export function proxyFailureReason(entry: Pick<LocalProxyLogEntry, "errorCode" | "error">) {
  const code = entry.errorCode || "";
  if (code.endsWith("_summary_tag_missing")) return "摘要缺少 <summary> 标记";
  if (code.endsWith("_summary_empty")) return "摘要为空";
  if (code.endsWith("_no_terminal_response")) return "未收到完整响应";
  if (code.endsWith("_not_completed")) return "上游响应未完成";
  if (code.endsWith("_tool_call_output")) return "压缩结果包含工具调用";
  if (code === "max_output_tokens") return "输出达到上限";
  if (/^upstream_http_\d{3}$/.test(code)) return `上游返回 HTTP ${code.slice(-3)}`;
  return entry.error || entry.errorCode || "";
}

export function formatRequestLatencyTitle(entry: Pick<LocalProxyLogEntry, "firstTokenMs" | "durationMs">) {
  return `首字 ${formatOptionalRequestDurationMs(entry.firstTokenMs)} | 耗时 ${formatOptionalRequestDurationMs(entry.durationMs)}`;
}

export function formatProtocolRoute(entry: Pick<LocalProxyLogEntry, "responseProtocol" | "transport" | "upstreamAttempts" | "method">) {
  return `${entry.responseProtocol || "未记录"} · ${formatRequestTransport(entry)} · ${formatProtocolMode(entry)}`;
}

export function formatRequestTransport(entry: Pick<LocalProxyLogEntry, "transport" | "upstreamAttempts" | "method">) {
  const transport = entry.upstreamAttempts?.at(-1)?.transport ?? entry.transport;
  return transport === "ws" || (!transport && entry.method === "WS") ? "WS" : "HTTP";
}

export function formatProtocolMode(entry: Pick<LocalProxyLogEntry, "responseProtocol">) {
  if (!entry.responseProtocol) return "未记录";
  return entry.responseProtocol !== "responses" ? "转换" : "直连";
}

export function statusClass(status: string) {
  if (["found", "installed", "ok", "running"].includes(status)) return "good";
  if (["failed", "missing"].includes(status)) return "bad";
  return "warn";
}

export function isSuccessStatus(status?: Status) {
  return status === "ok" || status === "accepted";
}

export function normalizeSettings(settings: BackendSettings): BackendSettings {
  const backendAggregates = new Map(
    (settings.aggregateRelayProfiles ?? []).map((aggregate) => [aggregate.id, aggregate] as const),
  );
  const splitCommon = splitContextConfigText(settings.relayCommonConfigContents || "");
  const relayCommonConfigContents = splitCommon.common;
  const relayContextConfigContents = joinTomlSectionsRootFirst([
    settings.relayContextConfigContents || "",
    splitCommon.context,
  ]);
  const defaultContextSelection = contextSelectionForAllEntries({
    ...settings,
    relayCommonConfigContents,
    relayContextConfigContents,
  });
  const profiles =
    settings.relayProfiles?.length
      ? settings.relayProfiles.map((profile) =>
          normalizeRelayProfile(hydrateAggregateRelayProfile(profile, backendAggregates.get(profile.id)), defaultContextSelection),
        )
      : [
          {
            id: settings.activeRelayId || "default",
            name: "默认中转",
            model: "",
            baseUrl: settings.relayBaseUrl || defaultSettings.relayBaseUrl,
            upstreamBaseUrl: settings.relayBaseUrl || defaultSettings.relayBaseUrl,
            apiKey: settings.relayApiKey || "",
            protocol: "responses" as RelayProtocol,
            localProxyEnabled: false,
            relayMode: "official" as RelayMode,
            officialMixApiKey: false,
            configContents: "",
            authContents: "",
            useCommonConfig: true,
            contextSelection: defaultContextSelection,
            contextSelectionInitialized: true,
            contextWindow: "",
            autoCompactLimit: "",
            modelMappings: [],
            modelList: "",
            responsesModelList: "",
            chatCompletionsModelList: "",
            anthropicModelList: "",
            responsesWebsocket: emptyResponsesWebsocketCapability(),
            responsesWebsocketEnabled: true,
            userAgent: "",
            systemPromptOverride: "",
          },
        ];
  const activeRelayId = profiles.some((profile) => profile.id === settings.activeRelayId)
    ? settings.activeRelayId
    : profiles[0]?.id || "default";
  const layeredCompactionModels = normalizeLayeredCompactionModels(settings);
  return syncLegacyRelayFields({
    ...defaultSettings,
    ...settings,
    codexHomePath: (settings.codexHomePath || "").trim(),
    relayProfilesEnabled: settings.relayProfilesEnabled !== false,
    lanProxyEnabled: settings.lanProxyEnabled === true,
    remoteAccess: { ...defaultRemoteAccessSettings, ...settings.remoteAccess },

    gptReasoningContinuationMaxRounds: clampNumber(settings.gptReasoningContinuationMaxRounds || 3, 1, 9),
    layeredCompactionRetainTokens: clampNumber(
      settings.layeredCompactionRetainTokens || 20000,
      20000,
      64000,
    ),
    layeredCompactionModelUsage:
      settings.layeredCompactionModelUsage === "default" ? "default" : "cacheMiss",
    layeredCompactionModels,
    layeredCompactionModel: "",
    relayCommonConfigContents,
    relayContextConfigContents,
    relayProfiles: profiles,
    activeRelayId,
  });
}

export function normalizeLayeredCompactionModels(settings: BackendSettings): Record<ModelFamily, string> {
  const normalized: Record<ModelFamily, string> = {
    gpt: settings.layeredCompactionModels?.gpt?.trim() || "",
    claude: settings.layeredCompactionModels?.claude?.trim() || "",
    other: settings.layeredCompactionModels?.other?.trim() || "",
  };
  if (normalized.gpt || normalized.claude || normalized.other) return normalized;
  const legacy = settings.layeredCompactionModel?.trim() || "";
  if (legacy) {
    normalized.gpt = legacy;
    normalized.claude = legacy;
    normalized.other = legacy;
  }
  return normalized;
}

export function clampNumber(value: number, min: number, max: number): number {
  if (!Number.isFinite(value)) return min;
  return Math.min(max, Math.max(min, Math.round(value)));
}

export function normalizeRelayProfile(profile: RelayProfile, defaultContextSelection = emptyContextSelection()): RelayProfile {
  // 用户要求废弃供应商级测试模型；旧导入数据也不能把该字段带回保存或测试候选。
  const { testModel: _retiredTestModel, ...currentProfile } = profile as RelayProfile & { testModel?: unknown };
  profile = currentProfile;
  const legacyMixedApi = profile.relayMode === "mixedApi";
  if (profile.relayMode === "aggregate" || profile.aggregate) {
    return normalizeAggregateRelayProfile(
      {
        ...profile,
        model: profile.model || "",
        baseUrl: "",
        upstreamBaseUrl: "",
        apiKey: "",
        protocol: "responses",
        relayMode: "aggregate",
        officialMixApiKey: false,
        configContents: "",
        authContents: "",
        useCommonConfig: profile.useCommonConfig !== false,
        contextSelection: profile.contextSelectionInitialized
          ? normalizeContextSelection(profile.contextSelection)
          : normalizeContextSelection(undefined, defaultContextSelection),
        contextSelectionInitialized: true,
        contextWindow: "",
        autoCompactLimit: "",
        modelList: "",
        responsesWebsocket: emptyResponsesWebsocketCapability(),
        responsesWebsocketEnabled: false,
        systemPromptOverride: "",
      },
      null,
    );
  }
  const relayMode = normalizeRelayMode(profile.relayMode);
  const officialMixApiKey = profile.officialMixApiKey === true || legacyMixedApi;
  const localProxyEnabled = typeof profile.localProxyEnabled === "boolean"
    ? profile.localProxyEnabled
    : false;
  const legacyModelList = profile.modelList || "";
  const responsesModelList = profile.responsesModelList || "";
  const chatCompletionsModelList = profile.chatCompletionsModelList || "";
  const anthropicModelList = profile.anthropicModelList || "";
  const modelMappings = normalizeRelayModelMappings(profile.modelMappings);
  let normalized: RelayProfile = {
    ...profile,
    model: profile.model || "",
    baseUrl: profile.baseUrl || defaultSettings.relayBaseUrl,
    upstreamBaseUrl: profile.upstreamBaseUrl || profile.baseUrl || "",
    apiKey: profile.apiKey || "",
    protocol: normalizeRelayProtocol(profile.protocol),
    localProxyEnabled,
    relayMode,
    officialMixApiKey,
    configContents: relayMode === "official" && !officialMixApiKey ? "" : profile.configContents || "",
    authContents: relayMode === "official" && !officialMixApiKey ? buildOfficialRelayAuthJson(profile.authContents || "") : profile.authContents || "",
    useCommonConfig: profile.useCommonConfig !== false,
    contextSelection: profile.contextSelectionInitialized
      ? normalizeContextSelection(profile.contextSelection)
      : normalizeContextSelection(undefined, defaultContextSelection),
    contextSelectionInitialized: true,
    contextWindow: profile.contextWindow || "",
    autoCompactLimit: profile.autoCompactLimit || "",
    modelMappings,
    modelList: legacyModelList,
    responsesModelList,
    chatCompletionsModelList,
    anthropicModelList,
    responsesWebsocket: normalizeResponsesWebsocketCapability(profile),
    responsesWebsocketEnabled: profile.responsesWebsocketEnabled !== false,
    userAgent: profile.userAgent || "",
    systemPromptOverride: profile.systemPromptOverride || "",
    aggregate: null,
  };
  return relayProfileUsesLiveFiles(normalized) ? deriveRelayProfileFromFiles(normalized) : normalized;
}

export function hydrateAggregateRelayProfile(profile: RelayProfile, aggregate: AggregateRelayProfile | undefined): RelayProfile {
  if (!aggregate) return profile;
  return {
    ...profile,
    name: profile.name || aggregate.name,
    relayMode: "aggregate",
    aggregate: {
      strategy: aggregate.strategy,
      members: aggregate.members.map((member) => ({
        profileId: member.relayId,
        weight: clampAggregateWeight(member.weight),
      })),
    },
  };
}

export function activeRelayProfile(settings: BackendSettings): RelayProfile {
  return (
    settings.relayProfiles.find((profile) => profile.id === settings.activeRelayId) ||
    settings.relayProfiles[0] ||
    defaultSettings.relayProfiles[0]
  );
}

export function normalizeRelayModelMappings(mappings: RelayModelMapping[] | undefined): RelayModelMapping[] {
  if (!Array.isArray(mappings)) return [];
  return mappings.map((item) => {
    const requestModel = item.requestModel || "";
    const alias = item.alias || "";
    const contextWindow = (item.contextWindow || "").replace(/[^\d]/g, "")
      || defaultModelContextWindow(requestModel);
    return {
      requestModel,
      alias,
      protocol: item.protocol == null ? defaultProtocolForModel(requestModel) : normalizeRelayProtocol(item.protocol),
      contextWindow,
      systemPromptOverride: item.systemPromptOverride || "",
    };
  });
}

export function normalizeRelayProtocol(protocol: RelayProtocol | undefined): RelayProtocol {
  if (protocol === "chatCompletions" || protocol === "anthropic") return protocol;
  return "responses";
}

export function defaultProtocolForModel(model: string): RelayProtocol {
  const slug = (model.trim().toLowerCase().split("/").filter(Boolean).pop() || "").trim();
  if (slug === "claude" || slug.startsWith("claude-") || slug.startsWith("anthropic.claude")) {
    return "anthropic";
  }
  if (
    slug === "gpt"
    || slug.startsWith("gpt-")
    || slug === "chatgpt"
    || slug.startsWith("chatgpt-")
    || slug === "codex"
    || slug.startsWith("codex-")
    || /^o\d/.test(slug)
  ) {
    return "responses";
  }
  return slug ? "anthropic" : "responses";
}

export function uniqueStrings(values: string[]): string[] {
  return Array.from(new Set(values));
}

export function relayProfileDefaultTestModel(profile: RelayProfile, form: BackendSettings): string {
  // 测试默认使用当前供应商模型；供应商未设置时才使用有明确设置入口的全局测试模型。
  return profile.model.trim() || form.relayTestModel.trim() || defaultSettings.relayTestModel;
}

export function relayProfileKnownModels(profile: RelayProfile, fallbackModel = ""): string[] {
  const requestModel = (model: string) => relayProfileRequestModelForCatalogModel(profile, model);
  const values = [
    requestModel(fallbackModel),
    requestModel(profile.model),
    ...relayProfileConfiguredModels(profile),
  ];
  return uniqueStrings(values.map((value) => value.trim()).filter(Boolean)).sort((left, right) =>
    left.localeCompare(right),
  );
}

export function relayProfileConfiguredModels(profile: RelayProfile): string[] {
  const values = [
    ...normalizeRelayModelMappings(profile.modelMappings).map((mapping) => mapping.requestModel),
    ...profile.responsesModelList.split(/\r?\n/),
    ...profile.chatCompletionsModelList.split(/\r?\n/),
    ...profile.anthropicModelList.split(/\r?\n/),
    ...profile.modelList.split(/\r?\n/),
  ];
  return uniqueStrings(values.map((value) => value.trim()).filter(Boolean)).sort((left, right) =>
    left.localeCompare(right),
  );
}

export function relayModelMappingCatalogIdentifier(
  mapping: RelayModelMapping,
  includeContextWindow: boolean,
): string {
  const requestModel = mapping.requestModel.trim();
  if (!requestModel) return "";
  const alias = mapping.alias.trim();
  if (alias) return alias;
  const contextWindow = mapping.contextWindow.trim();
  return includeContextWindow && contextWindow ? `${requestModel} ${contextWindow}` : requestModel;
}

export function relayModelMappingCatalogIdentifiers(mappings: RelayModelMapping[]): string[] {
  const unaliasedCounts = new Map<string, number>();
  for (const mapping of mappings) {
    const requestModel = mapping.requestModel.trim();
    if (requestModel && !mapping.alias.trim()) {
      unaliasedCounts.set(requestModel, (unaliasedCounts.get(requestModel) ?? 0) + 1);
    }
  }
  return mappings.map((mapping) => {
    const requestModel = mapping.requestModel.trim();
    return relayModelMappingCatalogIdentifier(
      mapping,
      (unaliasedCounts.get(requestModel) ?? 0) > 1,
    );
  });
}

export function normalizedRelayMappingContextWindow(value: string): string {
  const normalized = value.trim();
  if (!/^\d+$/.test(normalized)) return normalized;
  return normalized.replace(/^0+(?=\d)/, "");
}

export function relayModelMappingParameterKey(mapping: RelayModelMapping): string {
  return JSON.stringify([
    mapping.requestModel.trim(),
    normalizeRelayProtocol(mapping.protocol),
    normalizedRelayMappingContextWindow(mapping.contextWindow),
    (mapping.systemPromptOverride || "").trim(),
  ]);
}

export function legacyRelayModelMappingCatalogIdentifiers(mappings: RelayModelMapping[]): string[] {
  const reservedRequestModels = new Set(
    mappings.map((mapping) => mapping.requestModel.trim()).filter(Boolean),
  );
  const usedCatalogIdentifiers = new Set<string>();
  const occurrences = new Map<string, number>();
  return mappings.map((mapping) => {
    const requestModel = mapping.requestModel.trim();
    if (!requestModel) return "";
    const occurrence = (occurrences.get(requestModel) ?? 0) + 1;
    occurrences.set(requestModel, occurrence);
    if (occurrence === 1) {
      usedCatalogIdentifiers.add(requestModel);
      return requestModel;
    }
    const base = `${requestModel}--codex-elves-alias-${occurrence}`;
    let catalogIdentifier = base;
    let collisionIndex = 2;
    while (
      reservedRequestModels.has(catalogIdentifier)
      || usedCatalogIdentifiers.has(catalogIdentifier)
    ) {
      catalogIdentifier = `${base}-${collisionIndex}`;
      collisionIndex += 1;
    }
    usedCatalogIdentifiers.add(catalogIdentifier);
    return catalogIdentifier;
  });
}

export function relayModelMappingsHaveValidCatalogIdentifiers(mappings: RelayModelMapping[]): boolean {
  const catalogIdentifiers = relayModelMappingCatalogIdentifiers(mappings);
  const legacyIdentifiers = legacyRelayModelMappingCatalogIdentifiers(mappings);
  const seen = new Set<string>();
  for (let index = 0; index < mappings.length; index += 1) {
    const catalogIdentifier = catalogIdentifiers[index];
    const requestModel = mappings[index].requestModel.trim();
    if (!catalogIdentifier || seen.has(catalogIdentifier)) return false;
    seen.add(catalogIdentifier);
    if (mappings.some(
      (candidate, candidateIndex) =>
        candidateIndex !== index
        && candidate.requestModel.trim() === catalogIdentifier
        && candidate.requestModel.trim() !== requestModel,
    )) return false;
    if (legacyIdentifiers.some(
      (legacyIdentifier, candidateIndex) =>
        candidateIndex !== index
        && legacyIdentifier === catalogIdentifier
        && mappings[candidateIndex].requestModel.trim() !== requestModel,
    )) return false;
  }
  return true;
}

export function relayProfileCatalogMappings(profile: RelayProfile): RelayModelMapping[] {
  const mappings = normalizeRelayModelMappings(profile.modelMappings);
  const seenParameters = new Set<string>();
  const deduplicated = mappings.filter((mapping) => {
    const requestModel = mapping.requestModel.trim();
    if (!requestModel) return false;
    const key = relayModelMappingParameterKey(mapping);
    if (seenParameters.has(key)) return false;
    seenParameters.add(key);
    return true;
  });
  if (relayModelMappingsHaveValidCatalogIdentifiers(deduplicated)) return deduplicated;
  return deduplicated.map((mapping) => ({ ...mapping, alias: "" }));
}

export function relayProfileMappingForCatalogModel(
  profile: RelayProfile,
  model: string,
): RelayModelMapping | undefined {
  const normalizedModel = model.trim();
  if (!normalizedModel) return undefined;
  const mappings = normalizeRelayModelMappings(profile.modelMappings);
  const catalogIdentifiers = relayModelMappingCatalogIdentifiers(mappings);
  const catalogMapping = mappings.find(
    (_, index) => catalogIdentifiers[index] === normalizedModel,
  );
  const legacyIdentifiers = legacyRelayModelMappingCatalogIdentifiers(mappings);
  const legacyMapping = mappings.find(
    (_, index) => legacyIdentifiers[index] === normalizedModel,
  );
  const catalogMappings = relayProfileCatalogMappings(profile);
  const migratedCatalogIdentifiers = relayModelMappingCatalogIdentifiers(catalogMappings);
  const migratedMapping = catalogMappings.find(
    (_, index) => migratedCatalogIdentifiers[index] === normalizedModel,
  );
  const originalMigratedMapping = migratedMapping
    ? mappings.find(
      (item) => relayModelMappingParameterKey(item) === relayModelMappingParameterKey(migratedMapping),
    )
    : undefined;
  const requestMapping = mappings.find((item) => item.requestModel.trim() === normalizedModel);
  return relayModelMappingsHaveValidCatalogIdentifiers(mappings)
    ? catalogMapping || originalMigratedMapping || legacyMapping || requestMapping
    : legacyMapping || originalMigratedMapping || catalogMapping || requestMapping;
}

export function relayProfileRequestModelForCatalogModel(profile: RelayProfile, model: string): string {
  const normalizedModel = model.trim();
  if (!normalizedModel) return "";
  return relayProfileMappingForCatalogModel(profile, normalizedModel)?.requestModel.trim()
    || normalizedModel;
}

export function relayProfileContextWindowForActiveModel(profile: RelayProfile): string {
  return relayProfileContextWindowForModel(profile, profile.model)
    || profile.contextWindow.trim();
}

export function relayProfileContextWindowForModel(profile: RelayProfile, model: string): string {
  const normalizedModel = model.trim();
  if (!normalizedModel) return "";
  const mapping = relayProfileMappingForCatalogModel(profile, normalizedModel);
  const requestModel = mapping?.requestModel.trim() || normalizedModel;
  return mapping?.contextWindow.trim()
    || (profile.model.trim() === normalizedModel ? profile.contextWindow.trim() : "")
    || defaultModelContextWindow(requestModel);
}

export function ccsProviderSummary(result: CcsProvidersResult | null): string {
  if (!result) return "读取 ~/.cc-switch/cc-switch.db";
  if (!isSuccessStatus(result.status)) return result.message || "读取 cc-switch 供应商失败。";
  const count = result.providers.length;
  return count ? `发现 ${count} 个 Codex 供应商` : "未发现可导入供应商";
}

export function normalizeRelayMode(mode: RelayMode | undefined): RelayMode {
  if (mode === "aggregate") return mode;
  if (mode === "pureApi") return mode;
  return "official";
}

export function normalizeContextSelection(
  selection?: Partial<RelayContextSelection>,
  fallback: RelayContextSelection = emptyContextSelection(),
): RelayContextSelection {
  if (!selection) {
    return {
      mcpServers: [...fallback.mcpServers],
      skills: [...fallback.skills],
      plugins: [...fallback.plugins],
    };
  }
  return {
    mcpServers: Array.isArray(selection?.mcpServers) ? selection.mcpServers.map(String) : [],
    skills: Array.isArray(selection?.skills) ? selection.skills.map(String) : [],
    plugins: Array.isArray(selection?.plugins) ? selection.plugins.map(String) : [],
  };
}

export function relayModeLabel(mode: RelayMode): string {
  if (mode === "aggregate") return "聚合供应商";
  if (mode === "pureApi") return "纯 API";
  return "官方登录";
}

export function relayProtocolLabel(protocol: RelayProtocol): string {
  if (protocol === "chatCompletions") return "Chat Completions";
  if (protocol === "anthropic") return "Anthropic";
  return "Responses API";
}

export function relayProfileConfigBrief(profile: RelayProfile): string {
  if (isAggregateRelayProfile(profile)) {
    const aggregate = normalizeAggregateConfig(profile.aggregate, []);
    return `${aggregateStrategyLabel(aggregate.strategy)} · ${aggregate.members.length} 个成员`;
  }
  if (profile.relayMode === "official") return profile.officialMixApiKey ? "混入 API Key" : "不写 API 文件";
  const proxyLabel = profile.localProxyEnabled ? "本地代理" : "直连";
  return `${proxyLabel} · ${profile.baseUrl || "未填写 URL"}`;
}

export function relayProfileModeHelp(profile: RelayProfile): string {
  if (isAggregateRelayProfile(profile)) {
    return "聚合供应商只保存成员和策略配置，成员来自已有 API 供应商；切为当前后会通过本地协议代理轮转请求。";
  }
  if (profile.relayMode === "official") {
    if (profile.officialMixApiKey) {
      return "此供应商会保留官方登录模式，并把请求混入当前 API Key；功能增强仍使用兼容模式。";
    }
    return "此供应商会切回官方登录模式，使用 ChatGPT 官方账号，不写入 API Key。";
  }
  if (profile.relayMode === "pureApi") {
    return "此供应商会同时写入 config.toml 和 auth.json；API Key 同时写入 provider bearer token。";
  }
  return "此供应商会保留官方登录模式，并把请求混入当前 API Key；功能增强仍使用兼容模式。";
}

export function emptyResponsesWebsocketCapability(): ResponsesWebsocketCapability {
  return {
    state: "unknown",
    endpoint: "",
    checkedAtMs: null,
    message: "",
  };
}

export function normalizeResponsesWebsocketCapability(
  profile: Pick<RelayProfile, "responsesWebsocket">,
): ResponsesWebsocketCapability {
  const capability = profile.responsesWebsocket;
  const state: ResponsesWebsocketCapabilityState =
    capability?.state === "supported" || capability?.state === "unsupported"
      ? capability.state
      : "unknown";
  return {
    state,
    endpoint: capability?.endpoint || "",
    checkedAtMs: typeof capability?.checkedAtMs === "number" ? capability.checkedAtMs : null,
    message: capability?.message || "",
  };
}

export function relayCanProbeNativeResponsesWebsocket(profile: RelayProfile): boolean {
  const mappings = profile.modelMappings.filter((mapping) => mapping.requestModel.trim());
  const hasResponsesModel = mappings.length
    ? mappings.some((mapping) => mapping.protocol === "responses")
    : splitRelayModelList(profile.responsesModelList).length > 0 || profile.protocol === "responses";
  return (
    !isAggregateRelayProfile(profile)
    && (profile.relayMode !== "official" || profile.officialMixApiKey)
    && hasResponsesModel
  );
}

export function relaySupportsNativeResponsesWebsocket(profile: RelayProfile): boolean {
  return relayCanProbeNativeResponsesWebsocket(profile)
    && profile.responsesWebsocket.state === "supported";
}

export function relayPrefersNativeResponsesWebsocket(profile: RelayProfile): boolean {
  return profile.responsesWebsocketEnabled
    && relaySupportsNativeResponsesWebsocket(profile);
}

export function responsesWebsocketEndpointChanged(
  profile: RelayProfile,
  patch: Partial<RelayProfile>,
): boolean {
  if ("baseUrl" in patch && (patch.baseUrl || "") !== profile.baseUrl) return true;
  return "upstreamBaseUrl" in patch
    && (patch.upstreamBaseUrl || "") !== profile.upstreamBaseUrl;
}

export function withGeneratedRelayFiles(profile: RelayProfile): RelayProfile {
  if (isAggregateRelayProfile(profile)) {
    return { ...profile, configContents: "", authContents: "", aggregate: normalizeAggregateConfig(profile.aggregate, []) };
  }
  if (profile.relayMode === "official") {
    return {
      ...profile,
      configContents: profile.officialMixApiKey ? buildRelayConfigToml(profile, { includeBearerToken: true }) : "",
      authContents: profile.authContents || "",
    };
  }
  return {
    ...profile,
    configContents: buildRelayConfigToml(profile, { includeBearerToken: false }),
    authContents: buildRelayAuthJson(profile),
  };
}

export function buildRelayConfigToml(
  profile: Pick<
    RelayProfile,
    | "model"
    | "baseUrl"
    | "upstreamBaseUrl"
    | "apiKey"
    | "protocol"
    | "localProxyEnabled"
    | "relayMode"
    | "officialMixApiKey"
    | "modelMappings"
    | "chatCompletionsModelList"
    | "anthropicModelList"
    | "systemPromptOverride"
    | "responsesWebsocket"
    | "responsesWebsocketEnabled"
    | "aggregate"
  >,
  options: { includeBearerToken: boolean },
): string {
  const baseUrl = profile.localProxyEnabled ? PROTOCOL_PROXY_BASE_URL : profile.baseUrl.trim();
  const apiKey = profile.apiKey.trim();
  const rootLines = [
    profile.model.trim() ? `model = "${tomlString(profile.model.trim())}"` : null,
    'model_provider = "custom"',
    "",
  ].filter((line): line is string => line !== null);
  return [
    ...rootLines,
    "[model_providers.custom]",
    'name = "custom"',
    'wire_api = "responses"',
    "requires_openai_auth = true",
    `supports_websockets = ${relayPrefersNativeResponsesWebsocket(profile as RelayProfile) ? "true" : "false"}`,
    `base_url = "${tomlString(baseUrl)}"`,
    options.includeBearerToken && apiKey ? `experimental_bearer_token = "${tomlString(apiKey)}"` : null,
    "",
  ].filter((line): line is string => line !== null).join("\n");
}

export function buildRelayAuthJson(profile: Pick<RelayProfile, "apiKey">): string {
  return `${JSON.stringify({ OPENAI_API_KEY: profile.apiKey.trim() }, null, 2)}\n`;
}

export function buildOfficialRelayAuthJson(contents: string): string {
  const trimmed = contents.trim();
  if (!trimmed) return "";
  try {
    const parsed = JSON.parse(trimmed) as Record<string, unknown>;
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return "";
    delete parsed.OPENAI_API_KEY;
    return `${JSON.stringify(parsed, null, 2)}\n`;
  } catch {
    return "";
  }
}

export function deriveRelayProfileFromFiles(profile: RelayProfile): RelayProfile {
  if (isAggregateRelayProfile(profile)) {
    return normalizeAggregateRelayProfile(profile, null);
  }
  const configContents = profile.configContents || "";
  const authContents = profile.relayMode === "official" ? buildOfficialRelayAuthJson(profile.authContents || "") : profile.authContents || "";
  const configBaseUrl = codexBaseUrlFromConfig(configContents);
  const chatUpstreamBaseUrl = rootTomlStringValue(configContents, CHAT_UPSTREAM_BASE_URL_KEY);
  const isProxyConfig = configBaseUrl === PROTOCOL_PROXY_BASE_URL;
  const upstreamBaseUrl = profile.upstreamBaseUrl || chatUpstreamBaseUrl || (configBaseUrl && !isProxyConfig ? configBaseUrl : profile.baseUrl || "");
  const configApiKey = codexExperimentalBearerTokenFromConfig(configContents);
  const derived = {
    ...profile,
    model: codexModelFromConfig(configContents),
    baseUrl: upstreamBaseUrl,
    upstreamBaseUrl,
    localProxyEnabled: profile.localProxyEnabled ?? isProxyConfig,
    apiKey: profile.relayMode === "official"
      ? configApiKey || profile.apiKey || ""
      : codexApiKeyFromAuth(authContents) || configApiKey || profile.apiKey || "",
    contextWindow: codexTopLevelIntFromConfig(configContents, "model_context_window"),
    autoCompactLimit: codexTopLevelIntFromConfig(configContents, "model_auto_compact_token_limit"),
    configContents,
    authContents,
  };
  return {
    ...derived,
    responsesWebsocket: normalizeResponsesWebsocketCapability(derived),
  };
}

export function applyRelayProfilePatchToFiles(
  profile: RelayProfile,
  patch: Partial<RelayProfile>,
  options: { allowGenerateFiles?: boolean } = {},
): RelayProfile {
  let next: RelayProfile = { ...profile, ...patch };
  if (responsesWebsocketEndpointChanged(profile, patch)) {
    next.responsesWebsocket = emptyResponsesWebsocketCapability();
  } else {
    next.responsesWebsocket = normalizeResponsesWebsocketCapability(next);
  }
  if (isAggregateRelayProfile(next)) {
    return normalizeAggregateRelayProfile(next, null);
  }
  const shouldHaveFiles =
    next.relayMode !== "official" || next.officialMixApiKey || next.configContents.trim() || next.authContents.trim();
  const needsAuthFile = next.relayMode === "pureApi";
  if (options.allowGenerateFiles && shouldHaveFiles && (!next.configContents.trim() || (needsAuthFile && !next.authContents.trim()))) {
    next = withGeneratedRelayFiles(next);
  }

  if ("model" in patch) {
    next.configContents = setRootTomlStringKey(next.configContents, "model", patch.model || "");
  }
  if ("apiKey" in patch) {
    if (next.relayMode === "pureApi") {
      next.authContents = setAuthOpenAiApiKey(next.authContents, patch.apiKey || "");
      next.configContents = removeCodexExperimentalBearerToken(next.configContents);
    } else {
      next.configContents = setCodexExperimentalBearerToken(next.configContents, patch.apiKey || "");
    }
  }
  if ("baseUrl" in patch) {
    next.upstreamBaseUrl = patch.baseUrl || "";
  }
  if ("upstreamBaseUrl" in patch) {
    next.baseUrl = patch.upstreamBaseUrl || "";
  }
  if ("baseUrl" in patch || "upstreamBaseUrl" in patch || "protocol" in patch || "localProxyEnabled" in patch) {
    const baseUrlForConfig = next.localProxyEnabled ? PROTOCOL_PROXY_BASE_URL : next.upstreamBaseUrl || next.baseUrl;
    next.configContents = setCodexProviderStringKey(next.configContents, "base_url", baseUrlForConfig);
    next.configContents = removeRootTomlKey(next.configContents, CHAT_UPSTREAM_BASE_URL_KEY);
  }
  if ("model" in patch || "modelMappings" in patch || "contextWindow" in patch) {
    next.configContents = setRootTomlIntKey(
      next.configContents,
      "model_context_window",
      relayProfileContextWindowForActiveModel(next),
    );
  }
  if ("autoCompactLimit" in patch) {
    next.configContents = setRootTomlIntKey(
      next.configContents,
      "model_auto_compact_token_limit",
      patch.autoCompactLimit || "",
    );
  }
  if ("relayMode" in patch || "officialMixApiKey" in patch) {
    if (next.relayMode === "official" && !next.officialMixApiKey) {
      next.configContents = "";
      next.authContents = buildOfficialRelayAuthJson(next.authContents);
    } else if (options.allowGenerateFiles && (!next.configContents.trim() || (next.relayMode === "pureApi" && !next.authContents.trim()))) {
      next = withGeneratedRelayFiles(next);
    }
  }
  if (next.configContents.trim()) {
    next.configContents = setCodexProviderBoolKey(
      next.configContents,
      "supports_websockets",
      relayPrefersNativeResponsesWebsocket(next),
    );
  }

  return deriveRelayProfileFromFiles(next);
}

export function codexModelFromConfig(contents: string): string {
  for (const line of contents.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    if (trimmed.startsWith("[")) break;
    const match = /^model\s*=\s*(["'])(.*)\1\s*$/.exec(trimmed);
    if (match) return match[2].replace(/\\(["'\\])/g, "$1");
  }
  return "";
}

export function codexBaseUrlFromConfig(contents: string): string {
  return codexProviderStringFromConfig(contents, "base_url");
}

export function codexExperimentalBearerTokenFromConfig(contents: string): string {
  return codexProviderStringFromConfig(contents, "experimental_bearer_token");
}

export function codexProviderStringFromConfig(contents: string, key: string): string {
  const provider = rootTomlStringValue(contents, "model_provider");
  const targetSection = provider ? `model_providers.${provider}` : "";
  const lines = contents.split(/\r?\n/);
  let currentSection = "";
  const matches: string[] = [];

  for (const line of lines) {
    const section = tomlSectionName(line);
    if (section !== null) {
      currentSection = section;
      continue;
    }
    const value = tomlStringAssignmentValue(line, key);
    if (value === null) continue;
    if (targetSection && currentSection === targetSection) return value;
    if (!currentSection || !currentSection.startsWith("model_providers.")) matches.push(value);
  }

  return matches.length === 1 ? matches[0] : "";
}

export function codexApiKeyFromAuth(contents: string): string {
  try {
    const parsed = JSON.parse(contents || "{}") as { OPENAI_API_KEY?: unknown };
    return typeof parsed.OPENAI_API_KEY === "string" ? parsed.OPENAI_API_KEY : "";
  } catch {
    return "";
  }
}

export function codexTopLevelIntFromConfig(contents: string, key: string): string {
  const topLevel = splitTomlRootAndTables(contents).root;
  const pattern = new RegExp(`^\\s*${key}\\s*=\\s*(\\d+)\\s*(?:#.*)?$`);
  for (const line of topLevel.split(/\r?\n/)) {
    const match = pattern.exec(line);
    if (match) return match[1];
  }
  return "";
}

export function rootTomlStringValue(contents: string, key: string): string {
  const topLevel = splitTomlRootAndTables(contents).root;
  for (const line of topLevel.split(/\r?\n/)) {
    const value = tomlStringAssignmentValue(line, key);
    if (value !== null) return value;
  }
  return "";
}

export function tomlSectionName(line: string): string | null {
  const match = /^\s*\[([^\]]+)\]\s*$/.exec(line);
  return match ? match[1].trim() : null;
}

export function tomlStringAssignmentValue(line: string, key: string): string | null {
  const match = new RegExp(`^\\s*${key}\\s*=\\s*([\"'])(.*)\\1\\s*(?:#.*)?$`).exec(line.trim());
  if (!match) return null;
  return match[2].replace(/\\(["'\\])/g, "$1");
}

export function tomlSectionBoolValue(contents: string, sectionName: string, key: string): boolean {
  return tomlSectionOptionalBoolValue(contents, sectionName, key) === true;
}

export function tomlSectionOptionalBoolValue(
  contents: string,
  sectionName: string,
  key: string,
): boolean | null {
  let currentSection = "";
  const pattern = new RegExp(`^\\s*${key}\\s*=\\s*(true|false)\\s*(?:#.*)?$`);
  for (const line of contents.split(/\r?\n/)) {
    const section = tomlSectionName(line);
    if (section !== null) {
      currentSection = section;
      continue;
    }
    if (currentSection !== sectionName) continue;
    const match = pattern.exec(line);
    if (match) return match[1] === "true";
  }
  return null;
}

export function tomlSectionIntValue(contents: string, sectionName: string, key: string): number | null {
  let currentSection = "";
  const pattern = new RegExp(`^\\s*${key}\\s*=\\s*(\\d+)\\s*(?:#.*)?$`);
  for (const line of contents.split(/\r?\n/)) {
    const section = tomlSectionName(line);
    if (section !== null) {
      currentSection = section;
      continue;
    }
    if (currentSection !== sectionName) continue;
    const match = pattern.exec(line);
    if (match) return Number(match[1]);
  }
  return null;
}

export function setAuthOpenAiApiKey(contents: string, apiKey: string): string {
  let parsed: Record<string, unknown> = {};
  try {
    const value = JSON.parse(contents || "{}");
    if (value && typeof value === "object" && !Array.isArray(value)) parsed = value as Record<string, unknown>;
  } catch {
    parsed = {};
  }
  parsed.OPENAI_API_KEY = apiKey.trim();
  return `${JSON.stringify(parsed, null, 2)}\n`;
}

export function setRootTomlStringKey(contents: string, key: string, value: string): string {
  const trimmed = value.trim();
  if (!trimmed) return removeRootTomlKey(contents, key);
  return setRootTomlLine(contents, key, `${key} = "${tomlString(trimmed)}"`);
}

export function setRootTomlIntKey(contents: string, key: string, value: string): string {
  const trimmed = value.replace(/[^\d]/g, "");
  if (!trimmed) return removeRootTomlKey(contents, key);
  return setRootTomlLine(contents, key, `${key} = ${trimmed}`);
}

export function setRootTomlLine(contents: string, key: string, lineText: string): string {
  const lines = contents.split(/\r?\n/);
  const firstTable = lines.findIndex((line) => /^\s*\[[^\]]+\]\s*$/.test(line));
  const rootEnd = firstTable >= 0 ? firstTable : lines.length;
  for (let index = 0; index < rootEnd; index += 1) {
    if (new RegExp(`^\\s*${key}\\s*=`).test(lines[index])) {
      lines[index] = lineText;
      return ensureTrailingNewline(lines.join("\n").trimEnd());
    }
  }
  const insertAt = key === "model" ? 0 : rootEnd;
  lines.splice(insertAt, 0, lineText);
  return ensureTrailingNewline(lines.join("\n").trimEnd());
}

export function setCodexProviderStringKey(contents: string, key: string, value: string): string {
  const provider = rootTomlStringValue(contents, "model_provider") || "custom";
  let next = contents;
  if (!rootTomlStringValue(next, "model_provider")) {
    next = setRootTomlStringKey(next, "model_provider", provider);
  }
  next = ensureCodexProviderDefaults(next, provider);
  return setTomlSectionStringKey(next, `model_providers.${provider}`, key, value);
}

export function setCodexProviderBoolKey(contents: string, key: string, value: boolean): string {
  const provider = rootTomlStringValue(contents, "model_provider") || "custom";
  let next = contents;
  if (!rootTomlStringValue(next, "model_provider")) {
    next = setRootTomlStringKey(next, "model_provider", provider);
  }
  next = ensureCodexProviderDefaults(next, provider);
  return setTomlSectionBoolKey(next, `model_providers.${provider}`, key, value);
}

export function setCodexExperimentalBearerToken(contents: string, apiKey: string): string {
  const trimmed = apiKey.trim();
  return trimmed
    ? setCodexProviderStringKey(contents, "experimental_bearer_token", trimmed)
    : removeCodexExperimentalBearerToken(contents);
}

export function removeCodexExperimentalBearerToken(contents: string): string {
  const provider = rootTomlStringValue(contents, "model_provider") || "custom";
  return removeTomlSectionKey(contents, `model_providers.${provider}`, "experimental_bearer_token");
}

export function ensureCodexProviderDefaults(contents: string, provider: string): string {
  let next = contents;
  const section = `model_providers.${provider}`;
  if (!codexProviderStringFromConfig(next, "name").trim()) {
    next = setTomlSectionStringKey(next, section, "name", provider);
  }
  next = setTomlSectionStringKey(next, section, "wire_api", "responses");
  return setTomlSectionBoolKey(next, section, "requires_openai_auth", true);
}

export function setTomlSectionBoolKey(contents: string, sectionName: string, key: string, value: boolean): string {
  return setTomlSectionRawKey(contents, sectionName, key, value ? "true" : "false");
}

export function setTomlSectionStringKey(contents: string, sectionName: string, key: string, value: string): string {
  return setTomlSectionRawKey(contents, sectionName, key, `"${tomlString(value.trim())}"`);
}

export function setTomlSectionRawKey(contents: string, sectionName: string, key: string, value: string): string {
  const lines = contents.split(/\r?\n/);
  let sectionStart = -1;
  let sectionEnd = lines.length;
  for (let index = 0; index < lines.length; index += 1) {
    const section = tomlSectionName(lines[index]);
    if (section === null) continue;
    if (sectionStart >= 0) {
      sectionEnd = index;
      break;
    }
    if (section === sectionName) sectionStart = index;
  }
  if (sectionStart < 0) {
    const prefix = ensureTrailingNewline(lines.join("\n").trimEnd()).trimEnd();
    return joinTomlSections([prefix, `[${sectionName}]\n${key} = ${value}`]);
  }
  const replacement = `${key} = ${value}`;
  for (let index = sectionStart + 1; index < sectionEnd; index += 1) {
    if (new RegExp(`^\\s*${key}\\s*=`).test(lines[index])) {
      lines[index] = replacement;
      return ensureTrailingNewline(lines.join("\n").trimEnd());
    }
  }
  let insertAt = sectionEnd;
  while (insertAt > sectionStart + 1 && lines[insertAt - 1].trim() === "") insertAt -= 1;
  lines.splice(insertAt, 0, replacement);
  return ensureTrailingNewline(lines.join("\n").trimEnd());
}

export function removeTomlSectionKey(contents: string, sectionName: string, key: string): string {
  const lines = contents.split(/\r?\n/);
  let sectionStart = -1;
  let sectionEnd = lines.length;
  for (let index = 0; index < lines.length; index += 1) {
    const section = tomlSectionName(lines[index]);
    if (section === null) continue;
    if (sectionStart >= 0) {
      sectionEnd = index;
      break;
    }
    if (section === sectionName) sectionStart = index;
  }
  if (sectionStart < 0) return contents;
  const next = lines.filter((line, index) => {
    if (index <= sectionStart || index >= sectionEnd) return true;
    return !new RegExp(`^\\s*${key}\\s*=`).test(line);
  });
  return ensureTrailingNewline(next.join("\n").trimEnd());
}

export function removeTomlSection(contents: string, sectionName: string): string {
  const lines = contents.split(/\r?\n/);
  let sectionStart = -1;
  let sectionEnd = lines.length;
  for (let index = 0; index < lines.length; index += 1) {
    const section = tomlSectionName(lines[index]);
    if (section === null) continue;
    if (sectionStart >= 0) {
      sectionEnd = index;
      break;
    }
    if (section === sectionName) sectionStart = index;
  }
  if (sectionStart < 0) return contents;
  lines.splice(sectionStart, sectionEnd - sectionStart);
  return ensureTrailingNewline(lines.join("\n").trimEnd());
}

export function relayProfileUsesLiveFiles(profile: RelayProfile): boolean {
  return profile.relayMode !== "official" || profile.officialMixApiKey;
}

export function tomlString(value: string): string {
  return value.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}

export function syncLegacyRelayFields(settings: BackendSettings): BackendSettings {
  const relayProfiles = settings.relayProfiles.map((profile) =>
    isAggregateRelayProfile(profile) ? normalizeAggregateRelayProfile(profile, { ...settings, relayProfiles: settings.relayProfiles }) : deriveRelayProfileFromFiles(profile),
  );
  const active = activeRelayProfile({ ...settings, relayProfiles });
  const aggregateRelayProfiles = normalizeAggregateProfilesFromRelayProfiles(relayProfiles);
  const activeAggregateRelayId = isAggregateRelayProfile(active) ? active.id : "";
  return {
    ...settings,
    relayProfiles,
    activeRelayId: active.id,
    relayBaseUrl: isAggregateRelayProfile(active) ? PROTOCOL_PROXY_BASE_URL : active.baseUrl,
    relayApiKey: active.apiKey,
    aggregateRelayProfiles,
    activeAggregateRelayId,
  };
}

export function normalizeAggregateProfilesFromRelayProfiles(profiles: RelayProfile[]): AggregateRelayProfile[] {
  const candidates = profiles.filter((profile) => !isAggregateRelayProfile(profile));
  return profiles.filter(isAggregateRelayProfile).map((profile) => {
    const aggregate = normalizeAggregateConfig(profile.aggregate, candidates);
    return {
      id: profile.id,
      name: profile.name || "聚合供应商",
      strategy: aggregate.strategy,
      members: aggregate.members.map((member) => ({
        relayId: member.profileId,
        weight: clampAggregateWeight(member.weight),
      })),
    };
  });
}

export function updateRelayProfile(settings: BackendSettings, id: string, patch: Partial<RelayProfile>): BackendSettings {
  if (patch.relayMode === "aggregate" || patch.aggregate) {
    return syncLegacyRelayFields({
      ...settings,
      relayProfiles: settings.relayProfiles.map((profile) =>
        profile.id === id ? normalizeAggregateRelayProfile({ ...profile, ...patch }, settings) : profile,
      ),
    });
  }
  return syncLegacyRelayFields({
    ...settings,
    relayProfiles: settings.relayProfiles.map((profile) => {
      if (profile.id !== id) return profile;
      return applyRelayProfilePatchToFiles(profile, patch);
    }),
  });
}

export function createRelayProfile(settings: BackendSettings): RelayProfile {
  const id = `relay-${Date.now().toString(36)}`;
  const contextSelection = contextSelectionForAllEntries(settings);
  const next = {
    id,
    name: `供应商 ${settings.relayProfiles.length + 1}`,
    model: "",
    baseUrl: defaultSettings.relayBaseUrl,
    upstreamBaseUrl: defaultSettings.relayBaseUrl,
    apiKey: "",
    protocol: "responses" as RelayProtocol,
    localProxyEnabled: false,
    relayMode: "official" as RelayMode,
    officialMixApiKey: false,
    configContents: "",
    authContents: "",
    useCommonConfig: true,
    contextSelection,
    contextSelectionInitialized: true,
    contextWindow: "",
    autoCompactLimit: "",
    modelMappings: [],
    modelList: "",
    responsesModelList: "",
    chatCompletionsModelList: "",
    anthropicModelList: "",
    responsesWebsocket: emptyResponsesWebsocketCapability(),
    responsesWebsocketEnabled: true,
    userAgent: "",
    systemPromptOverride: "",
  };
  return withGeneratedRelayFiles(next);
}

export function createAggregateRelayProfile(settings: BackendSettings): RelayProfile {
  const id = `aggregate-${Date.now().toString(36)}`;
  const contextSelection = contextSelectionForAllEntries(settings);
  const candidates = aggregateMemberCandidates(settings, id);
  return normalizeAggregateRelayProfile(
    {
      id,
      name: `聚合供应商 ${settings.relayProfiles.filter(isAggregateRelayProfile).length + 1}`,
      model: "",
      baseUrl: "",
      upstreamBaseUrl: "",
      apiKey: "",
      protocol: "responses",
      localProxyEnabled: false,
      relayMode: "aggregate",
      officialMixApiKey: false,
      configContents: "",
      authContents: "",
      useCommonConfig: true,
      contextSelection,
      contextSelectionInitialized: true,
      contextWindow: "",
      autoCompactLimit: "",
      modelMappings: [],
      modelList: "",
      responsesModelList: "",
      chatCompletionsModelList: "",
      anthropicModelList: "",
      responsesWebsocket: emptyResponsesWebsocketCapability(),
      responsesWebsocketEnabled: true,
      userAgent: "",
      systemPromptOverride: "",
      aggregate: {
        strategy: "failover",
        members: candidates.slice(0, 1).map((profile) => ({ profileId: profile.id, weight: 1 })),
      },
    },
    settings,
  );
}

export function addRelayProfile(settings: BackendSettings, profile: RelayProfile): BackendSettings {
  const nextWithFiles = isAggregateRelayProfile(profile)
    ? normalizeAggregateRelayProfile(profile, settings)
    : deriveRelayProfileFromFiles(
        profile.configContents.trim() || profile.authContents.trim() ? profile : withGeneratedRelayFiles(profile),
      );
  const activeId = settings.relayProfiles.some((item) => item.id === settings.activeRelayId)
    ? settings.activeRelayId
    : activeRelayProfile(settings).id;
  return syncLegacyRelayFields({
    ...settings,
    relayProfiles: [...settings.relayProfiles, nextWithFiles],
    activeRelayId: activeId,
  });
}

export function duplicateRelayProfile(settings: BackendSettings, id: string): BackendSettings {
  const sourceIndex = settings.relayProfiles.findIndex((profile) => profile.id === id);
  const source = settings.relayProfiles[sourceIndex] || activeRelayProfile(settings);
  const nextId = `relay-${Date.now().toString(36)}`;
  const next = {
    ...source,
    id: nextId,
    name: `${source.name || "未命名供应商"} 副本`,
    responsesWebsocket: emptyResponsesWebsocketCapability(),
    responsesWebsocketEnabled: source.responsesWebsocketEnabled !== false,
  };
  const normalizedNext = isAggregateRelayProfile(next) ? normalizeAggregateRelayProfile(next, settings) : next;
  const relayProfiles = [...settings.relayProfiles];
  relayProfiles.splice(sourceIndex >= 0 ? sourceIndex + 1 : relayProfiles.length, 0, normalizedNext);
  return syncLegacyRelayFields({
    ...settings,
    relayProfiles,
  });
}

export function reorderRelayProfiles(settings: BackendSettings, sourceId: string, targetId: string): BackendSettings {
  if (sourceId === targetId) return settings;
  const sourceIndex = settings.relayProfiles.findIndex((profile) => profile.id === sourceId);
  const targetIndex = settings.relayProfiles.findIndex((profile) => profile.id === targetId);
  if (sourceIndex < 0 || targetIndex < 0) return settings;
  const relayProfiles = [...settings.relayProfiles];
  const [moved] = relayProfiles.splice(sourceIndex, 1);
  relayProfiles.splice(targetIndex, 0, moved);
  return syncLegacyRelayFields({
    ...settings,
    relayProfiles,
  });
}

export function relayModelMappingRowId(index: number): string {
  return `relay-model-mapping-${index}`;
}

export function relayModelMappingIndexFromId(id: string): number {
  const prefix = "relay-model-mapping-";
  if (!id.startsWith(prefix)) return -1;
  const parsed = Number.parseInt(id.slice(prefix.length), 10);
  return Number.isFinite(parsed) ? parsed : -1;
}

export function reorderRelayModelMappings(mappings: RelayModelMapping[], sourceId: string, targetId: string): RelayModelMapping[] {
  if (sourceId === targetId || mappings.length < 2) return mappings;
  const sourceIndex = relayModelMappingIndexFromId(sourceId);
  const targetIndex = relayModelMappingIndexFromId(targetId);
  if (sourceIndex < 0 || targetIndex < 0 || sourceIndex >= mappings.length || targetIndex >= mappings.length) return mappings;
  const next = [...mappings];
  const [moved] = next.splice(sourceIndex, 1);
  next.splice(targetIndex, 0, moved);
  return next;
}

export function removeRelayProfile(settings: BackendSettings, id: string): BackendSettings {
  const profiles = settings.relayProfiles.filter((profile) => profile.id !== id);
  const scrubbedProfiles = profiles.map((profile) =>
    isAggregateRelayProfile(profile)
      ? normalizeAggregateRelayProfile(
          {
            ...profile,
            aggregate: {
              ...normalizeAggregateConfig(profile.aggregate, []),
              members: normalizeAggregateConfig(profile.aggregate, []).members.filter((member) => member.profileId !== id),
            },
          },
          { ...settings, relayProfiles: profiles },
        )
      : profile,
  );
  return syncLegacyRelayFields({
    ...settings,
    relayProfiles: scrubbedProfiles.length ? scrubbedProfiles : defaultSettings.relayProfiles,
    activeRelayId: settings.activeRelayId === id ? scrubbedProfiles[0]?.id || "default" : settings.activeRelayId,
  });
}

export const aggregateStrategyOptions: Array<{ value: RelayAggregateStrategy; label: string; description: string }> = [
  {
    value: "failover",
    label: "失败切换",
    description: "按成员顺序请求，失败后切到下一个供应商。",
  },
  {
    value: "conversationRoundRobin",
    label: "按对话轮转",
    description: "同一对话保持一个成员，不同对话依次分配。",
  },
  {
    value: "requestRoundRobin",
    label: "按请求轮转",
    description: "每次请求按成员顺序切换，适合均匀摊请求量。",
  },
  {
    value: "weightedRoundRobin",
    label: "权重轮转",
    description: "按成员权重分配请求，权重越高承担越多。",
  },
];

export function isAggregateRelayProfile(profile: Pick<RelayProfile, "relayMode" | "aggregate">): boolean {
  return profile.relayMode === "aggregate" || !!profile.aggregate;
}

export function normalizeAggregateRelayProfile(profile: RelayProfile, settings: BackendSettings | null): RelayProfile {
  const candidates = settings ? aggregateMemberCandidates(settings, profile.id) : [];
  const aggregate = normalizeAggregateConfig(profile.aggregate, candidates);
  return {
    ...profile,
    baseUrl: "",
    upstreamBaseUrl: "",
    apiKey: "",
    protocol: "responses",
    relayMode: "aggregate",
    officialMixApiKey: false,
    configContents: "",
    authContents: "",
    responsesWebsocket: emptyResponsesWebsocketCapability(),
    responsesWebsocketEnabled: false,
    systemPromptOverride: "",
    aggregate,
  };
}

export function normalizeAggregateConfig(
  aggregate: RelayAggregateConfig | null | undefined,
  candidates: RelayProfile[],
): RelayAggregateConfig {
  const candidateIds = new Set(candidates.map((profile) => profile.id));
  const seen = new Set<string>();
  const strategy: RelayAggregateStrategy =
    aggregate?.strategy && aggregateStrategyOptions.some((option) => option.value === aggregate.strategy)
      ? aggregate.strategy
      : "failover";
  const members = (aggregate?.members ?? [])
    .filter((member) => member.profileId && !seen.has(member.profileId))
    .filter((member) => !candidateIds.size || candidateIds.has(member.profileId))
    .map((member) => {
      seen.add(member.profileId);
      return { profileId: member.profileId, weight: clampAggregateWeight(member.weight) };
    });
  return { strategy, members };
}

export function aggregateMemberCandidates(settings: BackendSettings, aggregateId: string): RelayProfile[] {
  return settings.relayProfiles.filter(
    (profile) => profile.id !== aggregateId && !isAggregateRelayProfile(profile) && isApiRelayProfile(profile),
  );
}

export function isApiRelayProfile(profile: RelayProfile): boolean {
  return Boolean(profile.baseUrl.trim() && profile.apiKey.trim());
}

export function clampAggregateWeight(value: number): number {
  if (!Number.isFinite(value)) return 1;
  return Math.max(1, Math.min(999, Math.round(value)));
}

export function aggregateStrategyLabel(strategy: RelayAggregateStrategy): string {
  return aggregateStrategyOptions.find((option) => option.value === strategy)?.label ?? "失败切换";
}

export function aggregateStrategyHelp(strategy: RelayAggregateStrategy): string {
  if (strategy === "failover") return "失败切换会保留成员顺序，优先使用第一个可用供应商。";
  if (strategy === "conversationRoundRobin") return "按对话轮转会让同一对话尽量保持固定成员，降低上下文漂移。";
  if (strategy === "requestRoundRobin") return "按请求轮转会逐请求切换成员，适合供应商能力接近的场景。";
  return "权重轮转会读取每个成员的权重值，权重越高的成员获得更多请求。";
}

export function aggregateRelayProfileValidation(profile: RelayProfile, settings?: BackendSettings): string | null {
  const aggregate = normalizeAggregateConfig(profile.aggregate, settings ? aggregateMemberCandidates(settings, profile.id) : []);
  return aggregate.members.length >= 1 ? null : "聚合供应商至少需要勾选 1 个已填写 Base URL / Key 的 API 供应商。";
}

export function formatTime(value: number) {
  if (!value) return "-";
  return new Date(value).toLocaleString("zh-CN");
}

export function upstreamResponseModelMismatch(entry: LocalProxyLogEntry): boolean {
  const requested = entry.upstreamRequestModel?.trim();
  const observed = entry.upstreamResponseModel?.trim();
  const independent = independentCompactionTarget(entry);
  return Boolean(
    requested
    && observed
    && (!independent || normalizeAuditModel(independent) !== normalizeAuditModel(observed))
    && normalizeAuditModel(requested) !== normalizeAuditModel(observed),
  );
}

export function independentCompactionTarget(
  entry: Pick<LocalProxyLogEntry, "independentCompactionModel" | "cacheMissCompactionModel">,
): string | null {
  return entry.independentCompactionModel?.trim()
    || entry.cacheMissCompactionModel?.trim()
    || null;
}

export function proxyLogDisplayModel(
  entry: Pick<
    LocalProxyLogEntry,
    "transport" | "model" | "upstreamResponseModel" | "independentCompactionModel" | "cacheMissCompactionModel"
  >,
): string | null {
  const model = entry.model?.trim();
  const observed = entry.upstreamResponseModel?.trim();
  const independent = independentCompactionTarget(entry);
  const legacyHttpIndependentLog = entry.transport !== "ws"
    && model
    && observed
    && independent
    && normalizeAuditModel(model) === normalizeAuditModel(independent)
    && normalizeAuditModel(model) !== normalizeAuditModel(observed);
  return legacyHttpIndependentLog ? observed : model || null;
}

export function normalizeAuditModel(model: string): string {
  const normalized = model.toLowerCase();
  const grok = /^(grok-4\.[567])(?:-latest|-build)?$/.exec(normalized);
  return grok ? `${grok[1]}-build` : normalized;
}

export function formatRequestLogListTime(value: number) {
  if (!value) return "-";
  return new Date(value).toLocaleString("zh-CN", {
    month: "numeric",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}
