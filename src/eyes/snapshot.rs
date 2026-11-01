//! Tek çağrılık, token-dostu masaüstü anlık görüntüsü (`pc_eye_snapshot`).
//!
//! `context` (~30 KB: tüm olay/günlük geçmişi) + `ui` (eleman başına D-Bus yolu, iki sınır kutusu)
//! + `vision` yerine ajanın bir sonraki adımı seçmek için gerçekten kullandığını döner: pencereler,
//! hedef pencerenin etkileşimli elemanları (mark + tıklama noktası) ve `seq` ile yalnız yeni olaylar.

use crate::daemon::server::{ensure_background_daemon, PersistedTelemetryState};
use crate::eyes::compositor::capture_desktop_state;
use crate::eyes::ui_atspi::inspect_window_ui;
use crate::protocol::types::UiElement;
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

const STATE_KEEP: &[&str] = &["checked", "focused", "editable", "selected", "expanded", "pressed"];
const MAX_EVENTS: usize = 10;
const MAX_TERMINAL_LINES: usize = 30;

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

fn compact_element(e: &UiElement) -> Value {
    let mut obj = json!({
        "mark": e.mark,
        "role": e.role,
        "name": truncate(&e.name, 80),
        "at": e.click_point,
    });
    if let Some(v) = e.value.as_ref().filter(|v| !v.is_empty() && !v.starts_with("conf:")) {
        obj["value"] = json!(truncate(v, 120));
    }
    // OCR güveni karar bilgisidir: düşükse ajan görüntüyle doğrular (0/O, 9/O karışıklıkları).
    // Snapshot'ın ilk sürümü bunu atınca ajan %95'e düştü.
    if e.role == "ocr-text" {
        if let Some(c) = ocr_conf(e) {
            obj["conf"] = json!(c.round() as i64);
        }
    }
    let states: Vec<&str> = e
        .states
        .iter()
        .map(String::as_str)
        .filter(|s| STATE_KEEP.contains(s))
        .collect();
    if !states.is_empty() {
        obj["states"] = json!(states);
    }
    obj
}

/// Bunun altı neredeyse her zaman ikon kenarı; üstü (ör. %51'lik gizli kod) `conf` ile gösterilir.
const MIN_OCR_CONF: f64 = 30.0;

/// İkon kenarlarından çıkan OCR çöpü ("pi,", "io) «"): iki harf/rakamdan az, sembolü harfi kadar
/// ya da daha çok, veya düşük güvenli. "Ortam »", "© Klasör seç..." gibi gerçek etiketler kalır.
fn is_ocr_noise(e: &UiElement) -> bool {
    if e.role != "ocr-text" {
        return false;
    }
    let alnum = e.name.chars().filter(|c| c.is_alphanumeric()).count();
    let symbols = e.name.chars().filter(|c| !c.is_alphanumeric() && !c.is_whitespace()).count();
    alnum < 2 || symbols >= alnum || ocr_conf(e).map_or(false, |c| c < MIN_OCR_CONF)
}

fn ocr_conf(e: &UiElement) -> Option<f64> {
    e.value
        .as_deref()
        .and_then(|v| v.strip_prefix("conf:"))
        .and_then(|v| v.trim_end_matches('%').parse::<f64>().ok())
}

/// Algısal ikon dedektörü düz zeminde onlarca sahte kutu üretebilir; metinli eleman varsa onları ele.
/// Delta karşılaştırması için kararlı kimlik: mark numaraları snapshot'lar arasında kayabilir.
fn element_key(e: &UiElement) -> String {
    if e.dbus_path.is_empty() {
        format!("{}|{}|{},{}", e.role, e.name, e.click_point[0], e.click_point[1])
    } else {
        format!("{}{}", e.dbus_service, e.dbus_path)
    }
}

fn select_elements(elements: &[UiElement], include_icons: bool, max: usize) -> (Vec<(String, Value)>, usize) {
    let has_text = elements.iter().any(|e| e.role != "icon-button" && !e.name.is_empty());
    let keep_icons = include_icons || !has_text;
    let mut dropped = 0;
    let picked = elements
        .iter()
        .filter(|e| {
            let keep = (keep_icons || e.role != "icon-button") && !is_ocr_noise(e);
            if !keep {
                dropped += 1;
            }
            keep
        })
        .take(max)
        .map(|e| (element_key(e), compact_element(e)))
        .collect();
    (picked, dropped)
}

pub fn build_snapshot(
    window: Option<&str>,
    max_elements: usize,
    include_icons: bool,
    seq_since: Option<u64>,
    since: Option<u64>,
) -> Result<Value> {
    let daemon_running = ensure_background_daemon();
    let desktop = capture_desktop_state()?;
    let telemetry = PersistedTelemetryState::load();

    let mut windows: Vec<&crate::protocol::types::WindowInfo> =
        desktop.windows.iter().filter(|w| w.mapped && !w.hidden).collect();
    windows.sort_by_key(|w| (!w.focused, w.workspace_id));
    let windows: Vec<Value> = windows
        .iter()
        .map(|w| {
            json!({
                "address": w.address,
                "title": truncate(&w.title, 80),
                "class": w.class,
                "workspace": w.workspace_id,
                "geometry": [w.x, w.y, w.width, w.height],
                "focused": w.focused,
            })
        })
        .collect();
    let monitors: Vec<Value> = desktop
        .monitors
        .iter()
        .map(|m| json!({ "name": m.name, "workspace": m.active_workspace_id, "size": [m.width, m.height], "focused": m.focused }))
        .collect();

    // Hedef pencere yoksa (ör. boş çalışma alanı) anlık görüntü yine de pencere listesini döner.
    let mut keyed_elements: Vec<(String, Value)> = Vec::new();
    let mut target_address = None;
    let target = match inspect_window_ui(window, None, max_elements.saturating_mul(2).max(20)) {
        Ok(ui) => {
            let (elements, dropped_icons) = select_elements(&ui.elements, include_icons, max_elements);
            target_address = Some(ui.window_address.clone());
            let mut t = json!({
                "address": ui.window_address,
                "title": truncate(&ui.window_title, 80),
                "class": ui.window_class,
                "perception": ui.perception_mode,
                "elements": elements.iter().map(|(_, v)| v.clone()).collect::<Vec<_>>(),
            });
            keyed_elements = elements;
            if dropped_icons > 0 {
                t["dropped_noise"] = json!(dropped_icons);
            }
            if let Some(term) = ui.terminal_context {
                let lines = &term.visible_lines;
                let tail = &lines[lines.len().saturating_sub(MAX_TERMINAL_LINES)..];
                t["terminal"] = json!({
                    "foreground_cmd": term.foreground_cmd,
                    "cwd": term.cwd,
                    "visible_lines": tail,
                });
            }
            t
        }
        Err(e) => json!({ "error": e.to_string() }),
    };

    let current = StoredSnap {
        id: NEXT_SNAP.fetch_add(1, Ordering::SeqCst),
        windows: windows.clone(),
        target_address,
        target: target.clone(),
        elements: keyed_elements,
    };
    let previous = since.and_then(|id| {
        let history = HISTORY.get()?.lock().ok()?;
        history.iter().find(|s| s.id == id).map(StoredSnap::clone_shallow)
    });

    let mut snap = match &previous {
        Some(prev) => delta_snapshot(prev, &current),
        None => json!({
            "monitors": monitors,
            "windows": windows,
            "target": target,
            "hint": "click-ui --mark N veya --name ile tıkla. Sonraki çağrıda since=<snap> yalnız değişenleri, seq_since=<seq> yalnız yeni pencere olaylarını verir. Arka plan için processes/logs/xray.",
        }),
    };
    snap["snap"] = json!(current.id);
    snap["seq"] = json!(telemetry.next_seq);
    snap["daemon_running"] = json!(daemon_running);
    if since.is_some() && previous.is_none() {
        // Bu süreçte o kimlik yok (ör. daemon yeniden başladı): tam görüntü döndü.
        snap["since_unknown"] = json!(true);
    }
    remember(current);
    if desktop.auth_guard_tripped {
        snap["auth_guard"] = json!(desktop.auth_guard_reason.unwrap_or_default());
    }
    if let Some(since) = seq_since {
        let events: Vec<String> = telemetry
            .recent_events
            .iter()
            .filter(|e| e.seq > since)
            .rev()
            .take(MAX_EVENTS)
            .map(|e| format!("#{} {} {}", e.seq, e.data.kind, truncate(&e.data.payload, 80)))
            .collect();
        snap["events_since"] = json!(events);
    }
    Ok(snap)
}

/// Delta için son snapshot'lar (süreç içi; daemon ve MCP sunucusunda yaşar).
const SNAP_HISTORY: usize = 16;
static NEXT_SNAP: AtomicU64 = AtomicU64::new(1);
static HISTORY: OnceLock<Mutex<VecDeque<StoredSnap>>> = OnceLock::new();

struct StoredSnap {
    id: u64,
    windows: Vec<Value>,
    target_address: Option<String>,
    target: Value,
    elements: Vec<(String, Value)>,
}

impl StoredSnap {
    fn clone_shallow(&self) -> StoredSnap {
        StoredSnap {
            id: self.id,
            windows: self.windows.clone(),
            target_address: self.target_address.clone(),
            target: self.target.clone(),
            elements: self.elements.clone(),
        }
    }
}

fn remember(snap: StoredSnap) {
    let history = HISTORY.get_or_init(Default::default);
    if let Ok(mut h) = history.lock() {
        h.push_back(snap);
        while h.len() > SNAP_HISTORY {
            h.pop_front();
        }
    }
}

/// Yalnız değişenler: eklenen/kaybolan/değişen pencereler ve hedef elemanlar. Hiçbir şey
/// değişmediyse `unchanged: true` ve neredeyse boş bir yanıt döner.
fn delta_snapshot(prev: &StoredSnap, cur: &StoredSnap) -> Value {
    let addr = |w: &Value| w["address"].as_str().unwrap_or_default().to_string();
    let prev_windows: HashMap<String, &Value> = prev.windows.iter().map(|w| (addr(w), w)).collect();
    let cur_windows: HashMap<String, &Value> = cur.windows.iter().map(|w| (addr(w), w)).collect();
    let windows_added: Vec<&Value> = cur.windows.iter().filter(|w| !prev_windows.contains_key(&addr(w))).collect();
    let windows_removed: Vec<String> = prev.windows.iter().map(addr).filter(|a| !cur_windows.contains_key(a)).collect();
    let windows_changed: Vec<&Value> = cur
        .windows
        .iter()
        .filter(|w| prev_windows.get(&addr(w)).map_or(false, |p| *p != *w))
        .collect();

    let mut out = json!({ "delta": true, "since": prev.id });
    let mut changed_any = !windows_added.is_empty() || !windows_removed.is_empty() || !windows_changed.is_empty();
    if !windows_added.is_empty() {
        out["windows_added"] = json!(windows_added);
    }
    if !windows_removed.is_empty() {
        out["windows_removed"] = json!(windows_removed);
    }
    if !windows_changed.is_empty() {
        out["windows_changed"] = json!(windows_changed);
    }

    if prev.target_address != cur.target_address {
        // Hedef pencere değişti: farkın anlamı yok, tam hedef gönderilir.
        out["target"] = cur.target.clone();
        out["target_replaced"] = json!(true);
        changed_any = true;
    } else {
        let prev_els: HashMap<&str, &Value> = prev.elements.iter().map(|(k, v)| (k.as_str(), v)).collect();
        let cur_keys: HashSet<&str> = cur.elements.iter().map(|(k, _)| k.as_str()).collect();
        let added: Vec<&Value> = cur.elements.iter().filter(|(k, _)| !prev_els.contains_key(k.as_str())).map(|(_, v)| v).collect();
        // Mark numarası tek başına değişiklik sayılmaz: üstte bir eleman belirince alttakilerin
        // numarası kayar ama ajan için yeni bir şey yoktur.
        let without_mark = |v: &Value| {
            let mut v = v.clone();
            if let Some(o) = v.as_object_mut() {
                o.remove("mark");
            }
            v
        };
        let changed: Vec<&Value> = cur
            .elements
            .iter()
            .filter(|(k, v)| prev_els.get(k.as_str()).map_or(false, |p| without_mark(p) != without_mark(v)))
            .map(|(_, v)| v)
            .collect();
        let removed: Vec<Value> = prev
            .elements
            .iter()
            .filter(|(k, _)| !cur_keys.contains(k.as_str()))
            .map(|(_, v)| json!({ "role": v["role"], "name": v["name"] }))
            .collect();
        let mut t = json!({ "address": cur.target["address"] });
        for field in ["title", "perception", "terminal", "error"] {
            if cur.target.get(field) != prev.target.get(field) {
                t[field] = cur.target.get(field).cloned().unwrap_or(Value::Null);
                changed_any = true;
            }
        }
        if !added.is_empty() {
            t["added"] = json!(added);
        }
        if !changed.is_empty() {
            t["changed"] = json!(changed);
        }
        if !removed.is_empty() {
            t["removed"] = json!(removed);
        }
        changed_any |= !added.is_empty() || !changed.is_empty() || !removed.is_empty();
        out["target"] = t;
    }
    out["unchanged"] = json!(!changed_any);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(mark: usize, role: &str, name: &str, states: &[&str]) -> UiElement {
        UiElement {
            mark,
            role: role.to_string(),
            name: name.to_string(),
            value: Some("conf:91%".to_string()),
            description: None,
            states: states.iter().map(|s| s.to_string()).collect(),
            actions: vec!["click".to_string()],
            rel_bounds: [0, 0, 10, 10],
            screen_bounds: [100, 100, 10, 10],
            click_point: [105, 105],
            dbus_service: ":1.42".to_string(),
            dbus_path: "/org/a11y/atspi/accessible/7".to_string(),
        }
    }

    #[test]
    fn compact_element_keeps_only_decision_fields() {
        let v = compact_element(&el(3, "check box", "Bülten", &["sensitive", "showing", "checked"]));
        assert_eq!(v["mark"], 3);
        assert_eq!(v["at"], json!([105, 105]));
        assert_eq!(v["states"], json!(["checked"]));
        assert!(v.get("value").is_none(), "OCR güven değeri değer sayılmaz");
        assert!(v.get("dbus_path").is_none() && v.get("rel_bounds").is_none());
    }

    #[test]
    fn icon_guesses_dropped_when_text_exists() {
        let els = vec![el(1, "ocr-text", "Gizli kod", &[]), el(2, "icon-button", "icon:top#2", &[])];
        let (picked, dropped) = select_elements(&els, false, 40);
        assert_eq!((picked.len(), dropped), (1, 1));
        let mut junk = el(3, "ocr-text", "io) «", &[]);
        assert!(is_ocr_noise(&junk));
        junk.name = "Kaydet".to_string();
        junk.value = Some("conf:25%".to_string());
        assert!(is_ocr_noise(&junk), "çok düşük güven");
        junk.value = Some("conf:51%".to_string());
        assert!(!is_ocr_noise(&junk), "orta güvenli metin elenmez, conf ile gösterilir");
        assert_eq!(compact_element(&junk)["conf"], 51);
        junk.value = Some("conf:88%".to_string());
        assert!(!is_ocr_noise(&junk));
        let (picked, _) = select_elements(&els, true, 40);
        assert_eq!(picked.len(), 2);
        // Metin hiç yoksa ikon tahminleri tek ipucu olduğu için tutulur.
        let only_icons = vec![el(1, "icon-button", "icon:a#1", &[])];
        assert_eq!(select_elements(&only_icons, false, 40).0.len(), 1);
    }

    fn stored(id: u64, addr: &str, els: Vec<(&str, Value)>) -> StoredSnap {
        StoredSnap {
            id,
            windows: vec![json!({"address": addr, "title": "A"})],
            target_address: Some(addr.to_string()),
            target: json!({"address": addr, "title": "A"}),
            elements: els.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        }
    }

    #[test]
    fn delta_reports_only_real_changes() {
        let a = json!({"mark": 1, "role": "button", "name": "Kaydet", "at": [10, 10]});
        let b = json!({"mark": 2, "role": "button", "name": "İptal", "at": [10, 40]});
        let prev = stored(1, "0x1", vec![("k1", a.clone()), ("k2", b.clone())]);

        let same = stored(2, "0x1", vec![("k1", a.clone()), ("k2", b.clone())]);
        assert_eq!(delta_snapshot(&prev, &same)["unchanged"], true);

        // Yalnız mark kaydı: değişiklik değil. Yeni eleman: eklendi. Kaybolan: kaldırıldı.
        let mut a2 = a.clone();
        a2["mark"] = json!(5);
        let c = json!({"mark": 1, "role": "label", "name": "Kaydedildi", "at": [10, 70]});
        let next = stored(3, "0x1", vec![("k1", a2), ("k3", c)]);
        let d = delta_snapshot(&prev, &next);
        assert_eq!(d["unchanged"], false);
        assert!(d["target"].get("changed").is_none());
        assert_eq!(d["target"]["added"][0]["name"], "Kaydedildi");
        assert_eq!(d["target"]["removed"][0]["name"], "İptal");

        // Hedef pencere değişti: tam hedef.
        let other = stored(4, "0x2", vec![("k1", a)]);
        assert_eq!(delta_snapshot(&prev, &other)["target_replaced"], true);
    }
}
