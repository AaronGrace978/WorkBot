# WorkBot

<p align="center">
  <img src="docs/workbot-icon.png" alt="WorkBot" width="160" />
</p>

<p align="center">
  <img alt="Built with Gemma" src="https://img.shields.io/badge/Built%20with-Gemma-4285F4?style=for-the-badge" />
  <img alt="TypeScript" src="https://img.shields.io/badge/TypeScript-3178C6?style=for-the-badge&logo=typescript&logoColor=white" />
  <img alt="React" src="https://img.shields.io/badge/React-61DAFB?style=for-the-badge&logo=react&logoColor=000" />
  <img alt="CSS" src="https://img.shields.io/badge/CSS-1572B6?style=for-the-badge&logo=css&logoColor=white" />
  <img alt="Rust" src="https://img.shields.io/badge/Rust-DEA584?style=for-the-badge&logo=rust&logoColor=000" />
  <img alt="Tauri" src="https://img.shields.io/badge/Tauri-24C8DB?style=for-the-badge&logo=tauri&logoColor=white" />
</p>

WorkBot is an independent desktop agent that gets browser work done. It is built with Tauri 2, React, TypeScript, CSS, and Rust. The default stack can call Gemma and Gemini models. WorkBot is not a Google product.

Gemma is a trademark of Google LLC.

## Features

- Full-page visual scan before acting
- Chrome clicking, typing, scrolling, forms, tabs, and navigation
- Reference-image comparison with high-detail screenshots
- Mandatory field review before Submit, Send, Save, Confirm, or payment
- Autonomous task loop with a 777-step ceiling
- Persistent isolated Chrome profile that remembers website sessions
- Windows, Linux x64/ARM64, macOS Apple Silicon/Intel, and Chromebook-via-Linux builds

## Install

Download the installer for your operating system from [Releases](https://github.com/AaronGrace978/WorkBot/releases).

Chromebooks cannot run a native ChromeOS build. Enable Linux in ChromeOS settings, then install the `.deb` from the Linux ARM64 release (most Chromebooks) or the Linux x64 release (Intel Chromebooks). You still need Chrome inside that Linux environment, or a Chromium package the bot can launch.

1. Create a Google AI Studio key at [aistudio.google.com/apikey](https://aistudio.google.com/apikey) for Gemini 3.5 Flash / Gemini 4 Argon / hosted Gemma.
2. For the local Google specialists, install [Ollama](https://ollama.com/) and pull `paligemma2:3b`, `gemma3:4b`, and `gemma3:1b`.
3. Open Gemma Work Bot, keep **Pure Google**, and paste the Studio key. Optionally switch to a single Cloud Google or Ollama Cloud model.
4. Sign into websites once in the separate Work Bot Chrome window.
5. Describe the outcome you want. Attach reference images when useful.

Current Chrome versions block remote debugging of the normal browser profile. Work Bot therefore uses its own persistent profile and never restarts or closes normal Chrome.

## Development

Prerequisites: Node.js 20+, stable Rust, the [Tauri 2 system dependencies](https://v2.tauri.app/start/prerequisites/), and Google Chrome or Chromium.

```bash
npm install
npm run tauri dev
```

On Windows, `start.bat` builds the release app once and launches that executable on later runs.

## Pure Google stack

| Model | Size / Type | Deployment | WorkBot role |
| --- | --- | --- | --- |
| PaliGemma 2 Mix | 3B multimodal | Local Ollama | Visual scan, OCR, coordinate mapping |
| Gemma 3 4B | 4B dense, 128k | Local Ollama | HTML DOM ingest and loop memory |
| Gemma 3 1B | 1B ultra-light | Local Ollama | Page-ready routing |
| Gemini 3.5 Flash | Frontier API | Google AI Studio | Tool orchestration |
| Gemini 4 Argon | Frontier API | Google AI Studio | Stuck-task fallback |

Local specialists are optional. If Ollama is offline, Flash still drives the browser. Screenshots, page text, prompts, and attached images go to the selected models. Website sessions stay in Work Bot's local Chrome profile.

## Model and privacy

Use a Google AI Studio key for Pure Google and Cloud Google models, or an [Ollama Cloud](https://ollama.com/settings/keys) key for hosted Ollama Gemma models.

Inspired by GrokBot. Built with Gemma. Not an official Google product. Gemma is a trademark of Google LLC. This independent project is not affiliated with or endorsed by Google or xAI.
