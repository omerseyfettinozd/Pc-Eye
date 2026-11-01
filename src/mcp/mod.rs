use crate::daemon::server::{
    build_context_report, build_xray_for_target, spawn_background_collectors,
};
use crate::director::input::{
    click_at, drag_mouse, move_cursor, press_key_combo, scroll_wheel,
    type_text,
};
use crate::director::process_ctl::{
    clipboard_action_ext, freeze_process_safe, launch_application, manage_window, thaw_process,
    trigger_hyprland_bind,
};
use crate::director::sequence::{execute_sequence, SequenceStep};
use crate::director::wait::wait_for_condition;
use crate::eyes::audio_pipewire::{capture_audio_report, control_audio_and_media};
use crate::eyes::compositor::list_hyprland_binds;
use crate::eyes::logs_notify::query_recent_journal_logs;
use crate::eyes::ui_atspi::inspect_window_ui;
use crate::eyes::vision::{capture_frame_sequence, capture_vision};
use anyhow::Result;
use base64::Engine;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::sync::mpsc;
use std::time::Duration;

/// Her MCP araç çağrısının varsayılan üst süresi; `PC_EYE_TOOL_TIMEOUT_MS` ile değiştirilebilir.
const DEFAULT_TOOL_TIMEOUT_MS: u64 = 45_000;
/// Kendi `timeout_ms` argümanı olan araçlar (wait, launch) için bu argümanın üstüne eklenen pay.
const TOOL_TIMEOUT_SLACK_MS: u64 = 10_000;

/// Standart Model Context Protocol (MCP) JSON-RPC 2.0 stdio sunucusunu çalıştırır.
/// Otomatik delta için bu MCP oturumunda pencere başına son snapshot kimliği.
static LAST_SNAP: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, (u64, std::time::Instant)>>,
> = std::sync::LazyLock::new(Default::default);
const AUTO_DELTA_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);

pub fn run_mcp_stdio_server() -> Result<()> {
    let _bg_state = spawn_background_collectors();
    // Uzun yaşayan süreç: AT-SPI olaylarını dinleyip eleman önbelleğini geçerli tut.
    crate::eyes::a11y_events::start();
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(resp) = handle_mcp_request(&req) {
            let serialized = serde_json::to_string(&resp)?;
            writeln!(stdout, "{}", serialized)?;
            stdout.flush()?;
        }
    }
    Ok(())
}

pub fn handle_mcp_request(req: &Value) -> Option<Value> {
    let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let id = req.get("id").cloned();

    if id.is_none() || method.starts_with("notifications/") {
        return None;
    }
    let id_val = id.unwrap_or(Value::Null);

    match method {
        "initialize" => Some(json!({
            "jsonrpc": "2.0",
            "id": id_val,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": { "listChanged": false }
                },
                "serverInfo": {
                    "name": "pc-eye",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        })),
        "ping" => Some(json!({
            "jsonrpc": "2.0",
            "id": id_val,
            "result": {}
        })),
        "tools/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id_val,
            "result": {
                "tools": mcp_tools_schema()
            }
        })),
        "tools/call" => {
            let params = req.get("params").cloned().unwrap_or(Value::Null);
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));

            match dispatch_tool_call_bounded(name, &args) {
                Ok(content_items) => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id_val,
                    "result": {
                        "content": content_items,
                        "isError": false
                    }
                })),
                Err(err) => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id_val,
                    "result": {
                        "content": [{
                            "type": "text",
                            "text": format!("ERROR: {}", err)
                        }],
                        "isError": true
                    }
                })),
            }
        }
        _ => Some(json!({
            "jsonrpc": "2.0",
            "id": id_val,
            "error": {
                "code": -32601,
                "message": format!("Method not found: {}", method)
            }
        })),
    }
}

/// Araç çağrısının üst süresi: varsayılan (ya da ortam değişkeni) ile aracın kendi `timeout_ms`'i + payın büyüğü.
fn tool_call_budget(args: &Value) -> Duration {
    let base = std::env::var("PC_EYE_TOOL_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .unwrap_or(DEFAULT_TOOL_TIMEOUT_MS);
    let own = args
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .map(|ms| ms.saturating_add(TOOL_TIMEOUT_SLACK_MS))
        .unwrap_or(0);
    Duration::from_millis(base.max(own))
}

/// `dispatch_tool_call`'ı ayrı bir iş parçacığında çalıştırır; üst süre dolarsa istemciye hata döner,
/// böylece takılan tek bir araç MCP oturumunu kilitlemez. Takılan iş parçacığı arkada kendi
/// alt süreç zaman aşımlarıyla sonlanır.
fn dispatch_tool_call_bounded(name: &str, args: &Value) -> Result<Vec<Value>> {
    let (owned_name, owned_args) = (name.to_string(), args.clone());
    let started = std::time::Instant::now();
    let result = run_with_deadline(name, tool_call_budget(args), move || {
        dispatch_tool_call(&owned_name, &owned_args)
    });
    // `PC_EYE_TRACE=<dosya>`: araç başına süre (hız darboğazlarını ölçmek için).
    if let Ok(path) = std::env::var("PC_EYE_TRACE") {
        let line = json!({
            "tool": name,
            "ms": started.elapsed().as_millis() as u64,
            "ok": result.is_ok(),
            "op": args.get("op").or_else(|| args.get("action")).cloned().unwrap_or(Value::Null),
            "steps": args.get("steps").and_then(|s| s.as_array()).map(|a| a.len()),
        });
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            use std::io::Write;
            let _ = writeln!(f, "{}", line);
        }
    }
    result
}

fn run_with_deadline<T, F>(name: &str, budget: Duration, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name(format!("mcp-{}", name))
        .spawn(move || {
            let _ = tx.send(work());
        })?;
    match rx.recv_timeout(budget) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(anyhow::anyhow!(
            "`{}` aracı {} ms üst süreyi aştı; çağrı iptal edildi",
            name,
            budget.as_millis()
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(anyhow::anyhow!("`{}` aracı sonuç vermeden çöktü", name))
        }
    }
}

fn dispatch_tool_call(name: &str, args: &Value) -> Result<Vec<Value>> {
    crate::session::ensure_alive()?;
    let then_mode = args.get("then").and_then(|v| v.as_str()).unwrap_or("none");

    let mut items = match name {
        "pc_eye_context" => {
            let seq_since = args.get("seq_since").and_then(|v| v.as_u64());
            let rep = build_context_report(seq_since)?;
            vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rep)? })]
        }
        "pc_eye_snapshot" => {
            let win = args.get("window").and_then(|v| v.as_str());
            let max = args.get("max_elements").and_then(|v| v.as_u64()).unwrap_or(40) as usize;
            let icons = args.get("icons").and_then(|v| v.as_bool()).unwrap_or(false);
            let seq_since = args.get("seq_since").and_then(|v| v.as_u64());
            // Otomatik delta: ajanlar isteğe bağlı `since`'i kendiliğinden kullanmıyordu (ölçüm 045024).
            // Bu oturumda aynı pencereye yakın zamanda tam snapshot verildiyse yalnız değişenler döner;
            // mark'lar kararlı olduğu için ajanın elindeki önceki liste geçerli kalır. `full: true` tam liste.
            let full = args.get("full").and_then(|v| v.as_bool()).unwrap_or(false);
            let since = args.get("since").and_then(|v| v.as_u64());
            let snap = auto_snapshot(win, max, icons, seq_since, since, full)?;
            // Kompakt JSON: girintiler bu araçta yükün belirgin bir kısmı olurdu.
            vec![json!({ "type": "text", "text": serde_json::to_string(&snap)? })]
        }
        "pc_eye_ui" => {
            let win = args.get("window").and_then(|v| v.as_str());
            let query = args.get("query").and_then(|v| v.as_str());
            let max_el = args
                .get("max_elements")
                .and_then(|v| v.as_u64())
                .unwrap_or(60) as usize;
            let rep = inspect_window_ui(win, query, max_el)?;
            vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rep)? })]
        }
        "pc_eye_vision" => {
            let win = args.get("window").and_then(|v| v.as_str());
            let marks = args.get("marks").and_then(|v| v.as_bool()).unwrap_or(false);
            // Görüntü her zaman yanıta eklenir: MCP istemcisi dosya okuyamaz; tam pencere yakalamada
            // yalnız yol dönmek ajana boşa bir tur yaktırıyordu ("ekran görüntüsü dönmedi").
            let include_b64 = args.get("include_base64").and_then(|v| v.as_bool()).unwrap_or(true);
            let region = args
                .get("region")
                .and_then(|v| v.as_array())
                .and_then(|arr| {
                    if arr.len() == 4 {
                        Some([
                            arr[0].as_i64()?,
                            arr[1].as_i64()?,
                            arr[2].as_i64()?,
                            arr[3].as_i64()?,
                        ])
                    } else {
                        None
                    }
                });
            let zoom = args.get("zoom").and_then(|v| v.as_array()).and_then(|arr| {
                if arr.len() == 2 {
                    Some([arr[0].as_i64()?, arr[1].as_i64()?])
                } else {
                    None
                }
            });
            // Kare dizisi (animasyon izlenebilirliği) modu: `frames` verilirse dizi yakalanır.
            if let Some(n) = args.get("frames").and_then(|v| v.as_u64()) {
                let interval_ms = args
                    .get("interval_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(100);
                let out_path = args.get("out").and_then(|v| v.as_str());
                let rep = capture_frame_sequence(
                    win,
                    region,
                    zoom,
                    n as usize,
                    interval_ms,
                    out_path,
                )?;
                let mut out =
                    vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rep)? })];
                if include_b64 {
                    if let Some(p) = rep.saved_frame_path.as_deref() {
                        if let Ok(bytes) = std::fs::read(p) {
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            out.push(json!({
                                "type": "image",
                                "data": b64,
                                "mimeType": "image/png"
                            }));
                        }
                    }
                }
                out
            } else {
                let rep = capture_vision(win, region, zoom, marks)?;
                let mut out =
                    vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rep)? })];
                if include_b64 {
                    if let Ok(bytes) = std::fs::read(&rep.file_path) {
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        out.push(json!({
                            "type": "image",
                            "data": b64,
                            "mimeType": "image/png"
                        }));
                    }
                }
                out
            }
        }
        "pc_eye_xray" => {
            let win = args.get("window").and_then(|v| v.as_str());
            let pid = args.get("pid").and_then(|v| v.as_i64()).map(|n| n as i32);
            let rep = build_xray_for_target(win, pid)?;
            vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rep)? })]
        }
        "pc_eye_processes" => {
            let query = args.get("match").and_then(|v| v.as_str());
            let port = args.get("port").and_then(|v| v.as_u64()).map(|n| n as u16);
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
            let found = crate::eyes::process_xray::find_processes(query, port, limit)?;
            vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&found)? })]
        }
        "pc_eye_logs" => {
            let pid = args.get("pid").and_then(|v| v.as_i64()).map(|n| n as i32);
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(30) as usize;
            let pids = pid.map(|p| vec![p]);
            let logs = query_recent_journal_logs(pids.as_deref(), limit)?;
            vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&logs)? })]
        }
        "pc_eye_audio" => {
            if let Some(op) = args.get("op").and_then(|v| v.as_str()) {
                let arg = args.get("arg").and_then(|v| v.as_str());
                let rec = control_audio_and_media(op, arg)?;
                vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rec)? })]
            } else {
                let rep = capture_audio_report()?;
                vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rep)? })]
            }
        }
        "pc_eye_click_ui" => {
            let win = args.get("window").and_then(|v| v.as_str());
            let name_q = args.get("name").and_then(|v| v.as_str());
            let mark = args
                .get("mark")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize);
            let btn = args
                .get("button")
                .and_then(|v| v.as_str())
                .unwrap_or("left");
            let prefer_dbus = args
                .get("prefer_dbus")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let set_text = args.get("set_text").and_then(|v| v.as_str());
            let allow_prot = args
                .get("allow_protected")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let clicks = args.get("clicks").and_then(|v| v.as_u64()).unwrap_or(1).clamp(1, 3) as u32;
            let rec = crate::director::input::click_ui_element_n(
                win, name_q, mark, btn, prefer_dbus, set_text, allow_prot, clicks,
            )?;
            note_click_for_recipes(win, name_q, set_text, btn, &rec);
            vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rec)? })]
        }
        "pc_eye_input" => {
            // Koordinatla girdi yeniden oynatılamaz: tarif izi burada kopar.
            crate::director::recipes::break_trail();
            let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("click");
            let allow_prot = args
                .get("allow_protected")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let win = args.get("window").and_then(|v| v.as_str());
            let rec = match op {
                "click" => {
                    let x = args.get("x").and_then(|v| v.as_i64());
                    let y = args.get("y").and_then(|v| v.as_i64());
                    let btn = args
                        .get("button")
                        .and_then(|v| v.as_str())
                        .unwrap_or("left");
                    let clicks = args.get("clicks").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                    click_at(x, y, btn, clicks, allow_prot)?
                }
                "move" => {
                    let x = args
                        .get("x")
                        .and_then(|v| v.as_i64())
                        .ok_or_else(|| anyhow::anyhow!("x gerekli"))?;
                    let y = args
                        .get("y")
                        .and_then(|v| v.as_i64())
                        .ok_or_else(|| anyhow::anyhow!("y gerekli"))?;
                    move_cursor(x, y)?
                }
                "drag" => {
                    let x1 = args
                        .get("x")
                        .and_then(|v| v.as_i64())
                        .ok_or_else(|| anyhow::anyhow!("x gerekli"))?;
                    let y1 = args
                        .get("y")
                        .and_then(|v| v.as_i64())
                        .ok_or_else(|| anyhow::anyhow!("y gerekli"))?;
                    let x2 = args
                        .get("x2")
                        .and_then(|v| v.as_i64())
                        .ok_or_else(|| anyhow::anyhow!("x2 gerekli"))?;
                    let y2 = args
                        .get("y2")
                        .and_then(|v| v.as_i64())
                        .ok_or_else(|| anyhow::anyhow!("y2 gerekli"))?;
                    drag_mouse(x1, y1, x2, y2, "left", allow_prot)?
                }
                "scroll" => {
                    let x = args.get("x").and_then(|v| v.as_i64());
                    let y = args.get("y").and_then(|v| v.as_i64());
                    let dx = args.get("dx").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    let dy = args.get("dy").and_then(|v| v.as_i64()).unwrap_or(-3) as i32;
                    scroll_wheel(x, y, dx, dy, allow_prot)?
                }
                "type" => {
                    let txt = args
                        .get("text")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow::anyhow!("text gerekli"))?;
                    let r = type_text(txt, win, allow_prot)?;
                    crate::director::input::settle_after_input();
                    r
                }
                "key" => {
                    let keys = args
                        .get("keys")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow::anyhow!("keys gerekli"))?;
                    let r = press_key_combo(keys, win, allow_prot)?;
                    crate::director::input::settle_after_input();
                    r
                }
                other => return Err(anyhow::anyhow!("Bilinmeyen input op: {}", other)),
            };
            vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rec)? })]
        }
        "pc_eye_window" => {
            let action = args
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("focus");
            let target = args.get("window").and_then(|v| v.as_str());
            let arg = args.get("arg").and_then(|v| v.as_str());
            let rec = manage_window(action, target, arg)?;
            vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rec)? })]
        }
        "pc_eye_display" => {
            let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("list");
            let rec = crate::director::display::manage_display(
                action,
                args.get("name").and_then(|v| v.as_str()),
                args.get("mode").and_then(|v| v.as_str()),
                args.get("position").and_then(|v| v.as_str()),
                args.get("scale").and_then(|v| v.as_f64()),
            )?;
            vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rec)? })]
        }
        "pc_eye_run" => {
            let allow_prot = args.get("allow_protected").and_then(|v| v.as_bool()).unwrap_or(false);
            let out = crate::director::program::run_program(&args, allow_prot)?;
            vec![json!({ "type": "text", "text": serde_json::to_string(&out)? })]
        }
        "pc_eye_launch" => {
            let cmd_arr: Vec<String> = args
                .get("command")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let ws = args.get("workspace").and_then(|v| v.as_str());
            let timeout = args
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(3000);
            crate::director::guard::enforce_mcp_launch_policy(&cmd_arr)?;
            let rec = launch_application(&cmd_arr, ws, timeout)?;
            vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rec)? })]
        }
        "pc_eye_wait" => {
            let event = args
                .get("event")
                .and_then(|v| v.as_str())
                .unwrap_or("visual_settle");
            let match_str = args.get("match").and_then(|v| v.as_str());
            let win = args.get("window").and_then(|v| v.as_str());
            let timeout = args
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(5000);
            let rep = wait_for_condition(event, match_str, win, timeout)?;
            vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rep)? })]
        }
        "pc_eye_director" => {
            let op = args
                .get("op")
                .and_then(|v| v.as_str())
                .unwrap_or("clipboard");
            match op {
                "freeze" => {
                    let pid = args
                        .get("pid")
                        .and_then(|v| v.as_i64())
                        .ok_or_else(|| anyhow::anyhow!("pid gerekli"))?
                        as i32;
                    let max_ms = args
                        .get("max_freeze_ms")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(500);
                    let rec = freeze_process_safe(pid, max_ms)?;
                    vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rec)? })]
                }
                "thaw" => {
                    let pid = args
                        .get("pid")
                        .and_then(|v| v.as_i64())
                        .ok_or_else(|| anyhow::anyhow!("pid gerekli"))?
                        as i32;
                    let rec = thaw_process(pid)?;
                    vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rec)? })]
                }
                "clipboard" => {
                    let text = args.get("text").and_then(|v| v.as_str());
                    let file = args.get("file").and_then(|v| v.as_str());
                    let restore_id = args.get("restore_id").and_then(|v| v.as_str());
                    let val = clipboard_action_ext(text, file, restore_id)?;
                    vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&val)? })]
                }
                "binds" => {
                    let binds = list_hyprland_binds()?;
                    vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&binds)? })]
                }
                "use_bind" => {
                    let combo = args.get("text").and_then(|v| v.as_str()).ok_or_else(|| {
                        anyhow::anyhow!("text içinde kısayol adı gerekli (ör. SUPER+Return)")
                    })?;
                    let rec = trigger_hyprland_bind(combo)?;
                    vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rec)? })]
                }
                "sequence" => {
                    let steps_val = args
                        .get("steps")
                        .cloned()
                        .unwrap_or_else(|| Value::Array(vec![]));
                    let steps: Vec<SequenceStep> = serde_json::from_value(steps_val)?;
                    let allow_prot = args
                        .get("allow_protected")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let rep = execute_sequence(&steps, allow_prot)?;
                    vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rep)? })]
                }
                other => return Err(anyhow::anyhow!("Bilinmeyen director op: {}", other)),
            }
        }
        other => return Err(anyhow::anyhow!("Bilinmeyen MCP aracı: {}", other)),
    };

    match then_mode {
        // Eylem sonrası durum kompakt snapshot'tır: tam `context` (~38 KB) ya da tam `ui` (~20 KB)
        // tek çağrıda bütün bir görevin taban maliyeti kadar token yiyordu (ajan ölçümü 045024).
        "context" | "ui" | "snapshot" => {
            let win = args.get("window").and_then(|v| v.as_str());
            if let Ok(snap) = after_action_snapshot(win) {
                items.push(json!({ "type": "text", "text": serde_json::to_string(&snap)? }));
            }
        }
        "full_context" => {
            if let Ok(ctx) = build_context_report(None) {
                items.push(json!({ "type": "text", "text": serde_json::to_string_pretty(&ctx)? }));
            }
        }
        "vision" | "screenshot" => {
            let win = args.get("window").and_then(|v| v.as_str());
            if let Ok(vis) = capture_vision(win, None, None, true) {
                items.push(json!({ "type": "text", "text": serde_json::to_string_pretty(&vis)? }));
            }
        }
        "xray" => {
            let win = args.get("window").and_then(|v| v.as_str());
            let pid = args.get("pid").and_then(|v| v.as_i64()).map(|n| n as i32);
            if let Ok(xr) = build_xray_for_target(win, pid) {
                items.push(json!({ "type": "text", "text": serde_json::to_string_pretty(&xr)? }));
            }
        }
        _ => {}
    }

    Ok(items)
}

fn mcp_tools_schema() -> Value {
    // Model her turda bu şemayı okur: açıklamalar kısa ve İngilizce (Türkçe karakterli metin daha
    // çok token'a bölünüyor). Ayrıntılı kullanım SKILL.md'de.
    let win = json!({ "type": "string", "description": "Window address, title or class (default: active)" });
    let then = json!({ "type": "string", "description": "After the action, also return: snapshot | vision | xray" });
    json!([
        {
            "name": "pc_eye_snapshot",
            "description": "CALL FIRST. Compact state: windows + target window's interactive elements (mark, role, name, value, states, click point `at`; terminal lines). Repeat calls return only changes (marks are stable); full=true for everything.",
            "annotations": { "readOnlyHint": true },
            "inputSchema": { "type": "object", "properties": {
                "window": win.clone(),
                "max_elements": { "type": "integer" },
                "icons": { "type": "boolean", "description": "Include guessed icon boxes" },
                "full": { "type": "boolean" },
                "since": { "type": "integer", "description": "Delta against this snap id" },
                "seq_since": { "type": "integer", "description": "Window events after this seq" }
            } }
        },
        {
            "name": "pc_eye_run",
            "description": "Run several steps in ONE call, verifying each; stops at the first failure and returns the final state (`after`). Use for forms and multi-step flows instead of repeated click_ui; include the final submit click and its expected result. Steps: {\"click\":T}, {\"open\":T} (double-click), {\"set\":T,\"text\":\"..\"}, {\"select\":T,\"option\":\"..\"}, {\"type\":\"..\"}, {\"key\":\"ctrl+s\"}, {\"wait\":\"text\"}. T = element name or {\"mark\":N}. Dropdowns: use select with the option's visible text; no need to open it first. Any step may add \"expect\":\"text that must appear\" or {\"gone\":\"..\"} (timeout_ms, default 3000). Or replay a recipe offered by snapshot: {\"recipe\":ID,\"args\":{..}}.",
            "annotations": { "readOnlyHint": false },
            "inputSchema": { "type": "object", "properties": {
                "window": win.clone(),
                "steps": { "type": "array", "items": { "type": "object" } },
                "recipe": { "type": "string" },
                "args": { "type": "object" }
            } }
        },
        {
            "name": "pc_eye_click_ui",
            "description": "Click an element by name or mark (AT-SPI or OCR), or type into it with set_text. Off-screen elements (state `offscreen`) are scrolled into view first — no manual scrolling needed. Returns verification.visible_change; false means it probably missed.",
            "annotations": { "readOnlyHint": false, "destructiveHint": true },
            "inputSchema": { "type": "object", "properties": {
                "window": win.clone(),
                "name": { "type": "string" },
                "mark": { "type": "integer" },
                "button": { "type": "string", "description": "left | right | middle" },
                "prefer_dbus": { "type": "boolean", "description": "Invoke via D-Bus without moving the mouse" },
                "set_text": { "type": "string" },
                "clicks": { "type": "integer", "description": "2 = double-click (open a folder/file)" },
                "then": then.clone()
            } }
        },
        {
            "name": "pc_eye_input",
            "description": "Raw mouse/keyboard: click, move, drag, scroll, type (Unicode), key (e.g. ctrl+shift+p).",
            "annotations": { "readOnlyHint": false, "destructiveHint": true },
            "inputSchema": { "type": "object", "required": ["op"], "properties": {
                "op": { "type": "string", "description": "click | move | drag | scroll | type | key" },
                "x": { "type": "integer" }, "y": { "type": "integer" },
                "x2": { "type": "integer" }, "y2": { "type": "integer" },
                "dx": { "type": "integer" }, "dy": { "type": "integer" },
                "button": { "type": "string" }, "clicks": { "type": "integer" },
                "text": { "type": "string" }, "keys": { "type": "string" },
                "window": win.clone(),
                "then": then.clone()
            } }
        },
        {
            "name": "pc_eye_ui",
            "description": "Full accessibility tree of a window (bounds, states, values, D-Bus paths) or terminal PTY state. Prefer snapshot.",
            "annotations": { "readOnlyHint": true },
            "inputSchema": { "type": "object", "properties": {
                "window": win.clone(),
                "query": { "type": "string", "description": "Name or role filter" },
                "max_elements": { "type": "integer" }
            } }
        },
        {
            "name": "pc_eye_vision",
            "description": "Screenshot of a window, region or zoom point (image included for zoom/region). marks=true draws numbered boxes; frames=N samples animation.",
            "annotations": { "readOnlyHint": true },
            "inputSchema": { "type": "object", "properties": {
                "window": win.clone(),
                "region": { "type": "array", "items": { "type": "integer" }, "description": "[x,y,w,h]; relative to window if window is given" },
                "zoom": { "type": "array", "items": { "type": "integer" }, "description": "[x,y] center" },
                "marks": { "type": "boolean" },
                "frames": { "type": "integer" },
                "interval_ms": { "type": "integer" },
                "out": { "type": "string", "description": "Save last frame to this path" },
                "include_base64": { "type": "boolean" }
            } }
        },
        {
            "name": "pc_eye_processes",
            "description": "Find processes (also windowless) by name/command-line text or listening port: PID, command line, RAM, listening endpoints.",
            "annotations": { "readOnlyHint": true },
            "inputSchema": { "type": "object", "properties": {
                "match": { "type": "string" },
                "port": { "type": "integer" },
                "limit": { "type": "integer" }
            } }
        },
        {
            "name": "pc_eye_xray",
            "description": "Process tree of a window or PID: open files, TCP/UDP connections, CPU/RAM history, recent errors.",
            "annotations": { "readOnlyHint": true },
            "inputSchema": { "type": "object", "properties": {
                "window": win.clone(),
                "pid": { "type": "integer" }
            } }
        },
        {
            "name": "pc_eye_logs",
            "description": "Merged journald, launched-app stdout/stderr, coredumps (signal, pid) and desktop notifications.",
            "annotations": { "readOnlyHint": true },
            "inputSchema": { "type": "object", "properties": {
                "pid": { "type": "integer" },
                "limit": { "type": "integer" }
            } }
        },
        {
            "name": "pc_eye_context",
            "description": "Large (~25 KB) full report: desktop, system load/GPU, audio/media, event and log history. Use only when snapshot is not enough.",
            "annotations": { "readOnlyHint": true },
            "inputSchema": { "type": "object", "properties": {
                "seq_since": { "type": "integer" }
            } }
        },
        {
            "name": "pc_eye_wait",
            "description": "Wait without sleeping: visual_settle | ui_match (UI/OCR text appears) | log_match | port_open | openwindow | closewindow | activewindow | workspace.",
            "annotations": { "readOnlyHint": true },
            "inputSchema": { "type": "object", "properties": {
                "event": { "type": "string" },
                "match": { "type": "string" },
                "window": win.clone(),
                "timeout_ms": { "type": "integer" }
            } }
        },
        {
            "name": "pc_eye_window",
            "description": "Window control: focus | close | workspace | movetoworkspace | fullscreen (arg on|max|off|toggle) | float | move | resize.",
            "annotations": { "readOnlyHint": false },
            "inputSchema": { "type": "object", "required": ["action"], "properties": {
                "action": { "type": "string" },
                "window": win.clone(),
                "arg": { "type": "string" },
                "then": then.clone()
            } }
        },
        {
            "name": "pc_eye_launch",
            "description": "Start an app, capture its output, return the new window. Inline shell/interpreter code (sh -c, python -c) is refused.",
            "annotations": { "readOnlyHint": false, "destructiveHint": true },
            "inputSchema": { "type": "object", "required": ["command"], "properties": {
                "command": { "type": "array", "items": { "type": "string" } },
                "workspace": { "type": "string" },
                "timeout_ms": { "type": "integer" },
                "then": then.clone()
            } }
        },
        {
            "name": "pc_eye_display",
            "description": "Virtual headless outputs isolated from the user's screens: list | create (returns workspace_id) | mode | remove.",
            "annotations": { "readOnlyHint": false },
            "inputSchema": { "type": "object", "required": ["action"], "properties": {
                "action": { "type": "string" },
                "name": { "type": "string" },
                "mode": { "type": "string", "description": "WxH or WxH@R" },
                "position": { "type": "string" },
                "scale": { "type": "number" },
                "then": then.clone()
            } }
        },
        {
            "name": "pc_eye_audio",
            "description": "Audio streams, PCM level, MPRIS media/browser URL; op: play | pause | play-pause | next | previous | stop | set-volume | mute | unmute | toggle-mute | record (arg = ms).",
            "annotations": { "readOnlyHint": false },
            "inputSchema": { "type": "object", "properties": {
                "op": { "type": "string" },
                "arg": { "type": "string" }
            } }
        },
        {
            "name": "pc_eye_director",
            "description": "freeze | thaw (watchdog) | clipboard (read; text, file or restore_id to write) | binds | use_bind | sequence (legacy; prefer pc_eye_run).",
            "annotations": { "readOnlyHint": false, "destructiveHint": true },
            "inputSchema": { "type": "object", "required": ["op"], "properties": {
                "op": { "type": "string" },
                "pid": { "type": "integer" },
                "max_freeze_ms": { "type": "integer" },
                "text": { "type": "string" },
                "file": { "type": "string" },
                "restore_id": { "type": "string" },
                "steps": { "type": "array", "items": { "type": "object" } },
                "then": then
            } }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn test_run_with_deadline_returns_error_when_tool_hangs() {
        let started = Instant::now();
        let res: Result<()> = run_with_deadline("slow", Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_secs(5));
            Ok(())
        });
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(res.unwrap_err().to_string().contains("üst süreyi aştı"));
    }

    #[test]
    fn test_run_with_deadline_passes_through_fast_result() {
        let res = run_with_deadline("fast", Duration::from_secs(1), || Ok(42));
        assert_eq!(res.unwrap(), 42);
    }

    #[test]
    fn test_tool_call_budget_respects_own_timeout() {
        assert!(tool_call_budget(&json!({})) >= Duration::from_millis(1));
        let long = tool_call_budget(&json!({ "timeout_ms": 120_000 }));
        assert_eq!(long, Duration::from_millis(120_000 + TOOL_TIMEOUT_SLACK_MS));
    }
}

/// Programdan hemen sonra gelen tekil `click_ui` (ör. ayrı basılan Gönder) tarif izine eklenir.
fn note_click_for_recipes(
    win: Option<&str>,
    name_q: Option<&str>,
    set_text: Option<&str>,
    button: &str,
    rec: &crate::director::input::ActionReceipt,
) {
    let missed = rec.verification.as_ref().map_or(false, |v| !v.visible_change);
    let name = name_q.map(String::from).or_else(|| {
        let rest = rec.target.split_once("] ").map(|(_, r)| r)?;
        Some(rest.rsplit_once(" (").map_or(rest, |(n, _)| n).trim().to_string()).filter(|n| !n.is_empty())
    });
    let (Some(name), false, "left") = (name, missed, button) else {
        crate::director::recipes::break_trail();
        return;
    };
    let Ok(ui) = inspect_window_ui(win, None, 80) else { return };
    let step = match set_text {
        Some(t) => json!({ "set": name, "text": t }),
        None => json!({ "click": name }),
    };
    let names: Vec<String> = ui.elements.iter().map(|e| e.name.clone()).collect();
    crate::director::recipes::record(&ui.window_address, &ui.window_class, &[step], &names);
}

/// Otomatik delta: ajanlar isteğe bağlı `since`'i kendiliğinden kullanmıyordu (ölçüm 045024). Bu
/// oturumda aynı pencereye yakın zamanda tam snapshot verildiyse yalnız değişenler döner; mark'lar
/// kararlı olduğu için ajanın elindeki önceki liste geçerli kalır. `full` tam liste.
fn auto_snapshot(
    win: Option<&str>,
    max: usize,
    icons: bool,
    seq_since: Option<u64>,
    since: Option<u64>,
    full: bool,
) -> Result<Value> {
    let window_key = win.unwrap_or("").to_string();
    let since = since.or_else(|| {
        if full {
            return None;
        }
        let last = LAST_SNAP.lock().ok()?;
        last.get(&window_key).filter(|(_, at)| at.elapsed() < AUTO_DELTA_WINDOW).map(|(id, _)| *id)
    });
    let mut snap = crate::eyes::snapshot::build_snapshot(win, max, icons, seq_since, since)?;
    crate::director::recipes::annotate_snapshot(&mut snap);
    if let (Some(id), Ok(mut last)) = (snap["snap"].as_u64(), LAST_SNAP.lock()) {
        last.insert(window_key, (id, std::time::Instant::now()));
    }
    Ok(snap)
}

/// Eylemden sonraki durum (`then: snapshot`): delta olarak (eskiden her seferinde ~3,5 KB tam liste).
/// Eylem pencereyi kapattıysa (diyalogda Tamam, sihirbazda sonraki adım) kısa süre yeni pencereyi
/// bekleyip ona bakar: ajan "pencere kayboldu" görüp ayrıca tam snapshot istiyordu.
fn after_action_snapshot(win: Option<&str>) -> Result<Value> {
    let snap = auto_snapshot(win, 40, false, None, None, false)?;
    let target_gone = snap["target"].get("error").is_some();
    if !target_gone || win.is_none() {
        return Ok(snap);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(150));
        let again = auto_snapshot(win, 40, false, None, None, true)?;
        if again["target"].get("error").is_none() {
            return Ok(again);
        }
    }
    // Aynı adla açılan pencere yok: odaktaki pencereye bak.
    let mut focused = auto_snapshot(None, 40, false, None, None, true)?;
    focused["note"] = json!("The window you acted on closed; this is the focused window now.");
    Ok(focused)
}
