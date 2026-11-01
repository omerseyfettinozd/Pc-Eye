use crate::daemon::ring_buffer::now_ms;
use crate::eyes::atspi_bus;
use crate::eyes::compositor::{capture_desktop_state, resolve_window_ref};
use crate::eyes::process_xray::collect_app_cluster;
use crate::protocol::types::{TerminalContext, UiElement, UiTreeReport, WindowInfo};
use anyhow::{anyhow, Result};
use serde_json::Value;
use futures_util::future::join_all;
use std::collections::HashSet;
use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

const TERMINAL_CLASSES: &[&str] = &[
    "kitty",
    "alacritty",
    "foot",
    "wezterm",
    "org.wezfurlong.wezterm",
    "ghostty",
    "konsole",
    "org.kde.konsole",
    "gnome-terminal",
    "xterm",
    "urxvt",
    "terminator",
    "tilix",
];

/// K1 Kuralı: Hyprland oturumunda AT-SPI2 erişilebilirlik veri yolunu (`IsEnabled = true`) zorunlu olarak etkinleştirir.
pub fn ensure_atspi_enabled() -> bool {
    let status = Command::new("busctl")
        .args([
            "--user",
            "set-property",
            "org.a11y.Bus",
            "/org/a11y/bus",
            "org.a11y.Status",
            "IsEnabled",
            "b",
            "true",
        ])
        .output();
    status.map(|o| o.status.success()).unwrap_or(false)
}

/// AT-SPI2 özel veri yolu adresini (`unix:path=/run/user/1000/at-spi/bus_0`) çözümler.
pub fn resolve_atspi_bus_address() -> Option<String> {
    let out = Command::new("busctl")
        .args([
            "--user",
            "call",
            "org.a11y.Bus",
            "/org/a11y/bus",
            "org.a11y.Bus",
            "GetAddress",
            "--json=short",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let val: Value = serde_json::from_slice(&out.stdout).ok()?;
    val.get("data")?
        .as_array()?
        .first()?
        .as_str()
        .map(|s| s.to_string())
}

pub fn is_terminal_window(class: &str) -> bool {
    let lower = class.to_ascii_lowercase();
    TERMINAL_CLASSES.iter().any(|t| lower.contains(t))
}

/// Bir UI elemanının "anlamlı içerik" taşıyıp taşımadığını söyler: boş olmayan `name` VEYA boş olmayan `value`.
fn element_has_content(el: &UiElement) -> bool {
    !el.name.trim().is_empty()
        || el
            .value
            .as_deref()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
}

/// Anlamlı içerikli eleman sayısı (boş `name` + `value: None` düğümler "trivial" sayılır).
pub fn count_contentful_elements(elements: &[UiElement]) -> usize {
    elements.iter().filter(|e| element_has_content(e)).count()
}

/// OCR geri dönüş kapısı (saf fonksiyon; birim test edilebilir).
///
/// Kural:
/// - Terminal penceresinde sorgu yoksa OCR'a DÜŞME (terminal PTY bağlamı yeterli).
/// - Aksi halde AT-SPI ağacı "anlamlı içerik" taşımıyorsa OCR dene: toplam eleman `<= 1` VEYA
///   içerikli (name/value dolu) eleman sayısı `<= 2`.
fn needs_ocr_fallback(elements: &[UiElement], is_terminal: bool, has_query: bool) -> bool {
    if is_terminal && !has_query {
        return false;
    }
    elements.len() <= 1 || count_contentful_elements(elements) <= 2
}

/// Göz 2a: Hedef pencere (veya odaktaki pencere) için Semantik UI Ağacı + Durum/Değer + Koordinat Çözümlemesi + Terminal PTY Bağlamı üretir.
pub fn inspect_window_ui(
    target_window_addr: Option<&str>,
    query_filter: Option<&str>,
    max_elements: usize,
) -> Result<UiTreeReport> {
    let ts = now_ms();
    let a11y_bus_enabled = ensure_atspi_enabled();
    let desktop = capture_desktop_state()?;

    let win: WindowInfo = if let Some(addr) = target_window_addr {
        let norm = resolve_window_ref(addr, &desktop.windows)?;
        desktop
            .windows
            .into_iter()
            .find(|w| w.address == norm)
            .ok_or_else(|| anyhow!("Pencere bulunamadı: {}", norm))?
    } else if let Some(active_addr) = desktop.active_window_address.as_deref() {
        desktop
            .windows
            .into_iter()
            .find(|w| w.address == active_addr)
            .ok_or_else(|| anyhow!("Aktif pencere bulunamadı"))?
    } else {
        return Err(anyhow!("Masaüstünde seçili veya aktif pencere yok"));
    };

    let win_geom = [win.x, win.y, win.width, win.height];

    let terminal_context = if is_terminal_window(&win.class) && win.pid > 0 {
        Some(extract_terminal_context(win.pid, win_geom))
    } else {
        None
    };

    let mut elements = Vec::new();
    let mut used_ocr = false;
    if let Ok(conn) = atspi_bus::connection() {
        // Tarayıcılar sayfa yüklendikten sonra belge ağacını gecikmeli yayınlar; o arada yalnız
        // araç çubuğu görünür. Belge henüz yoksa kısa aralıklarla birkaç kez yeniden oku.
        let browser = is_browser_window(&win.class);
        for attempt in 0..4 {
            let (found, saw_document) = atspi_bus::block_on(collect_atspi_elements_for_window(
                conn,
                &win,
                query_filter,
                max_elements.max(10),
            ));
            elements = found;
            // Belge varlığı filtreden bağımsız ölçülür: `--name "Ad Soyad"` gibi sorgular da beklemeli.
            if !browser || saw_document || attempt == 3 {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    // V4 Yükseltmesi (TRIVIAL ağaç düzeltmesi): AT-SPI2 ağacı "boş" olmasa bile ANLAMLI İÇERİK taşımıyorsa
    // (ör. WebKitGTK/Tauri yalnızca tek bir "frame" düğümü yayınlar; ad/değer yok, koordinat yok) OCR geri
    // dönüşünü DENE. Kapı `elements.is_empty()` yerine "içerikli eleman sayısı" ölçütüne bağlandı.
    let atspi_count = elements.len();
    if needs_ocr_fallback(&elements, is_terminal_window(&win.class), query_filter.is_some()) {
        if let Ok(mut ocr_els) = crate::eyes::ocr::ocr_region_elements(
            win_geom,
            query_filter,
            max_elements.max(10),
        ) {
            if !ocr_els.is_empty() {
                // AT-SPI elemanları KORUNUR; OCR mark numaraları onların üstünden devam eder ki
                // `click-ui --mark <N>` hem AT-SPI hem OCR elemanları için çakışmasız çalışsın.
                let base = elements.len();
                for el in ocr_els.iter_mut() {
                    el.mark += base;
                }
                elements.extend(ocr_els);
                used_ocr = true;
            }
        }
    }

    let perception_mode = if used_ocr && atspi_count > 0 {
        "hybrid-atspi-ocr".to_string()
    } else if used_ocr {
        "ocr-semantic".to_string()
    } else if !elements.is_empty() {
        "at-spi2".to_string()
    } else if terminal_context.is_some() {
        "terminal-pty".to_string()
    } else {
        "vision-fallback".to_string()
    };

    let hint = match perception_mode.as_str() {
        "at-spi2" => Some(
            "Semantik UI elemanları (states ve value dahil) hazır. `click-ui --mark <N>` veya `--name <AD>` (veya arka plan için `--dbus-action`) kullanabilirsiniz."
                .to_string(),
        ),
        "hybrid-atspi-ocr" => Some(format!(
            "AT-SPI2 ağacı {} anlamlı eleman taşıdı (trivial) ve {} OCR elemanı ile birleştirildi. \
             Mark numaraları AT-SPI elemanlarından başlayıp OCR ile devam eder; `click-ui --mark <N>` veya `--name <METİN>` ile tıklayın.",
            atspi_count,
            elements.len() - atspi_count
        )),
        "ocr-semantic" => Some(
            "AT-SPI2 ağacı anlamlı içerik taşımıyor (boş ya da yalnızca trivial düğümler) olduğu için Sıfır-Disk Bellek-İçi OCR (eng+tur) ile penceredeki metinler ve tıklama koordinatları çıkarıldı. `click-ui --name <METİN>` veya `--mark <N>` ile doğrudan tıklayabilirsiniz."
                .to_string(),
        ),
        "terminal-pty" => Some(
            "Bu pencere bir terminal emülatörüdür. Ön plandaki kabuk/süreç durumu ve ekrandaki görünür metin satırları (`visible_lines`) `terminal_context` altında verilmiştir."
                .to_string(),
        ),
        _ => Some(
            "Bu uygulama AT-SPI ağacı yayınlamıyor. Görsel inceleme ve otomatik kutulama için `pc-eye vision --window <ADDR> --marks` kullanın."
                .to_string(),
        ),
    };

    if !elements.is_empty() {
        crate::eyes::vision::save_persisted_marks(&elements);
    }

    Ok(UiTreeReport {
        timestamp_ms: ts,
        window_address: win.address,
        window_class: win.class,
        window_title: win.title,
        window_pid: win.pid,
        window_geometry: win_geom,
        a11y_bus_enabled,
        perception_mode,
        element_count: elements.len(),
        elements,
        terminal_context,
        hint,
    })
}

fn extract_terminal_context(term_pid: i32, win_geom: [i64; 4]) -> TerminalContext {
    let (members, _) = collect_app_cluster(term_pid).unwrap_or_default();
    let mut foreground_pid = term_pid;
    let mut foreground_cmd = String::new();
    let mut recent_children = Vec::new();

    for m in &members {
        if m.pid != term_pid {
            recent_children.push(format!("PID {} ({}): {}", m.pid, m.state, m.cmdline));
            if m.pid > foreground_pid {
                foreground_pid = m.pid;
                foreground_cmd = m.cmdline.clone();
            }
        } else if foreground_cmd.is_empty() {
            foreground_cmd = m.cmdline.clone();
        }
    }

    let cwd = fs::read_link(format!("/proc/{}/cwd", foreground_pid))
        .or_else(|_| fs::read_link(format!("/proc/{}/cwd", term_pid)))
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    let pty_path = fs::read_link(format!("/proc/{}/fd/0", foreground_pid))
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| s.contains("/dev/pts/"));

    // Katman 1: Kitty uzaktan kontrol soketi varsa doğrudan metni oku
    let mut visible_lines = read_kitty_socket_lines(term_pid);
    // Katman 2: Soket yoksa terminal penceresinin bellek-içi OCR satırlarını çıkar
    if visible_lines.is_empty() {
        visible_lines = crate::eyes::ocr::ocr_region_lines(win_geom, 35).unwrap_or_default();
    }

    TerminalContext {
        terminal_pid: term_pid,
        foreground_pid,
        foreground_cmd,
        cwd,
        pty_path,
        recent_child_processes: recent_children,
        visible_lines,
    }
}

fn read_kitty_socket_lines(term_pid: i32) -> Vec<String> {
    let candidates = [
        format!("/tmp/kitty-{}", term_pid),
        format!("/run/user/1000/kitty-{}", term_pid),
        "/tmp/mykitty".to_string(),
    ];
    for sock in &candidates {
        if std::path::Path::new(sock).exists() {
            if let Ok(out) = Command::new("kitty")
                .args(["@", "--to", &format!("unix:{}", sock), "get-text"])
                .output()
            {
                if out.status.success() {
                    let text = String::from_utf8_lossy(&out.stdout);
                    let lines: Vec<String> = text
                        .lines()
                        .map(|l| l.trim_end().to_string())
                        .filter(|l| !l.trim().is_empty())
                        .collect();
                    let skip = lines.len().saturating_sub(40);
                    return lines.into_iter().skip(skip).collect();
                }
            }
        }
    }
    Vec::new()
}

/// Okuma bütçesi: bu sınırlar içinde ağaç tam okunur, çıktı sınırı (`max_elements`) ayrıca uygulanır.
/// Eski 85 düğüm sınırı Firefox'ta araç çubuğunda tükenip sayfa içeriğine hiç ulaşmıyordu.
const NODE_BUDGET: usize = 3000;
const TIME_BUDGET: Duration = Duration::from_millis(1500);

struct NodeInfo {
    path: String,
    role: String,
    name: String,
    states: Vec<String>,
    children: Vec<(String, String)>,
    in_document: bool,
}

async fn read_node(conn: &zbus::Connection, service: &str, path: String, in_document: bool) -> NodeInfo {
    let (role, name, states, children) = futures_util::join!(
        atspi_bus::role_name(conn, service, &path),
        atspi_bus::name(conn, service, &path),
        atspi_bus::states(conn, service, &path),
        atspi_bus::children(conn, service, &path),
    );
    NodeInfo {
        path,
        role: role.unwrap_or_else(|| "unknown".to_string()),
        name: name.unwrap_or_default(),
        states,
        children,
        in_document,
    }
}

async fn collect_atspi_elements_for_window(
    conn: &zbus::Connection,
    win: &WindowInfo,
    query_filter: Option<&str>,
    max_elements: usize,
) -> (Vec<UiElement>, bool) {
    let root_children = atspi_bus::children(
        conn,
        "org.a11y.atspi.Registry",
        "/org/a11y/atspi/accessible/root",
    )
    .await;
    if root_children.is_empty() {
        return (Vec::new(), false);
    }

    let cluster_pids: HashSet<i32> = collect_app_cluster(win.pid)
        .map(|(m, _)| m.into_iter().map(|p| p.pid).collect())
        .unwrap_or_else(|_| {
            let mut s = HashSet::new();
            if win.pid > 0 {
                s.insert(win.pid);
            }
            s
        });

    let win_class_lower = win.class.to_ascii_lowercase();
    let mut pid_services = Vec::new();
    let mut name_services = Vec::new();

    let apps = join_all(root_children.into_iter().map(|(service, root_path)| async move {
        let (pid, name) = futures_util::join!(
            atspi_bus::service_pid(conn, &service),
            atspi_bus::name(conn, &service, &root_path),
        );
        (service, root_path, pid, name.unwrap_or_default().to_ascii_lowercase())
    }))
    .await;

    for (service, root_path, conn_pid, app_name) in apps {
        let pid_matches = conn_pid.map(|p| cluster_pids.contains(&p)).unwrap_or(false);
        let name_matches = !app_name.is_empty()
            && (win_class_lower.contains(&app_name) || app_name.contains(&win_class_lower));

        if pid_matches {
            pid_services.push((service, root_path));
        } else if name_matches {
            name_services.push((service, root_path));
        }
    }
    // Ad eşleşmesi yalnız PID çözülemediğinde: aksi halde aynı adlı başka örneklerin
    // (ikinci Firefox, eski bir süreç) elemanları bu pencerenin sonucuna karışır.
    let matching_services = if pid_services.is_empty() { name_services } else { pid_services };

    let filter_lower = query_filter.map(|q| q.to_lowercase());
    let started = Instant::now();
    let mut inspected = 0usize;
    let mut candidates: Vec<(String, NodeInfo)> = Vec::new();
    let mut saw_document = false;

    for (service, root_path) in matching_services {
        let mut visited = HashSet::new();
        visited.insert(root_path.clone());
        // Uygulamanın birden çok penceresi varsa yalnız başlığı bu pencereninki olanı tara.
        let top_level = atspi_bus::children(conn, &service, &root_path).await;
        let top_names =
            join_all(top_level.iter().map(|(_, p)| atspi_bus::name(conn, &service, p))).await;
        let same_title: Vec<String> = top_level
            .iter()
            .zip(top_names)
            .filter(|((srv, _), name)| *srv == service && name.as_deref() == Some(win.title.as_str()))
            .map(|((_, p), _)| p.clone())
            .collect();
        let mut frontier: Vec<(String, bool)> = if top_level.len() > 1 && !same_title.is_empty() {
            same_title.into_iter().map(|p| (p, false)).collect()
        } else {
            vec![(root_path, false)]
        };
        for (p, _) in &frontier {
            visited.insert(p.clone());
        }

        // Seviye seviye: bir seviyedeki tüm düğümler aynı anda okunur.
        let mut depth = 0usize;
        while !frontier.is_empty() && inspected < NODE_BUDGET && started.elapsed() < TIME_BUDGET {
            frontier.truncate(NODE_BUDGET - inspected);
            inspected += frontier.len();
            let level = join_all(
                frontier
                    .drain(..)
                    .map(|(path, in_doc)| read_node(conn, &service, path, in_doc)),
            )
            .await;

            for node in level {
                // Görünmeyen alt ağaçlar (arka plan sekmeleri, kapalı menüler) gürültüdür ve bütçeyi yer.
                if depth > 0 && !node.states.iter().any(|s| s == "showing") {
                    continue;
                }
                let is_document = matches!(node.role.as_str(), "document web" | "document frame");
                saw_document |= is_document;
                let in_doc = node.in_document || is_document;
                for (srv, child) in &node.children {
                    if *srv == service && visited.insert(child.clone()) {
                        frontier.push((child.clone(), in_doc));
                    }
                }
                if node.name.is_empty() && !is_interactive_role(&node.role) {
                    continue;
                }
                let matches_query = filter_lower.as_ref().map_or(true, |q| {
                    node.name.to_lowercase().contains(q) || node.role.to_lowercase().contains(q)
                });
                if matches_query {
                    candidates.push((service.clone(), NodeInfo { in_document: in_doc, ..node }));
                }
            }
            depth += 1;
        }
    }

    // Sayfa/belge içeriği önce: tarayıcıda araç çubuğu düğmeleri sayfanın önüne geçip çıktı
    // sınırını doldurmasın. Sıralama kararlı; her grupta ağaç sırası korunur.
    candidates.sort_by_key(|(_, n)| !n.in_document);
    // Boyutsuz düğümler sonradan elenir; sınırı tutturmak için biraz fazlasının konumu istenir.
    candidates.truncate(max_elements.saturating_mul(2) + 20);

    let details = join_all(candidates.iter().map(|(service, node)| async move {
        // Wayland'de istemci ekrandaki yerini bilmez: GTK4 `SCREEN` için her elemanı (0,0)
        // döndürür. `WINDOW` araç takımlarında pencereye göreli doğru konumu verir.
        let (window_ext, description) = futures_util::join!(
            atspi_bus::extents(conn, service, &node.path, atspi_bus::COORD_WINDOW),
            atspi_bus::description(conn, service, &node.path),
        );
        let extents = match window_ext {
            Some(e) => Some((e, true)),
            None => atspi_bus::extents(conn, service, &node.path, atspi_bus::COORD_SCREEN)
                .await
                .map(|e| (e, false)),
        };
        // Şifre alanlarının içeriği hiçbir zaman okunmaz.
        let wants_value = node.role != "password text"
            && (is_value_or_text_role(&node.role) || node.states.iter().any(|s| s == "editable"));
        let value = if wants_value {
            atspi_bus::text_or_value(conn, service, &node.path).await
        } else {
            None
        };
        (extents, description, value)
    }))
    .await;

    let mut elements = Vec::new();
    for ((service, node), (extents, description, value)) in candidates.into_iter().zip(details) {
        let Some(([rx, ry, rw, rh], window_relative)) = extents else { continue };
        if rw <= 2 || rh <= 2 {
            continue;
        }
        let (sx, sy) = if window_relative {
            (win.x + rx, win.y + ry)
        } else {
            resolve_screen_coordinates(rx, ry, rw, rh, win)
        };
        elements.push(UiElement {
            mark: elements.len() + 1,
            role: node.role,
            name: node.name,
            value,
            description,
            states: node.states,
            actions: vec!["click".to_string(), "dbus_do_action".to_string()],
            rel_bounds: [rx, ry, rw, rh],
            screen_bounds: [sx, sy, rw, rh],
            click_point: [sx + rw / 2, sy + rh / 2],
            dbus_service: service,
            dbus_path: node.path,
        });
        if elements.len() >= max_elements {
            break;
        }
    }
    (elements, saw_document)
}

/// Fareyi hareket ettirmeden doğrudan D-Bus `org.a11y.atspi.Action.DoAction(0)` ile kontrolü tetikler.
pub fn atspi_invoke_do_action(service: &str, path: &str, action_index: i32) -> Result<bool> {
    let conn = atspi_bus::connection()?;
    atspi_bus::block_on(atspi_bus::do_action(conn, service, path, action_index))
        .ok_or_else(|| anyhow!("AT-SPI DoAction başarısız: {} {}", service, path))
}

fn is_browser_window(class: &str) -> bool {
    let c = class.to_ascii_lowercase();
    ["firefox", "librewolf", "floorp", "zen", "chromium", "chrome", "brave", "vivaldi", "msedge"]
        .iter()
        .any(|b| c.contains(b))
}

/// Bir metin kutusunun güncel içeriği (yazma sonrası doğrulama için).
pub fn atspi_read_text(service: &str, path: &str) -> Option<String> {
    let conn = atspi_bus::connection().ok()?;
    atspi_bus::block_on(atspi_bus::text_or_value(conn, service, path))
}

/// Düzenlenebilir bir AT-SPI metin kutusunun içeriğini doğrudan D-Bus (`EditableText.SetTextContents`) üzerinden değiştirir.
pub fn atspi_set_editable_text(service: &str, path: &str, new_text: &str) -> Result<bool> {
    let conn = atspi_bus::connection()?;
    atspi_bus::block_on(atspi_bus::set_text_contents(conn, service, path, new_text))
        .ok_or_else(|| anyhow!("AT-SPI SetTextContents başarısız: {} {}", service, path))
}

/// K1 Kuralı: Wayland pencere-içi `(rx, ry)` koordinatlarını Hyprland ekran `(sx, sy)` koordinatlarına dönüştürür.
pub fn resolve_screen_coordinates(
    rx: i64,
    ry: i64,
    rw: i64,
    rh: i64,
    win: &WindowInfo,
) -> (i64, i64) {
    let already_global = (win.x > 50 || win.y > 50)
        && rx >= win.x
        && ry >= win.y
        && (rx + rw) <= (win.x + win.width + 20)
        && (ry + rh) <= (win.y + win.height + 20);

    if already_global {
        (rx, ry)
    } else {
        (win.x + rx, win.y + ry)
    }
}

/// 64-bit AT-SPI `StateSet` (`[low_u32, high_u32]`) bit maskesini okunabilir durum etiketlerine çevirir.
pub fn decode_atspi_states(low: u32, high: u32) -> Vec<String> {
    let bits = ((high as u64) << 32) | (low as u64);
    let table: &[(u32, &str)] = &[
        (1, "active"),
        (2, "armed"),
        (3, "busy"),
        (4, "checked"),
        (7, "editable"),
        (8, "enabled"),
        (9, "expandable"),
        (10, "expanded"),
        (12, "focused"),
        (15, "indeterminate"),
        (16, "modal"),
        (20, "pressed"),
        (23, "selected"),
        (24, "sensitive"),
        (25, "showing"),
        (28, "visible"),
        (36, "read_only"),
    ];

    let mut out = Vec::new();
    for &(bit, label) in table {
        if (bits >> bit) & 1 == 1 {
            out.push(label.to_string());
        }
    }
    out
}

fn is_value_or_text_role(role: &str) -> bool {
    matches!(
        role,
        "text"
            | "entry"
            | "password text"
            | "combo box"
            | "slider"
            | "spin button"
            | "progress bar"
            | "scroll bar"
    )
}

fn is_interactive_role(role: &str) -> bool {
    matches!(
        role,
        "push button"
            | "button"
            | "toggle button"
            | "check box"
            | "radio button"
            | "menu item"
            | "text"
            | "entry"
            | "password text"
            | "combo box"
            | "link"
            | "page tab"
            | "slider"
            | "spin button"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coordinate_space_resolver_relative_to_global() {
        let win = WindowInfo {
            address: "0x123".to_string(),
            class: "firefox".to_string(),
            title: "Test".to_string(),
            pid: 1000,
            workspace_id: 1,
            workspace_name: "1".to_string(),
            monitor: 0,
            x: 400,
            y: 200,
            width: 1200,
            height: 800,
            floating: false,
            fullscreen: 0,
            xwayland: false,
            mapped: true,
            hidden: false,
            focused: true,
            protected_window: false,
        };

        let (sx, sy) = resolve_screen_coordinates(20, 30, 80, 32, &win);
        assert_eq!((sx, sy), (420, 230));

        let (gx, gy) = resolve_screen_coordinates(450, 250, 80, 32, &win);
        assert_eq!((gx, gy), (450, 250));
    }

    #[test]
    fn test_decode_atspi_states_bitmask() {
        // Bit 4 (checked), Bit 7 (editable), Bit 8 (enabled), Bit 12 (focused)
        let mask: u32 = (1 << 4) | (1 << 7) | (1 << 8) | (1 << 12);
        let states = decode_atspi_states(mask, 0);
        assert!(states.contains(&"checked".to_string()));
        assert!(states.contains(&"editable".to_string()));
        assert!(states.contains(&"enabled".to_string()));
        assert!(states.contains(&"focused".to_string()));
    }

    fn el(mark: usize, name: &str, value: Option<&str>) -> UiElement {
        UiElement {
            mark,
            role: "push button".to_string(),
            name: name.to_string(),
            value: value.map(|v| v.to_string()),
            description: None,
            states: vec![],
            actions: vec![],
            rel_bounds: [0, 0, 10, 10],
            screen_bounds: [0, 0, 10, 10],
            click_point: [5, 5],
            dbus_service: String::new(),
            dbus_path: String::new(),
        }
    }

    #[test]
    fn test_needs_ocr_fallback_trivial_webkit_frame() {
        // Kanıtlanan senaryo: WebKitGTK/Tauri penceresi tek bir "frame" düğümü yayınlar
        // (name boş, value yok) -> eski `elements.is_empty()` kapısı OCR'ı atlıyordu.
        let trivial = vec![el(1, "", None)];
        assert_eq!(count_contentful_elements(&trivial), 0);
        assert!(needs_ocr_fallback(&trivial, false, false));
    }

    #[test]
    fn test_needs_ocr_fallback_rich_tree_skips_ocr() {
        let rich = vec![
            el(1, "Kaydet", None),
            el(2, "İptal", None),
            el(3, "Ara", Some("hello")),
        ];
        assert_eq!(count_contentful_elements(&rich), 3);
        assert!(!needs_ocr_fallback(&rich, false, false));
    }

    #[test]
    fn test_needs_ocr_fallback_contentful_threshold_two() {
        // Tam olarak 2 içerikli eleman -> çok az sayılır, OCR denenir.
        let two = vec![el(1, "A", None), el(2, "B", None)];
        assert!(needs_ocr_fallback(&two, false, false));

        // 3 içerikli eleman (trivial'ler araya karışsa bile) -> OCR denenmez.
        let three = vec![
            el(1, "A", None),
            el(2, "B", None),
            el(3, "", None),
            el(4, "C", None),
        ];
        assert_eq!(count_contentful_elements(&three), 3);
        assert!(!needs_ocr_fallback(&three, false, false));
    }

    #[test]
    fn test_needs_ocr_fallback_terminal_rule_preserved() {
        // Terminal + sorgu yok -> içerik trivial olsa bile OCR'a düşme (terminal PTY yeterli).
        let trivial = vec![el(1, "", None)];
        assert!(!needs_ocr_fallback(&trivial, true, false));
        // Terminal + sorgu var -> OCR denenir.
        assert!(needs_ocr_fallback(&trivial, true, true));
        // Terminal + sorgu var ama içerik zengin -> denenmez.
        let rich = vec![
            el(1, "A", None),
            el(2, "B", None),
            el(3, "C", None),
        ];
        assert!(!needs_ocr_fallback(&rich, true, true));
    }
}
