import { useEffect, useId, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, UnlistenFn } from "@tauri-apps/api/event";
import "./App.css";
import {
  AgentEvent,
  BrowserStatus,
  ChatMessage,
  defaultModel,
  modelsFor,
  providerFor,
  PROVIDERS,
  Settings,
  STACK_ROLES,
  usesGemmaApi,
  ToolTrace,
} from "./types";

const STORAGE_KEY = "gemma-work-bot-transcript";

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
    case "browser_click":
      return "Clicked";
    case "browser_type":
      return "Typed";
    case "browser_press":
      return "Pressed";
    case "browser_hover":
      return "Hovered";
    case "browser_scroll":
      return "Scrolled";
    case "browser_navigate":
      return "Opened";
    case "browser_back":
      return "Went back";
    case "browser_select":
      return "Selected";
    case "browser_tabs":
      return "Tabs";
    case "browser_switch_tab":
      return "Switched tab";
    case "browser_new_tab":
      return "New tab";
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
  });
  const [status, setStatus] = useState<BrowserStatus | null>(null);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [draft, setDraft] = useState("");
  const [images, setImages] = useState<string[]>([]);
  const [running, setRunning] = useState(false);
  const [connecting, setConnecting] = useState(false);
  const [banner, setBanner] = useState("");
  const [lightbox, setLightbox] = useState<string | null>(null);
  const [booting, setBooting] = useState(true);
  const [thinkingOpen, setThinkingOpen] = useState<Record<string, boolean>>({});
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
        const loaded = await invoke<Settings>("get_settings");
        if (!alive) return;
        setSettings(loaded);
        await refresh();
      } catch (error) {
        if (alive) setBanner(String(error));
      } finally {
        if (alive) setBooting(false);
      }
      const stop = await listen<AgentEvent>("agent-event", (event) => {
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

  async function addFiles(files: FileList | File[]) {
    const list = [...files].filter((file) => file.type.startsWith("image/")).slice(0, 4);
    const encoded = await Promise.all(list.map((file) => readImage(file)));
    setImages((prev) => [...prev, ...encoded].slice(0, 4));
  }

  async function send() {
    const text = draft.trim();
    if ((!text && images.length === 0) || running) return;
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
    setDraft("");
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

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand">
          <GemmaMark className="mark" />
          <div>
            <strong>Gemma Work Bot</strong>
            <em>
              {settings.model === "google-stack"
                ? "Pure Google stack"
                : usesGemmaApi(settings.model)
                  ? "Powered by Gemma"
                  : "Powered by Ollama Cloud"}
            </em>
            <small>Google-inspired · Gemma (not an official Google product)</small>
          </div>
        </div>

        <section className="card browser-card">
          <div className="card-head">
            <span className={connected ? "dot on" : "dot"} />
            <strong>{connected ? "Chrome attached" : "Chrome idle"}</strong>
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
          {status && status.tabs.length > 0 && (
            <ul className="tabs">
              {status.tabs.slice(0, 4).map((tab) => (
                <li key={tab.index}>{tab.title || tab.url || "Tab"}</li>
              ))}
            </ul>
          )}
        </section>

        <div className="section-label">Agent settings</div>
        <label>
          Provider
          <select
            value={providerFor(settings.model)}
            onChange={(event) => {
              const next = event.target.value as typeof PROVIDERS[number]["id"];
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

        {settings.model === "google-stack" ? (
          <ul className="stack-roles">
            {STACK_ROLES.map((item) => (
              <li key={item.model}>
                <strong>{item.model}</strong>
                <span>{item.tier} · {item.role}</span>
              </li>
            ))}
          </ul>
        ) : (
          <label>
            Model
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

        {settings.model === "google-stack" && (
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
          Think before acting
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
          Pure Google uses local Ollama for PaliGemma 2 Mix, Gemma 3 4B, and
          Gemma 3 1B, then Gemini 3.5 Flash (Argon if stuck) for tools. Pull
          those three models or Flash still runs alone.
        </p>
        {banner && <p className="banner">{banner.replace(/^NEEDS_RESTART:/, "")}</p>}
        <button
          type="button"
          className="texty"
          onClick={() => {
            setMessages([]);
            localStorage.removeItem(STORAGE_KEY);
          }}
        >
          Clear chat
        </button>
      </aside>

      <main className="main">
        <header className="top">
          <div>
            <div className="eyebrow"><GemmaMark /> Autonomous browser agent</div>
            <h1><span>Gemma</span> Work Bot</h1>
            <p>{settings.think ? "Deep reasoning, full-page vision, careful action." : "Full-page vision with fast, deliberate action."}</p>
          </div>
          <div className="top-badges" aria-label="Agent capabilities">
            <span className="chip">Full-page vision</span>
            <span className="chip">Review before submit</span>
            <span className="chip accent">777 steps</span>
          </div>
        </header>

        <div className="thread" ref={threadRef}>
          {messages.length === 0 && !booting && (
            <div className="empty">
              <GemmaMark className="hero-mark" />
              <h2>What should we get done?</h2>
              <p>Describe the outcome. Gemma scans the whole page, compares your references, and works through the task.</p>
              <div className="suggestions">
                <button type="button" onClick={() => setDraft("Open the current page, read it fully, and summarize the important details.")}>
                  <span>◎</span> Read and summarize this page
                </button>
                <button type="button" onClick={() => setDraft("Complete the form on this page using my instructions, review every field, then submit it.")}>
                  <span>✓</span> Complete and review a form
                </button>
                <button type="button" onClick={() => setDraft("Compare the attached reference image with the current page and fix every mismatch.")}>
                  <span>◈</span> Compare against an image
                </button>
              </div>
            </div>
          )}
          {messages.map((message) => (
            <article key={message.id} className={message.role}>
              <span className="who">{message.role === "user" ? "You" : "Gemma"}</span>
              {message.images && message.images.length > 0 && (
                <div className="shots">
                  {message.images.map((image, index) => (
                    <button key={`${message.id}-img-${index}`} type="button" className="shot" onClick={() => setLightbox(image)}>
                      <img src={imageSrc(image)} alt="Attached" />
                    </button>
                  ))}
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
              {message.tools.length > 0 && (
                <div className="tools">
                  {message.tools.map((tool, index) => (
                    <ToolCard key={`${message.id}-${index}`} tool={tool} onOpen={setLightbox} />
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
          <div className="composer-title"><GemmaMark /> Ask Gemma to work in Chrome</div>
          {images.length > 0 && (
            <div className="pending-shots">
              {images.map((image, index) => (
                <button
                  key={`${index}-${image.slice(0, 12)}`}
                  type="button"
                  className="shot mini"
                  onClick={() => setImages((prev) => prev.filter((_, item) => item !== index))}
                >
                  <img src={imageSrc(image)} alt="Remove attachment" />
                </button>
              ))}
            </div>
          )}
          <textarea
            value={draft}
            placeholder="Ask Gemma to click, type, or look at an image"
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
            <button type="button" className="ghost" onClick={() => fileRef.current?.click()}>
              Add image
            </button>
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
              <button type="submit" className="primary send" disabled={!draft.trim() && images.length === 0}>
                Run task <span aria-hidden="true">→</span>
              </button>
            )}
          </div>
        </form>
        <p className="affiliation">Google-inspired · Gemma (not an official Google product)</p>
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
