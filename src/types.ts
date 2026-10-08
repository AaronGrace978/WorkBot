export type Settings = {
  apiKey: string;
  gemmaApiKey: string;
  model: string;
  think: boolean;
  debugPort: number;
  maxSteps: number;
};

export type TabInfo = {
  index: number;
  title: string;
  url: string;
};

export type BrowserStatus = {
  connected: boolean;
  port: number;
  chromeRunning: boolean;
  debugOpen: boolean;
  tabs: TabInfo[];
  activeTitle?: string | null;
  activeUrl?: string | null;
};

export type ToolTrace = {
  name: string;
  args: string;
  result?: string;
  image?: string;
};

export type ChatMessage = {
  id: string;
  role: "user" | "assistant";
  content: string;
  thinking?: string;
  images?: string[];
  tools: ToolTrace[];
  pending?: boolean;
  status?: string;
  error?: string;
};

export type AgentEvent =
  | { type: "status"; text: string }
  | { type: "thinking"; text: string }
  | { type: "content"; text: string }
  | { type: "toolStart"; name: string; args: string }
  | { type: "toolEnd"; name: string; result: string; image?: string | null }
  | { type: "done" }
  | { type: "error"; text: string };

export type ProviderId = "gemma" | "ollama";

export type ModelOption = {
  id: string;
  label: string;
};

export const PROVIDERS: { id: ProviderId; label: string }[] = [
  { id: "gemma", label: "Gemma" },
  { id: "ollama", label: "Ollama Cloud" },
];

export const MODEL_GROUPS: { provider: ProviderId; models: ModelOption[] }[] = [
  {
    provider: "gemma",
    models: [
      { id: "gemma-4-31b-it", label: "Gemma 4 31B" },
      { id: "gemma-4-26b-a4b-it", label: "Gemma 4 26B A4B" },
      { id: "gemma-3-27b-it", label: "Gemma 3 27B" },
      { id: "gemma-3-12b-it", label: "Gemma 3 12B" },
      { id: "gemma-3-4b-it", label: "Gemma 3 4B" },
    ],
  },
  {
    provider: "ollama",
    models: [
      { id: "gemma4:31b", label: "Gemma 4 31B" },
      { id: "gemma4:26b", label: "Gemma 4 26B" },
      { id: "gemma4:12b", label: "Gemma 4 12B" },
      { id: "gemma4:e4b", label: "Gemma 4 E4B" },
      { id: "gemma4:e2b", label: "Gemma 4 E2B" },
      { id: "gemma4:31b-cloud", label: "Gemma 4 31B Cloud" },
      { id: "gemma3:27b", label: "Gemma 3 27B" },
      { id: "gemma3:12b", label: "Gemma 3 12B" },
      { id: "gemma3:4b", label: "Gemma 3 4B" },
    ],
  },
];

export function usesGemmaApi(model: string) {
  return model.startsWith("gemma-") || model.startsWith("gemini-");
}

export function providerFor(model: string): ProviderId {
  return usesGemmaApi(model) ? "gemma" : "ollama";
}

export function modelsFor(provider: ProviderId, current = "") {
  const group = MODEL_GROUPS.find((item) => item.provider === provider);
  const models = [...(group?.models ?? [])];
  if (current && !models.some((model) => model.id === current) && providerFor(current) === provider) {
    models.push({ id: current, label: current });
  }
  return models;
}

export function defaultModel(provider: ProviderId) {
  return modelsFor(provider)[0]?.id ?? (provider === "gemma" ? "gemma-4-31b-it" : "gemma4:31b");
}
