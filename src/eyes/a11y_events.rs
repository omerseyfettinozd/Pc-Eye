//! AT-SPI olay izleyicisi.
//!
//! Uzun yaşayan süreçlerde (daemon, MCP sunucusu) uygulamaların yayınladığı nesne/pencere/belge
//! olaylarını dinler ve uygulama (D-Bus benzersiz adı) başına bir "nesil" sayacı tutar. Eleman
//! önbelleği bu sayaç değişmediği sürece geçerlidir. Tek atımlık CLI süreçlerinde izleyici
//! başlatılmaz; `generation` `None` döner ve önbellek hiç kullanılmaz.

use crate::eyes::ui_atspi::resolve_atspi_bus_address;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use zbus::message::Type as MessageType;
use zbus::Connection;

/// Uygulamalar yalnız dinleyicisi kayıtlı olay türlerini yayınlar (at-spi2 kayıt servisi izler).
const EVENT_KINDS: &[&str] = &["object:", "window:", "document:"];
const EVENT_INTERFACES: &[&str] = &[
    "org.a11y.atspi.Event.Object",
    "org.a11y.atspi.Event.Window",
    "org.a11y.atspi.Event.Document",
];

static GENERATIONS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
static LISTENING: AtomicBool = AtomicBool::new(false);

/// İzleyiciyi bir kez başlatır (sonraki çağrılar etkisiz). Bağlantı koparsa yeniden bağlanır.
pub fn start() {
    if LISTENING.swap(true, Ordering::SeqCst) {
        return;
    }
    GENERATIONS.get_or_init(|| Mutex::new(HashMap::new()));
    thread::spawn(|| loop {
        let _ = async_io::block_on(listen());
        // Kopukluk sırasında önbellek güvenilmez: tüm nesilleri geçersiz kıl.
        if let Some(g) = GENERATIONS.get() {
            if let Ok(mut map) = g.lock() {
                for v in map.values_mut() {
                    *v += 1;
                }
            }
        }
        thread::sleep(Duration::from_secs(1));
    });
}

/// İzleyici çalışıyorsa uygulamanın güncel nesli; çalışmıyorsa `None` (önbellek kapalı).
pub fn generation(service: &str) -> Option<u64> {
    if !LISTENING.load(Ordering::SeqCst) {
        return None;
    }
    let map = GENERATIONS.get()?.lock().ok()?;
    Some(map.get(service).copied().unwrap_or(0))
}

async fn listen() -> zbus::Result<()> {
    let addr = resolve_atspi_bus_address()
        .ok_or_else(|| zbus::Error::Failure("AT-SPI veri yolu yok".into()))?;
    // Çağrı bağlantısından ayrı: olay akışı istek yanıtlarıyla aynı kuyruğu paylaşmasın.
    let conn: Connection = zbus::connection::Builder::address(addr.as_str())?.build().await?;
    for kind in EVENT_KINDS {
        conn.call_method(
            Some("org.a11y.atspi.Registry"),
            "/org/a11y/atspi/registry",
            Some("org.a11y.atspi.Registry"),
            "RegisterEvent",
            &(*kind, Vec::<String>::new(), ""),
        )
        .await?;
    }
    for iface in EVENT_INTERFACES {
        conn.call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "AddMatch",
            &(format!("type='signal',interface='{}'", iface),),
        )
        .await?;
    }

    let mut stream = zbus::MessageStream::from(&conn);
    while let Some(msg) = stream.next().await {
        let Ok(msg) = msg else { continue };
        if msg.message_type() != MessageType::Signal {
            continue;
        }
        let header = msg.header();
        let is_event = header
            .interface()
            .map_or(false, |i| i.as_str().starts_with("org.a11y.atspi.Event."));
        let Some(sender) = header.sender() else { continue };
        if !is_event {
            continue;
        }
        if let Some(g) = GENERATIONS.get() {
            if let Ok(mut map) = g.lock() {
                *map.entry(sender.as_str().to_string()).or_insert(0) += 1;
            }
        }
    }
    Ok(())
}

/// Okumadan ÖNCE alınan nesil görüntüsü: okuma sırasında gelen olay sonraki sorguda önbelleği bozar.
pub fn generations_snapshot() -> Option<HashMap<String, u64>> {
    if !LISTENING.load(Ordering::SeqCst) {
        return None;
    }
    GENERATIONS.get()?.lock().ok().map(|m| m.clone())
}

/// Eylem dönemi: pc-eye'ın her girdi eylemi (hangi süreçten gelirse gelsin) bunu artırır. Önbellek
/// yalnız dönem değişmediyse geçerlidir: GTK4 gibi araç takımları her değişiklikte AT-SPI olayı
/// yayınlamıyor (etiket metni değişimi), ama ajanın kendi eyleminden sonra okuma hep taze olmalı.
fn epoch_path() -> std::path::PathBuf {
    crate::eyes::vision::runtime_dir().join("action_epoch")
}

pub fn bump_action_epoch() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let _ = std::fs::write(epoch_path(), now.to_string());
}

pub fn action_epoch() -> String {
    std::fs::read_to_string(epoch_path()).unwrap_or_default()
}

thread_local! {
    static BYPASS_CACHE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Yoklama yapan kod (wait, doğrulama) önbelleği hiç kullanmamalı: değişimi beklerken eski
/// sonucu görmek, beklemeyi anlamsız kılar.
pub fn with_fresh_reads<T>(f: impl FnOnce() -> T) -> T {
    BYPASS_CACHE.with(|b| b.set(true));
    let out = f();
    BYPASS_CACHE.with(|b| b.set(false));
    out
}

pub fn cache_bypassed() -> bool {
    BYPASS_CACHE.with(|b| b.get())
}
