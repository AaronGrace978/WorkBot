export type Settings = {
  apiKey: string;
  gemmaApiKey: string;
  ollamaHost: string;
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

export type ProviderId = "google-stack" | "gemma" | "ollama";

export type ModelOption = {
  id: string;
  label: string;
};

export const PROVIDERS: { id: ProviderId; label: string }[] = [
  { id: "google-stack", label: "Pure Google" },
  { id: "gemma", label: "Cloud Google" },
  { id: "ollama", label: "Ollama Cloud" },
];

export const STACK_ROLES = [
  { tier: "Local Google", model: "PaliGemma 2 Mix 3B", role: "Visual scan, OCR, click coordinates" },
  { tier: "Local Google", model: "Gemma 3 4B", role: "DOM ingest and 128k loop memory" },
  { tier: "Local Google", model: "Gemma 3 1B", role: "Page-ready routing" },
  { tier: "Cloud Google", model: "Gemini 3.5 Flash", role: "Tool orchestration" },
  { tier: "Cloud Google", model: "Gemini 4 Argon", role: "Stuck-task fallback" },
];

export const MODEL_GROUPS: { provider: ProviderId; models: ModelOption[] }[] = [
  {
    provider: "google-stack",
    models: [{ id: "google-stack", label: "All-Google stack" }],
  },
  {
    provider: "gemma",
    models: [
      { id: "gemini-3.5-flash", label: "Gemini 3.5 Flash" },
      { id: "gemini-4-argon", label: "Gemini 4 Argon" },
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
      { id: "paligemma2:3b", label: "PaliGemma 2 Mix 3B" },
      { id: "gemma3:4b", label: "Gemma 3 4B" },
      { id: "gemma3:1b", label: "Gemma 3 1B" },
      { id: "gemma4:31b", label: "Gemma 4 31B" },
      { id: "gemma4:26b", label: "Gemma 4 26B" },
      { id: "gemma4:12b", label: "Gemma 4 12B" },
      { id: "gemma4:e4b", label: "Gemma 4 E4B" },
      { id: "gemma4:e2b", label: "Gemma 4 E2B" },
      { id: "gemma3:27b", label: "Gemma 3 27B" },
      { id: "gemma3:12b", label: "Gemma 3 12B" },
    ],
  },
];

export function usesGemmaApi(model: string) {
  return model === "google-stack" || model.startsWith("gemma-") || model.startsWith("gemini-");
}

export function providerFor(model: string): ProviderId {
  if (model === "google-stack" || model === "pure-google") return "google-stack";
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
  return modelsFor(provider)[0]?.id ?? "google-stack";
}
