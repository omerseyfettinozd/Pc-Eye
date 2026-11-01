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
fn select_elements(elements: &[UiElement], include_icons: bool, max: usize) -> (Vec<Value>, usize) {
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
        .map(compact_element)
        .collect();
    (picked, dropped)
}

pub fn build_snapshot(
    window: Option<&str>,
    max_elements: usize,
    include_icons: bool,
    seq_since: Option<u64>,
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
    let target = match inspect_window_ui(window, None, max_elements.saturating_mul(2).max(20)) {
        Ok(ui) => {
            let (elements, dropped_icons) = select_elements(&ui.elements, include_icons, max_elements);
            let mut t = json!({
                "address": ui.window_address,
                "title": truncate(&ui.window_title, 80),
                "class": ui.window_class,
                "perception": ui.perception_mode,
                "elements": elements,
            });
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

    let mut snap = json!({
        "seq": telemetry.next_seq,
        "daemon_running": daemon_running,
        "monitors": monitors,
        "windows": windows,
        "target": target,
        "hint": "click-ui --mark N veya --name ile tıkla; sonraki çağrıda seq_since=<seq> yalnız yeni olayları verir. Arka plan için processes/logs/xray.",
    });
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
}
