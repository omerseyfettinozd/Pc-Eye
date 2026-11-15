//! pc-eye-wm arka ucu (F7): ajan oturumu Hyprland yerine pc-eye-wm ile açıldığında
//! (`pc-eye session start <ad> --backend wm`) pc-eye'in kompozitörle konuştuğu tek noktalar
//! buraya yönlenir:
//!
//! - `hypr_request_raw`: Hyprland IPC metin istekleri (`j/clients`, `dispatch ...`, Lua `hl.dsp.*`)
//!   wm'in JSON IPC'sinden Hyprland biçiminde yanıtlanır; ~80 çağrı noktası değişmeden çalışır.
//! - Sanal girdi (`WaylandVirtualInput`): olaylar sanal fare/klavye protokolü yerine doğrudan
//!   wm'e gider; wm onları hedef yüzeye kaynağında verir.
//! - Ekran görüntüsü (grim): wm, istemci tamponlarından bileştirip PPM yazar (ekran yakalama yok).
//!
//! Etkinlik ölçütü `PC_EYE_WM_IPC` ortam değişkeni (oturum `activate` ile kurar).

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub const IPC_VAR: &str = "PC_EYE_WM_IPC";

/// Hyprland `.socket2.sock` biçimli olay akışı (IPC soketiyle aynı dizinde).
pub fn events_path() -> Option<std::path::PathBuf> {
    let ipc = std::env::var(IPC_VAR).ok()?;
    Some(std::path::Path::new(&ipc).parent()?.join("wm-events.sock"))
}

pub fn active() -> bool {
    std::env::var(IPC_VAR).map_or(false, |p| !p.is_empty())
}

/// wm IPC'ye tek istek (satır JSON). `wait_idle` gibi geç yanıtlar için süre isteğe göre uzar.
/// `state` yanıtı 10 ms önbellekte tutulur: Hyprland taklidi tek bir tıklamada wm'e 60–80 kez
/// "durum" soruyordu. Durum değiştirebilecek her istek (eylem, açma, kapama) önbelleği siler.
static STATE_CACHE: std::sync::Mutex<Option<(std::time::Instant, Value)>> = std::sync::Mutex::new(None);
const STATE_CACHE_MS: u128 = 10;

pub fn request(req: Value) -> Result<Value> {
    let op = req["op"].as_str().unwrap_or("");
    if op == "state" {
        if let Ok(g) = STATE_CACHE.lock() {
            if let Some((t, v)) = g.as_ref() {
                if t.elapsed().as_millis() < STATE_CACHE_MS {
                    return Ok(v.clone());
                }
            }
        }
    } else if !matches!(op, "capture" | "capture_region" | "wait_idle") {
        if let Ok(mut g) = STATE_CACHE.lock() {
            *g = None;
        }
    }
    let t0 = std::time::Instant::now();
    let r = request_inner(&req);
    if op == "state" {
        if let (Ok(v), Ok(mut g)) = (&r, STATE_CACHE.lock()) {
            *g = Some((std::time::Instant::now(), v.clone()));
        }
    }
    if std::env::var_os("PCEYE_WM_TRACE").is_some() {
        eprintln!("[wm] {:>6.1} ms {}", t0.elapsed().as_secs_f64() * 1000.0, req["op"].as_str().unwrap_or("?"));
    }
    r
}

fn request_inner(req: &Value) -> Result<Value> {
    let path = std::env::var(IPC_VAR).map_err(|_| anyhow!("pc-eye-wm oturumu etkin değil"))?;
    let mut stream =
        UnixStream::connect(&path).with_context(|| format!("pc-eye-wm IPC'ye bağlanılamadı: {}", path))?;
    let wait_ms = req.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(0);
    stream.set_read_timeout(Some(Duration::from_millis(5_000 + wait_ms)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut resp = String::new();
    BufReader::new(stream).read_line(&mut resp)?;
    let v: Value = serde_json::from_str(resp.trim()).with_context(|| format!("pc-eye-wm yanıtı: {}", resp))?;
    if v.get("ok").and_then(|o| o.as_bool()) == Some(false) {
        return Err(anyhow!("pc-eye-wm: {}", v.get("error").and_then(|e| e.as_str()).unwrap_or("hata")));
    }
    Ok(v)
}

/// wm pencere kimliği ↔ Hyprland biçimli adres.
pub fn address_of(id: u64) -> String {
    format!("0x{:x}", id)
}

pub fn id_of(address: &str) -> Option<u64> {
    let a = address.trim().trim_start_matches("address:");
    u64::from_str_radix(a.trim_start_matches("0x"), 16).ok()
}

fn client_json(w: &Value, size: &Value) -> Value {
    let id = w["id"].as_u64().unwrap_or(0);
    let app = w["app_id"].as_str().unwrap_or("");
    let title = w["title"].as_str().unwrap_or("");
    json!({
        "address": address_of(id),
        "mapped": true,
        "hidden": false,
        "at": w.get("at").cloned().unwrap_or(json!([0, 0])),
        "size": w.get("size").filter(|s| s[0].as_i64().unwrap_or(0) > 0).cloned().unwrap_or(size.clone()),
        "workspace": { "id": 1, "name": "1" },
        "floating": false,
        "pseudo": false,
        "monitor": 0,
        "class": app,
        "title": title,
        "initialClass": app,
        "initialTitle": title,
        "pid": w["pid"].as_i64().unwrap_or(0),
        "xwayland": false,
        "pinned": false,
        "fullscreen": 2,
        "fullscreenClient": 2,
        "grouped": [],
        "tags": [],
        "swallowing": "0x0",
        "focusHistoryID": w["focus_rank"].as_i64().unwrap_or(0),
    })
}

fn query(endpoint: &str) -> Result<Value> {
    let st = request(json!({ "op": "state" }))?;
    let size = st.get("size").cloned().unwrap_or(json!([1600, 900]));
    let windows = st["windows"].as_array().cloned().unwrap_or_default();
    let focused = st["focused"].as_u64();
    let active = focused.and_then(|f| windows.iter().find(|w| w["id"].as_u64() == Some(f)));
    Ok(match endpoint {
        "clients" => Value::Array(windows.iter().map(|w| client_json(w, &size)).collect()),
        "activewindow" => active.map(|w| client_json(w, &size)).unwrap_or(json!({})),
        "cursorpos" => json!({ "x": st["cursor"][0].as_f64().unwrap_or(0.0) as i64,
                               "y": st["cursor"][1].as_f64().unwrap_or(0.0) as i64 }),
        "monitors" => json!([{
            "id": 0, "name": "PCEYE-WM", "description": "pc-eye-wm headless",
            "width": size[0], "height": size[1], "refreshRate": 60.0, "x": 0, "y": 0,
            "activeWorkspace": { "id": 1, "name": "1" },
            "specialWorkspace": { "id": 0, "name": "" },
            "reserved": [0, 0, 0, 0], "scale": 1.0, "transform": 0,
            "focused": true, "dpmsStatus": true, "vrr": false, "disabled": false,
        }]),
        "workspaces" => json!([{
            "id": 1, "name": "1", "monitor": "PCEYE-WM", "monitorID": 0,
            "windows": windows.len(), "hasfullscreen": !windows.is_empty(),
            "lastwindow": active.map(|w| address_of(w["id"].as_u64().unwrap_or(0))).unwrap_or_else(|| "0x0".into()),
            "lastwindowtitle": active.and_then(|w| w["title"].as_str()).unwrap_or(""),
        }]),
        "activeworkspace" => json!({ "id": 1, "name": "1", "monitor": "PCEYE-WM", "windows": windows.len() }),
        "layers" => json!({}),
        "binds" => json!([]),
        "version" => json!({ "branch": "pc-eye-wm", "tag": "pc-eye-wm" }),
        other => return Err(anyhow!("pc-eye-wm: `{}` sorgusu desteklenmiyor", other)),
    })
}

/// Lua çağrısındaki `anahtar=sayı` alanı.
fn lua_num(s: &str, key: &str) -> Option<i64> {
    let i = s.find(&format!("{}=", key))? + key.len() + 1;
    let rest = &s[i..];
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '-')).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// İlk `"..."` dizgesi (kaçışlar çözülür).
fn first_lua_string(s: &str) -> Option<String> {
    let start = s.find('"')? + 1;
    let mut out = String::new();
    let mut chars = s[start..].chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?),
            '"' => return Some(out),
            c => out.push(c),
        }
    }
    None
}

fn window_arg(s: &str) -> Option<u64> {
    let i = s.find("address:")?;
    let rest = &s[i + "address:".len()..];
    let end = rest.find(|c: char| !c.is_ascii_hexdigit() && c != 'x').unwrap_or(rest.len());
    id_of(&rest[..end])
}

fn spawn(command: &str) -> Result<()> {
    request(json!({ "op": "spawn", "cmd": ["sh", "-c", command] }))?;
    Ok(())
}

/// `dispatch <...>` (Lua ya da klasik sözdizimi). Tek çıkışlı, tek çalışma alanlı ve pencereleri
/// hep tam ekran bir kompozitörde yerleşim komutları (taşı, boyutla, yüzdür, tam ekran) etkisizdir.
fn dispatch(args: &str) -> Result<String> {
    let a = args.trim();
    if let Some(lua) = a.strip_prefix("hl.dsp.") {
        if lua.starts_with("cursor.move") {
            let (x, y) = (lua_num(lua, "x").unwrap_or(0), lua_num(lua, "y").unwrap_or(0));
            request(json!({ "op": "pointer_move", "x": x, "y": y }))?;
        } else if lua.starts_with("focus(") {
            if let Some(id) = window_arg(lua) {
                request(json!({ "op": "focus", "id": id }))?;
            }
        } else if lua.starts_with("window.close") {
            match window_arg(lua) {
                Some(id) => request(json!({ "op": "close", "id": id }))?,
                None => return Err(anyhow!("pc-eye-wm: kapatılacak pencere yok")),
            };
        } else if lua.starts_with("exec_cmd") {
            spawn(&first_lua_string(lua).ok_or_else(|| anyhow!("exec_cmd komutu yok"))?)?;
        } else if !(lua.starts_with("window.") || lua.starts_with("focus(")) {
            return Err(anyhow!("pc-eye-wm: `{}` desteklenmiyor", a));
        }
        return Ok("ok".into());
    }
    let (cmd, rest) = a.split_once(' ').unwrap_or((a, ""));
    match cmd {
        "movecursor" => {
            let mut it = rest.split_whitespace().filter_map(|n| n.parse::<i64>().ok());
            let (x, y) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
            request(json!({ "op": "pointer_move", "x": x, "y": y }))?;
        }
        "focuswindow" => {
            if let Some(id) = window_arg(rest) {
                request(json!({ "op": "focus", "id": id }))?;
            }
        }
        "closewindow" => {
            let id = window_arg(rest).ok_or_else(|| anyhow!("pc-eye-wm: kapatılacak pencere yok"))?;
            request(json!({ "op": "close", "id": id }))?;
        }
        "killactive" => {
            let st = request(json!({ "op": "state" }))?;
            if let Some(id) = st["focused"].as_u64() {
                request(json!({ "op": "close", "id": id }))?;
            }
        }
        "exec" => {
            // `exec [kurallar] komut`
            let c = if rest.starts_with('[') { rest.split_once(']').map(|x| x.1).unwrap_or("") } else { rest };
            spawn(c.trim())?;
        }
        "workspace" | "movetoworkspacesilent" | "movetoworkspace" | "togglefloating" | "fullscreen"
        | "moveactive" | "movewindowpixel" | "resizeactive" | "resizewindowpixel" | "centerwindow" => {}
        other => return Err(anyhow!("pc-eye-wm: `{}` dispatcher'ı desteklenmiyor", other)),
    }
    Ok("ok".into())
}

/// Hyprland `.socket.sock` metin isteğinin wm karşılığı.
pub fn hypr_emulate(command: &str) -> Result<String> {
    let c = command.trim();
    if let Some(ep) = c.strip_prefix("j/") {
        return Ok(serde_json::to_string(&query(ep.trim())?)?);
    }
    if let Some(args) = c.strip_prefix("dispatch ") {
        return Ok(match dispatch(args) {
            Ok(r) => r,
            Err(e) => format!("error: {}", e),
        });
    }
    Err(anyhow!("pc-eye-wm: `{}` isteği desteklenmiyor (ekran/çıkış yönetimi oturumda yok)", c))
}

/// Ekran bölgesinin (yoksa tüm ekranın) PPM P6 görüntüsü: istemci tamponlarından bileştirilir.
pub fn capture_ppm(region: Option<[i64; 4]>) -> Result<Vec<u8>> {
    let dir = crate::eyes::vision::runtime_dir();
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = dir.join(format!("wm_cap_{}_{}.ppm", std::process::id(), n));
    let mut req = json!({ "op": "capture_region", "path": path.to_string_lossy() });
    if let Some([x, y, w, h]) = region {
        req["x"] = json!(x);
        req["y"] = json!(y);
        req["w"] = json!(w);
        req["h"] = json!(h);
    }
    request(req)?;
    let bytes = std::fs::read(&path);
    let _ = std::fs::remove_file(&path);
    Ok(bytes?)
}

/// Doğrulama yuvası (süreç başına): "önce" görüntüsü wm'in belleğinde kalır.
pub fn verify_slot() -> String {
    format!("v{}", std::process::id())
}

/// Bölgenin şimdiki görüntüsünü wm belleğinde `slot` adıyla saklar (dosya/aktarım yok).
pub fn snapshot_region(region: [i64; 4], slot: &str) -> Result<()> {
    request(json!({ "op": "snapshot_region", "x": region[0], "y": region[1], "w": region[2], "h": region[3], "slot": slot }))?;
    Ok(())
}

/// Şimdiki görüntüyü yuvayla wm içinde karşılaştırır: (değişen piksel, yüzde, görünür değişim).
pub fn diff_region(slot: &str, update: bool) -> Result<(u64, f64, bool)> {
    let r = request(json!({ "op": "diff_region", "slot": slot, "update": update }))?;
    Ok((
        r["changed_pixels"].as_u64().unwrap_or(0),
        r["change_pct"].as_f64().unwrap_or(0.0),
        r["visible_change"].as_bool().unwrap_or(false),
    ))
}

/// Bölgenin merkezini kaplayan en üstteki pencere kaç ms'dir kare göndermiyor.
pub fn idle_ms_at(region: [i64; 4]) -> Option<u64> {
    let st = request(json!({ "op": "state" })).ok()?;
    let (cx, cy) = (region[0] + region[2] / 2, region[1] + region[3] / 2);
    st["windows"].as_array()?.iter().find_map(|w| {
        let (x, y) = (w["at"][0].as_i64().unwrap_or(0), w["at"][1].as_i64().unwrap_or(0));
        let (ww, wh) = (w["size"][0].as_i64().unwrap_or(0), w["size"][1].as_i64().unwrap_or(0));
        (cx >= x && cx < x + ww && cy >= y && cy < y + wh).then(|| w["idle_ms"].as_u64()).flatten()
    })
}

/// `grim -g "x,y wxh"` biçimindeki geometriyi çözer.
pub fn parse_grim_geometry(g: &str) -> Option<[i64; 4]> {
    let (pos, size) = g.trim().split_once(' ')?;
    let (x, y) = pos.split_once(',')?;
    let (w, h) = size.split_once('x')?;
    Some([x.trim().parse().ok()?, y.trim().parse().ok()?, w.trim().parse().ok()?, h.trim().parse().ok()?])
}

pub fn pointer_button(code: u32, pressed: bool) -> Result<()> {
    request(json!({ "op": "pointer_button", "button": code, "pressed": pressed }))?;
    Ok(())
}

/// `axis`: 0 dikey, 1 yatay (wl_pointer); `steps` tekerlek çentiği.
pub fn pointer_axis(axis: u32, steps: i32) -> Result<()> {
    let (dx, dy) = if axis == 1 { (steps as f64, 0.0) } else { (0.0, steps as f64) };
    request(json!({ "op": "pointer_axis", "dx": dx, "dy": dy }))?;
    Ok(())
}

/// Karakterler tek tek, aralarında 3 ms ile gönderilir: tüm metin tek seferde gelince Qt'nin otomatik
/// tamamlayıcısı (Dolphin konum çubuğu) araya girip parçalar yutuyordu (aralıksız 3'te 1 bozuk, aralıklı
/// 5/5 doğru). Aralık wm'in içinde değil burada: wm beklerken istemci isteklerini işleyemiyordu (6/10).
pub fn type_text(text: &str) -> Result<()> {
    let mut skipped = Vec::new();
    for (i, ch) in text.chars().enumerate() {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(3));
        }
        let r = request(json!({ "op": "type", "text": ch.to_string() }))?;
        if let Some(s) = r["skipped"].as_array() {
            skipped.extend(s.iter().filter_map(|v| v.as_str().map(String::from)));
        }
    }
    if skipped.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("pc-eye-wm şu karakterleri yazamadı: {:?}", skipped))
    }
}

/// `ctrl+s`, `Return` gibi kısayol (pc-eye adları wm'in xkb adlarına çevrilir).
pub fn key_combo(combo: &str) -> Result<()> {
    let keys: Vec<String> = combo
        .split('+')
        .map(|k| match k.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "ctrl".to_string(),
            "super" | "win" | "meta" | "mod4" => "super".to_string(),
            "alt" | "mod1" => "alt".to_string(),
            "shift" => "shift".to_string(),
            "enter" | "return" => "Return".to_string(),
            "esc" | "escape" => "Escape".to_string(),
            "del" | "delete" => "Delete".to_string(),
            "backspace" => "BackSpace".to_string(),
            "pgup" | "pageup" => "Prior".to_string(),
            "pgdn" | "pagedown" => "Next".to_string(),
            "space" => "space".to_string(),
            "tab" => "Tab".to_string(),
            "up" | "down" | "left" | "right" | "home" | "end" | "insert" => {
                let mut c = k.trim().to_ascii_lowercase();
                c[..1].make_ascii_uppercase();
                c
            }
            _ => k.trim().to_string(),
        })
        .collect();
    request(json!({ "op": "key", "keys": keys.join("+") }))?;
    Ok(())
}

/// Kare doğruluğunda bekleme: hiçbir pencere `quiet_ms` boyunca yeni kare göndermeyene dek.
/// `grace_ms`: istekten sonra hiç kare gelmezse bu süre dolunca durgun sayılır (eylem sonrası).
/// Yalnız odaktaki pencere izlenir: arkada sürekli kare gönderen bir pencere (yükleme animasyonu,
/// başka uygulama) eylemin beklemesini uzatmasın. Odak yoksa tüm pencereler.
pub fn wait_idle(quiet_ms: u64, timeout_ms: u64, grace_ms: u64) -> Result<Value> {
    let focused = request(json!({ "op": "state" })).ok().and_then(|s| s["focused"].as_u64());
    wait_idle_window(focused, quiet_ms, timeout_ms, grace_ms)
}

pub fn wait_idle_window(id: Option<u64>, quiet_ms: u64, timeout_ms: u64, grace_ms: u64) -> Result<Value> {
    let mut req = json!({ "op": "wait_idle", "quiet_ms": quiet_ms, "timeout_ms": timeout_ms, "grace_ms": grace_ms });
    if let Some(id) = id {
        req["id"] = json!(id);
    }
    request(req)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_round_trip() {
        assert_eq!(address_of(26), "0x1a");
        assert_eq!(id_of("0x1a"), Some(26));
        assert_eq!(id_of("address:0x1a"), Some(26));
    }

    #[test]
    fn lua_fields_parse() {
        let s = "cursor.move({x=-3, y=40})";
        assert_eq!((lua_num(s, "x"), lua_num(s, "y")), (Some(-3), Some(40)));
        assert_eq!(window_arg("focus({window=\"address:0xab\"})"), Some(0xab));
        assert_eq!(
            first_lua_string("exec_cmd(\"kate -n \\\"a b\\\"\", { workspace = \"1 silent\" })").as_deref(),
            Some("kate -n \"a b\"")
        );
    }

    #[test]
    fn grim_geometry_parses() {
        assert_eq!(parse_grim_geometry("10,20 300x40"), Some([10, 20, 300, 40]));
        assert_eq!(parse_grim_geometry("bad"), None);
    }
}
