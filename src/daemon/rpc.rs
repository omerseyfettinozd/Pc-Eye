//! Daemon sorgu soketi: `$XDG_RUNTIME_DIR/pc-eye/daemon.sock`, satır başına bir JSON istek/yanıt.
//!
//! CLI (Codex/Gemini becerileri de onu çağırır) her çağrıda dünyayı baştan okumak yerine sorguyu
//! olay güdümlü önbelleği ve snapshot geçmişi yaşayan daemon'a iletir. Daemon farklı bir build'den
//! geliyorsa (binary yeniden kurulduysa) yanıt reddedilir, istemci yerelde çalışır ve daemon yenilenir.

use crate::daemon::ring_buffer::SeqRingBuffer;
use crate::daemon::server::LiveDaemonState;
use crate::eyes::vision::runtime_dir;
use crate::protocol::types::CompositorEvent;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

static DAEMON_STATE: OnceLock<Arc<Mutex<LiveDaemonState>>> = OnceLock::new();

pub fn set_state(s: Arc<Mutex<LiveDaemonState>>) {
    let _ = DAEMON_STATE.set(s);
}

/// Uzun OCR'lı sorgular (Electron penceresi) birkaç saniye sürebilir.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Tek istek satırı için bayt üst sınırı: yenisatır göndermeyen istemci okuma zaman aşımına
/// kadar belleği sınırsız büyütemez.
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

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
    // Soket yalnız sahibine açık olsun: dizin izinleri gevşekse (/tmp yedeği) başka yerel
    // kullanıcı masaüstü ağacını ve pano değerlerini bu soketten okuyabilirdi.
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || handle(stream));
        }
    });
}

fn handle(stream: UnixStream) {
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    // Yanıtı hiç okumayan istemci karşısında bloklanıp iş parçacığını süresiz tutmamak için
    // yazma tarafına da okumayla aynı zaman aşımı konur.
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let mut line = String::new();
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream).take(MAX_REQUEST_BYTES);
    if reader.read_line(&mut line).is_err() {
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
        Some("events") => query_events(a),
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

fn query_events(args: &Value) -> anyhow::Result<Value> {
    let state_arc = DAEMON_STATE
        .get()
        .ok_or_else(|| anyhow::anyhow!("olay akışı yalnız daemon'da"))?;
    let since = args["since"].as_u64().unwrap_or(0);
    let limit = args["limit"].as_u64().unwrap_or(200).min(2000) as usize;
    let kinds: Option<Vec<String>> = args["kinds"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty());

    let guard = state_arc
        .lock()
        .map_err(|_| anyhow::anyhow!("daemon durumu kilitlenemedi"))?;
    Ok(filter_events_from_ring(
        &guard.event_ring,
        since,
        kinds.as_deref(),
        limit,
    ))
}

fn filter_events_from_ring(
    ring: &SeqRingBuffer<CompositorEvent>,
    since: u64,
    kinds: Option<&[String]>,
    limit: usize,
) -> Value {
    let last_seq = ring.latest_seq();
    // Daemon yeniden başladıysa (güncelleme) sayaç sıfırlanmıştır: istemcinin eski seq'i halkanın
    // ilerisindedir. Hiçbir şey dönmemek yerine baştan verilir ve `restarted` ile bildirilir.
    let restarted = since > last_seq;
    let since = if restarted { 0 } else { since };
    let (items, dropped) = match kinds {
        Some(allowed) => {
            // Tür süzgeci limitten önce uygulanır ve since'ten sonraki EN ESKİ `limit` öğe döner: en
            // yenileri seçmek, istemci sonraki çağrıda son seq'ten devam edince aradakileri kaybettirirdi.
            let (all, dropped) = ring.since_seq(since, ring.capacity());
            let filtered: Vec<_> = all
                .into_iter()
                .filter(|item| allowed.iter().any(|k| k == &item.data.kind))
                .take(limit)
                .collect();
            (filtered, dropped)
        }
        None => ring.since_seq(since, limit),
    };
    let events: Vec<Value> = items
        .into_iter()
        .map(|item| {
            json!({
                "seq": item.seq,
                "t": item.timestamp_ms,
                "kind": item.data.kind,
                "payload": item.data.payload,
            })
        })
        .collect();
    json!({
        "events": events,
        "last_seq": last_seq,
        "dropped": dropped,
        "restarted": restarted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_events_kinds_before_limit_and_dropped() {
        let mut ring = SeqRingBuffer::new(4);
        ring.push_at(CompositorEvent { kind: "journal".into(), payload: "j1".into() }, 10);
        ring.push_at(CompositorEvent { kind: "file".into(), payload: "f1".into() }, 20);
        ring.push_at(CompositorEvent { kind: "journal".into(), payload: "j2".into() }, 30);
        ring.push_at(CompositorEvent { kind: "file".into(), payload: "f2".into() }, 40);
        ring.push_at(CompositorEvent { kind: "journal".into(), payload: "j3".into() }, 50);

        let kinds = vec!["journal".to_string()];
        let out = filter_events_from_ring(&ring, 1, Some(&kinds), 2);
        assert_eq!(out["last_seq"], 5);
        assert_eq!(out["dropped"], 0);
        let arr = out["events"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["seq"], 3);
        assert_eq!(arr[0]["t"], 30);
        assert_eq!(arr[1]["seq"], 5);
        assert_eq!(arr[1]["payload"], "j3");
    }
}
