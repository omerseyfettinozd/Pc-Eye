use crate::daemon::ring_buffer::now_ms;
use crate::protocol::types::{
    CompositorEvent, DesktopState, KeybindInfo, LayerSurfaceInfo, MonitorInfo, WindowInfo,
    WorkspaceInfo,
};
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

const AUTH_KEYWORDS: &[&str] = &[
    "polkit",
    "pinentry",
    "hyprlock",
    "swaylock",
    "gtklock",
    "gcr-prompter",
    "lxpolkit",
    "xfce-polkit",
    "kde-polkit",
];

const PROTECTED_WINDOW_KEYWORDS: &[&str] = &[
    "antigravity",
    "pc-eye",
    "1password",
    "bitwarden",
    "keepassxc",
];

/// Hyprland `.socket.sock` veya `.socket2.sock` yolunu bulur.
pub fn resolve_hyprland_socket(socket_name: &str) -> Result<PathBuf> {
    let runtime_dir = env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".to_string());
    if let Ok(sig) = env::var("HYPRLAND_INSTANCE_SIGNATURE") {
        let p = PathBuf::from(&runtime_dir)
            .join("hypr")
            .join(&sig)
            .join(socket_name);
        if p.exists() {
            return Ok(p);
        }
    }
    // Ajan oturumunda başka bir örneğe düşmek, ajanın kullanıcının masaüstünü sürmesi demektir:
    // iç Hyprland kapanınca ajan kullanıcının pencerelerini görmeye başlamıştı (paralel ölçüm 125142).
    if let Some(name) = crate::session::active_name() {
        return Err(anyhow!("ajan oturumu `{}` kapalı: Hyprland soketi yok ({})", name, socket_name));
    }

    let hypr_dir = PathBuf::from(&runtime_dir).join("hypr");
    if hypr_dir.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&hypr_dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.join(socket_name).exists())
            .collect();
        entries.sort();
        if let Some(last) = entries.pop() {
            return Ok(last.join(socket_name));
        }
    }

    Err(anyhow!(
        "Hyprland UNIX soketi bulunamadı ({}/hypr/*/{})",
        runtime_dir,
        socket_name
    ))
}

/// Hyprland `.socket.sock` üzerinden süreç başlatmadan (<0.3ms) ham komut/sorgu çalıştırır.
pub fn hypr_request_raw(command: &str) -> Result<String> {
    let sock_path = resolve_hyprland_socket(".socket.sock")?;
    let mut stream = UnixStream::connect(&sock_path)
        .with_context(|| format!("Hyprland soketine bağlanılamadı: {}", sock_path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(command.as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

/// Hyprland `.socket.sock` üzerinden JSON sorgusu (`j/<endpoint>`) çalıştırır.
pub fn hypr_query_json(endpoint: &str) -> Result<Value> {
    let cmd = format!("j/{}", endpoint.trim_start_matches("j/"));
    let raw = hypr_request_raw(&cmd)?;
    let val: Value = serde_json::from_str(&raw)
        .with_context(|| format!("Hyprland JSON ayrıştırma hatası ({}): {}", cmd, raw))?;
    Ok(val)
}

/// Hyprland `dispatch` komutunu ham metinle gönderir. Yanıt yalnızca `ok` ise başarıdır;
/// `error:`/`warning:` gibi her şey (Lua tarafı pencere bulunamadı uyarıları dahil) hata sayılır.
pub fn hypr_dispatch(args: &str) -> Result<String> {
    let cmd = format!("dispatch {}", args.trim());
    let resp = hypr_request_raw(&cmd)?;
    if resp.trim() == "ok" {
        Ok("ok".to_string())
    } else {
        Err(anyhow!(
            "Hyprland dispatch hatası (`{}`): {}",
            cmd,
            if resp.trim().is_empty() { "(boş yanıt)" } else { resp.trim() }
        ))
    }
}

/// Yapılandırılmış Hyprland dispatch isteği. Hyprland >= 0.55 Lua tabanlı (`hl.dsp.*`),
/// eski sürümler `movecursor X Y` biçimini kullanır; her iki sözdizimi de burada üretilir.
#[derive(Debug, Clone)]
pub enum HyprDispatch {
    CursorMove { x: i64, y: i64 },
    FocusWindow(String),
    FocusWorkspace(String),
    CloseWindow(String),
    MoveToWorkspaceSilent { workspace: String, address: String },
    ToggleFloat(Option<String>),
    /// `maximized=false` gerçek tam ekran, `true` yalnız büyütme. `state`: `None` geçiş (toggle),
    /// `Some(true)` kur, `Some(false)` kaldır. Hedef pencere yoksa aktif pencere.
    Fullscreen { maximized: bool, state: Option<bool>, window: Option<String> },
    /// `X Y` mutlak konum; hedef pencere adresi (yoksa aktif pencere).
    MoveExact { coords: String, window: Option<String> },
    /// `W H` mutlak boyut; hedef pencere adresi (yoksa aktif pencere).
    ResizeExact { coords: String, window: Option<String> },
    /// Komutu Hyprland üzerinden başlatır; pencere odak çalmadan verilen çalışma alanında açılır.
    ExecSilent { command: String, workspace: String },
}

fn lua_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Çalışma alanı kimliği sayıysa Lua sayı, değilse (isim, `e+1`, `special:x`) metin olarak yazılır.
fn lua_ws(ws: &str) -> String {
    match ws.trim().parse::<i64>() {
        Ok(n) => n.to_string(),
        Err(_) => lua_str(ws.trim()),
    }
}

fn two_nums(s: &str) -> Option<(i64, i64)> {
    let mut it = s.split_whitespace();
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    Some((a, b))
}

impl HyprDispatch {
    /// Hyprland 0.55+ Lua dispatcher ifadesi.
    pub fn lua(&self) -> String {
        let win = |a: &str| lua_str(&format!("address:{}", a));
        match self {
            Self::CursorMove { x, y } => format!("hl.dsp.cursor.move({{x={}, y={}}})", x, y),
            Self::FocusWindow(a) => format!("hl.dsp.focus({{window={}}})", win(a)),
            Self::FocusWorkspace(w) => format!("hl.dsp.focus({{workspace={}}})", lua_ws(w)),
            Self::CloseWindow(a) => format!("hl.dsp.window.close({{window={}}})", win(a)),
            Self::MoveToWorkspaceSilent { workspace, address } => format!(
                "hl.dsp.window.move({{workspace={}, follow=false, window={}}})",
                lua_ws(workspace),
                win(address)
            ),
            Self::ToggleFloat(None) => "hl.dsp.window.float({action=\"toggle\"})".to_string(),
            Self::ToggleFloat(Some(a)) => {
                format!("hl.dsp.window.float({{action=\"toggle\", window={}}})", win(a))
            }
            Self::Fullscreen { maximized, state, window } => {
                let mut fields = vec![format!(
                    "mode=\"{}\"",
                    if *maximized { "maximized" } else { "fullscreen" }
                )];
                if let Some(on) = state {
                    fields.push(format!("action=\"{}\"", if *on { "set" } else { "unset" }));
                }
                if let Some(a) = window {
                    fields.push(format!("window={}", win(a)));
                }
                format!("hl.dsp.window.fullscreen({{{}}})", fields.join(", "))
            }
            Self::MoveExact { coords, window } => {
                let w = window.as_deref().map(|a| format!(", window={}", win(a))).unwrap_or_default();
                match two_nums(coords) {
                    Some((x, y)) => format!("hl.dsp.window.move({{x={}, y={}{}}})", x, y, w),
                    None => format!("hl.dsp.window.move({{x={}{}}})", lua_str(coords), w),
                }
            }
            Self::ResizeExact { coords, window } => {
                let w = window.as_deref().map(|a| format!(", window={}", win(a))).unwrap_or_default();
                match two_nums(coords) {
                    Some((x, y)) => format!("hl.dsp.window.resize({{x={}, y={}{}}})", x, y, w),
                    None => format!("hl.dsp.window.resize({{x={}{}}})", lua_str(coords), w),
                }
            }
            Self::ExecSilent { command, workspace } => format!(
                "hl.dsp.exec_cmd({}, {{ workspace = {} }})",
                lua_str(command),
                lua_str(&format!("{} silent", workspace))
            ),
        }
    }

    /// Klasik sözdizimi bu isteği aynı anlamla ifade edebiliyor mu? Hedefli/kur-kaldır fullscreen
    /// edemez; oraya düşmek isteği yanlışlıkla kullanıcının aktif penceresine uygular.
    pub fn legacy_equivalent(&self) -> bool {
        !matches!(self, Self::Fullscreen { window: Some(_), .. } | Self::Fullscreen { state: Some(_), .. })
    }

    /// Hyprland < 0.55 klasik dispatcher sözdizimi.
    pub fn legacy(&self) -> String {
        match self {
            Self::CursorMove { x, y } => format!("movecursor {} {}", x, y),
            Self::FocusWindow(a) => format!("focuswindow address:{}", a),
            Self::FocusWorkspace(w) => format!("workspace {}", w),
            Self::CloseWindow(a) => format!("closewindow address:{}", a),
            Self::MoveToWorkspaceSilent { workspace, address } => {
                format!("movetoworkspacesilent {},address:{}", workspace, address)
            }
            Self::ToggleFloat(None) => "togglefloating".to_string(),
            Self::ToggleFloat(Some(a)) => format!("togglefloating address:{}", a),
            // Klasik sözdizimi hedef pencere/kur-kaldır desteklemez: aktif pencerede geçiş yapar.
            Self::Fullscreen { maximized, .. } => {
                format!("fullscreen {}", if *maximized { 1 } else { 0 })
            }
            Self::MoveExact { coords, window: None } => format!("moveactive exact {}", coords),
            Self::MoveExact { coords, window: Some(a) } => {
                format!("movewindowpixel exact {},address:{}", coords, a)
            }
            Self::ResizeExact { coords, window: None } => format!("resizeactive exact {}", coords),
            Self::ResizeExact { coords, window: Some(a) } => {
                format!("resizewindowpixel exact {},address:{}", coords, a)
            }
            Self::ExecSilent { command, workspace } => format!("exec [workspace {} silent] {}", workspace, command),
        }
    }
}

/// 0 = bilinmiyor, 1 = Lua sözdizimi çalıştı, 2 = klasik sözdizimi çalıştı.
static DISPATCH_MODE: AtomicU8 = AtomicU8::new(0);

/// Tek merkezî dispatch noktası: önce bilinen/çalışan sözdizimini dener, hata olursa diğerine düşer.
/// Başarılı sözdizimi sonraki çağrılar için hatırlanır. Her iki yol da başarısızsa ilk hata döner.
pub fn hypr_dispatch_cmd(d: &HyprDispatch) -> Result<String> {
    let attempts: [(u8, String); 2] = if DISPATCH_MODE.load(Ordering::Relaxed) == 2 {
        [(2, d.legacy()), (1, d.lua())]
    } else {
        [(1, d.lua()), (2, d.legacy())]
    };
    let mut first_err = None;
    for (mode, cmd) in attempts {
        if mode == 2 && !d.legacy_equivalent() {
            continue;
        }
        match hypr_dispatch(&cmd) {
            Ok(r) => {
                DISPATCH_MODE.store(mode, Ordering::Relaxed);
                return Ok(r);
            }
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    Err(first_err.unwrap())
}

pub fn is_auth_surface(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    AUTH_KEYWORDS.iter().any(|k| lower.contains(k))
}

pub fn is_protected_window(class: &str, title: &str) -> bool {
    let c = class.to_ascii_lowercase();
    let t = title.to_ascii_lowercase();
    PROTECTED_WINDOW_KEYWORDS
        .iter()
        .any(|k| c.contains(k) || t.contains(k))
        || is_auth_surface(class)
        || is_auth_surface(title)
}

/// `0x…` ya da en az 8 haneli onaltılık dizgi adres sayılır; "cafe" gibi kısa başlıklar sayılmaz.
fn looks_like_window_address(s: &str) -> bool {
    let t = s.trim().to_ascii_lowercase();
    let hex = t.strip_prefix("0x").unwrap_or(&t);
    !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()) && (t.starts_with("0x") || hex.len() >= 8)
}

fn hex_address(s: &str) -> String {
    let t = s.trim().to_ascii_lowercase();
    if t.starts_with("0x") { t } else { format!("0x{}", t) }
}

fn describe_windows<'a>(windows: impl Iterator<Item = &'a WindowInfo>) -> String {
    windows
        .take(8)
        .map(|w| format!("{} «{}» ({})", w.address, w.title, w.class))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Pencere referansını adrese çözer: adres, tam başlık, başlık parçası, sınıf (bu sırayla).
/// Ajanlar başlıkla sormayı doğal bulur; belirsizlikte tahmin etmek yerine adayları listeler.
pub fn resolve_window_ref(reference: &str, windows: &[WindowInfo]) -> Result<String> {
    if looks_like_window_address(reference) {
        let norm = hex_address(reference);
        return if windows.iter().any(|w| w.address == norm) {
            Ok(norm)
        } else {
            Err(anyhow!(
                "Pencere bulunamadı: {} (açık pencereler: {})",
                norm,
                describe_windows(windows.iter())
            ))
        };
    }
    let q = reference.trim().to_lowercase();
    let tiers: [&dyn Fn(&WindowInfo) -> bool; 4] = [
        &|w| w.title.to_lowercase() == q,
        &|w| w.title.to_lowercase().contains(&q),
        &|w| w.class.to_lowercase() == q,
        &|w| w.class.to_lowercase().contains(&q),
    ];
    for tier in tiers {
        let hits: Vec<&WindowInfo> = windows.iter().filter(|w| tier(w)).collect();
        match hits.len() {
            0 => continue,
            1 => return Ok(hits[0].address.clone()),
            _ => {
                return Err(anyhow!(
                    "`{}` birden çok pencereyle eşleşti, adres ver: {}",
                    reference,
                    describe_windows(hits.into_iter())
                ))
            }
        }
    }
    Err(anyhow!(
        "`{}` adres, başlık veya sınıfla eşleşen pencere yok (açık pencereler: {})",
        reference,
        describe_windows(windows.iter())
    ))
}

/// Pencere referansını adrese çevirir; başlık/sınıf verildiyse Hyprland'den çözer.
/// Çözülemezse girdiyi olduğu gibi döner, böylece çağıranın "bulunamadı" hatası onu gösterir.
pub fn normalize_window_address(addr: &str) -> String {
    if looks_like_window_address(addr) {
        return hex_address(addr);
    }
    capture_desktop_state()
        .ok()
        .and_then(|d| resolve_window_ref(addr, &d.windows).ok())
        .unwrap_or_else(|| addr.trim().to_string())
}

/// Hyprland `j/binds` üzerinden kullanıcının tanımladığı tüm kısayolları çözümler.
pub fn list_hyprland_binds() -> Result<Vec<KeybindInfo>> {
    let val = hypr_query_json("binds")?;
    let mut binds = Vec::new();
    if let Some(arr) = val.as_array() {
        for b in arr {
            let modmask = b.get("modmask").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let key = b
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if key.is_empty() {
                continue;
            }
            let dispatcher = b
                .get("dispatcher")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let arg = b
                .get("arg")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let description = b
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let submap = b
                .get("submap")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let combo = format_modmask_combo(modmask, &key);
            binds.push(KeybindInfo {
                combo,
                modmask,
                key,
                dispatcher,
                arg,
                description,
                submap,
            });
        }
    }
    Ok(binds)
}

pub fn format_modmask_combo(modmask: u32, key: &str) -> String {
    let mut parts = Vec::new();
    if modmask & 64 != 0 {
        parts.push("SUPER");
    }
    if modmask & 4 != 0 {
        parts.push("CTRL");
    }
    if modmask & 8 != 0 {
        parts.push("ALT");
    }
    if modmask & 1 != 0 {
        parts.push("SHIFT");
    }
    parts.push(key);
    parts.join("+")
}

/// Göz 1: Tüm masaüstü, monitör, çalışma alanı, pencere ve katman durumunu tek seferde toplar.
pub fn capture_desktop_state() -> Result<DesktopState> {
    let ts = now_ms();

    let cursor_val = hypr_query_json("cursorpos").unwrap_or(Value::Null);
    let cursor_x = cursor_val.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
    let cursor_y = cursor_val.get("y").and_then(|v| v.as_i64()).unwrap_or(0);

    let active_val = hypr_query_json("activewindow").unwrap_or(Value::Null);
    let active_window_address = active_val
        .get("address")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "0x0")
        .map(normalize_window_address);
    let active_window_class = active_val
        .get("class")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let active_window_title = active_val
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let active_window_pid = active_val
        .get("pid")
        .and_then(|v| v.as_i64())
        .filter(|&p| p > 0)
        .map(|p| p as i32);

    let monitors_val = hypr_query_json("monitors").unwrap_or(Value::Array(vec![]));
    let mut monitors = Vec::new();
    if let Some(arr) = monitors_val.as_array() {
        for m in arr {
            monitors.push(MonitorInfo {
                id: m.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
                name: m
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                width: m.get("width").and_then(|v| v.as_i64()).unwrap_or(0),
                height: m.get("height").and_then(|v| v.as_i64()).unwrap_or(0),
                x: m.get("x").and_then(|v| v.as_i64()).unwrap_or(0),
                y: m.get("y").and_then(|v| v.as_i64()).unwrap_or(0),
                scale: m.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0),
                active_workspace_id: m
                    .get("activeWorkspace")
                    .and_then(|w| w.get("id"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(1),
                active_workspace_name: m
                    .get("activeWorkspace")
                    .and_then(|w| w.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("1")
                    .to_string(),
                focused: m.get("focused").and_then(|v| v.as_bool()).unwrap_or(false),
            });
        }
    }

    let workspaces_val = hypr_query_json("workspaces").unwrap_or(Value::Array(vec![]));
    let mut workspaces = Vec::new();
    if let Some(arr) = workspaces_val.as_array() {
        for w in arr {
            workspaces.push(WorkspaceInfo {
                id: w.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
                name: w
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                monitor: w
                    .get("monitor")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                windows: w.get("windows").and_then(|v| v.as_i64()).unwrap_or(0),
                has_fullscreen: w
                    .get("hasfullscreen")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                last_window: w
                    .get("lastwindow")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                last_window_title: w
                    .get("lastwindowtitle")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    workspaces.sort_by_key(|w| w.id);

    let clients_val = hypr_query_json("clients").unwrap_or(Value::Array(vec![]));
    let mut windows = Vec::new();
    let mut auth_guard_tripped = false;
    let mut auth_guard_reason = None;

    if let Some(arr) = clients_val.as_array() {
        for c in arr {
            let addr = c
                .get("address")
                .and_then(|v| v.as_str())
                .map(normalize_window_address)
                .unwrap_or_default();
            let class = c
                .get("class")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let title = c
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let pid = c.get("pid").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
            let at = c.get("at").and_then(|v| v.as_array());
            let size = c.get("size").and_then(|v| v.as_array());
            let x = at
                .and_then(|a| a.first())
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let y = at
                .and_then(|a| a.get(1))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let width = size
                .and_then(|s| s.first())
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let height = size
                .and_then(|s| s.get(1))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);

            let mapped = c.get("mapped").and_then(|v| v.as_bool()).unwrap_or(true);
            let hidden = c.get("hidden").and_then(|v| v.as_bool()).unwrap_or(false);
            let focused = active_window_address.as_deref() == Some(addr.as_str());
            let protected = is_protected_window(&class, &title);

            if mapped && !hidden && (is_auth_surface(&class) || is_auth_surface(&title)) {
                auth_guard_tripped = true;
                auth_guard_reason = Some(format!(
                    "Kimlik doğrulama / güvenlik penceresi aktif: {} ({})",
                    class, title
                ));
            }

            windows.push(WindowInfo {
                address: addr,
                class,
                title,
                pid,
                workspace_id: c
                    .get("workspace")
                    .and_then(|w| w.get("id"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
                workspace_name: c
                    .get("workspace")
                    .and_then(|w| w.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                monitor: c.get("monitor").and_then(|v| v.as_i64()).unwrap_or(0),
                x,
                y,
                width,
                height,
                floating: c.get("floating").and_then(|v| v.as_bool()).unwrap_or(false),
                fullscreen: c.get("fullscreen").and_then(|v| v.as_i64()).unwrap_or(0),
                xwayland: c.get("xwayland").and_then(|v| v.as_bool()).unwrap_or(false),
                mapped,
                hidden,
                focused,
                protected_window: protected,
            });
        }
    }

    let layers_val = hypr_query_json("layers").unwrap_or(Value::Null);
    let mut layers = Vec::new();
    if let Some(mon_map) = layers_val.as_object() {
        for (mon_name, mon_obj) in mon_map {
            if let Some(levels) = mon_obj.get("levels").and_then(|l| l.as_object()) {
                for (lvl, arr_val) in levels {
                    if let Some(arr) = arr_val.as_array() {
                        for item in arr {
                            let ns = item
                                .get("namespace")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let is_auth = is_auth_surface(&ns) || lvl == "3";
                            if is_auth_surface(&ns) {
                                auth_guard_tripped = true;
                                auth_guard_reason =
                                    Some(format!("Güvenlik/kilit katmanı aktif: {}", ns));
                            }
                            layers.push(LayerSurfaceInfo {
                                monitor: mon_name.clone(),
                                level: lvl.clone(),
                                address: item
                                    .get("address")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                namespace: ns,
                                x: item.get("x").and_then(|v| v.as_i64()).unwrap_or(0),
                                y: item.get("y").and_then(|v| v.as_i64()).unwrap_or(0),
                                width: item.get("w").and_then(|v| v.as_i64()).unwrap_or(0),
                                height: item.get("h").and_then(|v| v.as_i64()).unwrap_or(0),
                                pid: item.get("pid").and_then(|v| v.as_i64()).unwrap_or(-1) as i32,
                                is_auth_or_lock: is_auth,
                            });
                        }
                    }
                }
            }
        }
    }

    Ok(DesktopState {
        timestamp_ms: ts,
        cursor_x,
        cursor_y,
        active_window_address,
        active_window_class,
        active_window_title,
        active_window_pid,
        auth_guard_tripped,
        auth_guard_reason,
        monitors,
        workspaces,
        windows,
        layers,
    })
}

/// Hyprland `.socket2.sock` üzerinden belirli bir olayın gerçekleşmesini deterministik olarak bekler.
pub fn wait_for_compositor_event(
    event_prefix: &str,
    substring_match: Option<&str>,
    timeout_ms: u64,
) -> Result<CompositorEvent> {
    let sock_path = resolve_hyprland_socket(".socket2.sock")?;
    let stream = UnixStream::connect(&sock_path)?;
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    let mut reader = BufReader::new(stream);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    let mut line = String::new();
    while Instant::now() < deadline {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if let Some((kind, payload)) = trimmed.split_once(">>") {
                    let kind_matches = event_prefix.is_empty()
                        || event_prefix == "*"
                        || kind.eq_ignore_ascii_case(event_prefix);
                    let payload_matches = substring_match
                        .map(|m| {
                            payload
                                .to_ascii_lowercase()
                                .contains(&m.to_ascii_lowercase())
                        })
                        .unwrap_or(true);
                    if kind_matches && payload_matches {
                        return Ok(CompositorEvent {
                            kind: kind.to_string(),
                            payload: payload.to_string(),
                        });
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }

    Err(anyhow!(
        "Zaman aşımı ({} ms): beklenen Hyprland olayı (`{}`) gerçekleşmedi",
        timeout_ms,
        event_prefix
    ))
}

/// Odaktaki pencere bir XWayland istemcisi mi? (`activewindow` -> `"xwayland": true`)
pub fn active_window_is_xwayland() -> bool {
    hypr_query_json("activewindow")
        .ok()
        .and_then(|v| v.get("xwayland").and_then(|x| x.as_bool()))
        .unwrap_or(false)
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;

    #[test]
    fn lua_and_legacy_syntax() {
        let d = HyprDispatch::CursorMove { x: 3, y: 4 };
        assert_eq!(d.lua(), "hl.dsp.cursor.move({x=3, y=4})");
        assert_eq!(d.legacy(), "movecursor 3 4");
        let m = HyprDispatch::MoveToWorkspaceSilent { workspace: "2".into(), address: "0xab".into() };
        assert_eq!(m.lua(), "hl.dsp.window.move({workspace=2, follow=false, window=\"address:0xab\"})");
        assert_eq!(m.legacy(), "movetoworkspacesilent 2,address:0xab");
        assert_eq!(HyprDispatch::FocusWindow("0xab".into()).lua(), "hl.dsp.focus({window=\"address:0xab\"})");
        assert_eq!(HyprDispatch::FocusWorkspace("e+1".into()).lua(), "hl.dsp.focus({workspace=\"e+1\"})");
        let f = HyprDispatch::Fullscreen { maximized: false, state: Some(true), window: Some("0xab".into()) };
        assert_eq!(
            f.lua(),
            "hl.dsp.window.fullscreen({mode=\"fullscreen\", action=\"set\", window=\"address:0xab\"})"
        );
        // Hedefli istek klasik sözdizimine düşmemeli: o yol aktif pencereye uygulanır.
        assert!(!f.legacy_equivalent());
        let t = HyprDispatch::Fullscreen { maximized: false, state: None, window: None };
        assert_eq!(t.lua(), "hl.dsp.window.fullscreen({mode=\"fullscreen\"})");
        assert!(t.legacy_equivalent());
    }
}

#[cfg(test)]
mod window_ref_tests {
    use super::*;

    fn win(address: &str, title: &str, class: &str) -> WindowInfo {
        WindowInfo {
            address: address.to_string(),
            class: class.to_string(),
            title: title.to_string(),
            pid: 1,
            workspace_id: 1,
            workspace_name: "1".to_string(),
            monitor: 0,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            floating: false,
            fullscreen: 0,
            xwayland: false,
            mapped: true,
            hidden: false,
            focused: false,
            protected_window: false,
        }
    }

    fn sample() -> Vec<WindowInfo> {
        vec![
            win("0x55aa00000001", "Eval Düğmeler LKJG", "eval_app.py"),
            win("0x55aa00000002", "Mozilla Firefox", "firefox"),
            win("0x55aa00000003", "Firefox — Ayarlar", "firefox"),
            win("0x55aa00000004", "cafe", "kitty"),
        ]
    }

    #[test]
    fn resolves_address_title_and_class_in_order() {
        let w = sample();
        assert_eq!(resolve_window_ref("0x55AA00000002", &w).unwrap(), "0x55aa00000002");
        assert_eq!(resolve_window_ref("55aa00000002", &w).unwrap(), "0x55aa00000002");
        assert_eq!(resolve_window_ref("eval düğmeler lkjg", &w).unwrap(), "0x55aa00000001");
        assert_eq!(resolve_window_ref("Ayarlar", &w).unwrap(), "0x55aa00000003");
        // Tam başlık eşleşmesi, parça eşleşmelerinden önce gelir.
        assert_eq!(resolve_window_ref("Mozilla Firefox", &w).unwrap(), "0x55aa00000002");
        // Kısa onaltılık görünümlü başlık adres sanılmaz.
        assert_eq!(resolve_window_ref("cafe", &w).unwrap(), "0x55aa00000004");
    }

    #[test]
    fn ambiguous_or_missing_lists_candidates() {
        let w = sample();
        let err = resolve_window_ref("firefox", &w).unwrap_err().to_string();
        assert!(err.contains("birden çok") && err.contains("0x55aa00000003"), "{}", err);
        let err = resolve_window_ref("yok böyle", &w).unwrap_err().to_string();
        assert!(err.contains("Eval Düğmeler"), "{}", err);
        assert!(resolve_window_ref("0xdeadbeef", &w).is_err());
    }
}
