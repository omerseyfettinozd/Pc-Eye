//! Daemon sorgu soketi: `$XDG_RUNTIME_DIR/pc-eye/daemon.sock`, satır başına bir JSON istek/yanıt.
//!
//! CLI (Codex/Gemini becerileri de onu çağırır) her çağrıda dünyayı baştan okumak yerine sorguyu
//! olay güdümlü önbelleği ve snapshot geçmişi yaşayan daemon'a iletir. Daemon farklı bir build'den
//! geliyorsa (binary yeniden kurulduysa) yanıt reddedilir, istemci yerelde çalışır ve daemon yenilenir.

use crate::eyes::vision::runtime_dir;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

/// Uzun OCR'lı sorgular (Electron penceresi) birkaç saniye sürebilir.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

pub fn socket_path() -> PathBuf {
    runtime_dir().join("daemon.sock")
}

/// Bu binary'nin kimliği: dosya değişme zamanı. Süreç başında bir kez alınır; kurulumdan sonra
/// eski daemon'un `current_exe`'si silinmiş dosyayı gösterir, o yüzden saklanan değer kullanılır.
/// Hyprland oturum imzası da kimliğe katılır: yeniden girişten kalan bir daemon eski oturumun
/// soketlerine bakıyor olur.
pub fn build_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let mtime = std::env::current_exe()
            .and_then(|p| p.metadata())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let session = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap_or_default();
        format!("{}:{}", mtime, session)
    })
}

pub fn serve() {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let Ok(listener) = UnixListener::bind(&path) else { return };
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || handle(stream));
        }
    });
}

fn handle(stream: UnixStream) {
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let mut line = String::new();
    let Ok(mut writer) = stream.try_clone() else { return };
    if BufReader::new(stream).read_line(&mut line).is_err() {
        return;
    }
    let reply = match serde_json::from_str::<Value>(&line) {
        Ok(req) => dispatch(&req),
        Err(e) => json!({ "ok": false, "error": format!("geçersiz istek: {}", e) }),
    };
    let _ = writeln!(writer, "{}", reply);
}

fn dispatch(req: &Value) -> Value {
    if req["build"].as_str() != Some(build_id()) {
        return json!({ "ok": false, "error": "build-mismatch", "build": build_id() });
    }
    let a = &req["args"];
    let s = |k: &str| a[k].as_str();
    let u = |k: &str| a[k].as_u64();
    let result = match req["cmd"].as_str() {
        Some("snapshot") => crate::eyes::snapshot::build_snapshot(
            s("window"),
            u("max_elements").unwrap_or(40) as usize,
            a["icons"].as_bool().unwrap_or(false),
            u("seq_since"),
            u("since"),
        ),
        Some("ui") => crate::eyes::ui_atspi::inspect_window_ui(
            s("window"),
            s("query"),
            u("max_elements").unwrap_or(60) as usize,
        )
        .and_then(|r| Ok(serde_json::to_value(r)?)),
        other => Err(anyhow::anyhow!("bilinmeyen komut: {:?}", other)),
    };
    match result {
        Ok(v) => json!({ "ok": true, "result": v }),
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }
}

/// Daemon'a sorar. `None`: daemon yok, eski build ya da iletişim hatası → çağıran yerelde çalışır.
/// Komutun kendi hatası (ör. pencere bulunamadı) `Some(Err)` olarak döner, yerelde tekrarlanmaz.
pub fn request(cmd: &str, args: Value) -> Option<Result<Value, String>> {
    let mut stream = UnixStream::connect(socket_path()).ok()?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT)).ok()?;
    let req = json!({ "build": build_id(), "cmd": cmd, "args": args });
    writeln!(stream, "{}", req).ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    let reply: Value = serde_json::from_str(&line).ok()?;
    if reply["ok"].as_bool() == Some(true) {
        return Some(Ok(reply["result"].clone()));
    }
    match reply["error"].as_str() {
        Some("build-mismatch") | None => None,
        Some(e) => Some(Err(e.to_string())),
    }
}
