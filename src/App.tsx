import { useEffect, useId, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, UnlistenFn } from "@tauri-apps/api/event";
import "./App.css";
import {
  AgentEvent,
  BrowserStatus,
  ChatMessage,
  defaultModel,
  isStackModel,
  MemoryItem,
  MemorySnapshot,
  modelsFor,
  providerFor,
  PROVIDERS,
  Settings,
  STACK_ROLES,
  TelemetryState,
  ToolTrace,
  usesGemmaApi,
} from "./types";

const STORAGE_KEY = "gemma-work-bot-transcript";

const DEFAULT_MEMORY_SNAPSHOT: MemorySnapshot = {
  items: [
    {
      id: "starter-1",
      kind: "skill",
      domain: "*",
      title: "Deep Page Audit & Table Extract",
      content:
        "Scan the full page from top to bottom, extract all structured tables, key metrics, and action items using browser_extract, and produce a clean executive brief.",
      tags: ["extract", "table", "audit"],
      uses: 0,
      confidence: 95,
      source: "starter",
      updatedAt: 0,
    },
    {
      id: "starter-2",
      kind: "skill",
      domain: "*",
      title: "Form Autopilot with Pre-Submit Verification",
      content:
        "Locate every form input, dropdown, and checkbox on the current page. Populate all fields accurately from my instructions, run browser_review to verify zero mismatches, and submit.",
      tags: ["form", "review", "submit"],
      uses: 0,
      confidence: 95,
      source: "starter",
      updatedAt: 0,
    },
    {
      id: "starter-3",
      kind: "skill",
      domain: "*",
      title: "Learn & Map This Website",
      content:
        "Perform a full-page scan of the current website, map out its primary navigation, search controls, forms, and quirks, and save a reusable domain playbook with memory_save.",
      tags: ["learn", "playbook", "map"],
      uses: 0,
      confidence: 95,
      source: "starter",
      updatedAt: 0,
    },
  ],
  stats: {
    totalMemories: 3,
    playbooksCount: 0,
    pitfallsCount: 0,
    preferencesCount: 0,
    skillsCount: 3,
    domainsLearned: 0,
    totalApplications: 0,
    tasksCompleted: 0,
    selfHeals: 0,
  },
};

function inTauri() {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

function applyEvent(message: ChatMessage, event: AgentEvent): ChatMessage {
  switch (event.type) {
    case "status":
      return { ...message, status: event.text };
    case "thinking":
      return {
        ...message,
        thinking: ((message.thinking || "") + event.text).slice(-12_000),
      };
    case "content":
      return { ...message, content: (message.content + event.text).slice(-40_000) };
    case "toolStart":
      return {
        ...message,
        tools: [...message.tools, { name: event.name, args: event.args }],
      };
    case "toolEnd": {
      // Keeping every full screenshot made long runs consume a large amount of
      // memory and caused the composer to lag. Retain only the newest screenshot.
      const tools: ToolTrace[] = message.tools.map((tool) => ({
        ...tool,
        image: undefined,
      }));
      let index = -1;
      for (let i = tools.length - 1; i >= 0; i -= 1) {
        if (tools[i].name === event.name && tools[i].result === undefined) {
          index = i;
          break;
        }
      }
      const image = event.image || undefined;
      if (index === -1) {
        tools.push({ name: event.name, args: "", result: event.result, image });
      } else {
        tools[index] = { ...tools[index], result: event.result, image };
      }
      return { ...message, tools };
    }
    case "plan":
      return { ...message, plan: event.steps };
    case "learned": {
      const existing = message.learned || [];
      const filtered = existing.filter((item) => item.id !== event.item.id);
      return { ...message, learned: [...filtered, event.item] };
    }
    case "telemetry":
      return message;
    case "done":
      return { ...message, pending: false, status: undefined };
    case "error":
      return { ...message, pending: false, status: undefined, error: event.text };
  }
}

function historyPayload(messages: ChatMessage[]) {
  return messages
    .filter((message) => message.role === "user" || (message.role === "assistant" && !message.pending))
    .map((message) => ({
      role: message.role,
      content: storedContent(message),
    }))
    .filter((message) => message.content.trim().length > 0);
}

function storedContent(message: ChatMessage) {
  const actions = message.tools
    .filter((tool) => tool.result)
    .map((tool) => `${toolLabel(tool.name)}: ${tool.result}`)
    .join("\n");
  return [message.content.trim(), actions ? `Actions:\n${actions}` : ""]
    .filter(Boolean)
    .join("\n\n");
}

function toolLabel(name: string) {
  switch (name) {
    case "browser_look":
      return "Looked";
    case "browser_scan":
      return "Full-page scan";
    case "browser_click":
      return "Clicked";
    case "browser_type":
      return "Typed";
    case "browser_press":
      return "Pressed";
    case "browser_key_combo":
      return "Shortcut";
    case "browser_hover":
      return "Hovered";
    case "browser_drag":
      return "Dragged";
    case "browser_scroll":
      return "Scrolled";
    case "browser_navigate":
      return "Opened";
    case "browser_back":
      return "Went back";
    case "browser_forward":
      return "Went forward";
    case "browser_reload":
      return "Reloaded";
    case "browser_select":
      return "Selected";
    case "browser_read":
      return "Read page";
    case "browser_extract":
      return "Extracted data";
    case "browser_review":
      return "Reviewed form";
    case "browser_tabs":
      return "Tabs";
    case "browser_switch_tab":
      return "Switched tab";
    case "browser_new_tab":
      return "New tab";
    case "browser_close_tab":
      return "Closed tab";
    case "plan_update":
      return "Updated plan";
    case "memory_save":
      return "Learned memory";
    case "memory_search":
      return "Searched memory";
    default:
      return name;
  }
}

function imageSrc(image: string) {
  return image.startsWith("data:") ? image : `data:image/jpeg;base64,${image}`;
}

function GemmaMark({ className }: { className?: string }) {
  const id = `gemma${useId().replace(/:/g, "")}`;
  return (
    <svg className={className} viewBox="0 0 64 64" aria-hidden="true">
      <defs>
        <linearGradient id={id} x1="8" y1="6" x2="56" y2="58" gradientUnits="userSpaceOnUse">
          <stop stopColor="#4285F4" />
          <stop offset=".32" stopColor="#8E62DB" />
          <stop offset=".62" stopColor="#EA4335" />
          <stop offset="1" stopColor="#FBBC04" />
        </linearGradient>
      </defs>
      <path fill={`url(#${id})`} d="M32 3c2.7 16.5 12.5 26.3 29 29-16.5 2.7-26.3 12.5-29 29C29.3 44.5 19.5 34.7 3 32 19.5 29.3 29.3 19.5 32 3Z" />
    </svg>
  );
}

async function readImage(file: File) {
  if (!file.type.startsWith("image/")) {
    throw new Error("Attach a PNG, JPEG, or WebP image.");
  }
  return new Promise<string>((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result));
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(file);
  });
}

export default function App() {
  const [settings, setSettings] = useState<Settings>({
    apiKey: "",
    gemmaApiKey: "",
    ollamaHost: "http://127.0.0.1:11434",
    model: "google-stack",
    think: false,
    debugPort: 9222,
    maxSteps: 777,
    autoLearn: true,
    theme: "light",
  });
  const [status, setStatus] = useState<BrowserStatus | null>(null);
  const [memories, setMemories] = useState<MemorySnapshot>(DEFAULT_MEMORY_SNAPSHOT);
  const [telemetry, setTelemetry] = useState<TelemetryState | null>(null);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [draft, setDraft] = useState("");
  const [quickUrl, setQuickUrl] = useState("");
  const [images, setImages] = useState<string[]>([]);
  const [running, setRunning] = useState(false);
  const [connecting, setConnecting] = useState(false);
  const [banner, setBanner] = useState("");
  const [lightbox, setLightbox] = useState<string | null>(null);
  const [booting, setBooting] = useState(true);
  const [thinkingOpen, setThinkingOpen] = useState<Record<string, boolean>>({});
  const [activeTab, setActiveTab] = useState<"console" | "vault">("console");
  const [vaultFilter, setVaultFilter] = useState<"all" | "playbook" | "pitfall" | "preference" | "skill">("all");
  const [newMemKind, setNewMemKind] = useState<"playbook" | "pitfall" | "preference" | "skill">("playbook");
  const [newMemDomain, setNewMemDomain] = useState("*");
  const [newMemTitle, setNewMemTitle] = useState("");
  const [newMemContent, setNewMemContent] = useState("");
  const [newMemTags, setNewMemTags] = useState("");

  const threadRef = useRef<HTMLDivElement>(null);
  const runId = useRef<string | null>(null);
  const runningRef = useRef(false);
  const connectingRef = useRef(false);
  const fileRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    runningRef.current = running;
  }, [running]);

  useEffect(() => {
    connectingRef.current = connecting;
  }, [connecting]);

  useEffect(() => {
    const saved = localStorage.getItem(STORAGE_KEY);
    if (saved) {
      try {
        const parsed = JSON.parse(saved) as ChatMessage[];
        setMessages(parsed.map((message) => ({ ...message, pending: false, images: undefined })));
      } catch {
        localStorage.removeItem(STORAGE_KEY);
      }
    }
  }, []);

  const skipSave = useRef(true);
  useEffect(() => {
    if (skipSave.current) {
      skipSave.current = false;
      return;
    }
    const slim = messages
      .filter((message) => !message.pending)
      .map((message) => ({
        ...message,
        images: undefined,
        tools: message.tools.map((tool) => ({ ...tool, image: undefined })),
      }));
    localStorage.setItem(STORAGE_KEY, JSON.stringify(slim));
  }, [messages]);

  useEffect(() => {
    threadRef.current?.scrollTo({ top: threadRef.current.scrollHeight });
  }, [messages, running]);

  async function refreshMemories() {
    if (!inTauri()) return;
    try {
      const snap = await invoke<MemorySnapshot>("get_memories");
      setMemories(snap);
    } catch {
      // ignore
    }
  }

  useEffect(() => {
    if (!inTauri()) {
      setBooting(false);
      setBanner("Open this with npm run tauri dev so it can reach Chrome and Ollama.");
      return;
    }
    let unlisten: UnlistenFn | undefined;
    let timer = 0;
    let alive = true;
    let refreshing = false;
    const refresh = async () => {
      if (refreshing || runningRef.current || connectingRef.current) return;
      refreshing = true;
      try {
        setStatus(await invoke<BrowserStatus>("browser_status"));
      } catch (error) {
        setBanner(String(error));
      } finally {
        refreshing = false;
      }
    };
    (async () => {
      try {
        const [loaded, memSnap] = await Promise.all([
          invoke<Settings>("get_settings"),
          invoke<MemorySnapshot>("get_memories"),
        ]);
        if (!alive) return;
        setSettings(loaded);
        setMemories(memSnap);
        await refresh();
      } catch (error) {
        if (alive) setBanner(String(error));
      } finally {
        if (alive) setBooting(false);
      }
      const stop = await listen<AgentEvent>("agent-event", (event) => {
        if (event.payload.type === "telemetry") {
          setTelemetry({
            step: event.payload.step,
            maxSteps: event.payload.maxSteps,
            activeModel: event.payload.activeModel,
            domain: event.payload.domain,
            memoriesApplied: event.payload.memoriesApplied,
          });
          return;
        }
        if (event.payload.type === "learned") {
          void refreshMemories();
        }
        const id = runId.current;
        if (!id) return;
        setMessages((prev) => {
          const exists = prev.some((message) => message.id === id);
          const next = exists
            ? prev
            : [
                ...prev,
                {
                  id,
                  role: "assistant" as const,
                  content: "",
                  tools: [],
                  pending: true,
                },
              ];
          return next.map((message) => (message.id === id ? applyEvent(message, event.payload) : message));
        });
        if (event.payload.type === "done" || event.payload.type === "error") {
          setRunning(false);
          void refresh();
          void refreshMemories();
        }
      });
      if (!alive) {
        stop();
        return;
      }
      unlisten = stop;
      // Start a dedicated persistent bot browser automatically. Modern Chrome
      // intentionally blocks automation of the normal profile.
      setConnecting(true);
      connectingRef.current = true;
      try {
        const browser = await invoke<BrowserStatus>("launch_chrome", { mode: "auto" });
        if (alive) setStatus(browser);
      } catch (error) {
        if (alive) setBanner(String(error));
      } finally {
        connectingRef.current = false;
        if (alive) setConnecting(false);
      }
      timer = window.setInterval(() => void refresh(), 10_000);
    })();
    return () => {
      alive = false;
      unlisten?.();
      window.clearInterval(timer);
    };
  }, []);

  async function save(next: Settings) {
    setSettings(next);
    if (!inTauri()) return;
    try {
      const saved = await invoke<Settings>("save_settings", {
        apiKey: next.apiKey,
        gemmaApiKey: next.gemmaApiKey,
        ollamaHost: next.ollamaHost,
        model: next.model,
        think: next.think,
        debugPort: Number(next.debugPort) || 9222,
        maxSteps: Number(next.maxSteps) || 777,
        autoLearn: next.autoLearn,
        theme: next.theme,
      });
      setSettings(saved);
    } catch (error) {
      setBanner(String(error));
    }
  }

  async function connect(mode: string) {
    setBanner("");
    setConnecting(true);
    connectingRef.current = true;
    await save(settings);
    try {
      setStatus(await invoke<BrowserStatus>("launch_chrome", { mode }));
    } catch (error) {
      setBanner(String(error));
    } finally {
      connectingRef.current = false;
      setConnecting(false);
    }
  }

  async function quickBrowser(action: string, value = "") {
    if (!inTauri() || running) return;
    setBanner("");
    try {
      setStatus(await invoke<BrowserStatus>("browser_quick_action", { action, value }));
    } catch (error) {
      setBanner(String(error));
    }
  }

  async function handleSaveMemory(event: React.FormEvent) {
    event.preventDefault();
    if (!newMemTitle.trim() || !newMemContent.trim()) return;
    const tags = newMemTags
      .split(",")
      .map((t) => t.trim())
      .filter(Boolean);
    if (!inTauri()) {
      const item: MemoryItem = {
        id: `local-${Date.now()}`,
        kind: newMemKind,
        domain: newMemDomain.trim() || "*",
        title: newMemTitle.trim(),
        content: newMemContent.trim(),
        tags,
        uses: 0,
        confidence: 98,
        source: "user",
        updatedAt: Math.floor(Date.now() / 1000),
      };
      setMemories((prev) => ({
        ...prev,
        items: [item, ...prev.items],
        stats: { ...prev.stats, totalMemories: prev.stats.totalMemories + 1 },
      }));
    } else {
      try {
        const snap = await invoke<MemorySnapshot>("save_memory", {
          kind: newMemKind,
          domain: newMemDomain.trim() || "*",
          title: newMemTitle.trim(),
          content: newMemContent.trim(),
          tags,
        });
        setMemories(snap);
      } catch (error) {
        setBanner(String(error));
        return;
      }
    }
    setNewMemTitle("");
    setNewMemContent("");
    setNewMemTags("");
  }

  async function handleDeleteMemory(id: string) {
    if (!inTauri()) {
      setMemories((prev) => ({
        ...prev,
        items: prev.items.filter((item) => item.id !== id),
      }));
      return;
    }
    try {
      setMemories(await invoke<MemorySnapshot>("delete_memory", { id }));
    } catch (error) {
      setBanner(String(error));
    }
  }

  async function handleClearLearned() {
    if (!inTauri()) return;
    try {
      setMemories(await invoke<MemorySnapshot>("clear_learned_memories"));
    } catch (error) {
      setBanner(String(error));
    }
  }

  async function addFiles(files: FileList | File[]) {
    const list = [...files].filter((file) => file.type.startsWith("image/")).slice(0, 4);
    const encoded = await Promise.all(list.map((file) => readImage(file)));
    setImages((prev) => [...prev, ...encoded].slice(0, 4));
  }

  async function send(customPrompt?: string) {
    const text = (customPrompt ?? draft).trim();
    if ((!text && images.length === 0) || running) return;
    setActiveTab("console");
    const prior = messages.filter((message) => !message.pending);
    if (inTauri()) {
      try {
        const saved = await invoke<Settings>("save_settings", {
          apiKey: settings.apiKey,
          gemmaApiKey: settings.gemmaApiKey,
          ollamaHost: settings.ollamaHost,
          model: settings.model,
          think: settings.think,
          debugPort: Number(settings.debugPort) || 9222,
          maxSteps: Number(settings.maxSteps) || 777,
          autoLearn: settings.autoLearn,
          theme: settings.theme,
        });
        setSettings(saved);
      } catch (error) {
        setBanner(String(error));
        return;
      }
    }
    const userId = crypto.randomUUID();
    const assistantId = crypto.randomUUID();
    const userImages = images;
    runId.current = assistantId;
    if (customPrompt === undefined) {
      setDraft("");
    }
    setImages([]);
    setRunning(true);
    setBanner("");
    setMessages((prev) => [
      ...prev.filter((message) => !message.pending),
      {
        id: userId,
        role: "user",
        content: text,
        images: userImages,
        tools: [],
      },
      {
        id: assistantId,
        role: "assistant",
        content: "",
        thinking: "",
        tools: [],
        pending: true,
        status: "Starting…",
      },
    ]);
    try {
      await invoke("run_agent", {
        message: text,
        images: userImages,
        history: historyPayload(prior),
      });
    } catch (error) {
      setMessages((prev) =>
        prev.map((message) =>
          message.id === assistantId
            ? { ...message, pending: false, status: undefined, error: String(error) }
            : message,
        ),
      );
    } finally {
      setRunning(false);
    }
  }

  async function stop() {
    if (!inTauri()) return;
    await invoke("stop_agent");
  }

  const connected = status?.connected ?? false;
  const isDark = settings.theme === "dark";
  const skills = memories.items.filter((item) => item.kind === "skill");
  const filteredMemories = memories.items.filter(
    (item) => vaultFilter === "all" || item.kind === vaultFilter,
  );

  return (
    <div className="app" data-theme={isDark ? "dark" : "light"}>
      <aside className="sidebar">
        <div className="brand">
          <GemmaMark className="mark" />
          <div>
            <strong>Gemma Work Bot</strong>
            <em>
              {settings.model === "argon-stack"
                ? "Argon Apex Stack"
                : isStackModel(settings.model)
                  ? "Pure Google stack"
                  : usesGemmaApi(settings.model)
                    ? "Powered by Gemma"
                    : "Powered by Ollama Cloud"}
            </em>
            <small>Google-inspired · Gemma (not an official Google product)</small>
          </div>
        </div>

        <div className="nav-pills" role="tablist" aria-label="Workspace mode">
          <button
            type="button"
            className={activeTab === "console" ? "nav-pill active" : "nav-pill"}
            onClick={() => setActiveTab("console")}
          >
            <span>✦</span> Mission Console
          </button>
          <button
            type="button"
            className={activeTab === "vault" ? "nav-pill active" : "nav-pill"}
            onClick={() => setActiveTab("vault")}
          >
            <span>◈</span> Memory Vault
            <strong className="pill-count">{memories.stats.totalMemories}</strong>
          </button>
        </div>

        <section className="card browser-card">
          <div className="card-head">
            <span className={connected ? "dot on" : "dot"} />
            <strong>{connected ? "Chrome attached" : "Chrome idle"}</strong>
            {connected && (
              <button
                type="button"
                className="icon-btn"
                title="Reload current tab"
                disabled={running}
                onClick={() => void quickBrowser("reload")}
              >
                ↻
              </button>
            )}
          </div>
          <p className="muted">
            {status?.activeUrl
              ? status.activeUrl
              : connected
                ? "A tab is ready for Gemma to see and click."
                : "Attach Chrome so Gemma can look, think, and click."}
          </p>
          <div className="stack">
            <button
              type="button"
              className="primary"
              onClick={() => void connect("auto")}
              disabled={running || connecting}
            >
              {connecting ? "Starting bot Chrome…" : connected ? "Reconnect bot Chrome" : "Start bot Chrome"}
            </button>
          </div>
          {connected && (
            <form
              className="quick-url-bar"
              onSubmit={(event) => {
                event.preventDefault();
                if (!quickUrl.trim()) return;
                void quickBrowser("navigate", quickUrl.trim());
                setQuickUrl("");
              }}
            >
              <input
                value={quickUrl}
                placeholder="Open URL in bot Chrome…"
                disabled={running}
                onChange={(event) => setQuickUrl(event.target.value)}
              />
              <button type="submit" className="ghost mini-btn" disabled={running || !quickUrl.trim()}>
                Go
              </button>
            </form>
          )}
          {status && status.tabs.length > 0 && (
            <ul className="tabs">
              {status.tabs.slice(0, 5).map((tab) => (
                <li key={tab.index}>
                  <button
                    type="button"
                    className={
                      status.activeUrl && tab.url === status.activeUrl ? "tab-btn active" : "tab-btn"
                    }
                    disabled={running}
                    onClick={() => void quickBrowser("switch_tab", String(tab.index))}
                    title={tab.url}
                  >
                    <span className="tab-idx">{tab.index + 1}</span>
                    <span className="tab-title">{tab.title || tab.url || "Tab"}</span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </section>

        <div className="section-label">Agent harness</div>
        <label>
          Provider
          <select
            value={providerFor(settings.model)}
            onChange={(event) => {
              const next = event.target.value as (typeof PROVIDERS)[number]["id"];
              if (next === providerFor(settings.model)) return;
              void save({ ...settings, model: defaultModel(next) });
            }}
          >
            {PROVIDERS.map((provider) => (
              <option key={provider.id} value={provider.id}>
                {provider.label}
              </option>
            ))}
          </select>
        </label>

        <label>
          {isStackModel(settings.model) ? "Orchestration mode" : "Model"}
          <select
            value={settings.model}
            onChange={(event) => void save({ ...settings, model: event.target.value })}
          >
            {modelsFor(providerFor(settings.model), settings.model).map((model) => (
              <option key={model.id} value={model.id}>
                {model.label}
              </option>
            ))}
          </select>
        </label>

        {isStackModel(settings.model) && (
          <ul className="stack-roles">
            {STACK_ROLES.map((item) => {
              const isActive =
                running &&
                (telemetry?.activeModel === item.id ||
                  (item.id === "gemini-4-argon" && settings.model === "argon-stack"));
              return (
                <li key={item.model} className={isActive ? "active-role" : ""}>
                  <strong>
                    {item.model}
                    {isActive && <span className="live-tag">ACTIVE</span>}
                  </strong>
                  <span>
                    {item.tier} · {item.role}
                  </span>
                </li>
              );
            })}
          </ul>
        )}

        {usesGemmaApi(settings.model) && (
          <label>
            Google AI Studio key
            <input
              type="password"
              value={settings.gemmaApiKey}
              placeholder="From aistudio.google.com/apikey"
              onChange={(event) => setSettings({ ...settings, gemmaApiKey: event.target.value })}
              onBlur={() => void save(settings)}
            />
          </label>
        )}

        {isStackModel(settings.model) && (
          <label>
            Local Ollama
            <input
              value={settings.ollamaHost}
              placeholder="http://127.0.0.1:11434"
              onChange={(event) => setSettings({ ...settings, ollamaHost: event.target.value })}
              onBlur={() => void save(settings)}
            />
          </label>
        )}

        {providerFor(settings.model) === "ollama" && (
          <label>
            Ollama API key
            <input
              type="password"
              value={settings.apiKey}
              placeholder="From ollama.com/settings/keys"
              onChange={(event) => setSettings({ ...settings, apiKey: event.target.value })}
              onBlur={() => void save(settings)}
            />
          </label>
        )}

        <label className="check">
          <input
            type="checkbox"
            checked={settings.think}
            onChange={(event) => void save({ ...settings, think: event.target.checked })}
          />
          Deep reasoning before acting
        </label>

        <label className="check">
          <input
            type="checkbox"
            checked={settings.autoLearn}
            onChange={(event) => void save({ ...settings, autoLearn: event.target.checked })}
          />
          Self-learn site playbooks & skills
        </label>

        <div className="split">
          <label>
            Debug port
            <input
              type="number"
              min={1}
              max={65535}
              value={settings.debugPort}
              onChange={(event) => setSettings({ ...settings, debugPort: Number(event.target.value) })}
              onBlur={() => void save(settings)}
            />
          </label>
          <label>
            Max steps
            <input
              type="number"
              min={1}
              max={777}
              value={settings.maxSteps}
              onChange={(event) => setSettings({ ...settings, maxSteps: Number(event.target.value) })}
              onBlur={() => void save(settings)}
            />
          </label>
        </div>

        <p className="fine">
          Work Bot uses its own persistent Chrome profile because current Chrome
          blocks automation of your normal profile. Sign in once in the bot window.
          Pure Google combines local PaliGemma 2 Mix, Gemma 3 4B, and Gemma 3 1B
          with Gemini 3.5 Flash &amp; Gemini 4 Argon, plus continuous domain memory.
        </p>
        {banner && <p className="banner">{banner.replace(/^NEEDS_RESTART:/, "")}</p>}
        <button
          type="button"
          className="texty"
          onClick={() => {
            setMessages([]);
            setTelemetry(null);
            localStorage.removeItem(STORAGE_KEY);
          }}
        >
          Clear chat
        </button>
      </aside>

      <main className="main">
        <header className="top">
          <div>
            <div className="eyebrow">
              <GemmaMark /> ARGON-Standard Autonomous Browser Agent
            </div>
            <h1>
              <span>Gemma</span> Work Bot
            </h1>
            <p>
              {settings.think
                ? "Deep Argon reasoning, full-page vision, and continuous domain learning."
                : "Full-page vision, pre-submit verification, and self-evolving site playbooks."}
            </p>
          </div>
          <div className="top-badges" aria-label="Agent capabilities">
            <span className="chip">Full-page + Shadow DOM</span>
            <span className="chip">Self-learning memory</span>
            <span className="chip accent">{settings.maxSteps} steps</span>
            <button
              type="button"
              className="theme-btn"
              onClick={() =>
                void save({ ...settings, theme: isDark ? "light" : "dark" })
              }
              title="Switch between Google Daylight and Gemini Argon Dark theme"
            >
              {isDark ? "☀ Daylight" : "✦ Argon Dark"}
            </button>
          </div>
        </header>

        {telemetry && (running || telemetry.step > 0) && activeTab === "console" && (
          <div className="telemetry-bar" role="status" aria-live="polite">
            <div className="telemetry-meta">
              <span className="telemetry-badge">
                Step {telemetry.step} / {telemetry.maxSteps}
              </span>
              <span className="telemetry-model">
                Orchestrator: <strong>{telemetry.activeModel}</strong>
              </span>
              {telemetry.domain && telemetry.domain !== "*" && (
                <span className="telemetry-domain">Domain: {telemetry.domain}</span>
              )}
              {telemetry.memoriesApplied.length > 0 && (
                <span
                  className="telemetry-memory"
                  title={telemetry.memoriesApplied.join(", ")}
                >
                  ◈ {telemetry.memoriesApplied.length} memory playbook
                  {telemetry.memoriesApplied.length === 1 ? "" : "s"} active
                </span>
              )}
            </div>
            <div className="telemetry-track">
              <div
                className="telemetry-fill"
                style={{
                  width: `${Math.min(100, Math.max(2, (telemetry.step / telemetry.maxSteps) * 100))}%`,
                }}
              />
            </div>
          </div>
        )}

        {activeTab === "vault" ? (
          <div className="vault-view">
            <section className="vault-stats">
              <div className="stat-card">
                <span>Total Memories</span>
                <strong>{memories.stats.totalMemories}</strong>
                <small>{memories.stats.playbooksCount} site playbooks · {memories.stats.pitfallsCount} pitfalls</small>
              </div>
              <div className="stat-card">
                <span>Domains Learned</span>
                <strong>{memories.stats.domainsLearned}</strong>
                <small>Auto-recalled when visiting matching sites</small>
              </div>
              <div className="stat-card">
                <span>Memory Applications</span>
                <strong>{memories.stats.totalApplications}</strong>
                <small>Injected into [memory-vault] briefings</small>
              </div>
              <div className="stat-card">
                <span>Tasks &amp; Self-Heals</span>
                <strong>
                  {memories.stats.tasksCompleted} / {memories.stats.selfHeals}
                </strong>
                <small>Completed runs &amp; Argon obstacle recoveries</small>
              </div>
            </section>

            <div className="vault-grid">
              <section className="card teach-card">
                <h3>Teach WorkBot a Playbook, Preference, or Skill</h3>
                <p className="muted">
                  Anything saved here is persistently stored in WorkBot&apos;s Memory Vault and automatically injected when relevant.
                </p>
                <form className="teach-form" onSubmit={(e) => void handleSaveMemory(e)}>
                  <div className="split">
                    <label>
                      Category
                      <select
                        value={newMemKind}
                        onChange={(e) =>
                          setNewMemKind(e.target.value as typeof newMemKind)
                        }
                      >
                        <option value="playbook">Site Playbook</option>
                        <option value="pitfall">Pitfall / Workaround</option>
                        <option value="preference">User Preference</option>
                        <option value="skill">1-Click Reusable Skill</option>
                      </select>
                    </label>
                    <label>
                      Target Domain
                      <input
                        value={newMemDomain}
                        placeholder="e.g. github.com or * for global"
                        onChange={(e) => setNewMemDomain(e.target.value)}
                      />
                    </label>
                  </div>
                  <label>
                    Title
                    <input
                      value={newMemTitle}
                      placeholder="e.g. GitHub PR Creation Workflow or Default Sign-off"
                      onChange={(e) => setNewMemTitle(e.target.value)}
                    />
                  </label>
                  <label>
                    Instructions / Learned Knowledge
                    <textarea
                      rows={3}
                      value={newMemContent}
                      placeholder="Describe the steps, selector hints, user facts, or reusable skill prompt…"
                      onChange={(e) => setNewMemContent(e.target.value)}
                    />
                  </label>
                  <label>
                    Tags (comma separated)
                    <input
                      value={newMemTags}
                      placeholder="e.g. github, form, table, qa"
                      onChange={(e) => setNewMemTags(e.target.value)}
                    />
                  </label>
                  <div className="teach-actions">
                    <button
                      type="submit"
                      className="primary"
                      disabled={!newMemTitle.trim() || !newMemContent.trim()}
                    >
                      Save to Memory Vault
                    </button>
                    <button
                      type="button"
                      className="ghost"
                      onClick={() => void handleClearLearned()}
                    >
                      Reset Auto-Learned
                    </button>
                  </div>
                </form>
              </section>

              <section className="vault-list-section">
                <div className="vault-filters">
                  {(
                    [
                      ["all", "All"],
                      ["playbook", "Playbooks"],
                      ["pitfall", "Pitfalls"],
                      ["preference", "Preferences"],
                      ["skill", "1-Click Skills"],
                    ] as const
                  ).map(([id, label]) => (
                    <button
                      key={id}
                      type="button"
                      className={vaultFilter === id ? "filter-chip active" : "filter-chip"}
                      onClick={() => setVaultFilter(id)}
                    >
                      {label}
                    </button>
                  ))}
                </div>

                <div className="memory-cards">
                  {filteredMemories.length === 0 ? (
                    <p className="muted empty-vault">
                      No memories in this category yet. Run tasks with Self-learn enabled or add one on the left!
                    </p>
                  ) : (
                    filteredMemories.map((item) => (
                      <article key={item.id} className="memory-card">
                        <div className="memory-card-head">
                          <span className={`kind-badge kind-${item.kind}`}>
                            {item.kind.toUpperCase()}
                          </span>
                          <span className="domain-badge">
                            {item.domain === "*" ? "Global (*)" : item.domain}
                          </span>
                          <span className="meta-badge">
                            {item.confidence}% conf · Used {item.uses}×
                          </span>
                          <span className="source-badge">{item.source}</span>
                        </div>
                        <h4>{item.title}</h4>
                        <p>{item.content}</p>
                        <div className="memory-card-foot">
                          <div className="tag-list">
                            {item.tags.map((tag) => (
                              <span key={tag} className="tag-chip">
                                #{tag}
                              </span>
                            ))}
                          </div>
                          <div className="memory-card-btns">
                            {(item.kind === "skill" || item.kind === "playbook") && (
                              <button
                                type="button"
                                className="ghost mini-btn"
                                onClick={() => {
                                  setDraft(item.content);
                                  setActiveTab("console");
                                }}
                              >
                                Load in Console →
                              </button>
                            )}
                            <button
                              type="button"
                              className="texty danger-text"
                              onClick={() => void handleDeleteMemory(item.id)}
                            >
                              Delete
                            </button>
                          </div>
                        </div>
                      </article>
                    ))
                  )}
                </div>
              </section>
            </div>
          </div>
        ) : (
          <>
            <div className="thread" ref={threadRef}>
              {messages.length === 0 && !booting && (
                <div className="empty">
                  <GemmaMark className="hero-mark" />
                  <h2>What should we get done?</h2>
                  <p>
                    Describe the outcome. Gemma scans the whole page, applies learned domain playbooks, compares your references, and works through the task.
                  </p>
                  <div className="suggestions">
                    <button
                      type="button"
                      onClick={() =>
                        setDraft(
                          "Open the current page, read it fully, extract any key tables or lists with browser_extract, and summarize the important details.",
                        )
                      }
                    >
                      <span>◎</span> Deep read &amp; extract structured data
                    </button>
                    <button
                      type="button"
                      onClick={() =>
                        setDraft(
                          "Complete the form on this page using my instructions, review every field with browser_review, then submit it.",
                        )
                      }
                    >
                      <span>✓</span> Complete and verify a form
                    </button>
                    <button
                      type="button"
                      onClick={() =>
                        setDraft(
                          "Perform a full-page scan of this website, learn its navigation and key workflows, and save a domain playbook with memory_save.",
                        )
                      }
                    >
                      <span>◈</span> Learn &amp; map this website
                    </button>
                  </div>

                  {skills.length > 0 && (
                    <div className="starter-skills">
                      <div className="section-label">Saved 1-Click Skills</div>
                      <div className="skill-chips">
                        {skills.slice(0, 5).map((skill) => (
                          <button
                            key={skill.id}
                            type="button"
                            className="skill-pill"
                            onClick={() => setDraft(skill.content)}
                            title={skill.content}
                          >
                            ✦ {skill.title}
                          </button>
                        ))}
                      </div>
                    </div>
                  )}
                </div>
              )}
              {messages.map((message) => (
                <article key={message.id} className={message.role}>
                  <span className="who">
                    {message.role === "user" ? "You" : "Gemma"}
                  </span>
                  {message.images && message.images.length > 0 && (
                    <div className="shots">
                      {message.images.map((image, index) => (
                        <button
                          key={`${message.id}-img-${index}`}
                          type="button"
                          className="shot"
                          onClick={() => setLightbox(image)}
                        >
                          <img src={imageSrc(image)} alt="Attached" />
                        </button>
                      ))}
                    </div>
                  )}
                  {message.plan && message.plan.length > 0 && (
                    <div className="plan-card">
                      <div className="plan-head">
                        <strong>Mission Checkpoints</strong>
                        <span>
                          {message.plan.filter((s) => s.status === "done").length} /{" "}
                          {message.plan.length} complete
                        </span>
                      </div>
                      <ul className="plan-list">
                        {message.plan.map((step, idx) => (
                          <li key={`${step.title}-${idx}`} className={`plan-${step.status}`}>
                            <span className="plan-icon">
                              {step.status === "done"
                                ? "✓"
                                : step.status === "in_progress"
                                  ? "◎"
                                  : "○"}
                            </span>
                            <span>{step.title}</span>
                          </li>
                        ))}
                      </ul>
                    </div>
                  )}
                  {message.thinking && (
                    <details
                      className="think"
                      open={thinkingOpen[message.id] ?? true}
                      onToggle={(event) => {
                        const open = event.currentTarget.open;
                        setThinkingOpen((prev) => ({ ...prev, [message.id]: open }));
                      }}
                    >
                      <summary>Thinking</summary>
                      <p>{message.thinking}</p>
                    </details>
                  )}
                  {message.content && <p className="body">{message.content}</p>}
                  {message.learned && message.learned.length > 0 && (
                    <div className="learned-banner">
                      {message.learned.map((item) => (
                        <div key={item.id} className="learned-chip">
                          <span>◈ Learned {item.kind} ({item.domain}):</span>
                          <strong>{item.title}</strong>
                        </div>
                      ))}
                    </div>
                  )}
                  {message.tools.length > 0 && (
                    <div className="tools">
                      {message.tools.map((tool, index) => (
                        <ToolCard
                          key={`${message.id}-${index}`}
                          tool={tool}
                          onOpen={setLightbox}
                        />
                      ))}
                    </div>
                  )}
                  {message.status && <p className="status">{message.status}</p>}
                  {message.error && <p className="error">{message.error}</p>}
                </article>
              ))}
            </div>

            <form
              className="composer"
              onSubmit={(event) => {
                event.preventDefault();
                void send();
              }}
              onDragOver={(event) => event.preventDefault()}
              onDrop={(event) => {
                event.preventDefault();
                void addFiles(event.dataTransfer.files);
              }}
            >
              <div className="composer-title">
                <GemmaMark /> Ask Gemma to work, extract, or learn in Chrome
              </div>
              {images.length > 0 && (
                <div className="pending-shots">
                  {images.map((image, index) => (
                    <button
                      key={`${index}-${image.slice(0, 12)}`}
                      type="button"
                      className="shot mini"
                      onClick={() =>
                        setImages((prev) => prev.filter((_, item) => item !== index))
                      }
                    >
                      <img src={imageSrc(image)} alt="Remove attachment" />
                    </button>
                  ))}
                </div>
              )}
              <textarea
                value={draft}
                placeholder="Ask Gemma to click, type, extract tables, compare an image, or learn a site workflow"
                rows={3}
                onChange={(event) => setDraft(event.target.value)}
                onPaste={(event) => {
                  const files = [...event.clipboardData.files];
                  if (files.length > 0) {
                    event.preventDefault();
                    void addFiles(files);
                  }
                }}
                onKeyDown={(event) => {
                  if (event.key === "Enter" && !event.shiftKey) {
                    event.preventDefault();
                    void send();
                  }
                }}
              />
              <div className="composer-bar">
                <div className="composer-left">
                  <button
                    type="button"
                    className="ghost"
                    onClick={() => fileRef.current?.click()}
                  >
                    Add image
                  </button>
                  <button
                    type="button"
                    className="ghost"
                    onClick={() => setActiveTab("vault")}
                  >
                    ◈ Memory ({memories.stats.totalMemories})
                  </button>
                </div>
                <input
                  ref={fileRef}
                  type="file"
                  accept="image/*"
                  multiple
                  hidden
                  onChange={(event) => {
                    if (event.target.files) void addFiles(event.target.files);
                    event.target.value = "";
                  }}
                />
                {running ? (
                  <button type="button" className="stop" onClick={() => void stop()}>
                    Stop
                  </button>
                ) : (
                  <button
                    type="submit"
                    className="primary send"
                    disabled={!draft.trim() && images.length === 0}
                  >
                    Run task <span aria-hidden="true">→</span>
                  </button>
                )}
              </div>
            </form>
            <p className="affiliation">
              Google-inspired · Gemma (not an official Google product)
            </p>
          </>
        )}
      </main>

      {lightbox && (
        <button type="button" className="lightbox" onClick={() => setLightbox(null)}>
          <img src={imageSrc(lightbox)} alt="Full size screenshot" />
        </button>
      )}
    </div>
  );
}

function ToolCard({ tool, onOpen }: { tool: ToolTrace; onOpen: (image: string) => void }) {
  return (
    <div className="tool">
      <div className="tool-line">
        <strong>{toolLabel(tool.name)}</strong>
        <span>{tool.result || "Working…"}</span>
      </div>
      {tool.image && (
        <button type="button" className="shot" onClick={() => onOpen(tool.image || "")}>
          <img src={imageSrc(tool.image)} alt="What Gemma saw" />
        </button>
      )}
    </div>
  );
}
