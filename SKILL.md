---
name: pc-eye
description: Full White-Box OS Observability & Deterministic Control Engine for Linux Wayland/Hyprland. Use whenever inspecting desktop windows, UI accessibility trees (AT-SPI2), Terminal PTY context, screenshots with Set-of-Marks/dHash, process descendant trees/sockets/VRAM (X-Ray), PipeWire physical PCM audio signals & MPRIS media/browser URLs, merged journald/notification logs, or controlling mouse/keyboard/windows/clipboard/apps deterministically.
---

# PC-Eye: Full White-Box OS Observability & Director Engine

`pc-eye` gives AI agents complete "White-Box" visibility and deterministic control over the Linux Wayland (`Hyprland`) operating system without relying on slow, blind screenshot-guessing loops.

Binary path: `/home/chaple/.local/bin/pc-eye` (also available via MCP server `pc-eye` with `pc_eye_*` tools).

---

## Core White-Box Workflow for AI Agents

Always prefer **semantic & telemetry inspection first**, and use **Vision (`pc-eye vision`) only when visual verification or custom canvas interaction is needed**:

1. **Orient (`context`)**:
   - Call `pc_eye_context` (or `pc-eye context`) to get the entire desktop state in `<5ms`:
     - All monitors, workspaces, windows (`address`, `class`, `title`, `pid`, `[x,y]`, `[w,h]`), layer-shell overlays (`waybar`, `rofi`, `dunst`), `auth_guard_active`, and `protected` window flags.
     - System health (`cpu_usage_pct`, RAM, Linux PSI pressure stalls, NVIDIA & AMD GPU utilization/VRAM/temp/power).
     - Audio & media summary (default sink volume, active streams, real PCM RMS/Peak dBFS, MPRIS active media title/artist/URL).
     - Monotonic `seq` number, delta compositor events, and recent errors.

2. **Inspect Semantic UI or Terminal PTY (`ui`)**:
   - Call `pc_eye_ui` (or `pc-eye ui --window <addr> --query <filter>`) to read:
     - **AT-SPI2 Semantic Tree**: Every button, input, menu item, and label with its **global screen coordinates** `(x, y, w, h)`, `center_x`, `center_y`, **state flags** (`checked`, `focused`, `editable`, `enabled`, `selected`), and **current text/numeric value**.
     - **Terminal PTY Context**: If the target window is a terminal (`kitty`, `alacritty`, `wezterm`, `foot`), automatically returns `foreground_pid`, `foreground_cmd`, `cwd`, and `pty_path`.

3. **Inspect Process X-Ray (`xray`)**:
   - Call `pc_eye_xray` (or `pc-eye xray --window <addr>` / `--pid <pid>`) to inspect the entire **Application Process Cluster** (all descendant threads/helper processes), CPU/RSS/Disk I/O, `p50/p95/p99` spike statistics, open file descriptors (`/proc/<pid>/fd`), and live `TCP/UDP` network connections.

4. **Inspect Audio, Physical Waveform & Browser URL (`audio`)**:
   - Call `pc_eye_audio` (or `pc-eye audio`) to read PipeWire streams, **real physical PCM 16-bit audio signal metrics** (`rms_dbfs`, `peak_dbfs`, `is_physically_silent` — detects when a stream is playing silence), and **D-Bus MPRIS media metadata** (including active browser tab URL `xesam:url` and playback status).
   - Control media/volume via `pc-eye audio --op play-pause|set-volume|mute --arg 0.5`.

5. **Inspect Merged Logs & Desktop Notifications (`logs`)**:
   - Call `pc_eye_logs` (or `pc-eye logs --limit 30 --pid <pid>`) to read a chronologically merged stream of `systemd-journald`, `pc-eye launch` managed `stdout/stderr` logs, and `dunstctl` desktop notifications.

6. **Visual Inspection & Set-of-Marks (`vision`)**:
   - Call `pc_eye_vision` (or `pc-eye vision --marks` / `--zoom x,y` / `--region x,y,w,h`) to capture an in-memory screenshot with:
     - `dhash`: 64-bit perceptual difference hash.
     - `pixel_diff_pct`: Percentage of pixels changed since the previous capture (`0.0%` = screen settled).
     - `--marks`: Numbered `[1]`, `[2]`, ... bounding boxes drawn over interactive elements (uses AT-SPI2 coordinates or automatic high-contrast visual control box detection for games/Electron/canvas apps).

7. **Act Deterministically (`click-ui`, `input`, `window`, `launch`, `director`)**:
   - **Semantic / Mark Click & Direct D-Bus Invocation**:
     - `pc-eye click-ui --name "Save" --prefer-dbus` (triggers `org.a11y.atspi.Action.DoAction(0)` directly without moving the mouse, falling back to virtual pointer click).
     - `pc-eye click-ui --name "Search" --set-text "query"` (writes directly via `EditableText.SetTextContents` or virtual keyboard).
     - `pc-eye click-ui --mark 4` (clicks Set-of-Marks box `#4`).
   - **Pure-Rust Wayland Virtual Pointer & Keyboard (`input`)**:
     - `pc-eye input click --x 500 --y 300`
     - `pc-eye input type --text "Merhaba Dünya! 123"` (supports full UTF-8/Turkish via dynamic XKB `memfd` keymap, no `wtype`/`ydotool` needed).
     - `pc-eye input key --keys "Ctrl+Shift+P"`
   - **Compositor Keybind Discovery & Execution (`director`)**:
     - `pc-eye director binds` (lists all configured Hyprland keybinds).
     - `pc-eye director use-bind --text "SUPER+Return"` (executes the exact Hyprland dispatcher bound to that shortcut).
   - **Atomic Action Batching (`director sequence`) & Action+Observation Fusion (`then=`)**:
     - Every MCP action tool supports `"then": "context" | "ui" | "vision" | "vision_marks"`, returning the post-action state in the same tool call.
     - Use `pc_eye_director` with `op: "sequence"` to run multi-step flows (`focus` -> `key` -> `wait` -> `type`) in a single call.

8. **Synchronize Without Blind `sleep` (`wait`)**:
   - Never use arbitrary `sleep`. Use `pc-eye wait <condition>`:
     - `pc-eye wait visual_settle --timeout-ms 3000` (waits until `pixel_diff_pct < 0.2%`).
     - `pc-eye wait ui_match --match "Sign In" --timeout-ms 5000`
     - `pc-eye wait log_match --match "Listening on" --timeout-ms 5000`
     - `pc-eye wait port_open --match "8080" --timeout-ms 5000`
     - `pc-eye wait openwindow --match "firefox" --timeout-ms 5000`
