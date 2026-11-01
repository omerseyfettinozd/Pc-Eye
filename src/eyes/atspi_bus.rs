//! AT-SPI için yerel D-Bus istemcisi (zbus).
//!
//! Eski yol her özellik için ayrı bir `busctl` süreci başlatıyordu (çağrı başına ~3,6 ms,
//! Firefox penceresinde yüzlerce çağrı → 1,4–1,9 s). Burada süreç başına tek bağlantı açılır ve
//! bir ağaç seviyesindeki tüm istekler aynı anda gönderilir: gidiş-dönüş beklemesi
//! düğüm sayısıyla değil ağaç derinliğiyle ölçeklenir.

use crate::eyes::ui_atspi::{decode_atspi_states, resolve_atspi_bus_address};
use anyhow::{anyhow, Result};
use std::sync::OnceLock;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::Connection;

const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const COMPONENT: &str = "org.a11y.atspi.Component";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

pub const COORD_SCREEN: u32 = 0;
pub const COORD_WINDOW: u32 = 1;

static CONNECTION: OnceLock<Option<Connection>> = OnceLock::new();

/// Süreç ömrü boyunca paylaşılan AT-SPI veri yolu bağlantısı (MCP sunucusunda her çağrıda yeniden açılmaz).
pub fn connection() -> Result<&'static Connection> {
    CONNECTION
        .get_or_init(|| {
            let addr = resolve_atspi_bus_address()?;
            async_io::block_on(async {
                zbus::connection::Builder::address(addr.as_str()).ok()?.build().await.ok()
            })
        })
        .as_ref()
        .ok_or_else(|| anyhow!("AT-SPI veri yoluna bağlanılamadı"))
}

pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    async_io::block_on(future)
}

async fn call<B, R>(conn: &Connection, service: &str, path: &str, iface: &str, method: &str, body: &B) -> Option<R>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    R: for<'de> serde::Deserialize<'de> + zbus::zvariant::Type,
{
    let msg = conn
        .call_method(Some(service), path, Some(iface), method, body)
        .await
        .ok()?;
    msg.body().deserialize::<R>().ok()
}

async fn property(conn: &Connection, service: &str, path: &str, iface: &str, prop: &str) -> Option<OwnedValue> {
    call::<_, OwnedValue>(conn, service, path, PROPERTIES, "Get", &(iface, prop)).await
}

pub async fn role_name(conn: &Connection, service: &str, path: &str) -> Option<String> {
    call(conn, service, path, ACCESSIBLE, "GetRoleName", &()).await
}

pub async fn name(conn: &Connection, service: &str, path: &str) -> Option<String> {
    property(conn, service, path, ACCESSIBLE, "Name").await.and_then(|v| String::try_from(v).ok())
}

pub async fn description(conn: &Connection, service: &str, path: &str) -> Option<String> {
    property(conn, service, path, ACCESSIBLE, "Description")
        .await
        .and_then(|v| String::try_from(v).ok())
        .filter(|d| !d.is_empty())
}

pub async fn states(conn: &Connection, service: &str, path: &str) -> Vec<String> {
    match call::<_, Vec<u32>>(conn, service, path, ACCESSIBLE, "GetState", &()).await {
        Some(bits) if bits.len() >= 2 => decode_atspi_states(bits[0], bits[1]),
        _ => Vec::new(),
    }
}

pub async fn children(conn: &Connection, service: &str, path: &str) -> Vec<(String, String)> {
    call::<_, Vec<(String, OwnedObjectPath)>>(conn, service, path, ACCESSIBLE, "GetChildren", &())
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(s, p)| (s, p.as_str().to_string()))
        .collect()
}

pub async fn extents(conn: &Connection, service: &str, path: &str, coord_type: u32) -> Option<[i64; 4]> {
    call::<_, (i32, i32, i32, i32)>(conn, service, path, COMPONENT, "GetExtents", &(coord_type,))
        .await
        .map(|(x, y, w, h)| [x as i64, y as i64, w as i64, h as i64])
}

/// Metin kutusu içeriği ya da sayısal kontrol değeri. Şifre alanları ÇAĞIRAN tarafından elenir.
pub async fn text_or_value(conn: &Connection, service: &str, path: &str) -> Option<String> {
    if let Some(txt) = call::<_, String>(conn, service, path, "org.a11y.atspi.Text", "GetText", &(0i32, 500i32)).await {
        if !txt.is_empty() {
            return Some(txt);
        }
    }
    property(conn, service, path, "org.a11y.atspi.Value", "CurrentValue")
        .await
        .and_then(|v| f64::try_from(v).ok())
        .map(|n| format!("{:.2}", n))
}

pub async fn service_pid(conn: &Connection, service: &str) -> Option<i32> {
    call::<_, u32>(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "GetConnectionUnixProcessID",
        &(service,),
    )
    .await
    .map(|p| p as i32)
}

pub async fn do_action(conn: &Connection, service: &str, path: &str, index: i32) -> Option<bool> {
    call(conn, service, path, "org.a11y.atspi.Action", "DoAction", &(index,)).await
}

pub async fn set_text_contents(conn: &Connection, service: &str, path: &str, text: &str) -> Option<bool> {
    call(conn, service, path, "org.a11y.atspi.EditableText", "SetTextContents", &(text,)).await
}
