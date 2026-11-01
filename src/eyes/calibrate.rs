//! AT-SPI koordinat kalibrasyonu: araç takımının bildirdiği konum ile ekrandaki gerçek konum
//! arasındaki kaymayı OCR ile ölçer.
//!
//! Bazı pencerelerde (ör. libadwaita yapılı zenity diyaloğu) AT-SPI `WINDOW` koordinatları
//! sabit bir miktar kayık geliyor (+24, +32): tıklama satırın değil üstündeki boşluğun üstüne düşüyor.
//! Kayma pencere başına sabittir; tıklama ıskalayınca bir kez OCR ile ölçülür, pencere adresi +
//! geometrisiyle saklanır ve o pencerenin sonraki tüm AT-SPI elemanlarına uygulanır.

use crate::protocol::types::UiElement;
use anyhow::Result;
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

/// Bu kadar küçük kayma OCR ölçüm gürültüsüdür (düzgün bir GTK penceresinde 7 px ölçüldü;
/// gerçek kaymalar istemci tarafı kenar boşlukları kadardır, 20 px üstü).
const NOISE_PX: i64 = 10;
/// Aynı pencere yeniden ölçülmeden önce beklenen süre (bilerek boşluğa tıklayan ajan her seferinde
/// OCR maliyeti ödemesin).
const RECHECK_SECS: u64 = 30;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn store_path() -> std::path::PathBuf {
    crate::eyes::vision::runtime_dir().join("calibration.json")
}

fn load() -> Value {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

/// Pencere adresi kompozitörde yeniden kullanılabiliyor: ajan oturumunda yeni bir pencere, kapanan
/// zenity diyaloğunun adresini ve boyutunu alıp onun kaymasını devraldı (tıklamalar 26 px kaydı).
/// Kayıt PID ile de eşleşmeli.
fn window_pid(address: &str) -> i64 {
    crate::eyes::compositor::capture_desktop_state()
        .ok()
        .and_then(|d| d.windows.into_iter().find(|w| w.address == address).map(|w| w.pid as i64))
        .unwrap_or(0)
}

fn entry_for(address: &str, pid: i64, geom: [i64; 4]) -> Option<Value> {
    load()
        .get(address)
        .filter(|e| e["geom"] == json!(geom) && e["pid"].as_i64() == Some(pid))
        .cloned()
}

fn entry(address: &str, geom: [i64; 4]) -> Option<Value> {
    entry_for(address, window_pid(address), geom)
}

/// Pencere için ölçülmüş kayma (geometri ya da süreç değiştiyse yok sayılır).
pub fn offset_for(address: &str, pid: i64, geom: [i64; 4]) -> Option<(i64, i64)> {
    let e = entry_for(address, pid, geom)?;
    Some((e["dx"].as_i64()?, e["dy"].as_i64()?))
}

pub fn current_offset(address: &str, geom: [i64; 4]) -> Option<(i64, i64)> {
    offset_for(address, window_pid(address), geom)
}

fn store(address: &str, geom: [i64; 4], dx: i64, dy: i64) {
    let pid = window_pid(address);
    let mut all = load();
    if let Some(obj) = all.as_object_mut() {
        // Kapanmış pencerelerin kayıtları birikmesin.
        if obj.len() > 64 {
            obj.clear();
        }
        obj.insert(address.to_string(), json!({ "geom": geom, "pid": pid, "dx": dx, "dy": dy, "checked": now() }));
    }
    crate::eyes::vision::atomic_write_str(&store_path(), &all.to_string());
}

/// Kaymayı elemanlara uygular (yalnız ekran koordinatları; pencereye göreli değerler araç takımınındır).
pub fn apply(elements: &mut [UiElement], (dx, dy): (i64, i64)) {
    if dx == 0 && dy == 0 {
        return;
    }
    for e in elements.iter_mut().filter(|e| !e.dbus_service.is_empty()) {
        e.screen_bounds[0] += dx;
        e.screen_bounds[1] += dy;
        e.click_point[0] += dx;
        e.click_point[1] += dy;
    }
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

fn median(mut v: Vec<i64>) -> i64 {
    v.sort_unstable();
    v[v.len() / 2]
}

/// AT-SPI ve OCR elemanlarını adla eşleyip kaymayı tahmin eder (en az iki eşleşme gerekir).
/// Dikey kayma merkezlerden; yatay kayma, kutusu metne sıkı oturan etiketlerin merkezlerinden
/// (geniş bir etiket kutusunda sola yaslı metin merkezi yanıltır), yoksa sol kenarlardan.
pub fn estimate(atspi: &[UiElement], ocr: &[UiElement]) -> Option<(i64, i64)> {
    let mut dys = Vec::new();
    let mut tight_dx = Vec::new();
    for o in ocr {
        let key = norm(&o.name);
        if key.chars().count() < 3 {
            continue;
        }
        // Aynı adlı (satır, hücre, etiket) adaylardan en küçük kutu metne en yakın olandır.
        let Some(a) = atspi
            .iter()
            .filter(|a| !a.dbus_service.is_empty() && norm(&a.name) == key)
            .min_by_key(|a| a.screen_bounds[2] * a.screen_bounds[3])
        else {
            continue;
        };
        let [ax, ay, aw, ah] = a.screen_bounds;
        let [ox, oy, ow, oh] = o.screen_bounds;
        dys.push((oy + oh / 2) - (ay + ah / 2));
        // Yatay kayma yalnız metne sıkı oturan kutulardan: geniş düğmede metin ortalı, geniş
        // etikette sola yaslı; sol kenar karşılaştırması düğmelerde 239 px hayali kayma verdi.
        if aw <= ow * 3 / 2 + 10 {
            tight_dx.push((ox + ow / 2) - (ax + aw / 2));
        }
    }
    if dys.len() < 2 {
        return None;
    }
    let dx = if tight_dx.len() >= 2 { median(tight_dx) } else { 0 };
    Some((dx, median(dys)))
}

/// Pencereyi OCR ile ölçer ve kaymayı saklar. Dönüş: o anki kayıtlı kaymaya göre ek düzeltme.
/// Yakın zamanda aynı geometriyle ölçüldüyse yeniden ölçmez (`None`).
pub fn recalibrate(address: &str, geom: [i64; 4]) -> Result<Option<(i64, i64)>> {
    if let Some(e) = entry(address, geom) {
        if now().saturating_sub(e["checked"].as_u64().unwrap_or(0)) < RECHECK_SECS {
            return Ok(None);
        }
    }
    calibrate_now(address, geom)
}

/// Pencere bu geometriyle hiç ölçülmediyse ölçer (ilk tıklamadan önce; sonrakiler bedava).
/// Dönüş: yeni bir kayma bulundu (önceden kaydedilmiş mark koordinatları artık eski).
pub fn ensure_calibrated(address: &str, geom: [i64; 4]) -> bool {
    entry(address, geom).is_none() && matches!(calibrate_now(address, geom), Ok(Some(_)))
}

/// Kaymanın bu kadarına kadar ölçülebilmesi için örnek bölge kenarlardan genişletilir.
const MAX_SHIFT_PX: i64 = 64;

/// OCR yalnız birkaç kısa etiketin çevresinde koşar: tüm pencerede tesseract 5 sn sürüyordu.
/// En üstteki birkaç etiket seçilir (aynı satırlarda kalsın, bölge küçük olsun).
fn probe_region(elements: &[UiElement], geom: [i64; 4]) -> Option<[i64; 4]> {
    let mut labels: Vec<&UiElement> = elements
        .iter()
        .filter(|e| !e.dbus_service.is_empty())
        .filter(|e| norm(&e.name).chars().count() >= 3)
        .filter(|e| e.screen_bounds[2] > 0 && e.screen_bounds[2] <= 500 && e.screen_bounds[3] <= 60)
        .collect();
    if labels.len() < 2 {
        return None;
    }
    labels.sort_by_key(|e| (e.screen_bounds[1], e.screen_bounds[0]));
    let pick = &labels[..labels.len().min(4)];
    let x0 = pick.iter().map(|e| e.screen_bounds[0]).min()? - MAX_SHIFT_PX;
    let y0 = pick.iter().map(|e| e.screen_bounds[1]).min()? - MAX_SHIFT_PX;
    let x1 = pick.iter().map(|e| e.screen_bounds[0] + e.screen_bounds[2]).max()? + MAX_SHIFT_PX;
    let y1 = pick.iter().map(|e| e.screen_bounds[1] + e.screen_bounds[3]).max()? + MAX_SHIFT_PX;
    let [wx, wy, ww, wh] = geom;
    let (x0, y0) = (x0.max(wx), y0.max(wy));
    let (x1, y1) = (x1.min(wx + ww), y1.min(wy + wh));
    (x1 - x0 > 8 && y1 - y0 > 8).then_some([x0, y0, x1 - x0, y1 - y0])
}

/// Koşulsuz ölçüm.
pub fn calibrate_now(address: &str, geom: [i64; 4]) -> Result<Option<(i64, i64)>> {
    let current = offset_for(address, window_pid(address), geom).unwrap_or((0, 0));
    // Okunan elemanlara mevcut kayma uygulanmış gelir: ham konuma çevirip mutlak kaymayı ölç.
    // (Ek düzeltme biriktirmek, bir kez yanlış ölçülen kaymayı kalıcı yapıyordu.)
    let mut elements = crate::eyes::ui_atspi::inspect_window_ui(Some(address), None, 200)?.elements;
    apply(&mut elements, (-current.0, -current.1));
    let measured = match probe_region(&elements, geom) {
        Some(region) => estimate(&elements, &crate::eyes::ocr::ocr_region_elements(region, None, 120)?),
        None => None,
    };
    let snap = |v: i64| if v.abs() <= NOISE_PX { 0 } else { v };
    let (dx, dy) = measured.map(|(x, y)| (snap(x), snap(y))).unwrap_or((0, 0));
    store(address, geom, dx, dy);
    Ok(((dx, dy) != current).then_some((dx - current.0, dy - current.1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(name: &str, b: [i64; 4], atspi: bool) -> UiElement {
        UiElement {
            mark: 0,
            role: if atspi { "label".into() } else { "ocr-text".into() },
            name: name.into(),
            value: None,
            description: None,
            states: vec![],
            actions: vec![],
            rel_bounds: b,
            screen_bounds: b,
            click_point: [b[0] + b[2] / 2, b[1] + b[3] / 2],
            dbus_service: if atspi { ":1.5".into() } else { String::new() },
            dbus_path: String::new(),
            container: None,
        }
    }

    #[test]
    fn estimates_constant_offset_from_tight_labels() {
        // zenity'de ölçülen durum: etiketler (+24, +32) kayık, başlık geniş bir kutuda.
        let atspi = vec![
            el("pdf-okuyucu", [0, 147, 82, 17], true),
            el("takvim", [0, 105, 42, 17], true),
            el("Kurulacak paketi seçin", [0, 52, 1552, 17], true),
            el("pdf-okuyucu", [0, 147, 1552, 21], true), // aynı adlı satır: daha büyük kutu
        ];
        let ocr = vec![
            el("pdf-okuyucu", [26, 181, 78, 15], false),
            el("takvim", [26, 139, 40, 15], false),
            el("Kurulacak paketi seçin", [24, 85, 150, 15], false),
        ];
        let (dx, dy) = estimate(&atspi, &ocr).unwrap();
        assert!((dx - 24).abs() <= 2, "dx={}", dx);
        assert!((dy - 32).abs() <= 2, "dy={}", dy);
    }

    #[test]
    fn wide_centered_buttons_do_not_shift_x() {
        let atspi = vec![el("Elma", [0, 82, 506, 31], true), el("Armut", [512, 82, 508, 31], true)];
        let ocr = vec![el("Elma", [235, 90, 40, 15], false), el("Armut", [744, 90, 50, 15], false)];
        assert_eq!(estimate(&atspi, &ocr).map(|(dx, _)| dx), Some(0));
    }

    #[test]
    fn needs_two_matches() {
        let atspi = vec![el("Tamam", [0, 0, 60, 20], true)];
        let ocr = vec![el("Tamam", [10, 10, 50, 16], false)];
        assert!(estimate(&atspi, &ocr).is_none());
    }

    #[test]
    fn apply_shifts_only_atspi_elements() {
        let mut els = vec![el("a", [0, 0, 10, 10], true), el("b", [5, 5, 10, 10], false)];
        apply(&mut els, (3, 4));
        assert_eq!(els[0].click_point, [8, 9]);
        assert_eq!(els[1].click_point, [10, 10]);
    }
}
