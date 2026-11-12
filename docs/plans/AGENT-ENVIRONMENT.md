# Talkee: dictation, device mesh, agents

Status: **plan**, updated 2026-11-12. Nothing here is built yet.

## Decisions (2026-11-12)

| Question | Decision |
| --- | --- |
| Build or ride on T3 Code? | **Build our own**, in Rust. T3 Code is a source of ideas. Small pieces of its code may be borrowed; anything copied keeps its MIT notice. |
| Order | **Dictation first**, then the device mesh, then agents. Agents get dictation for free because it is already in the apps. |
| Transcription | **Cloud API**, not a local model. Groq by default, OpenRouter as the alternative. Cleanup is a separate, optional step. |
| Name | Everything stays **Talkee**. |
| Devices | This Windows desktop, the Android phone, and a **Linux server** (an old Compaq, reachable over SSH). |

## 1. The idea

Talkee already links your own devices securely (pairing, encryption, discovery) and streams audio
between them. Grow that into three layers on the same link:

1. **Dictation.** Speak on the phone or the PC, get clean text where you need it: at the cursor of
   any app on the PC (like Wispr Flow), on the phone, or later into an agent.
2. **Device mesh.** Any number of your devices (PC, phone, the Linux server) paired to each other,
   not just one phone to one PC.
3. **Agents.** Every coding agent installed on every device (Claude Code, Codex, Antigravity,
   Gemini, OpenCode, …) visible and controllable from the phone and the PC, and every agent given an
   MCP server that lets it see and drive the others.

## 2. Research summary

### T3 Code (pingdotgg/t3code, MIT, read at commit `4fd40e1`)

"The open-source control plane for coding agents." A 32,000-file monorepo: web, Electron desktop,
React Native mobile, Node server, Cloudflare relay. It already has multi-agent sessions (Claude via
the Agent SDK, Codex via `codex app-server`, many more over ACP), multi-machine environments linked
over LAN / Tailscale / SSH / its own tunnel, phone apps, in-composer dictation, and an MCP server
injected into every agent (`delegate_task`, `create_threads`, `t3_thread_send/wait/read/interrupt`,
`schedule_task`).

What it does not do, and Talkee will: dictation outside its own text box, the phone as the PC's
microphone, a small native footprint, a device mesh that is not only about agents.

Its adapters are very large (Claude ≈13.7k lines, Codex ≈8.6k, ACP ≈9.5k). We will not match them
feature for feature.

### Ideas to take from T3 Code

1. **Execution stays on the machine that owns it.** Agents, terminals, files, and API keys belong to
   the host. A client never substitutes its own.
2. **Durable intent, then side effects.** Record a command first, do the work afterwards; retries are
   idempotent through a client-chosen request id.
3. **Honest capabilities.** Each agent adapter reports what it really supports. The UI and MCP tools
   read capabilities, never the agent's name.
4. **Scoped MCP credentials.** A token per agent session, bound to device, session, and agent;
   expires when idle, revoked when the session ends, never logged or persisted.
5. **Privilege ceiling.** Anything an agent starts runs with the same or narrower permissions, never
   broader. Calls forwarded to another device carry that limit.
6. **Normalized events** at the adapter boundary: session, turn, delta, tool call, approval,
   question, error, usage limit.
7. **Voice never sends itself** anywhere the user did not point it, and never falls back to another
   destination.
8. **Identity is separate from the route.** A device keeps its id while its address changes;
   addresses are checked before a credential is sent. (Talkee already works this way.)

### Driving agents from Rust (for later phases)

ACP (Agent Client Protocol, JSON-RPC over stdio) covers most agents with one client: Claude Code
through `claude-agent-acp`, Antigravity, Grok, Gemini CLI, and the ACP Registry. Codex speaks its
own `codex app-server` JSON-RPC. Rust crates exist for both ACP and MCP (`agent-client-protocol`,
`rmcp`); to be checked when Phase 3 starts.

### Wispr Flow

Hold a key, speak, release; polished text appears at the cursor in any app. It removes fillers,
punctuates, formats lists, applies spoken corrections ("5, actually 6" → "6"), and has a personal
dictionary, snippets, and a style per app. Android inserts text through an Accessibility Service.

### Speech-to-text APIs

| | Groq (default) | OpenRouter (alternative) |
| --- | --- | --- |
| Endpoint | `POST https://api.groq.com/openai/v1/audio/transcriptions`, multipart, OpenAI-compatible | `POST https://openrouter.ai/api/v1/audio/transcriptions`, JSON with base64 audio and a `format` |
| Model | `whisper-large-v3-turbo` | `openai/whisper-large-v3-turbo` (served by Groq), and others |
| Price | $0.04 per audio hour | per second, by model |
| Limits | 25 MB (free tier) | by model |
| Formats | FLAC, MP3, M4A, OGG, WAV, WEBM | mp3, wav, flac, ogg, webm, … |
| Extras | `prompt` for spelling hints, `language` | |

Whisper resamples to 16 kHz itself, so we upload 16 kHz mono 16-bit WAV: 32 KB per second, 2 MB per
minute, no encoder needed. Groq transcribes at about 200× real time, so a 15 s clip comes back in well
under a second plus the network.

## 3. Phases

Each phase ships something usable.

### Phase 1: Talkee dictation

Works on the pairing that exists today (one phone, one PC). No mesh needed.

**Flow**

```
trigger (PC hotkey held, or phone button held)
  → record: PC mic, or the phone's mic over the existing encrypted stream
    (after echo cancellation and noise suppression)
  → stop on release (max 5 minutes)
  → 16 kHz mono WAV in memory
  → transcribe: Groq or OpenRouter, with the dictionary as a spelling hint
  → clean up (on by default): a fast text model rewrites the speech as finished
    writing, like Wispr Flow: punctuation, paragraphs, lists, numbers, emails,
    spoken formatting commands, fillers removed, corrections applied. Text only,
    never audio. It never answers what was said.
  → deliver: type at the cursor of the focused PC app, and show the text on the phone
```

**Where things live.** The PC does the API calls and holds the keys (DPAPI through `talkee-store`,
like the pairing key). The phone only sends audio and receives text. This keeps a single key in
one place and needs nothing new on the phone's side to stay secure.

**Desktop work** (`talkee-desktop`, plus a new `talkee-dictate` crate for the pure parts)

* `talkee-dictate`: recording buffer, resample to 16 kHz, WAV writer, provider trait with Groq and
  OpenRouter implementations (`ureq` + rustls, already a dependency), cleanup call, dictionary and
  snippet expansion. No Windows code here, so it builds on Linux too and is testable.
* Hotkey: a low-level keyboard hook, because `RegisterHotKey` reports key-down only and hold-to-talk
  needs the release. Modes: hold, toggle, auto (tap toggles, hold holds).
  **Default: hold Ctrl+Win.** Ctrl+Space was ruled out because it is autocomplete in VS Code and most
  IDEs, and switches the input method on Windows with Chinese, Japanese, or Korean keyboards.
  Shift+Space was ruled out because it fires by accident while typing (Shift still held after a
  capital, then Space). Ctrl+Win alone does nothing in Windows. The hook must stop the Start menu
  opening when Win is released, by sending an unassigned key code before the release (the usual
  trick). The chord is a setting.
* PC microphone capture with `cpal` for dictation from the PC itself.
* Phone audio: tap the processed stream into the recording buffer while dictation is active.
  While dictating, the virtual microphone is muted by default, so people on a call do not hear the
  email you are dictating. A setting turns this off.
* Text insertion: `SendInput` with `KEYEVENTF_UNICODE` for short text; clipboard plus Ctrl+V for
  long text, restoring the previous clipboard afterwards. Known limit: Windows blocks input into
  apps running as administrator when Talkee is not; the window says so instead of failing silently.
* A small overlay or tray state while recording and transcribing (listening, working, done, failed).
* Settings in the window: provider, API key (Save stores it encrypted), transcription model,
  language, cleanup on/off and its model and instructions, dictionary, snippets, hotkey, mute-while-
  dictating. A "test" button that records 5 seconds and shows the transcript.
* History: the last few transcripts in the window, so a failed insert can be copied by hand.

**Protocol additions** (`talkee-proto` control channel, version bump)

| Message | Direction | Meaning |
| --- | --- | --- |
| `DICTATE_START { id, target }` | phone → PC | Start a dictation; the phone unmutes capture for it. `target` is `cursor` for now. |
| `DICTATE_STOP { id }` | phone → PC | Stop recording and transcribe. |
| `DICTATE_CANCEL { id }` | either | Throw the recording away. |
| `DICTATE_STATE { id, phase, text?, error? }` | PC → phone | `recording`, `transcribing`, `cleaning`, `delivered`, `failed`, with the final text. |
| `caps` gains `"dictate"` | both | Older peers simply do not offer it. |

**Live preview (decided 2026-11-12).** The text that gets typed always comes from the cloud
pipeline after release; nothing is streamed to the API. While recording, a free on-device recognizer
shows the words as they are spoken, for looks only, and the final text replaces it:

* Phone: Android's built-in `SpeechRecognizer` (on-device where available). Talkee already holds the
  microphone, so feed it Talkee's own captured audio (an audio-source extra added in Android 13, to be
  checked against the docs) rather than letting it open the mic. No preview on older Android.
* PC microphone: the Windows desktop speech recognizer (SAPI), offline and nearly free. Later; until
  then the overlay says "Listening…".
* Not a bundled engine such as Vosk: a native library plus a ~40 MB model, against the
  lightweight rule.

**Phone work** (Flutter `main.dart`, Kotlin plugin)

* A large hold-to-talk button, and the same from the foreground-service notification.
* Live phase and the resulting text, with copy.
* Later in this phase, optional: an Android Accessibility Service to insert text into phone apps,
  which would need the phone to call the API itself (key in the Android Keystore).

**Done when:** holding the hotkey or the phone button, speaking a sentence with a correction in it,
and releasing puts the corrected sentence at the cursor in Notepad, Slack, and a browser, within
about two seconds of release on Wi-Fi.

### Phase 2: device mesh, including the Linux server

* Device registry: many paired devices, each with a stable id, name, kind (pc, phone, server), and
  capabilities (`mic`, `dictate`, `type-at-cursor`, `agents`, `files`).
* Pair any device with any device; same QR + challenge/response handshake, one key per pair. A server
  has no camera or screen, so it pairs by printing a pairing link in the terminal (over SSH) that the
  phone or PC scans or pastes; `talkee-cli` already draws QR codes in the terminal.
* The control channel becomes typed request/response messages with ids, so features add message
  types rather than sockets.
* `talkeed`: a headless Talkee for Linux, run as a systemd user service. Needs a Linux key store
  (a file readable only by the user, or the Secret Service when present) behind `talkee-store`.
* Routes: LAN and mDNS today; Tailscale addresses accepted as ordinary routes. No relay for now.

### Phase 3: agent host

On each PC or server with the `agents` capability:

* Discover installed agents (`claude`, `codex`, `gemini`, `opencode`, Antigravity's ACP agent, any
  ACP command added by hand), their version, and whether they are signed in, without opening a
  session (opening one can start MCP servers or a login).
* One `AgentAdapter` trait, two implementations to start: ACP and Codex app-server. Normalized
  events as in Section 2.
* Projects, sessions, and turns persisted in SQLite; intent first, then effects.
* Approvals and questions go to every connected device; the phone can answer.
* Dictation into an agent's draft: the transcript lands in the composer, you press send.

### Phase 4: Talkee MCP

A Rust MCP server on loopback, injected into each agent session with a scoped token. Starting tools:
`devices_list`, `agents_list`, `session_start`, `session_send`, `session_read`, `session_wait`,
`session_interrupt`, `files_list`, `files_read`. Calls that name another device are forwarded over
the mesh with the caller's permission limit. Any agent can use them; there is no special
orchestrator.

### Phase 5: the cockpit

Phone and PC screens for devices, agents, sessions, approvals, and notifications when a turn finishes
or needs an answer.

## 4. Progress

* **Done:** `talkee-dictate` crate (Groq and OpenRouter speech-to-text and chat, the cleanup prompt
  and its guard against the model answering, silence trimming, Ogg Opus upload, `.env` keys), and
  `talkee dictate` in the CLI. 22 unit tests.
* **Tested against Groq on 2026-11-12** with six spoken clips (corrections, a spoken list, a question,
  an email address and money, a trivia question, casual speech with swearing). All came out right;
  the trivia question stayed a question.
  * Speech-to-text: the upload was the slow part (this PC uploads at 70–210 KB/s). Ogg Opus at
    24 kbps is 12× smaller than WAV and cut transcription from 0.7–3.2 s to 0.4–1.0 s, with the same
    words. A name Whisper got wrong ("Compaq" → "compact") is fixed by the vocabulary hint.
  * Cleanup model: `qwen/qwen3.8-27b` with thinking off, 0.35–0.8 s. Groq's `llama-3.3-70b-versatile`
    no longer exists; `gpt-oss-20b` and `-120b` dropped clauses.
  * Groq's free tier allows 1,000 output tokens a minute for that model and counts the requested
    cap, so `max_tokens` is sized to the transcript. Heavy dictation can still hit it; Talkee then
    types the raw transcript and says why.
  * The OpenRouter key on file is expired (HTTP 401).
* **Done in v0.2.0:** the desktop app (Ctrl+Win: tap to start and again to stop, or hold; PC mic;
  text typed at the cursor, always one line; the indicator pill; the Dictation card with encrypted
  keys and the vocabulary) and the phone (DICTATE_* messages, Hold to dictate, live preview from
  Android's recognizer on Android 13+, the final text in place of the PC's name).
* **Next:** Phase 2, the device mesh with the Compaq.

## 5. The Linux server (checked 2026-11-12)

`ssh compaq` → 192.168.137.2, on this PC's shared Ethernet (Internet Connection Sharing).

* Ubuntu 24.04.3 LTS, x86-64, kernel 6.8. Intel i5-2400 (4 cores, no AVX2), 15 GB RAM, 404 GB free.
* Has git, Docker, Node 24 (nvm, so only on a login shell's `PATH`). No Rust, no coding agents yet.
* User services keep running without a login (linger is on), so `talkeed` can be a systemd user
  service.
* Already busy: a redroid Android container (`phone2`, with Telegram and Notion) keeps the load around
  5 on 4 cores.
* Build `talkeed` on the Windows PC for `x86_64-unknown-linux-gnu` at the baseline CPU level (never
  `target-cpu=native`: no AVX2 there) and copy the binary over.
* Agents for Phase 3 install with npm; ask before installing anything there.
