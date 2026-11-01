---
name: pc-eye
description: Full White-Box OS Observability & Deterministic Control Engine (V3.0) for Linux Wayland/Hyprland. Use whenever inspecting desktop windows, UI accessibility trees (AT-SPI2 + Zero-Disk In-Memory OCR eng+tur fallback), Terminal PTY context & visible screen lines, screenshots with Set-of-Marks/dHash, process descendant trees/sockets/VRAM (X-Ray), PipeWire physical PCM audio signals/WAV recording & MPRIS media/browser URLs, merged journald/coredump/notification logs, rich clipboard/cliphist, or controlling mouse/keyboard/windows/apps deterministically.
---

# PC-Eye V3.0: Full White-Box OS Observability & Deterministic Director Engine

`pc-eye` gives AI agents complete "White-Box" visibility and deterministic control over the Linux Wayland (`Hyprland`) operating system without relying on slow, blind screenshot-guessing loops.

Binary path: `/home/chaple/.local/bin/pc-eye` (also registered as MCP server `pc-eye` with `pc_eye_*` tools).

---

## Core White-Box Workflow for AI Agents

Always prefer **semantic & telemetry inspection first**, and use **Vision (`pc-eye vision`) only when visual verification or pixel-art inspection is needed**:

0. **Start with `snapshot` (cheapest orientation, ~4 KB)**:
   - Call `pc_eye_snapshot` (or `pc-eye snapshot --window <addr|title|class>`) first: monitors, windows, and the target window's interactive elements (`mark`, `role`, `name`, `value`, decision states, click point `at`; terminal visible lines) in one call. Pass the returned `seq` back as `seq_since` to get only new window events.
   - Marks are stable: an element keeps its number while it exists (shared across daemon, MCP and CLI), so a mark from an earlier snapshot is still valid.
   - Via MCP, a repeated snapshot of the same window automatically returns only what changed (`delta: true`, `unchanged: true` when nothing did; ~100 bytes instead of ~4 KB). Pass `full: true` for the whole list, or `since: <snap>` for a specific base. Unknown ids fall back to a full snapshot (`since_unknown: true`).

0b. **Act in one call with a program (`run`)**:
   - For forms and multi-step flows use `pc_eye_run` (CLI: `pc-eye run '<json>'`) instead of repeated `click_ui`: `{"window": "...", "steps": [{"set": "Ad Soyad", "text": "Öykü"}, {"click": "Bültene abone ol"}, {"select": "Şehir", "option": "İzmir"}, {"click": "Gönder", "expect": "Kaydedildi"}]}`.
   - Steps: `click`, `set`+`text`, `select`+`option`, `type`, `key`, `wait`; targets are names or `{"mark": N}`; any step may add `expect` (text that must appear) or `{"gone": ".."}`. Execution stops at the first failure and returns per-step results plus the final compact state in `after` — continue from there with a new program.
   - **Recipes**: every successful program (plus a `click_ui` that immediately follows it on the same window, e.g. a separate submit) is remembered as a recipe — targets by name, typed values as parameters (values are never stored). When a full snapshot shows `recipes` for the window, replay with `pc_eye_run {"recipe": ID, "args": {"Ad Soyad": "..", "Şehir": ".."}, "window": ".."}` if `does` matches the task. Without any model: `pc-eye recipe list [--window W]`, `pc-eye recipe run <id|name part> --window W --arg "Ad Soyad=Ali"`, `recipe show|rename|rm`. Store: `~/.local/share/pc-eye/recipes.json` (`PC_EYE_RECIPES` overrides).
   - **Agent session (isolated desktop)**: `pc-eye session start [name]` opens a nested Hyprland on an invisible headless output of the user's session (~0.5 s, no focus steal). With `PC_EYE_SESSION=<name>` every pc-eye process (CLI, MCP, daemon) targets that compositor: virtual mouse/keyboard, screenshots, windows and launched apps stay inside it, so the user's cursor, focus and clipboard never change and several agents can work in parallel (one session each). The variable alone is enough — the first call opens the session if needed (e.g. `claude mcp add pc-eye-agent -e PC_EYE_SESSION=agent -- pc-eye mcp`). `pc-eye session list|stop [name]`. Use the user's own session (no variable) only when the task is about the user's open windows. X11-only apps cannot open inside a session (`DISPLAY` is cleared so they never land on the user's screen).
   - Cheap routing: the `pc-eye` subagent (`agents/pc-eye.md`, model haiku) runs desktop work for a larger main model; Haiku passes the full eval set at ~1/13 of Sonnet's cost.
   - Reads are served from an event-driven cache in the daemon/MCP process (~6–8 ms when warm); any pc-eye action invalidates it, so a look after an action is always fresh.
   - Every `window` parameter accepts an address (`0x…`), a full/partial title, or a class; ambiguous matches return the candidates instead of guessing.
   - Use `context` only when you need system health, audio, or the full event/log history (it is ~25–30 KB).

1. **Full context (`context`)**:
   - Call `pc_eye_context` (or `pc-eye context`) for the entire desktop + system state:
     - All monitors, workspaces, windows (`address`, `class`, `title`, `pid`, `[x,y]`, `[w,h]`), layer-shell overlays (`waybar`, `rofi`, `dunst`), `auth_guard_tripped`, `protected_window` flags, and `daemon_running` (auto-starts continuous `.socket2.sock` flight recorder).
     - System health (`load_avg`, RAM, Linux PSI pressure stalls, NVIDIA & AMD GPU utilization/VRAM/temp).
     - Audio & media summary (default sink volume, active streams, real PCM RMS/Peak dBFS, MPRIS active media title/artist/URL).
     - Monotonic `seq` number, delta compositor events (`seq_since`), and recent errors/crashes.

2. **Inspect Semantic UI, In-Memory OCR & Terminal Screen (`ui`)**:
   - Call `pc_eye_ui` (or `pc-eye ui --window <addr> --query <filter>`) to read:
     - **AT-SPI2 Semantic Tree**: Every button, input, menu item, and label with its **global screen coordinates** `(x, y, w, h)`, `click_point: [cx, cy]`, **state flags** (`checked`, `focused`, `editable`, `enabled`, `selected`), and **current text/numeric value**.
     - **Automatic Zero-Disk In-Memory OCR (`ocr-semantic`)**: When an application (Electron, VS Code, Antigravity, games, PDFs, custom Canvas) does not expose an `AT-SPI2` tree, `pc-eye ui` automatically pipes `grim -t ppm -` into `tesseract` (`eng+tur` TSV) in memory, groups words into clickable phrases (`"New Conversation"`, `"File"`, `"Kaydet"`), resolves HiDPI & window offsets, and returns them as `UiElement`s (`role: "ocr-text"`)!
     - **Terminal PTY & Screen Reader**: If the target window is a terminal (`kitty`, `alacritty`, `wezterm`, `foot`), returns `foreground_pid`, `foreground_cmd`, `cwd`, `pty_path`, AND `visible_lines: Vec<String>` (the actual text lines currently visible on the terminal screen via Kitty socket or in-memory line OCR).

3. **Inspect Process X-Ray (`xray`)**:
   - Call `pc_eye_xray` (or `pc-eye xray --window <addr>` / `--pid <pid>`) to inspect the entire **Application Process Cluster** (all descendant threads/helper processes), CPU/RSS/Disk I/O, `p50/p95/p99` spike statistics, open file descriptors (`/proc/<pid>/fd`), and live `TCP/UDP` network connections.

   - **Find windowless processes (`processes`)**: `pc_eye_processes` / `pc-eye ps --match <text>` or `--port <n>` returns PID, command line, RAM and listening endpoints — use it to get the PID for `xray`. `pc_eye_launch` refuses inline shell/interpreter code (`sh -c`, `python -c`); it is for starting apps.

4. **Inspect Audio, Physical Waveform, WAV Recording & Browser URL (`audio`)**:
   - Call `pc_eye_audio` (or `pc-eye audio`) to read PipeWire streams, **real physical PCM 16-bit audio signal metrics** (`rms_dbfs`, `peak_dbfs`, `audible_signal`), and **D-Bus MPRIS media metadata** (including active browser tab URL `xesam:url` and playback status).
   - **Record Desktop Audio to WAV**: `pc-eye audio --op record --arg 3000` records 3000ms of desktop audio (`@DEFAULT_AUDIO_SINK@`) to `/run/user/1000/pc-eye/audio_capture.wav` with signal dBFS stats.
   - Control media/volume via `pc-eye audio --op play-pause|set-volume|mute --arg 0.5`.

5. **Inspect Merged Logs, App `stdout/stderr`, Coredumps & Notifications (`logs`)**:
   - Call `pc_eye_logs` (or `pc-eye logs --limit 30 --pid <pid>`) to read a chronologically merged stream of:
     - `systemd-journald` system/service logs (`source: "journald"`),
     - `pc-eye launch` managed application `stdout/stderr` logs (`source: "app-stderr"`),
     - `coredumpctl` process crashes with signal names (`SIGSEGV`, `SIGABRT`, `exe`, `pid` — `source: "coredump"`),
     - `dunstctl` desktop notifications (`source: "desktop-notify"`).

6. **Visual Inspection & Set-of-Marks (`vision`)**:
   - Call `pc_eye_vision` (or `pc-eye vision --marks` / `--zoom x,y` / `--region x,y,w,h`) to capture an in-memory screenshot with `dhash_hex`, `pixel_diff_pct`, and `--marks` (draws numbered `[1]`, `[2]` boxes over AT-SPI2 / OCR / high-contrast elements).
   - `pc-eye vision --frames 10 --interval-ms 100` captures a zero-disk frame sequence (per-frame dHash, brightness, change %, and the frame where motion settled) to verify animations; `--out <path>` saves only the last frame.
   - `pc-eye ui` returns `perception_mode: "hybrid-atspi-ocr"` when the AT-SPI tree is trivial (e.g. WebKitGTK/Tauri expose a single empty "frame"); OCR marks are numbered after the AT-SPI ones.

7. **Act Deterministically (`click-ui`, `input`, `window`, `launch`, `director`)**:
   - **Semantic / OCR / Mark Click & Direct D-Bus Invocation**:
     - `pc-eye click-ui --name "Save" --prefer-dbus` (triggers `org.a11y.atspi.Action.DoAction(0)` directly without moving the mouse, or clicks the AT-SPI2 / OCR text coordinates).
     - `pc-eye click-ui --name "Search" --set-text "query"` (writes directly via `EditableText.SetTextContents` or virtual keyboard).
     - `pc-eye click-ui --mark 4` (clicks Set-of-Marks or `pc-eye ui` element `#4`).
     - Every `click-ui` returns `verification` (`changed_pixels`, `change_pct`, `visible_change`) from before/after captures of the target window. `ok: true` only means the action was sent; if `visible_change` is `false` the details start with `UYARI` — the click probably missed, so re-inspect instead of reporting success. Among same-named elements the actionable role (button, check box…) wins over containers (grid cell, label).
   - **Pure-Rust Wayland Virtual Pointer & Keyboard (`input`)**:
     - `pc-eye input click --x 500 --y 300`
     - `pc-eye input type --text "Merhaba Dünya! ŞĞÜÖÇİ"` (supports full UTF-8/Turkish via dynamic XKB `memfd` keymap, no `wtype`/`ydotool` needed).
     - `pc-eye input key --keys "Ctrl+Shift+P"`
   - **Rich Multi-Format Clipboard & `cliphist` (`director clipboard`)**:
     - `pc-eye director clipboard` (returns `mime_types`, `current_text`, `source_url` from Chromium/Electron copies, `has_image`, and last 15 `cliphist` history items).
     - `pc-eye director clipboard --file /path/to/image.png` (copies image as `image/png` or file as `text/uri-list` into Wayland clipboard for instant `Ctrl+V` pasting).
     - `pc-eye director clipboard --restore-id 16` (restores item `#16` from `cliphist` history).
   - **Compositor Keybind Discovery & Execution (`director binds` / `use-bind`)**:
     - `pc-eye director binds` & `pc-eye director use-bind --text "SUPER+Return"`
   - **Atomic Action Batching (`director sequence`) & Action+Observation Fusion (`then=`)**:
     - Every MCP action tool supports `"then": "context" | "ui" | "vision" | "xray"`, returning the post-action state in the same tool call.

8. **Synchronize Without Blind `sleep` (`wait`)**:
   - Never use arbitrary `sleep`. Use `pc-eye wait <condition>`:
     - `pc-eye wait visual_settle --timeout-ms 3000` (waits until `pixel_diff_pct < 0.2%`).
     - `pc-eye wait ui_match --match "Sign In" --timeout-ms 5000` (works on both AT-SPI2 and OCR text!).
     - `pc-eye wait log_match --match "Listening on" --timeout-ms 5000`
     - `pc-eye wait port_open --match "8080" --timeout-ms 5000`
     - `pc-eye wait openwindow --match "firefox" --timeout-ms 5000`

9. **Work on an Isolated Virtual Screen (`display`) — never on the user's screen**:
   - `pc-eye display create --name AGENT-1 --mode 1920x1080@60` opens a headless output 1000 px to the right of every real monitor (not adjacent, so the user's cursor cannot drift into it) and returns its `workspace_id`.
   - `pc-eye launch --workspace <workspace_id> -- <app>` puts the app there; `pc-eye window fullscreen --window <addr>` makes it opaque and fills the output (`--arg on|max|off|toggle`; `--window` targets that window only — without `--window` it toggles the user's active window).
   - `pc-eye display mode --name AGENT-1 --mode 1920x2400@30` makes the output tall to capture a whole long page in one `vision` shot (re-run `window fullscreen` afterwards). `pc-eye display list`, `pc-eye display remove --name AGENT-1` (only headless outputs can be removed).
   - **Input etiquette:** before `input key/type`, focus the target window; afterwards restore the user's previously active window and cursor position. Never send keys without focusing the target first.
   - **Known limit:** `input scroll` reaches GTK3/WebKitGTK windows (Tauri, WebKitGTK apps) as a correct wheel notch, but WebKitGTK does not scroll the page; kitty and other clients do. For WebKitGTK apps use a tall output instead.
