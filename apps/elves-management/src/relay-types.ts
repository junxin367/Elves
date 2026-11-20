export type RelayProtocol = "responses" | "chatCompletions" | "anthropic" | "gemini";

export type RelayMode = "official" | "mixedApi" | "pureApi" | "aggregate";

export type RelayModelMapping = {
  requestModel: string;
  alias: string;
  protocol: RelayProtocol;
  contextWindow: string;
  systemPromptOverride?: string;
};
