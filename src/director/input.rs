use crate::director::guard::enforce_action_guard;
use crate::eyes::compositor::{
    active_window_is_xwayland, capture_desktop_state, hypr_dispatch_cmd, normalize_window_address, HyprDispatch,
};
use crate::director::x11_input::X11Keyboard;
use crate::eyes::ui_atspi::inspect_window_ui;
use crate::eyes::vision::load_persisted_marks;
use crate::protocol::types::UiElement;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::Duration;

const BTN_LEFT: u32 = 0x110; // 272
const BTN_RIGHT: u32 = 0x111; // 273
const BTN_MIDDLE: u32 = 0x112; // 274

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionReceipt {
    pub ok: bool,
    pub action: String,
    pub target: String,
    pub coordinates: Option<[i64; 2]>,
    pub details: String,
    /// Eylem öncesi/sonrası görüntü karşılaştırması; `ok` yalnız eylemin gönderildiğini söyler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<ActionVerification>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionVerification {
    pub region: [i64; 4],
    pub changed_pixels: u64,
    pub change_pct: f64,
    /// `false` ise hedef bölgede hiçbir şey değişmedi: tıklama büyük olasılıkla ıskaladı.
    pub visible_change: bool,
}

/// Bir pikselin "değişti" sayılması için kanal farkı; sıkıştırma/titreme gürültüsünü eler.
const PIXEL_DELTA: u8 = 32;
/// Odak halkası gibi küçük geri bildirimler de değişim sayılsın diye düşük tutulur.
const MIN_CHANGED_PIXELS: u64 = 30;
/// Tıklamadan sonra değişim aranan en uzun süre (yalnız ıskalamada tamamen beklenir).
const VERIFY_MAX_MS: u64 = 350;
/// Değişim görüldükten sonra bir kare daha: yarım çizilmiş ara kare yerine oturan durum ölçülsün.
const VERIFY_POLL_MS: u64 = 40;

fn compare_frames(region: [i64; 4], before: &image::RgbImage, after: &image::RgbImage) -> ActionVerification {
    let total = (before.width() as u64 * before.height() as u64).max(1);
    let changed = if before.dimensions() != after.dimensions() {
        total
    } else {
        before
            .pixels()
            .zip(after.pixels())
            .filter(|(a, b)| a.0.iter().zip(b.0.iter()).any(|(x, y)| x.abs_diff(*y) > PIXEL_DELTA))
            .count() as u64
    };
    ActionVerification {
        region,
        changed_pixels: changed,
        change_pct: (changed as f64 * 10000.0 / total as f64).round() / 100.0,
        visible_change: changed >= MIN_CHANGED_PIXELS,
    }
}

/// Eylemi çalıştırır ve bölgenin önce/sonra görüntüsünü karşılaştırır (yakalama başarısızsa doğrulama yok).
fn with_verification<F>(region: [i64; 4], act: F) -> Result<(ActionReceipt, Option<ActionVerification>)>
where
    F: FnOnce() -> Result<ActionReceipt>,
{
    let before = crate::eyes::vision::grab_region_ppm(region).ok();
    with_verification_from(region, before, act)
}

/// "Önce" karesi hazırsa (bkz. `stable_frame`) onu kullanır. Sonrası sabit 250 ms beklemek yerine
/// ~40 ms'de bir yoklanır: değişim görülüp bir kare boyunca oturunca biter. Tıklama başına ~1 sn'nin
/// büyük kısmı sabit beklemelerdi (iz: click_ui ortalama 1065 ms).
fn with_verification_from<F>(
    region: [i64; 4],
    before: Option<image::RgbImage>,
    act: F,
) -> Result<(ActionReceipt, Option<ActionVerification>)>
where
    F: FnOnce() -> Result<ActionReceipt>,
{
    let receipt = act()?;
    let verification = before.and_then(|b| {
        let start = std::time::Instant::now();
        let mut last: Option<(ActionVerification, image::RgbImage)> = None;
        loop {
            thread::sleep(Duration::from_millis(VERIFY_POLL_MS));
            let frame = crate::eyes::vision::grab_region_ppm(region).ok()?;
            let v = compare_frames(region, &b, &frame);
            if v.visible_change {
                if let Some((_, prev)) = &last {
                    // Önceki kareyle aynıysa değişim oturmuştur.
                    if !compare_frames(region, prev, &frame).visible_change {
                        return Some(v);
                    }
                }
            }
            let done = start.elapsed() >= Duration::from_millis(VERIFY_MAX_MS);
            last = Some((v, frame));
            if done {
                return last.map(|(v, _)| v);
            }
        }
    });
    Ok((receipt, verification))
}

/// Bölge durulana kadar (iki ardışık kare aynı) kare alır, en çok `max_ms`; son kareyi döner.
fn stable_frame(region: [i64; 4], max_ms: u64) -> Option<image::RgbImage> {
    let start = std::time::Instant::now();
    let mut prev = crate::eyes::vision::grab_region_ppm(region).ok()?;
    loop {
        thread::sleep(Duration::from_millis(30));
        let cur = crate::eyes::vision::grab_region_ppm(region).ok()?;
        if !compare_frames(region, &prev, &cur).visible_change || start.elapsed() >= Duration::from_millis(max_ms) {
            return Some(cur);
        }
        prev = cur;
    }
}

fn attach_verification(mut receipt: ActionReceipt, verification: Option<ActionVerification>) -> ActionReceipt {
    if let Some(v) = &verification {
        if !v.visible_change {
            receipt.details = format!(
                "UYARI: eylemden sonra hedef bölgede görünür değişiklik olmadı; hedef ıskalanmış olabilir, `vision` ile doğrula. {}",
                receipt.details
            );
        }
    }
    receipt.verification = verification;
    receipt
}

const ACTIONABLE_ROLES: &[&str] = &[
    "push button", "button", "toggle button", "check box", "radio button", "menu item",
    "combo box", "link", "page tab", "text", "entry", "password text", "spin button", "slider",
    "switch", "list item", "text box", "menu button",
];

fn is_editable(e: &UiElement) -> bool {
    matches!(e.role.as_str(), "text" | "entry" | "text box" | "password text" | "spin button")
        || e.states.iter().any(|s| s == "editable")
}

/// Değer seçilen kontroller (açılır liste ve onu açan düğme).
pub fn is_chooser(e: &UiElement) -> bool {
    matches!(e.role.as_str(), "combo box" | "menu button" | "toggle button" | "list box")
}

fn is_numeric_control(e: &UiElement) -> bool {
    matches!(e.role.as_str(), "spin button" | "slider")
}

/// Form deseni: etiket solda, kontrol aynı satırda sağda. GTK4 açılır listesinin erişilebilir adı
/// seçili değerdir ("Açık"), etiket ("Tema") ayrı bir `label`; ada göre hedeflenince kontrol değil
/// etiket bulunuyordu.
pub fn control_beside(elements: &[UiElement], label: &UiElement, want: fn(&UiElement) -> bool) -> Option<UiElement> {
    let [lx, ly, lw, lh] = label.screen_bounds;
    let lcy = ly + lh / 2;
    elements
        .iter()
        .filter(|e| e.mark != label.mark && want(e))
        .filter(|e| {
            let [ex, ey, _, eh] = e.screen_bounds;
            let ecy = ey + eh / 2;
            ex >= lx + lw / 2 && (ecy - lcy).abs() <= lh.max(eh) / 2 + 6
        })
        .min_by_key(|e| e.screen_bounds[0] - lx)
        .cloned()
}

/// `select` hedefi: adı taşıyan eleman bir seçici değilse yanındaki seçicinin mark'ı.
pub fn resolve_chooser(window_addr: Option<&str>, name: &str) -> Option<usize> {
    let ui = inspect_window_ui(window_addr, None, 150).ok()?;
    let found = pick_named_element(&ui.elements, name)?;
    if is_chooser(&found) {
        return None;
    }
    control_beside(&ui.elements, &found, is_chooser).map(|c| c.mark)
}

/// Aynı adlı adaylar farklı kaplardaysa (Dolphin'de "Yerler" panelindeki Belgeler = kullanıcının ev
/// klasörü, açık klasördeki Belgeler = hedef) kabı pencere başlığında geçen tercih edilir: içerik
/// görünümleri çoğunlukla açık belgenin/klasörün adını taşır.
fn pick_named_element_in(elements: &[UiElement], query: &str, window_title: &str) -> Option<UiElement> {
    let q = query.to_lowercase();
    let title = window_title.to_lowercase();
    let rank = |e: &UiElement| {
        let name = e.name.to_lowercase();
        let name_rank = if name == q { 0 } else if name.contains(&q) { 1 } else { 2 };
        let role_rank = if ACTIONABLE_ROLES.contains(&e.role.as_str()) { 0 } else { 1 };
        let container_rank = match &e.container {
            Some(c) if !c.is_empty() && title.contains(&c.to_lowercase()) => 0,
            None => 1,
            Some(_) => 2,
        };
        (name_rank, role_rank, container_rank, e.mark)
    };
    elements.iter().min_by_key(|e| rank(e)).cloned()
}

/// Ada göre en uygun elemanı seçer: tam eşleşme > içerme, aynı adlılar arasında eylem alabilen rol
/// (düğme, onay kutusu…) kapsayıcıdan (`grid cell`, `label`, `filler`) önce gelir.
fn pick_named_element(elements: &[UiElement], query: &str) -> Option<UiElement> {
    let q = query.to_lowercase();
    let rank = |e: &UiElement| {
        let name = e.name.to_lowercase();
        let name_rank = if name == q { 0 } else if name.contains(&q) { 1 } else { 2 };
        let role_rank = if ACTIONABLE_ROLES.contains(&e.role.as_str()) { 0 } else { 1 };
        (name_rank, role_rank, e.mark)
    };
    elements.iter().min_by_key(|e| rank(e)).cloned()
}

fn verification_region(window_geometry: Option<[i64; 4]>, cx: i64, cy: i64) -> [i64; 4] {
    if let Some(g) = window_geometry.filter(|g| g[2] > 0 && g[3] > 0) {
        return g;
    }
    // Mark ile hedeflenince pencere geometrisi bilinmez. Tıklama noktası çevresindeki küçük bir
    // kutu yetmez: pencere genişliğindeki bir onay kutusunun merkezi sağda, işaret kutucuğu
    // solda kalır ve değişim hiç görülmez. Noktayı içeren pencerenin tamamı doğrulanır.
    if let Ok(desk) = capture_desktop_state() {
        let active_ws: Vec<i64> = desk.monitors.iter().map(|m| m.active_workspace_id).collect();
        if let Some(w) = desk
            .windows
            .iter()
            .filter(|w| {
                w.mapped
                    && !w.hidden
                    && active_ws.contains(&w.workspace_id)
                    && cx >= w.x
                    && cx < w.x + w.width
                    && cy >= w.y
                    && cy < w.y + w.height
            })
            .min_by_key(|w| (!w.focused, !w.floating, w.width * w.height))
        {
            return [w.x, w.y, w.width, w.height];
        }
    }
    [(cx - 300).max(0), (cy - 150).max(0), 600, 300]
}

/// Fare düğmesi adını Linux evdev koduna çevirir.
pub fn parse_mouse_button(btn: &str) -> u32 {
    match btn.to_ascii_lowercase().as_str() {
        "right" | "r" | "2" => BTN_RIGHT,
        "middle" | "mid" | "m" | "3" => BTN_MIDDLE,
        _ => BTN_LEFT,
    }
}

/// İmleci Hyprland kompozitörünün kendi `movecursor` komutuyla tam küresel koordinata taşır.
pub fn move_cursor(x: i64, y: i64) -> Result<ActionReceipt> {
    enforce_action_guard(None, false)?;
    hypr_dispatch_cmd(&HyprDispatch::CursorMove { x: x, y: y })?;
    Ok(ActionReceipt {
        ok: true,
        action: "move_cursor".to_string(),
        target: format!("{},{}", x, y),
        coordinates: Some([x, y]),
        details: format!("İmleç ({}, {}) konumuna taşındı", x, y),
        verification: None,
    })
}

/// (x, y) noktasındaki görünür pencereyi bulur; odakta değilse odaklar ve `activewindow` eşleşene dek bekler.
fn focus_window_at(x: i64, y: i64) {
    let Ok(desk) = capture_desktop_state() else { return };
    let active_ws: Vec<i64> = desk.monitors.iter().map(|m| m.active_workspace_id).collect();
    let target = desk
        .windows
        .iter()
        .filter(|w| {
            w.mapped
                && !w.hidden
                && active_ws.contains(&w.workspace_id)
                && x >= w.x
                && x < w.x + w.width
                && y >= w.y
                && y < w.y + w.height
        })
        .min_by_key(|w| (!w.focused, !w.floating, w.width * w.height));
    let Some(win) = target else { return };
    if win.focused {
        return;
    }
    if hypr_dispatch_cmd(&HyprDispatch::FocusWindow(win.address.clone())).is_err() {
        return;
    }
    for _ in 0..25 {
        let cur = crate::eyes::compositor::hypr_query_json("activewindow")
            .ok()
            .and_then(|v| v.get("address").and_then(|a| a.as_str().map(String::from)));
        if cur.as_deref() == Some(win.address.as_str()) {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    // Odak yeni alınan pencere bir süre yeniden boyanır (odak halkası, aktif başlık): bu değişim
    // tıklama doğrulamasının "önce" karesine girmeden bitsin, yoksa ıskalama "değişti" görünür.
    let _ = crate::director::wait::wait_for_condition("visual_settle", None, Some(&win.address), 500);
}

/// Belirtilen koordinata (veya mevcut imleç konumuna) Wayland `zwlr_virtual_pointer_v1` üzerinden tıklar.
pub fn click_at(
    x: Option<i64>,
    y: Option<i64>,
    button: &str,
    clicks: u32,
    allow_protected: bool,
) -> Result<ActionReceipt> {
    enforce_action_guard(None, allow_protected)?;
    if let (Some(cx), Some(cy)) = (x, y) {
        // Yeni açılmış (özellikle XWayland) pencerede ilk tık odak tıkı olarak yutulur:
        // önce hedef pencereyi odakla, odak oturana kadar bekle, sonra imleci gerçek konuma götür.
        focus_window_at(cx, cy);
        // Ön hareket: imleç girişini (enter) istemciye kaydettir, sonra asıl noktaya otur.
        hypr_dispatch_cmd(&HyprDispatch::CursorMove { x: cx + 1, y: cy })?;
        thread::sleep(Duration::from_millis(15));
        hypr_dispatch_cmd(&HyprDispatch::CursorMove { x: cx, y: cy })?;
        thread::sleep(Duration::from_millis(30));
    }

    let btn_code = parse_mouse_button(button);
    let mut wl = WaylandVirtualInput::connect()?;
    for i in 0..clicks.max(1) {
        if i > 0 {
            thread::sleep(Duration::from_millis(45));
        }
        wl.pointer_button(btn_code, 1)?;
        thread::sleep(Duration::from_millis(20));
        wl.pointer_button(btn_code, 0)?;
    }

    let desktop = capture_desktop_state()?;
    let coords = [desktop.cursor_x, desktop.cursor_y];
    Ok(ActionReceipt {
        ok: true,
        action: "click".to_string(),
        target: format!("{} ({}x)", button, clicks.max(1)),
        coordinates: Some(coords),
        details: format!(
            "({}, {}) konumuna {} tıklandı",
            coords[0], coords[1], button
        ),
        verification: None,
    })
}

/// Başlangıç `(x1, y1)` noktasından `(x2, y2)` noktasına yumuşak adımlarla sürükle-bırak yapar.
pub fn drag_mouse(
    x1: i64,
    y1: i64,
    x2: i64,
    y2: i64,
    button: &str,
    allow_protected: bool,
) -> Result<ActionReceipt> {
    enforce_action_guard(None, allow_protected)?;
    let btn_code = parse_mouse_button(button);
    hypr_dispatch_cmd(&HyprDispatch::CursorMove { x: x1, y: y1 })?;
    thread::sleep(Duration::from_millis(25));

    let mut wl = WaylandVirtualInput::connect()?;
    wl.pointer_button(btn_code, 1)?;
    thread::sleep(Duration::from_millis(25));

    let steps = 12i64;
    for s in 1..=steps {
        let ix = x1 + (x2 - x1) * s / steps;
        let iy = y1 + (y2 - y1) * s / steps;
        hypr_dispatch_cmd(&HyprDispatch::CursorMove { x: ix, y: iy })?;
        thread::sleep(Duration::from_millis(12));
    }

    wl.pointer_button(btn_code, 0)?;
    Ok(ActionReceipt {
        ok: true,
        action: "drag".to_string(),
        target: format!("({},{}) -> ({},{})", x1, y1, x2, y2),
        coordinates: Some([x2, y2]),
        details: "Sürükle-bırak tamamlandı".to_string(),
        verification: None,
    })
}

/// Fare tekerleği kaydırması (`dy`: pozitif aşağı, negatif yukarı; `dx`: yatay).
pub fn scroll_wheel(
    x: Option<i64>,
    y: Option<i64>,
    dx: i32,
    dy: i32,
    allow_protected: bool,
) -> Result<ActionReceipt> {
    enforce_action_guard(None, allow_protected)?;
    if let (Some(cx), Some(cy)) = (x, y) {
        hypr_dispatch_cmd(&HyprDispatch::CursorMove { x: cx, y: cy })?;
        thread::sleep(Duration::from_millis(15));
    }

    let mut wl = WaylandVirtualInput::connect()?;
    if dy != 0 {
        wl.pointer_scroll(0, dy)?;
    }
    if dx != 0 {
        wl.pointer_scroll(1, dx)?;
    }

    Ok(ActionReceipt {
        ok: true,
        action: "scroll".to_string(),
        target: format!("dx={}, dy={}", dx, dy),
        coordinates: x.zip(y).map(|(a, b)| [a, b]),
        details: "Tekerlek kaydırma gönderildi".to_string(),
        verification: None,
    })
}

/// Semantik UI elemanına (`--name` veya Set-of-Marks `--mark` numarasına) doğrudan tıklar.
pub fn click_ui_element(
    window_addr: Option<&str>,
    name_query: Option<&str>,
    mark_id: Option<usize>,
    button: &str,
    allow_protected: bool,
) -> Result<ActionReceipt> {
    click_ui_element_ext(
        window_addr,
        name_query,
        mark_id,
        button,
        false,
        None,
        allow_protected,
    )
}

/// Semantik UI elemanına tıklar, D-Bus `DoAction` tetikler veya `set_text` ile doğrudan içine metin yazar.
pub fn click_ui_element_ext(
    window_addr: Option<&str>,
    name_query: Option<&str>,
    mark_id: Option<usize>,
    button: &str,
    prefer_dbus: bool,
    set_text: Option<&str>,
    allow_protected: bool,
) -> Result<ActionReceipt> {
    click_ui_element_n(window_addr, name_query, mark_id, button, prefer_dbus, set_text, allow_protected, 1)
}

/// `clicks: 2` çift tıklama: dosya yöneticisinde klasör açmak tek tıkla seçmekten ayrı bir eylem
/// (ajan önce tıklayıp sonra koordinatla çift tıklıyordu: fazladan tur).
#[allow(clippy::too_many_arguments)]
pub fn click_ui_element_n(
    window_addr: Option<&str>,
    name_query: Option<&str>,
    mark_id: Option<usize>,
    button: &str,
    prefer_dbus: bool,
    set_text: Option<&str>,
    allow_protected: bool,
    clicks: u32,
) -> Result<ActionReceipt> {
    enforce_action_guard(window_addr, allow_protected)?;

    // Pencerenin AT-SPI koordinatları ilk tıklamadan önce bir kez OCR ile doğrulanır (bkz.
    // `calibrate`): kayık bir araç takımında "görünür değişiklik" (üzerine gelme vurgusu) olsa
    // bile tıklama yanlış satıra düşebiliyordu ve bu ıskalama algılanamıyordu.
    let mut window_shifted = false;
    let recalibrated = !prefer_dbus
        && inspect_window_ui(window_addr, None, 10).map_or(false, |ui| {
            let changed = crate::eyes::calibrate::ensure_calibrated(&ui.window_address, ui.window_geometry);
            window_shifted = crate::eyes::calibrate::current_offset(&ui.window_address, ui.window_geometry)
                .map_or(false, |o| o != (0, 0));
            changed
        });

    let (matched, window_geometry): (UiElement, Option<[i64; 4]>) = if let Some(m_id) = mark_id {
        let marks = if recalibrated { Vec::new() } else { load_persisted_marks() };
        if let Some(found) = marks.into_iter().find(|e| e.mark == m_id) {
            (found, None)
        } else {
            let ui = inspect_window_ui(window_addr, None, 80)?;
            let geom = ui.window_geometry;
            let found = ui
                .elements
                .into_iter()
                .find(|e| e.mark == m_id)
                .ok_or_else(|| anyhow!("Mark #{} bulunamadı", m_id))?;
            (found, Some(geom))
        }
    } else if let Some(q) = name_query {
        let ui = inspect_window_ui(window_addr, Some(q), 80)?;
        let mut found = pick_named_element_in(&ui.elements, q, &ui.window_title).ok_or_else(|| {
            anyhow!(
                "`{}` adında erişilebilir UI elemanı bulunamadı (Görsel Set-of-Marks için `pc-eye vision --marks` çalıştırın)",
                q
            )
        })?;
        // Metin yazılacak ama ad bir etikete denk geldiyse aynı satırdaki düzenlenebilir alan hedeftir.
        if set_text.is_some() && !is_editable(&found) {
            if let Ok(full) = inspect_window_ui(window_addr, None, 150) {
                if let Some(c) = control_beside(&full.elements, &found, is_editable) {
                    found = c;
                }
            }
        }
        (found, Some(ui.window_geometry))
    } else {
        return Err(anyhow!("`click-ui` için `--name` veya `--mark` belirtilmelidir"));
    };

    // Sayfanın görünmeyen kısmındaki eleman önce görünür alana kaydırılır, sonra yeni konumu okunur.
    let mut matched = matched;
    if matched.states.iter().any(|s| s == "offscreen") && !matched.dbus_service.is_empty() {
        if crate::eyes::ui_atspi::atspi_scroll_into_view(&matched.dbus_service, &matched.dbus_path) {
            thread::sleep(Duration::from_millis(200));
            if let Ok(ui) = inspect_window_ui(window_addr, None, 400) {
                if let Some(e) = ui.elements.into_iter().find(|e| e.dbus_path == matched.dbus_path) {
                    matched = e;
                }
            }
        }
    }
    let [cx, cy] = matched.click_point;
    let target = format!("[{}] {} ({})", matched.mark, matched.name, matched.role);
    let region = verification_region(window_geometry, cx, cy);

    if let Some(txt) = set_text {
        let has_dbus = !matched.dbus_service.is_empty();
        // Sayısal kontrol: değer doğrudan yazılır (metin + Enter yedeği aşağıda).
        if has_dbus && is_numeric_control(&matched) {
            if let Ok(v) = txt.trim().replace(',', ".").parse::<f64>() {
                if crate::eyes::ui_atspi::atspi_set_value(&matched.dbus_service, &matched.dbus_path, v) {
                    return Ok(ActionReceipt {
                        ok: true,
                        action: "set_ui_value_dbus".to_string(),
                        target: target.clone(),
                        coordinates: Some([cx, cy]),
                        details: format!("AT-SPI Value ile değer {} yapıldı (geri okunarak doğrulandı)", txt),
                        verification: None,
                    });
                }
            }
        }
        let reads_back = |want: &str| {
            has_dbus
                && crate::eyes::ui_atspi::atspi_read_text(&matched.dbus_service, &matched.dbus_path)
                    .as_deref()
                    == Some(want)
        };
        let (receipt, verification) = with_verification(region, || {
            // SetTextContents bazı araç takımlarında (Firefox) "başarılı" döner ama alanın gerçek
            // değerini değiştirmez; ancak geri okunan metin tutarsa D-Bus yolu kabul edilir.
            if has_dbus
                && crate::eyes::ui_atspi::atspi_set_editable_text(
                    &matched.dbus_service,
                    &matched.dbus_path,
                    txt,
                )
                .unwrap_or(false)
                && reads_back(txt)
            {
                return Ok(ActionReceipt {
                    ok: true,
                    action: "set_ui_text_dbus".to_string(),
                    target: target.clone(),
                    coordinates: Some([cx, cy]),
                    details: format!("D-Bus EditableText ile `{}` metni yazıldı (geri okunarak doğrulandı)", txt),
                    verification: None,
                });
            }
            // Gerçek klavye girdisi uygulamanın kendi olaylarını tetikler. Ctrl+A ile mevcut içerik
            // seçilir ki "yaz" eklemek değil değiştirmek olsun.
            click_at(Some(cx), Some(cy), "left", 1, allow_protected)?;
            thread::sleep(Duration::from_millis(25));
            press_key_combo("ctrl+a", window_addr, allow_protected)?;
            type_text(txt, window_addr, allow_protected)?;
            if is_numeric_control(&matched) {
                // Spin button yazılan metni ancak Enter ile değere çevirir.
                press_key_combo("Return", window_addr, allow_protected)?;
            }
            let confirmed = reads_back(txt);
            Ok(ActionReceipt {
                ok: true,
                action: "set_ui_text_pointer".to_string(),
                target: target.clone(),
                coordinates: Some([cx, cy]),
                details: if confirmed {
                    format!("Kontrole tıklanıp `{}` metni yazıldı (geri okunarak doğrulandı)", txt)
                } else {
                    format!("Kontrole tıklanıp `{}` metni yazıldı (geri okunamadı; görsel doğrulamaya bak)", txt)
                },
                verification: None,
            })
        })?;
        return Ok(attach_verification(receipt, verification));
    }

    // Eylemi olmayan bir kapsayıcıda DoAction yanlış olumlu verir; yalnız eylem alabilen rollerde dene.
    // Kaymalı (kalibre edilmiş) pencerede tek bir pencere kayması her kaba uymayabilir (zenity'de
    // içerik kayık, alt düğme çubuğu değil): düğmeler koordinatsız D-Bus eylemiyle tıklanır.
    let auto_dbus = window_shifted
        && clicks == 1
        && matches!(matched.role.as_str(), "push button" | "button" | "toggle button" | "check box" | "radio button");
    let use_dbus = (prefer_dbus || auto_dbus)
        && !matched.dbus_service.is_empty()
        && ACTIONABLE_ROLES.contains(&matched.role.as_str());
    if !use_dbus {
        // "Önce" karesine tıklamadan bağımsız değişimler girmesin ki doğrulama hedefin kendi
        // tepkisini ölçsün: headless çıkışta imleç kareye çizilir, üzerine gelme vurgusu oluşur ve
        // odaksız pencere ilk tıklamada pasif→aktif görünüme geçip yeniden boyanır.
        let _ = hypr_dispatch_cmd(&HyprDispatch::CursorMove { x: cx, y: cy });
        focus_window_at(cx, cy);
        // İmleç ışınlaması istemciye hareket olayı vermez: önceki görevin tıkladığı yerde açılan
        // pencerede düğme üzerine gelme vurgusu tıklamanın düğme olayıyla kalkıyor (~1000 piksel)
        // ve ıskalanan tıklama "değişti" görünüyordu. Gerçek bir hareket "önce" karesinden önce.
        let _ = WaylandVirtualInput::connect().and_then(|mut wl| wl.pointer_nudge());
        thread::sleep(Duration::from_millis(30));
    }
    // Üzerine gelme vurgusu gibi tepkiler bitene kadar bekle; son kare "önce" karesidir.
    let before = if use_dbus {
        crate::eyes::vision::grab_region_ppm(region).ok()
    } else {
        stable_frame(region, 400)
    };

    let (receipt, verification) = with_verification_from(region, before, || {
        if use_dbus {
            if let Ok(true) = crate::eyes::ui_atspi::atspi_invoke_do_action(
                &matched.dbus_service,
                &matched.dbus_path,
                0,
            ) {
                return Ok(ActionReceipt {
                    ok: true,
                    action: "click_ui_dbus".to_string(),
                    target: target.clone(),
                    coordinates: Some([cx, cy]),
                    details: format!(
                        "UI elemanı `{}` [mark={}] fare hareket ettirilmeden doğrudan D-Bus DoAction(0) ile tetiklendi",
                        matched.name, matched.mark
                    ),
                    verification: None,
                });
            }
        }

        click_at(Some(cx), Some(cy), button, clicks.max(1), allow_protected)?;
        Ok(ActionReceipt {
            ok: true,
            action: if clicks > 1 { "double_click_ui".to_string() } else { "click_ui".to_string() },
            target: target.clone(),
            coordinates: Some([cx, cy]),
            details: format!(
                "UI elemanı `{}` [mark={}] ({}, {}) koordinatında tıklandı",
                matched.name, matched.mark, cx, cy
            ),
            verification: None,
        })
    })?;

    // Iskaladıysa nedeni çoğu zaman araç takımının kayık koordinatıdır: pencereyi OCR ile ölç,
    // kayma bulunursa düzeltilmiş noktaya bir kez daha tıkla. Sonraki okumalar da düzeltilmiş gelir.
    let missed = verification.as_ref().map_or(false, |v| !v.visible_change);
    if missed && !use_dbus && !matched.dbus_service.is_empty() {
        if let Some((addr, geom)) = window_containing(cx, cy) {
            if let Ok(Some((dx, dy))) = crate::eyes::calibrate::recalibrate(&addr, geom) {
                let (nx, ny) = (cx + dx, cy + dy);
                let _ = hypr_dispatch_cmd(&HyprDispatch::CursorMove { x: nx, y: ny });
                thread::sleep(Duration::from_millis(100));
                let (retry, retry_verification) = with_verification(region, || {
                    click_at(Some(nx), Some(ny), button, 1, allow_protected)?;
                    Ok(ActionReceipt {
                        ok: true,
                        action: "click_ui".to_string(),
                        target: target.clone(),
                        coordinates: Some([nx, ny]),
                        details: format!(
                            "UI elemanı `{}` [mark={}] ({}, {}) koordinatında tıklandı (AT-SPI konumu ({}, {}) kayıktı; OCR ile ölçülen düzeltme {:+}, {:+})",
                            matched.name, matched.mark, nx, ny, cx, cy, dx, dy
                        ),
                        verification: None,
                    })
                })?;
                return Ok(attach_verification(retry, retry_verification));
            }
        }
    }
    Ok(attach_verification(receipt, verification))
}

/// Noktayı içeren görünür pencere (adres, geometri).
fn window_containing(x: i64, y: i64) -> Option<(String, [i64; 4])> {
    let desk = capture_desktop_state().ok()?;
    let active_ws: Vec<i64> = desk.monitors.iter().map(|m| m.active_workspace_id).collect();
    desk.windows
        .iter()
        .filter(|w| w.mapped && !w.hidden && active_ws.contains(&w.workspace_id))
        .filter(|w| x >= w.x && x < w.x + w.width && y >= w.y && y < w.y + w.height)
        .min_by_key(|w| w.width * w.height)
        .map(|w| (w.address.clone(), [w.x, w.y, w.width, w.height]))
}

/// Unicode-güvenli metin yazma (`zwp_virtual_keyboard_v1` dinamik XKB keymap ile Türkçe karakterler dahil).
pub fn type_text(
    text: &str,
    window_addr: Option<&str>,
    allow_protected: bool,
) -> Result<ActionReceipt> {
    enforce_action_guard(window_addr, allow_protected)?;
    if let Some(addr) = window_addr {
        let norm = normalize_window_address(addr);
        hypr_dispatch_cmd(&HyprDispatch::FocusWindow(norm))?;
        thread::sleep(Duration::from_millis(30));
    }

    let mut path = "wayland";
    if active_window_is_xwayland() {
        match X11Keyboard::connect().and_then(|kb| kb.type_text(text)) {
            Ok(()) => path = "x11-xtest",
            Err(e) => {
                eprintln!("pc-eye: X11 yazma yolu başarısız ({e:#}); Wayland sanal klavyesine düşülüyor");
                WaylandVirtualInput::connect()?.type_unicode_string(text)?;
            }
        }
    } else {
        WaylandVirtualInput::connect()?.type_unicode_string(text)?;
    }

    Ok(ActionReceipt {
        ok: true,
        action: "type_text".to_string(),
        target: window_addr.unwrap_or("focused").to_string(),
        coordinates: None,
        details: format!("{} karakter yazıldı ({})", text.chars().count(), path),
        verification: None,
    })
}

/// Ajanın doğrudan gönderdiği tuş/yazma eyleminden sonra uygulamanın tepkisini bekler (en çok
/// ~360 ms). Ajanlar Ctrl+L ve yazmayı art arda gönderiyor: adres çubuğu düzenleme kipine
/// geçmeden gelen metin başka yere yazılıyor, Dolphin'de taşıma bu yüzden sessizce başarısız
/// oluyordu. Piksel tabanı ajanda adımlar arası ekran görüntüsü bu beklemeyi kendiliğinden yapıyor.
pub fn settle_after_input() {
    thread::sleep(Duration::from_millis(60));
    let active = capture_desktop_state().ok().and_then(|d| d.active_window_address);
    let _ = crate::director::wait::wait_for_condition("visual_settle", None, active.as_deref(), 300);
}

/// Klavye kısayolu veya tuş kombinasyonu gönderir (`ctrl+l`, `Return`, `Escape`, `ctrl+shift+t` vb.).
pub fn press_key_combo(
    combo: &str,
    window_addr: Option<&str>,
    allow_protected: bool,
) -> Result<ActionReceipt> {
    enforce_action_guard(window_addr, allow_protected)?;
    if let Some(addr) = window_addr {
        let norm = normalize_window_address(addr);
        hypr_dispatch_cmd(&HyprDispatch::FocusWindow(norm))?;
        thread::sleep(Duration::from_millis(25));
    }

    if active_window_is_xwayland() {
        if let Err(e) = X11Keyboard::connect().and_then(|kb| kb.send_combo(combo)) {
            eprintln!("pc-eye: X11 tuş yolu başarısız ({e:#}); Wayland sanal klavyesine düşülüyor");
            WaylandVirtualInput::connect()?.send_key_combo(combo)?;
        }
    } else {
        WaylandVirtualInput::connect()?.send_key_combo(combo)?;
    }

    Ok(ActionReceipt {
        ok: true,
        action: "press_key".to_string(),
        target: combo.to_string(),
        coordinates: None,
        details: format!("Tuş kombinasyonu gönderildi: {}", combo),
        verification: None,
    })
}

// ============================================================================
// Saf Rust Wayland Wire Protokol İstemcisi (`zwlr_virtual_pointer_v1` & `zwp_virtual_keyboard_v1`)
// ============================================================================

struct WaylandVirtualInput {
    stream: UnixStream,
    ptr_id: u32,
    kbd_id: u32,
    next_id: u32,
}

impl WaylandVirtualInput {
    fn connect() -> Result<Self> {
        let runtime_dir =
            env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".to_string());
        let display = env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-1".to_string());
        let sock_path = format!("{}/{}", runtime_dir, display);

        let mut stream = UnixStream::connect(&sock_path)
            .with_context(|| format!("Wayland soketine bağlanılamadı: {}", sock_path))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;

        // 1. wl_display(1).get_registry(opcode=1, new_id=2) + wl_display(1).sync(opcode=0, new_id=3)
        let mut init_pkt = Vec::new();
        write_wl_msg(&mut init_pkt, 1, 1, &[2]);
        write_wl_msg(&mut init_pkt, 1, 0, &[3]);
        stream.write_all(&init_pkt)?;

        let mut buf = vec![0u8; 65536];
        let mut collected = Vec::new();
        loop {
            let n = stream.read(&mut buf)?;
            if n == 0 {
                break;
            }
            collected.extend_from_slice(&buf[..n]);
            // wl_callback(3).done(opcode=0) gelene kadar oku
            if contains_callback_done(&collected, 3) {
                break;
            }
        }

        let globals = parse_registry_globals(&collected);
        let seat_global = globals
            .iter()
            .find(|g| g.interface == "wl_seat")
            .ok_or_else(|| anyhow!("Wayland wl_seat bulunamadı"))?;
        let ptr_global = globals
            .iter()
            .find(|g| g.interface == "zwlr_virtual_pointer_manager_v1")
            .ok_or_else(|| anyhow!("Wayland zwlr_virtual_pointer_manager_v1 bulunamadı"))?;
        let kbd_global = globals
            .iter()
            .find(|g| g.interface == "zwp_virtual_keyboard_manager_v1")
            .ok_or_else(|| anyhow!("Wayland zwp_virtual_keyboard_manager_v1 bulunamadı"))?;

        let seat_id = 4u32;
        let ptr_mgr_id = 5u32;
        let kbd_mgr_id = 6u32;
        let ptr_id = 7u32;
        let kbd_id = 8u32;
        let sync_id = 9u32;

        let mut bind_pkt = Vec::new();
        write_registry_bind(
            &mut bind_pkt,
            2,
            seat_global.name,
            "wl_seat",
            seat_global.version.min(7),
            seat_id,
        );
        write_registry_bind(
            &mut bind_pkt,
            2,
            ptr_global.name,
            "zwlr_virtual_pointer_manager_v1",
            ptr_global.version.min(2),
            ptr_mgr_id,
        );
        write_registry_bind(
            &mut bind_pkt,
            2,
            kbd_global.name,
            "zwp_virtual_keyboard_manager_v1",
            1,
            kbd_mgr_id,
        );

        // create_virtual_pointer(seat_id, ptr_id) -> opcode 0
        write_wl_msg(&mut bind_pkt, ptr_mgr_id, 0, &[seat_id, ptr_id]);
        // create_virtual_keyboard(seat_id, kbd_id) -> opcode 0
        write_wl_msg(&mut bind_pkt, kbd_mgr_id, 0, &[seat_id, kbd_id]);
        // sync
        write_wl_msg(&mut bind_pkt, 1, 0, &[sync_id]);

        stream.write_all(&bind_pkt)?;
        Self::wait_sync(&mut stream, sync_id)?;

        Ok(Self {
            stream,
            ptr_id,
            kbd_id,
            next_id: 10,
        })
    }

    fn wait_sync(stream: &mut UnixStream, sync_id: u32) -> Result<()> {
        let mut buf = [0u8; 8192];
        let mut acc = Vec::new();
        loop {
            let n = stream.read(&mut buf)?;
            if n == 0 {
                break;
            }
            acc.extend_from_slice(&buf[..n]);
            if contains_callback_done(&acc, sync_id) {
                break;
            }
        }
        Ok(())
    }

    fn flush_sync(&mut self) -> Result<()> {
        let sync_id = self.next_id;
        self.next_id += 1;
        let mut pkt = Vec::new();
        write_wl_msg(&mut pkt, 1, 0, &[sync_id]);
        self.stream.write_all(&pkt)?;
        Self::wait_sync(&mut self.stream, sync_id)
    }

    fn pointer_button(&mut self, button: u32, state: u32) -> Result<()> {
        let mut pkt = Vec::new();
        // zwlr_virtual_pointer_v1.button (opcode 2): time, button, state
        write_wl_msg(&mut pkt, self.ptr_id, 2, &[wl_time_ms(), button, state]);
        // zwlr_virtual_pointer_v1.frame (opcode 4)
        write_wl_msg(&mut pkt, self.ptr_id, 4, &[]);
        self.stream.write_all(&pkt)?;
        self.flush_sync()
    }

    /// Gerçek bir göreli hareket (+1 piksel, geri): istemci imlecin yerini ancak bir hareket olayıyla
    /// öğrenir; kompozitörün imleç ışınlaması (`CursorMove`) olay üretmiyor.
    fn pointer_nudge(&mut self) -> Result<()> {
        let mut pkt = Vec::new();
        let one = 256u32; // wl_fixed 1.0
        // zwlr_virtual_pointer_v1.motion (opcode 0): time, dx, dy
        write_wl_msg(&mut pkt, self.ptr_id, 0, &[wl_time_ms(), one, 0]);
        write_wl_msg(&mut pkt, self.ptr_id, 4, &[]);
        write_wl_msg(&mut pkt, self.ptr_id, 0, &[wl_time_ms(), (-256i32) as u32, 0]);
        write_wl_msg(&mut pkt, self.ptr_id, 4, &[]);
        self.stream.write_all(&pkt)?;
        self.flush_sync()
    }

    fn pointer_scroll(&mut self, axis: u32, discrete_steps: i32) -> Result<()> {
        let mut pkt = Vec::new();
        // axis_source (opcode 5): 0 (wheel)
        write_wl_msg(&mut pkt, self.ptr_id, 5, &[0]);
        // axis_discrete (opcode 7): time, axis, value (wl_fixed = steps * 15 * 256), discrete
        let fixed_val = (discrete_steps * 15 * 256) as u32;
        write_wl_msg(
            &mut pkt,
            self.ptr_id,
            7,
            &[wl_time_ms(), axis, fixed_val, discrete_steps as u32],
        );
        // frame (opcode 4)
        write_wl_msg(&mut pkt, self.ptr_id, 4, &[]);
        self.stream.write_all(&pkt)?;
        self.flush_sync()
    }

    fn type_unicode_string(&mut self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        // Her parçada en çok SAFE_EVDEV.len() benzersiz karakter: her biri bir "güvenli" fiziksel
        // tuş koduna bağlanır (bkz. SAFE_EVDEV). Daha uzun metinler parça parça, her parça kendi
        // keymap'iyle yazılır.
        let chars: Vec<char> = text.chars().collect();
        let mut start = 0;
        while start < chars.len() {
            let mut unique_chars: Vec<char> = Vec::new();
            let mut end = start;
            while end < chars.len() {
                let ch = chars[end];
                if !unique_chars.contains(&ch) {
                    if unique_chars.len() == SAFE_EVDEV.len() {
                        break;
                    }
                    unique_chars.push(ch);
                }
                end += 1;
            }
            let keymap_str = build_dynamic_xkb_keymap(&unique_chars, &[]);
            self.upload_keymap(&keymap_str)?;
            self.flush_sync()?;

            let mut pkt = Vec::new();
            let mut t = wl_time_ms();
            for &ch in &chars[start..end] {
                let idx = unique_chars.iter().position(|&c| c == ch).unwrap_or(0);
                let evdev_keycode = SAFE_EVDEV[idx];
                write_wl_msg(&mut pkt, self.kbd_id, 1, &[t, evdev_keycode, 1]);
                t += 5;
                write_wl_msg(&mut pkt, self.kbd_id, 1, &[t, evdev_keycode, 0]);
                t += 5;
            }
            self.stream.write_all(&pkt)?;
            self.flush_sync()?;
            start = end;
        }
        Ok(())
    }

    /// Kısayol gönderir. SDL3/oyun istemcileri tuşları evdev scancode'undan okur ve modifier'ları
    /// gerçek tuş olaylarından izler; bu yüzden her tuş fiziksel evdev koduyla, modifier'lar da
    /// gerçek Ctrl/Shift/Alt/Super tuş basışlarıyla gönderilir (`modifiers` isteği kullanılmaz).
    fn send_key_combo(&mut self, combo: &str) -> Result<()> {
        let parts: Vec<&str> = combo.split('+').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
        // (keysym adı, evdev kodu)
        let mut mods: Vec<(String, u32)> = Vec::new();
        let mut keys: Vec<(String, u32)> = Vec::new();
        let mut fallback = 200u32;
        let add_mod = |mods: &mut Vec<(String, u32)>, name: &str, code: u32| {
            if !mods.iter().any(|(n, _)| n == name) {
                mods.push((name.to_string(), code));
            }
        };
        for part in parts {
            match part.to_ascii_lowercase().as_str() {
                "shift" => add_mod(&mut mods, "Shift_L", 42),
                "ctrl" | "control" => add_mod(&mut mods, "Control_L", 29),
                "alt" | "mod1" => add_mod(&mut mods, "Alt_L", 56),
                "super" | "win" | "logo" | "mod4" => add_mod(&mut mods, "Super_L", 125),
                _ => {
                    let mut name = normalize_keysym_name(part);
                    let mut chars = name.chars();
                    if let (Some(c), None) = (chars.next(), chars.next()) {
                        if c.is_ascii_uppercase() {
                            add_mod(&mut mods, "Shift_L", 42);
                            name = c.to_ascii_lowercase().to_string();
                        }
                    }
                    let code = evdev_for_keysym_name(&name).unwrap_or_else(|| {
                        fallback += 1;
                        fallback
                    });
                    keys.push((name, code));
                }
            }
        }
        if keys.is_empty() {
            return Err(anyhow!("Kombinasyonda tuş yok: `{}`", combo));
        }

        let mut entries: Vec<(String, u32)> = mods.clone();
        entries.extend(keys.iter().cloned());
        let keymap_str = build_evdev_xkb_keymap(&entries);
        self.upload_keymap(&keymap_str)?;

        // Gerçek XKB modifier indeksleri sabittir: Shift=0, Lock=1, Control=2, Mod1=3, Mod4=6.
        let mask: u32 = mods
            .iter()
            .map(|(name, _)| match name.as_str() {
                "Shift_L" => 1 << 0,
                "Control_L" => 1 << 2,
                "Alt_L" => 1 << 3,
                "Super_L" => 1 << 6,
                _ => 0,
            })
            .sum();

        let mut pkt = Vec::new();
        let mut t = wl_time_ms();
        for (_, code) in &mods {
            write_wl_msg(&mut pkt, self.kbd_id, 1, &[t, *code, 1]);
            t += 10;
        }
        // GTK/Firefox modifier durumunu kompozitörün wl_keyboard.modifiers olayından okur; Hyprland
        // bunu sanal klavyede yalnız `modifiers` isteğinden (opcode 2) üretir. Yalnız fiziksel tuş
        // gönderildiğinde Ctrl+A, "a" olarak yazılıyordu. SDL3 için fiziksel basışlar da kalır.
        if mask != 0 {
            write_wl_msg(&mut pkt, self.kbd_id, 2, &[mask, 0, 0, 0]);
        }
        for (_, code) in &keys {
            write_wl_msg(&mut pkt, self.kbd_id, 1, &[t, *code, 1]);
            t += 30;
            write_wl_msg(&mut pkt, self.kbd_id, 1, &[t, *code, 0]);
            t += 10;
        }
        for (_, code) in mods.iter().rev() {
            write_wl_msg(&mut pkt, self.kbd_id, 1, &[t, *code, 0]);
            t += 10;
        }
        if mask != 0 {
            write_wl_msg(&mut pkt, self.kbd_id, 2, &[0, 0, 0, 0]);
        }
        self.stream.write_all(&pkt)?;
        self.flush_sync()
    }

    fn upload_keymap(&mut self, keymap_text: &str) -> Result<()> {
        let mut bytes = keymap_text.as_bytes().to_vec();
        bytes.push(0); // Null-terminated XKB string
        let size = bytes.len() as u32;

        let fd = create_memfd_with_bytes(&bytes)?;
        // zwp_virtual_keyboard_v1.keymap (opcode 0): format=1 (XKB_V1), fd (ancillary), size
        let mut msg = Vec::new();
        write_wl_msg(&mut msg, self.kbd_id, 0, &[1, size]);
        send_msg_with_fd(self.stream.as_raw_fd(), &msg, fd)?;
        unsafe {
            libc::close(fd);
        }
        self.flush_sync()
    }
}

/// Keysym adının ABD QWERTY düzenindeki fiziksel evdev kodu (SDL scancode eşlemesi için).
fn evdev_for_keysym_name(name: &str) -> Option<u32> {
    let code = match name {
        "Escape" => 1,
        "BackSpace" => 14,
        "Tab" => 15,
        "Return" => 28,
        "space" => 57,
        "Delete" => 111,
        "Insert" => 110,
        "Home" => 102,
        "End" => 107,
        "Prior" => 104,
        "Next" => 109,
        "Up" => 103,
        "Left" => 105,
        "Right" => 106,
        "Down" => 108,
        "F11" => 87,
        "F12" => 88,
        "-" => 12,
        "=" => 13,
        "[" => 26,
        "]" => 27,
        ";" => 39,
        "'" => 40,
        "`" => 41,
        "\\" => 43,
        "," => 51,
        "." => 52,
        "/" => 53,
        n if n.len() >= 2 && n.starts_with('F') => match n[1..].parse::<u32>() {
            Ok(k @ 1..=10) => 58 + k,
            _ => return None,
        },
        n if n.len() == 1 => {
            let c = n.chars().next().unwrap();
            match c {
                '1'..='9' => 2 + (c as u32 - '1' as u32),
                '0' => 11,
                'a'..='z' => {
                    const ROWS: &str = "qwertyuiop";
                    const ROW2: &str = "asdfghjkl";
                    const ROW3: &str = "zxcvbnm";
                    if let Some(i) = ROWS.find(c) {
                        16 + i as u32
                    } else if let Some(i) = ROW2.find(c) {
                        30 + i as u32
                    } else {
                        44 + ROW3.find(c)? as u32
                    }
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(code)
}

/// Her girdiyi kendi evdev koduna (xkb kodu = evdev + 8) bağlayan, modifier_map'li dinamik keymap.
fn build_evdev_xkb_keymap(entries: &[(String, u32)]) -> String {
    let mut keycodes = String::new();
    let mut symbols = String::new();
    let mut modmap = String::new();
    let mut max_kc = 9;
    for (idx, (sym, code)) in entries.iter().enumerate() {
        let kc = code + 8;
        max_kc = max_kc.max(kc);
        keycodes.push_str(&format!("        <K{}> = {};\n", idx, kc));
        let sym_name = if sym.chars().count() == 1 && !sym.chars().next().unwrap().is_ascii_alphanumeric() {
            char_to_xkb_keysym(sym.chars().next().unwrap())
        } else {
            sym.clone()
        };
        symbols.push_str(&format!("        key <K{}> {{ [ {} ] }};\n", idx, sym_name));
        let m = match sym.as_str() {
            "Shift_L" => Some("Shift"),
            "Control_L" => Some("Control"),
            "Alt_L" => Some("Mod1"),
            "Super_L" => Some("Mod4"),
            _ => None,
        };
        if let Some(m) = m {
            modmap.push_str(&format!("        modifier_map {} {{ <K{}> }};\n", m, idx));
        }
    }
    format!(
        "xkb_keymap {{\n\
         xkb_keycodes \"pceye\" {{\n\
             minimum = 8;\n\
             maximum = {};\n\
         {}\
         }};\n\
         xkb_types \"pceye\" {{\n\
             virtual_modifiers NumLock;\n\
             type \"ONE_LEVEL\" {{\n\
                 modifiers = none;\n\
                 level_name[Level1] = \"Any\";\n\
             }};\n\
         }};\n\
         xkb_compatibility \"pceye\" {{\n\
             interpret Any + AnyOf(all) {{ action = SetMods(modifiers=modMapMods); }};\n\
         }};\n\
         xkb_symbols \"pceye\" {{\n\
         {}\
         {}\
         }};\n\
         }};\n",
        max_kc,
        keycodes,
        symbols,
        modmap
    )
}

fn normalize_keysym_name(raw: &str) -> String {
    match raw.to_ascii_lowercase().as_str() {
        "enter" | "return" => "Return".to_string(),
        "esc" | "escape" => "Escape".to_string(),
        "tab" => "Tab".to_string(),
        "space" => "space".to_string(),
        "backspace" | "bs" => "BackSpace".to_string(),
        "delete" | "del" => "Delete".to_string(),
        "up" => "Up".to_string(),
        "down" => "Down".to_string(),
        "left" => "Left".to_string(),
        "right" => "Right".to_string(),
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        "pageup" | "pgup" => "Prior".to_string(),
        "pagedown" | "pgdn" => "Next".to_string(),
        s if s.starts_with('f') && s[1..].parse::<u8>().is_ok() => s.to_ascii_uppercase(),
        _ => raw.to_string(),
    }
}

fn char_to_xkb_keysym(ch: char) -> String {
    match ch {
        '\n' | '\r' => "Return".to_string(),
        '\t' => "Tab".to_string(),
        ' ' => "space".to_string(),
        c => format!("U{:04X}", c as u32),
    }
}

/// Metin yazarken karakterlerin bağlandığı fiziksel (evdev) tuş kodları: yalnız ana bloktaki harf,
/// rakam ve noktalama tuşları. Sıralı 1, 2, 3… kodları kullanılınca 14. karakter Backspace'in,
/// 15. karakter Tab'ın, 1. karakter Esc'in koduna düşüyordu; Qt (Kate) bu tuşları fiziksel kodundan
/// tanıyıp karakteri yutuyordu ("(H9EC)" → "(9EC)", "Şğİ" → "ğİ").
const SAFE_EVDEV: [u32; 48] = [
    2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, // 1..0 - =
    16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, // q..]
    30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, // a..`
    43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 57, // \ z../ space
];

fn build_dynamic_xkb_keymap(chars: &[char], named_keysyms: &[String]) -> String {
    let mut all_syms: Vec<String> = chars.iter().map(|&c| char_to_xkb_keysym(c)).collect();
    all_syms.extend_from_slice(named_keysyms);
    if all_syms.is_empty() {
        all_syms.push("NoSymbol".to_string());
    }
    all_syms.truncate(SAFE_EVDEV.len());

    let max_kc = 8 + *SAFE_EVDEV.iter().max().unwrap() as usize;
    let mut keycode_lines = String::new();
    let mut symbol_lines = String::new();

    for (idx, sym) in all_syms.iter().enumerate() {
        let kc = 8 + SAFE_EVDEV[idx] as usize;
        keycode_lines.push_str(&format!("        <K{}> = {};\n", idx, kc));
        symbol_lines.push_str(&format!("        key <K{}> {{ [ {} ] }};\n", idx, sym));
    }

    format!(
        "xkb_keymap {{\n\
         xkb_keycodes \"pceye\" {{\n\
             minimum = 8;\n\
             maximum = {};\n\
         {}\
         }};\n\
         xkb_types \"pceye\" {{\n\
             virtual_modifiers NumLock;\n\
             type \"ONE_LEVEL\" {{\n\
                 modifiers = none;\n\
                 level_name[Level1] = \"Any\";\n\
             }};\n\
         }};\n\
         xkb_compatibility \"pceye\" {{\n\
             interpret Any + AnyOf(all) {{ action = SetMods(modifiers=modMapMods); }};\n\
         }};\n\
         xkb_symbols \"pceye\" {{\n\
         {}\
         }};\n\
         }};\n",
        max_kc.max(9),
        keycode_lines,
        symbol_lines
    )
}

fn create_memfd_with_bytes(bytes: &[u8]) -> Result<RawFd> {
    let name = CString::new("pceye_keymap")?;
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(anyhow!("memfd_create başarısız oldu"));
    }
    let written = unsafe { libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len()) };
    if written < 0 || (written as usize) != bytes.len() {
        unsafe { libc::close(fd) };
        return Err(anyhow!("memfd yazma hatası"));
    }
    Ok(fd)
}

fn send_msg_with_fd(sock_fd: RawFd, data: &[u8], pass_fd: RawFd) -> Result<()> {
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };

    let cmsg_space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_space];

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_space;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as usize;
        let data_ptr = libc::CMSG_DATA(cmsg) as *mut RawFd;
        std::ptr::write_unaligned(data_ptr, pass_fd);

        let sent = libc::sendmsg(sock_fd, &msg, 0);
        if sent < 0 {
            return Err(anyhow!("sendmsg (SCM_RIGHTS) başarısız oldu"));
        }
    }
    Ok(())
}

#[derive(Debug)]
struct WlGlobal {
    name: u32,
    interface: String,
    version: u32,
}

/// Wayland girdi olaylarının zaman damgası: compositor'ın kullandığı `CLOCK_MONOTONIC` ms.
/// Sabit bir değer (eski `100`) gerçek cihaz olaylarının gerisinde kalır; WebKitGTK gibi
/// istemciler zamanda geri giden tekerlek olaylarını sessizce atar.
fn wl_time_ms() -> u32 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: geçerli bir timespec işaretçisi; CLOCK_MONOTONIC her Linux'ta vardır.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000) as u32
}

fn write_wl_msg(buf: &mut Vec<u8>, obj_id: u32, opcode: u16, args: &[u32]) {
    let size = (8 + args.len() * 4) as u32;
    buf.extend_from_slice(&obj_id.to_ne_bytes());
    buf.extend_from_slice(&((size << 16) | (opcode as u32)).to_ne_bytes());
    for &arg in args {
        buf.extend_from_slice(&arg.to_ne_bytes());
    }
}

fn write_registry_bind(
    buf: &mut Vec<u8>,
    registry_id: u32,
    global_name: u32,
    interface: &str,
    version: u32,
    new_id: u32,
) {
    let str_bytes = interface.as_bytes();
    let str_len_with_nul = (str_bytes.len() + 1) as u32;
    let padded_str_len = ((str_len_with_nul + 3) & !3) as usize;
    let total_size = (8 + 4 + 4 + padded_str_len + 4 + 4) as u32;

    buf.extend_from_slice(&registry_id.to_ne_bytes());
    buf.extend_from_slice(&(total_size << 16).to_ne_bytes()); // opcode 0 = bind
    buf.extend_from_slice(&global_name.to_ne_bytes());
    buf.extend_from_slice(&str_len_with_nul.to_ne_bytes());
    buf.extend_from_slice(str_bytes);
    buf.resize(buf.len() + (padded_str_len - str_bytes.len()), 0);
    buf.extend_from_slice(&version.to_ne_bytes());
    buf.extend_from_slice(&new_id.to_ne_bytes());
}

fn contains_callback_done(buf: &[u8], callback_id: u32) -> bool {
    let mut pos = 0;
    while pos + 8 <= buf.len() {
        let sender = u32::from_ne_bytes(buf[pos..pos + 4].try_into().unwrap());
        let word2 = u32::from_ne_bytes(buf[pos + 4..pos + 8].try_into().unwrap());
        let size = (word2 >> 16) as usize;
        let opcode = (word2 & 0xFFFF) as u16;
        if size < 8 || pos + size > buf.len() {
            break;
        }
        if sender == callback_id && opcode == 0 {
            return true;
        }
        pos += size;
    }
    false
}

fn parse_registry_globals(buf: &[u8]) -> Vec<WlGlobal> {
    let mut res = Vec::new();
    let mut pos = 0;
    while pos + 8 <= buf.len() {
        let sender = u32::from_ne_bytes(buf[pos..pos + 4].try_into().unwrap());
        let word2 = u32::from_ne_bytes(buf[pos + 4..pos + 8].try_into().unwrap());
        let size = (word2 >> 16) as usize;
        let opcode = (word2 & 0xFFFF) as u16;
        if size < 8 || pos + size > buf.len() {
            break;
        }
        // wl_registry(2).global(opcode=0): name: uint, interface: string, version: uint
        if sender == 2 && opcode == 0 && size >= 20 {
            let name = u32::from_ne_bytes(buf[pos + 8..pos + 12].try_into().unwrap());
            let str_len = u32::from_ne_bytes(buf[pos + 12..pos + 16].try_into().unwrap()) as usize;
            let padded = (str_len + 3) & !3;
            if pos + 16 + padded + 4 <= pos + size && str_len > 0 {
                let s_bytes = &buf[pos + 16..pos + 16 + str_len - 1];
                let iface = String::from_utf8_lossy(s_bytes).to_string();
                let v_off = pos + 16 + padded;
                let version = u32::from_ne_bytes(buf[v_off..v_off + 4].try_into().unwrap());
                res.push(WlGlobal {
                    name,
                    interface: iface,
                    version,
                });
            }
        }
        pos += size;
    }
    res
}

#[cfg(test)]
mod evdev_tests {
    use super::*;

    #[test]
    fn evdev_codes_match_us_qwerty() {
        assert_eq!(evdev_for_keysym_name("s"), Some(31));
        assert_eq!(evdev_for_keysym_name("q"), Some(16));
        assert_eq!(evdev_for_keysym_name("m"), Some(50));
        assert_eq!(evdev_for_keysym_name("Return"), Some(28));
        assert_eq!(evdev_for_keysym_name("F5"), Some(63));
        assert_eq!(evdev_for_keysym_name("0"), Some(11));
        assert_eq!(evdev_for_keysym_name("Bogus"), None);
    }

    #[test]
    fn keymap_binds_real_codes_and_modifier_map() {
        let km = build_evdev_xkb_keymap(&[("Control_L".into(), 29), ("s".into(), 31)]);
        assert!(km.contains("<K0> = 37;") && km.contains("<K1> = 39;"));
        assert!(km.contains("modifier_map Control { <K0> };"));
    }
}

#[cfg(test)]
mod click_ui_tests {
    use super::*;

    fn el(mark: usize, role: &str, name: &str) -> UiElement {
        UiElement {
            mark,
            role: role.to_string(),
            name: name.to_string(),
            value: None,
            description: None,
            states: Vec::new(),
            actions: Vec::new(),
            rel_bounds: [0, 0, 10, 10],
            screen_bounds: [0, 0, 10, 10],
            click_point: [5, 5],
            dbus_service: String::new(),
            dbus_path: String::new(),
            container: None,
        }
    }

    #[test]
    fn button_beats_container_with_same_name() {
        let els = vec![el(1, "grid cell", "Şeftali"), el(2, "label", "Şeftali"), el(3, "button", "Şeftali")];
        assert_eq!(pick_named_element(&els, "şeftali").unwrap().mark, 3);
    }

    #[test]
    fn exact_name_beats_substring_and_turkish_case_folds() {
        let els = vec![el(1, "button", "İptal et"), el(2, "label", "İPTAL")];
        assert_eq!(pick_named_element(&els, "iptal").map(|e| e.mark), Some(1));
        assert_eq!(pick_named_element(&els, "İPTAL").unwrap().mark, 2);
        assert!(pick_named_element(&[], "x").is_none());
    }

    #[test]
    fn frame_comparison_ignores_noise_and_counts_real_change() {
        let before = image::RgbImage::from_pixel(20, 20, image::Rgb([100, 100, 100]));
        let mut noisy = before.clone();
        noisy.put_pixel(0, 0, image::Rgb([110, 100, 100]));
        let v = compare_frames([0, 0, 20, 20], &before, &noisy);
        assert_eq!(v.changed_pixels, 0);
        assert!(!v.visible_change);

        let mut changed = before.clone();
        for x in 0..20 {
            for y in 0..2 {
                changed.put_pixel(x, y, image::Rgb([255, 255, 255]));
            }
        }
        let v = compare_frames([0, 0, 20, 20], &before, &changed);
        assert_eq!(v.changed_pixels, 40);
        assert!(v.visible_change);
        assert_eq!(v.change_pct, 10.0);
    }
}
