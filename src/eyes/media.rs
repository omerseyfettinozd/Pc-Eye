//! Pencerenin medya durumu (MPRIS): bir video ya da şarkı gerçekten çalıyor mu.
//!
//! "Videoyu aç ve oynat" görevinde ajan oynatmayı kanıtlamak için 20 tur harcadı (ekran görüntüleri,
//! ses kaydı, düğme adları). Tarayıcı ve oynatıcılar durumu oturum D-Bus'unda zaten yayınlıyor:
//! pencerenin süreci bir MPRIS oynatıcısıysa snapshot `media` alanında bunu verir.

use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::OnceLock;
use zbus::zvariant::OwnedValue;

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

fn session() -> Option<&'static zbus::Connection> {
    static CONN: OnceLock<Option<zbus::Connection>> = OnceLock::new();
    CONN.get_or_init(|| crate::eyes::atspi_bus::block_on(async { zbus::Connection::session().await.ok() }))
        .as_ref()
}

async fn call<B, R>(conn: &zbus::Connection, dest: &str, path: &str, iface: &str, method: &str, body: &B) -> Option<R>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    R: for<'de> serde::Deserialize<'de> + zbus::zvariant::Type,
{
    let msg = conn.call_method(Some(dest), path, Some(iface), method, body).await.ok()?;
    msg.body().deserialize::<R>().ok()
}

async fn prop(conn: &zbus::Connection, dest: &str, name: &str) -> Option<OwnedValue> {
    call(conn, dest, "/org/mpris/MediaPlayer2", "org.freedesktop.DBus.Properties", "Get", &(PLAYER_IFACE, name)).await
}

fn meta_str(meta: &std::collections::HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    let v = meta.get(key)?;
    if let Ok(s) = String::try_from(v.try_clone().ok()?) {
        return Some(s);
    }
    // xesam:artist bir dizi.
    Vec::<String>::try_from(v.try_clone().ok()?).ok().map(|a| a.join(", "))
}

fn parent_pid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    // "pid (komut adı) durum ppid ..." — komut adı boşluk ve parantez içerebilir.
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// Oynatıcı süreci pencere sürecinin kendisi ya da (en çok 8 kuşak) torunu mu.
fn descends_from(mut pid: i32, root: i32) -> bool {
    for _ in 0..8 {
        if pid == root {
            return true;
        }
        match parent_pid(pid) {
            Some(p) if p > 1 => pid = p,
            _ => return false,
        }
    }
    false
}

/// Pencere sürecine (ya da onun alt süreçlerine) ait oynatıcının durumu. Önce oynatıcılar
/// listelenir (çoğu zaman hiç yok): her snapshot'ta süreç ağacını taramak gecikmeyi %40 artırıyordu.
pub fn media_for_window(window_pid: i32) -> Option<Value> {
    if window_pid <= 0 {
        return None;
    }
    media_matching(|p| descends_from(p, window_pid))
}

/// `pids` süreçlerinden birine ait oynatıcıların durumu (genelde tek oynatıcı).
pub fn media_for_pids(pids: &HashSet<i32>) -> Option<Value> {
    if pids.is_empty() {
        return None;
    }
    media_matching(|p| pids.contains(&p))
}

fn media_matching(owns: impl Fn(i32) -> bool) -> Option<Value> {
    let conn = session()?;
    crate::eyes::atspi_bus::block_on(async {
        let names: Vec<String> =
            call(conn, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "ListNames", &()).await?;
        for name in names.iter().filter(|n| n.starts_with(MPRIS_PREFIX) && !n.ends_with(".playerctld")) {
            let pid: Option<u32> = call(
                conn,
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "GetConnectionUnixProcessID",
                &(name.as_str(),),
            )
            .await;
            if !pid.map_or(false, |p| owns(p as i32)) {
                continue;
            }
            let status = prop(conn, name, "PlaybackStatus").await.and_then(|v| String::try_from(v).ok());
            let meta: std::collections::HashMap<String, OwnedValue> = prop(conn, name, "Metadata")
                .await
                .and_then(|v| v.try_into().ok())
                .unwrap_or_default();
            let mut out = json!({ "status": status.unwrap_or_default() });
            for (key, field) in [("xesam:title", "title"), ("xesam:artist", "artist"), ("xesam:url", "url")] {
                if let Some(s) = meta_str(&meta, key).filter(|s| !s.is_empty()) {
                    out[field] = json!(s.chars().take(160).collect::<String>());
                }
            }
            return Some(out);
        }
        None
    })
}
