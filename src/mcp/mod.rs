use crate::daemon::server::{
    build_context_report, build_xray_for_target, spawn_background_collectors,
};
use crate::director::input::{
    click_at, click_ui_element_ext, drag_mouse, move_cursor, press_key_combo, scroll_wheel,
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
use crate::eyes::vision::capture_vision;
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
pub fn run_mcp_stdio_server() -> Result<()> {
    let _bg_state = spawn_background_collectors();
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
    run_with_deadline(name, tool_call_budget(args), move || {
        dispatch_tool_call(&owned_name, &owned_args)
    })
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
    let then_mode = args.get("then").and_then(|v| v.as_str()).unwrap_or("none");

    let mut items = match name {
        "pc_eye_context" => {
            let seq_since = args.get("seq_since").and_then(|v| v.as_u64());
            let rep = build_context_report(seq_since)?;
            vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rep)? })]
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
            let include_b64 = args
                .get("include_base64")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
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
        "pc_eye_xray" => {
            let win = args.get("window").and_then(|v| v.as_str());
            let pid = args.get("pid").and_then(|v| v.as_i64()).map(|n| n as i32);
            let rep = build_xray_for_target(win, pid)?;
            vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rep)? })]
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
            let rec =
                click_ui_element_ext(win, name_q, mark, btn, prefer_dbus, set_text, allow_prot)?;
            vec![json!({ "type": "text", "text": serde_json::to_string_pretty(&rec)? })]
        }
        "pc_eye_input" => {
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
                    type_text(txt, win, allow_prot)?
                }
                "key" => {
                    let keys = args
                        .get("keys")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow::anyhow!("keys gerekli"))?;
                    press_key_combo(keys, win, allow_prot)?
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
        "context" => {
            if let Ok(ctx) = build_context_report(None) {
                items.push(json!({ "type": "text", "text": serde_json::to_string_pretty(&ctx)? }));
            }
        }
        "ui" => {
            let win = args.get("window").and_then(|v| v.as_str());
            if let Ok(ui) = inspect_window_ui(win, None, 60) {
                items.push(json!({ "type": "text", "text": serde_json::to_string_pretty(&ui)? }));
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
    json!([
        {
            "name": "pc_eye_context",
            "description": "Tek çağrıda tam Beyaz-Kutu masaüstü + sistem yükü (PSI/RAM/CPU/GPU p50/p95/p99) + ses/MPRIS medya/tarayıcı URL + son olay/log/bildirim delta özeti döner.",
            "annotations": { "readOnlyHint": true },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "seq_since": { "type": "integer", "description": "Sadece bu sıra numarasından sonraki olayları (delta) döndürür." }
                }
            }
        },
        {
            "name": "pc_eye_ui",
            "description": "Hedef pencerenin AT-SPI2 erişilebilirlik ağacını (ekran koordinatları, checked/focused/editable durumları ve metin/değer içerikleriyle) veya terminal ise doğrudan PTY/kabuk durumunu okur.",
            "annotations": { "readOnlyHint": true },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": { "type": "string", "description": "Pencere adresi (ör. 0x55fd...). Boşsa aktif pencere." },
                    "query": { "type": "string", "description": "İsim veya rol filtresi." },
                    "max_elements": { "type": "integer", "description": "Maksimum eleman sayısı (varsayılan 60)." }
                }
            }
        },
        {
            "name": "pc_eye_vision",
            "description": "Sıfır-disk PPM hattı üzerinden tam ekran, pencere, bölge veya zoom görüntüsü alır; dHash/pixel-diff hesaplar ve isteğe bağlı Set-of-Marks numaralı kutularını çizer.",
            "annotations": { "readOnlyHint": true },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": { "type": "string", "description": "Hedef pencere adresi." },
                    "region": { "type": "array", "items": { "type": "integer" }, "description": "[x, y, w, h] kırpma bölgesi." },
                    "zoom": { "type": "array", "items": { "type": "integer" }, "description": "[x, y] noktası etrafında hassas yakınlaştırma." },
                    "marks": { "type": "boolean", "description": "Tıklanabilir bölgelerin üzerine numaralı Set-of-Marks kutuları çizer." },
                    "include_base64": { "type": "boolean", "description": "Görüntüyü base64 olarak MCP yanıtına ekler." }
                }
            }
        },
        {
            "name": "pc_eye_xray",
            "description": "Bir pencerenin veya PID'nin arkasındaki tüm süreç soy ağacını (AppCluster), açık dosyalarını (fd), aktif TCP/UDP ağ bağlantılarını, p50/p95/p99 bellek/CPU geçmişini ve son hata loglarını röntgenler.",
            "annotations": { "readOnlyHint": true },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": { "type": "string", "description": "Pencere adresi." },
                    "pid": { "type": "integer", "description": "Kök süreç PID numarası." }
                }
            }
        },
        {
            "name": "pc_eye_logs",
            "description": "Sistem/kullanıcı journald loglarını, başlatılan uygulamaların stdout/stderr kayıtlarını ve masaüstü bildirimlerini (dunst) birleştirerek döner.",
            "annotations": { "readOnlyHint": true },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pid": { "type": "integer", "description": "Belirli bir PID için filtrele." },
                    "limit": { "type": "integer", "description": "Maksimum satır sayısı (varsayılan 30)." }
                }
            }
        },
        {
            "name": "pc_eye_audio",
            "description": "PipeWire ses grafiğini, gerçek fiziksel PCM ses sinyalini (RMS/Peak dBFS), D-Bus MPRIS medya/tarayıcı URL durumunu okur, masaüstü sesini WAV olarak kaydeder (op=record) veya ses/medya kontrolü (play/pause/volume/mute) yapar.",
            "annotations": { "readOnlyHint": false },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "op": { "type": "string", "description": "Boş bırakılırsa rapor döner. Kontrol için: play | pause | play-pause | next | previous | stop | set-volume | mute | unmute | toggle-mute | record" },
                    "arg": { "type": "string", "description": "set-volume için seviye (ör. 0.5), record için süre ms (ör. 2000) veya medya oynatıcı filtresi (ör. firefox)." }
                }
            }
        },
        {
            "name": "pc_eye_click_ui",
            "description": "Piksel tahmini yapmadan doğrudan UI elemanının adına veya Set-of-Marks numarasına tıklar (AT-SPI2 ve otomatik bellek-içi OCR destekli), D-Bus DoAction tetikler veya içine metin yazar.",
            "annotations": { "readOnlyHint": false, "destructiveHint": true },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": { "type": "string" },
                    "name": { "type": "string", "description": "Tıklanacak kontrolün veya ekrandaki OCR metninin adı." },
                    "mark": { "type": "integer", "description": "Set-of-Marks numarası." },
                    "button": { "type": "string", "description": "left | right | middle" },
                    "prefer_dbus": { "type": "boolean", "description": "Fareyi hareket ettirmeden doğrudan D-Bus DoAction(0) kullan." },
                    "set_text": { "type": "string", "description": "Kontrol bir metin kutusuysa içine bu metni yaz." },
                    "then": { "type": "string", "description": "context | ui | vision | xray" }
                }
            }
        },
        {
            "name": "pc_eye_input",
            "description": "Wayland yerel sanal işaretçi ve dinamik XKB sanal klavye üzerinden fare tıklaması, taşıma, sürükleme, tekerlek kaydırma, Unicode metin yazma ve kısayol gönderme.",
            "annotations": { "readOnlyHint": false, "destructiveHint": true },
            "inputSchema": {
                "type": "object",
                "required": ["op"],
                "properties": {
                    "op": { "type": "string", "description": "click | move | drag | scroll | type | key" },
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "x2": { "type": "integer" },
                    "y2": { "type": "integer" },
                    "dx": { "type": "integer" },
                    "dy": { "type": "integer" },
                    "button": { "type": "string" },
                    "clicks": { "type": "integer" },
                    "text": { "type": "string" },
                    "keys": { "type": "string" },
                    "window": { "type": "string" },
                    "then": { "type": "string", "description": "context | ui | vision | xray" }
                }
            }
        },
        {
            "name": "pc_eye_window",
            "description": "Hyprland IPC üzerinden pencere odaklama, kapatma, çalışma alanı değiştirme, taşıma ve boyutlandırma.",
            "annotations": { "readOnlyHint": false },
            "inputSchema": {
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": { "type": "string", "description": "focus | close | workspace | movetoworkspace | fullscreen | float | move | resize. fullscreen: window verilirse o pencereye uygulanır; arg on | max | off | toggle" },
                    "window": { "type": "string" },
                    "arg": { "type": "string" },
                    "then": { "type": "string" }
                }
            }
        },
        {
            "name": "pc_eye_display",
            "description": "Sanal (headless) ekran yönetimi: list | create | mode | remove. create kullanıcının ekranlarından yalıtılmış bir çıkış açar ve workspace_id döner; uygulamayı pc_eye_launch(workspace=...) ile oraya gönderip kullanıcının ekranına dokunmadan gözlemle/kontrol et.",
            "annotations": { "readOnlyHint": false },
            "inputSchema": {
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": { "type": "string", "description": "list | create | mode | remove" },
                    "name": { "type": "string", "description": "Çıkış adı (create'te isteğe bağlı)" },
                    "mode": { "type": "string", "description": "WxH veya WxH@R, ör. 1920x1080@60" },
                    "position": { "type": "string", "description": "X,Y veya isolated (varsayılan)" },
                    "scale": { "type": "number" },
                    "then": { "type": "string" }
                }
            }
        },
        {
            "name": "pc_eye_launch",
            "description": "Uygulama başlatır, stdout/stderr akışını yakalar ve Hyprland openwindow olayını bekleyip yeni pencere adresini döner.",
            "annotations": { "readOnlyHint": false, "destructiveHint": true },
            "inputSchema": {
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": { "type": "array", "items": { "type": "string" } },
                    "workspace": { "type": "string" },
                    "timeout_ms": { "type": "integer" },
                    "then": { "type": "string" }
                }
            }
        },
        {
            "name": "pc_eye_wait",
            "description": "Çok-modlu deterministik senkronizasyon bariyeri: visual_settle (ekran durulması), log_match (logda metin), ui_match (UI veya OCR metni belirmesi), port_open (TCP port açılması) veya Hyprland soket olaylarını bekler.",
            "annotations": { "readOnlyHint": true },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "event": { "type": "string", "description": "visual_settle | log_match | ui_match | port_open | openwindow | closewindow | activewindow | workspace" },
                    "match": { "type": "string", "description": "Aranan metin, UI/OCR eleman adı veya TCP port numarası." },
                    "window": { "type": "string", "description": "visual_settle veya ui_match için hedef pencere adresi." },
                    "timeout_ms": { "type": "integer" }
                }
            }
        },
        {
            "name": "pc_eye_director",
            "description": "Watchdog korumalı süreç dondurma/çözme (freeze/thaw), çok-adımlı atomik sequence yürütme, Hyprland kısayollarını listeleme/çalıştırma (binds/use_bind) ve zengin çok-formatlı Wayland pano (clipboard: MIME, source_url, cliphist geçmişi, görsel/dosya kopyalama) yönetimi.",
            "annotations": { "readOnlyHint": false, "destructiveHint": true },
            "inputSchema": {
                "type": "object",
                "required": ["op"],
                "properties": {
                    "op": { "type": "string", "description": "freeze | thaw | clipboard | binds | use_bind | sequence" },
                    "pid": { "type": "integer" },
                    "max_freeze_ms": { "type": "integer" },
                    "text": { "type": "string" },
                    "file": { "type": "string", "description": "clipboard için panoya kopyalanacak görsel (PNG/JPG) veya dosya yolu." },
                    "restore_id": { "type": "string", "description": "clipboard için cliphist geçmişinden geri yüklenecek ID." },
                    "steps": { "type": "array", "items": { "type": "object" } },
                    "then": { "type": "string" }
                }
            }
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
