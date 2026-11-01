//! Ajan oturumu: ajana kullanıcının oturumundan ayrı bir Hyprland örneği.
//!
//! Sanal fare/klavye olayları Wayland bağlantısının ait olduğu kompozitöre gider. Ajan kendi
//! (iç içe) Hyprland örneğinde çalışınca kullanıcının imleci, odağı ve panosu hiç değişmez; aynı
//! anda birden çok ajan da çalışabilir.
//!
//! Hyprland yalnız headless arka uçla açılamıyor; `WAYLAND_DISPLAY` varken Wayland arka ucuyla iç
//! içe açılır. İç örneğin penceresi kullanıcının oturumundaki görünmez bir headless çıkışa, odak
//! çalmadan (`exec_cmd` + `silent`) yerleştirilir ve tam ekran yapılır: iç ekran o çıkışın boyutundadır.
//!
//! `PC_EYE_SESSION=<ad>` ile çalışan her pc-eye süreci (CLI, MCP, daemon) o oturumun
//! `HYPRLAND_INSTANCE_SIGNATURE` ve `WAYLAND_DISPLAY` değerlerini kullanır; çalışma dosyaları
//! (daemon soketi, mark kayıtları, yakalamalar) oturuma özel dizindedir.

use crate::director::display::manage_display;
use crate::eyes::compositor::{hypr_dispatch_cmd, HyprDispatch};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const START_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_MODE: &str = "1600x900";
const TAG_VAR: &str = "PC_EYE_SESSION_TAG";
const START_ATTEMPTS: usize = 3;
static ACTIVATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 32 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Bu sürecin bağlı olduğu ajan oturumu (`PC_EYE_SESSION`).
pub fn active_name() -> Option<String> {
    std::env::var("PC_EYE_SESSION").ok().filter(|n| valid_name(n))
}

fn runtime_base() -> PathBuf {
    PathBuf::from(std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into()))
}

fn session_dir(name: &str) -> PathBuf {
    runtime_base().join("pc-eye").join("sessions").join(name)
}

fn load(name: &str) -> Option<Value> {
    let text = std::fs::read_to_string(session_dir(name).join("session.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn alive(info: &Value) -> bool {
    let pid = info["pid"].as_i64().unwrap_or(0);
    let sig = info["signature"].as_str().unwrap_or("");
    pid > 0
        && has_tag(pid, info["tag"].as_str().unwrap_or(""))
        && runtime_base().join("hypr").join(sig).join(".socket.sock").exists()
}

/// `PC_EYE_SESSION` ayarlıysa süreç ortamını o oturumun kompozitörüne yönlendirir.
/// `main` başında, iş parçacıkları açılmadan çağrılır. `autostart`: oturum yoksa açılır; böylece
/// `PC_EYE_SESSION=agent pc-eye mcp` diye kayıtlı bir MCP sunucusu ilk çağrıda kendi oturumunu kurar.
/// (Daemon için kapalı: ortamı zaten oturuma çevrilmiş bir süreç dış oturumu bilemez.)
pub fn activate(autostart: bool) -> Result<()> {
    let Some(name) = active_name() else { return Ok(()) };
    let info = match load(&name).filter(alive) {
        Some(info) => info,
        None if autostart => start_unchecked(&name, DEFAULT_MODE)?,
        None => return Err(anyhow!("ajan oturumu çalışmıyor: {} (`pc-eye session start {}`)", name, name)),
    };
    std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", info["signature"].as_str().unwrap_or(""));
    ACTIVATED.store(true, std::sync::atomic::Ordering::SeqCst);
    std::env::set_var("WAYLAND_DISPLAY", info["wayland_display"].as_str().unwrap_or(""));
    // Yalnız X11 konuşan bir uygulama kullanıcının Xwayland'ine, yani onun ekranına açılırdı.
    std::env::remove_var("DISPLAY");
    Ok(())
}

/// Uzun yaşayan süreçler (MCP) için: oturumun kompozitörü hâlâ ayakta mı. Kapandıysa ajan boş bir
/// masaüstü yerine açık bir hata görür.
pub fn ensure_alive() -> Result<()> {
    let Some(name) = active_name() else { return Ok(()) };
    if !ACTIVATED.load(std::sync::atomic::Ordering::SeqCst) {
        return activate(true);
    }
    match load(&name) {
        // Aynı adla yeniden açılmışsa bu süreç hâlâ eski örneğe bakıyordur.
        Some(info)
            if alive(&info)
                && info["signature"].as_str() == std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok().as_deref() =>
        {
            Ok(())
        }
        _ => Err(anyhow!("ajan oturumu `{}` kapandı; yeniden açmak için `pc-eye session start {}`", name, name)),
    }
}

/// Belirli bir Hyprland örneğine istek (süreç ortamından bağımsız).
fn instance_request(signature: &str, command: &str) -> Result<String> {
    let path = runtime_base().join("hypr").join(signature).join(".socket.sock");
    let mut s = UnixStream::connect(&path).with_context(|| format!("{} bağlanılamadı", path.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.write_all(command.as_bytes())?;
    s.shutdown(std::net::Shutdown::Write)?;
    let mut out = String::new();
    s.read_to_string(&mut out)?;
    Ok(out)
}

fn instance_dirs() -> HashSet<String> {
    std::fs::read_dir(runtime_base().join("hypr"))
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default()
}

/// Sürecin ortamında oturum etiketi var mı (`/proc/<pid>/environ`).
fn has_tag(pid: i64, tag: &str) -> bool {
    let want = format!("{}={}", TAG_VAR, tag);
    std::fs::read(format!("/proc/{}/environ", pid))
        .map(|env| env.split(|b| *b == 0).any(|kv| kv == want.as_bytes()))
        .unwrap_or(false)
}

/// Bu oturumun örneğinin kimliği, PID'i ve Wayland soketi (`hyprland.lock`: "pid\nwayland-N").
/// "Önceden olmayan ilk örnek" yetmez: aynı anda açılan iki oturum aynı örneği sahiplenip birinin
/// kapanışı ötekinin kompozitörünü kapatıyordu (paralel ölçüm 125142). Örnek etiketinden tanınır.
fn wait_new_instance(before: &HashSet<String>, tag: &str) -> Result<(String, i64, String)> {
    let start = Instant::now();
    while start.elapsed() < START_TIMEOUT {
        for sig in instance_dirs().difference(before) {
            let dir = runtime_base().join("hypr").join(sig);
            let lock = std::fs::read_to_string(dir.join("hyprland.lock")).unwrap_or_default();
            let mut lines = lock.lines();
            if let (Some(pid), Some(wl)) = (lines.next().and_then(|p| p.trim().parse::<i64>().ok()), lines.next()) {
                if !has_tag(pid, tag) {
                    continue;
                }
                if dir.join(".socket.sock").exists() && instance_request(sig, "j/monitors").is_ok() {
                    return Ok((sig.clone(), pid, wl.trim().to_string()));
                }
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(anyhow!("iç Hyprland {} sn içinde açılmadı", START_TIMEOUT.as_secs()))
}

fn config_lua(mode: &str) -> String {
    // Yalnız ajan için: animasyon, bulanıklık, kenar ve boşluk yok (her kare doğrulama için daha
    // az değişen piksel, eylemden sonra daha hızlı durulma). Haber/bağış pencereleri ilk açılışta
    // uygulamanın üstüne çıkıp tıklamaları yutuyordu. İmleç görünmez: iç oturumda yazılımla kareye
    // çiziliyor ve tıklama doğrulamasında imlecin yer değiştirmesi (~1000 piksel) "değişti" sayılıyordu.
    format!(
        r#"hl.monitor({{ output = "", mode = "{mode}@60", position = "0x0", scale = 1 }})
hl.config({{
    general = {{ gaps_in = 0, gaps_out = 0, border_size = 0 }},
    decoration = {{ rounding = 0, blur = {{ enabled = false }}, shadow = {{ enabled = false }} }},
    animations = {{ enabled = false }},
    misc = {{ disable_hyprland_logo = true, disable_splash_rendering = true, force_default_wallpaper = 0 }},
    ecosystem = {{ no_update_news = true, no_donation_nag = true }},
    input = {{ kb_layout = "tr" }},
    cursor = {{ invisible = true }},
}})
"#
    )
}

fn output_name(name: &str) -> String {
    format!("PCEYE-{}", name.to_ascii_uppercase())
}

/// Oturumu açar (çalışıyorsa olduğu gibi döner).
pub fn start(name: &str, mode: &str) -> Result<Value> {
    if active_name().is_some() {
        return Err(anyhow!("ajan oturumunun içinden yeni oturum açılamaz (PC_EYE_SESSION ayarlı)"));
    }
    start_unchecked(name, mode)
}

fn start_unchecked(name: &str, mode: &str) -> Result<Value> {
    if !valid_name(name) {
        return Err(anyhow!("oturum adı yalnız harf, rakam, `-`, `_` (en çok 32): {}", name));
    }
    if std::env::var("WAYLAND_DISPLAY").map_or(true, |d| d.is_empty()) {
        // Wayland soketi yoksa Hyprland DRM arka ucunu dener: kullanıcının ekranını ele geçirir.
        return Err(anyhow!("WAYLAND_DISPLAY yok: ajan oturumu yalnız bir Wayland oturumunun içinden açılır"));
    }
    // Aynı anda açılmaya çalışan iki süreç (ör. iki CLI çağrısı) iki iç Hyprland başlatmasın.
    std::fs::create_dir_all(session_dir(name))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(session_dir(name).join("start.lock"))?;
    unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) };
    if let Some(info) = load(name).filter(alive) {
        let mut out = info;
        out["reused"] = json!(true);
        return Ok(out);
    }

    let output = output_name(name);
    let _ = manage_display("remove", Some(&output), None, None, None);
    let created = manage_display("create", Some(&output), Some(mode), None, None)?;
    let details: Value = serde_json::from_str(&created.details).unwrap_or_default();
    let workspace = details["workspace_id"]
        .as_i64()
        .map(|w| w.to_string())
        .ok_or_else(|| anyhow!("headless çıkışın çalışma alanı bilinmiyor: {}", created.details))?;

    let dir = session_dir(name);
    std::fs::create_dir_all(&dir)?;
    let cfg = dir.join("hyprland.lua");
    std::fs::write(&cfg, config_lua(mode))?;

    // NVIDIA'da iç örneğin ilk GBM tamponu ara sıra ayrılamıyor ("bo null"): dış pencere hiç
    // oluşmuyor, iç ekran 0x0 FALLBACK kalıyor (6 açılışta 2). İç ekran istenen boyuta gelmezse
    // örnek kapatılıp yeniden denenir.
    let want: Vec<i64> = mode.split('x').filter_map(|n| n.parse().ok()).collect();
    let mut last_err = anyhow!("iç Hyprland açılmadı");
    let mut launched_instance = None;
    for _attempt in 0..START_ATTEMPTS {
        match launch_instance(name, &cfg, &workspace, &want) {
            Ok(v) => {
                launched_instance = Some(v);
                break;
            }
            Err(e) => last_err = e,
        }
    }
    let Some((signature, pid, wayland_display, tag, size)) = launched_instance else {
        let _ = manage_display("remove", Some(&output), None, None, None);
        return Err(last_err);
    };

    let info = json!({
        "name": name,
        "signature": signature,
        "wayland_display": wayland_display,
        "pid": pid,
        "tag": tag,
        "output": output,
        "outer_workspace": workspace,
        "outer_signature": std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap_or_default(),
        "size": size,
        "created": SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        "use": format!("PC_EYE_SESSION={} pc-eye <komut>  |  MCP: env PC_EYE_SESSION={}", name, name),
    });
    std::fs::write(dir.join("session.json"), serde_json::to_string_pretty(&info)?)?;
    Ok(info)
}

/// İç örneği başlatır ve ekranı istenen boyuta gelene kadar bekler; gelmezse örneği kapatır.
fn launch_instance(
    name: &str,
    cfg: &std::path::Path,
    workspace: &str,
    want: &[i64],
) -> Result<(String, i64, String, String, Value)> {
    let before = instance_dirs();
    let tag = format!(
        "{}-{}-{}",
        name,
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    );
    hypr_dispatch_cmd(&HyprDispatch::ExecSilent {
        command: format!("{}={} HYPRLAND_NO_SD_NOTIFY=1 Hyprland -c '{}'", TAG_VAR, tag, cfg.display()),
        workspace: workspace.to_string(),
    })?;
    let (signature, pid, wayland_display) = wait_new_instance(&before, &tag)?;

    let start = Instant::now();
    let mut size = Value::Null;
    let mut fullscreened = false;
    while start.elapsed() < Duration::from_secs(3) {
        // İç ekranın boyutu dış penceresininkidir: tam ekranla çıkışın tamamını kaplasın.
        if !fullscreened {
            if let Ok(desk) = crate::eyes::compositor::capture_desktop_state() {
                if let Some(w) = desk.windows.iter().find(|w| w.pid as i64 == pid) {
                    let _ = hypr_dispatch_cmd(&HyprDispatch::Fullscreen {
                        maximized: false,
                        state: Some(true),
                        window: Some(w.address.clone()),
                    });
                    fullscreened = true;
                }
            }
        }
        if let Some(m) = instance_request(&signature, "j/monitors")
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.as_array().and_then(|a| a.first().cloned()))
        {
            size = json!([m["width"], m["height"]]);
            if want.len() == 2 && m["width"] == want[0] && m["height"] == want[1] {
                return Ok((signature, pid, wayland_display, tag, size));
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    terminate(pid as i32);
    Err(anyhow!("iç Hyprland ekranı {:?} boyutuna gelmedi (son: {})", want, size))
}

fn terminate(pid: i32) {
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(4) && unsafe { libc::kill(pid, 0) } == 0 {
        thread::sleep(Duration::from_millis(50));
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        unsafe { libc::kill(pid, libc::SIGKILL) };
        thread::sleep(Duration::from_millis(200));
    }
}

/// Oturumu kapatır: önce iç Hyprland (pencere çıkış silinmeden gitsin, yoksa kullanıcının
/// çalışma alanına taşınır), sonra headless çıkış.
pub fn stop(name: &str) -> Result<Value> {
    if active_name().is_some() {
        return Err(anyhow!("oturum kendi içinden kapatılamaz (PC_EYE_SESSION ayarlı)"));
    }
    let info = load(name).ok_or_else(|| anyhow!("ajan oturumu yok: {}", name))?;
    let pid = info["pid"].as_i64().unwrap_or(0) as i32;
    let mut killed = false;
    // PID yeniden kullanılmış olabilir: yalnız bu oturumun etiketini taşıyan süreç öldürülür.
    if pid > 0 && has_tag(pid as i64, info["tag"].as_str().unwrap_or("")) {
        terminate(pid);
        killed = true;
    }
    // Oturumun daemon'u kendi Hyprland'ı gidince işe yaramaz.
    if let Some(dpid) = std::fs::read_to_string(session_dir(name).join("daemon.pid"))
        .ok()
        .and_then(|p| p.trim().parse::<i32>().ok())
    {
        unsafe { libc::kill(dpid, libc::SIGTERM) };
    }
    let output = info["output"].as_str().unwrap_or("");
    let removed = !output.is_empty() && manage_display("remove", Some(output), None, None, None).is_ok();
    let _ = std::fs::remove_dir_all(session_dir(name));
    Ok(json!({ "ok": true, "name": name, "compositor_stopped": killed, "output_removed": removed }))
}

pub fn list() -> Vec<Value> {
    let root = runtime_base().join("pc-eye").join("sessions");
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(mut info) = load(&name) {
                info["alive"] = json!(alive(&info));
                out.push(info);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_restricted() {
        assert!(valid_name("agent-1"));
        assert!(!valid_name("../x"));
        assert!(!valid_name(""));
        assert!(!valid_name("a b"));
    }

    #[test]
    fn exec_silent_lua_quotes_command() {
        let d = HyprDispatch::ExecSilent { command: "Hyprland -c '/a b.lua'".into(), workspace: "7".into() };
        assert_eq!(d.lua(), "hl.dsp.exec_cmd(\"Hyprland -c '/a b.lua'\", { workspace = \"7 silent\" })");
        assert_eq!(d.legacy(), "exec [workspace 7 silent] Hyprland -c '/a b.lua'");
    }
}
