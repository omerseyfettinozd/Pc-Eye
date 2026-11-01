use crate::daemon::ring_buffer::now_ms;
use crate::eyes::{a11y_events, atspi_bus};
use crate::eyes::compositor::{capture_desktop_state, resolve_window_ref};
use crate::eyes::process_xray::collect_app_cluster;
use crate::protocol::types::{TerminalContext, UiElement, UiTreeReport, WindowInfo};
use anyhow::{anyhow, Result};
use serde_json::Value;
use futures_util::future::join_all;
use std::collections::{HashMap, HashSet};
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
    let mut page_loading = false;
    if let Ok(conn) = atspi_bus::connection() {
        // Firefox içerik erişilebilirliğini ilk AT-SPI sorgusunda başlatır ve belge ağacı ~1 sn
        // sonra gelir; o arada yalnız araç çubuğu görünür. Belge yoksa (yalnız tarayıcıda) en fazla
        // ~2,5 sn yeniden oku: ajanın bir turu boşa gitmesin.
        let browser = is_browser_window(&win.class);
        let key = tree_cache_key(&win, query_filter, max_elements.max(10));
        if let Some(cached) = cached_tree(&key, &win) {
            elements = cached;
        } else {
            let deadline = Instant::now() + Duration::from_millis(2500);
            loop {
                let generations_before = a11y_events::generations_snapshot();
                let (found, saw_document, services) =
                    atspi_bus::block_on(collect_atspi_elements_for_window(
                        conn,
                        &win,
                        query_filter,
                        max_elements.max(10),
                    ));
                elements = found;
                // Belge varlığı filtreden bağımsız ölçülür: `--name "Ad Soyad"` gibi sorgular da beklemeli.
                // Belge görüldü ama neredeyse boşsa (Firefox içerik erişilebilirliğini ilk sorguda
                // başlatıyor; kullanıcının açık Firefox'unda ilk okuma 0 eleman döndü) biraz daha bekle.
                let sparse = browser && query_filter.is_none()
                    && found_named(&elements) < 3;
                let done = !browser || (saw_document && !sparse) || Instant::now() >= deadline;
                if done {
                    // Tarayıcıda belge henüz yoksa sonuç geçicidir; önbelleğe alınmaz.
                    if !browser || saw_document {
                        store_tree(key, &win, generations_before, &services, &elements);
                    } else {
                        page_loading = true;
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }

    // Bu pencere için ölçülmüş AT-SPI kayması varsa uygulanır (bkz. `calibrate`).
    if let Some(offset) = crate::eyes::calibrate::offset_for(&win.address, win.pid as i64, win_geom) {
        crate::eyes::calibrate::apply(&mut elements, offset);
    }

    // V4 Yükseltmesi (TRIVIAL ağaç düzeltmesi): AT-SPI2 ağacı "boş" olmasa bile ANLAMLI İÇERİK taşımıyorsa
    // (ör. WebKitGTK/Tauri yalnızca tek bir "frame" düğümü yayınlar; ad/değer yok, koordinat yok) OCR geri
    // dönüşünü DENE. Kapı `elements.is_empty()` yerine "içerikli eleman sayısı" ölçütüne bağlandı.
    let atspi_count = elements.len();
    // Yüklenmekte olan sayfada OCR'a düşmek gürültülü metin verir ("Youlube") ve birkaç saniye
    // sürer; belge birazdan erişilebilirlik ağacına gelecek. Ajana "yükleniyor" denir.
    // Tarayıcıda hiç: içerik yüklenirken iskelet sayfanın birkaç elemanı eşiği tutturamıyor ve tüm
    // pencere OCR'lanıyordu (3 sn, gürültülü). Web içeriği erişilebilirlik ağacından gelir.
    let browser_window = is_browser_window(&win.class);
    if !page_loading && !browser_window && needs_ocr_fallback(&elements, is_terminal_window(&win.class), query_filter.is_some()) {
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

    let sparse_page = browser_window && !page_loading && elements.iter().filter(|e| !e.name.is_empty()).count() < 3;
    let perception_mode = if page_loading || sparse_page {
        "at-spi2-page-loading".to_string()
    } else if used_ocr && atspi_count > 0 {
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
        _ if page_loading || sparse_page => Some(
            "Sayfa hâlâ yükleniyor (belge erişilebilirlik ağacına gelmedi); kısa süre sonra tekrar bak ya da `wait ui_match` ile bekle."
                .to_string(),
        ),
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
        stabilize_marks(&win.address, &mut elements);
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
/// Ekran dışı belge düğümleri için üst sınır (uzun tablo ~600 düğüm).
const MAX_OFFSCREEN_NODES: usize = 1200;
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
) -> (Vec<UiElement>, bool, Vec<String>) {
    let root_children = atspi_bus::children(
        conn,
        "org.a11y.atspi.Registry",
        "/org/a11y/atspi/accessible/root",
    )
    .await;
    if root_children.is_empty() {
        return (Vec::new(), false, Vec::new());
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
    let services: Vec<String> = matching_services.iter().map(|(s, _)| s.clone()).collect();

    let filter_lower = query_filter.map(|q| q.to_lowercase());
    // Tablo satırı bağlamı için: belge düğümlerinin rolü/adı/çocukları ve ebeveyn eşlemesi.
    let mut offscreen_seen = 0usize;
    let mut parent_of: HashMap<String, String> = HashMap::new();
    let mut doc_nodes: HashMap<String, (String, String, Vec<String>)> = HashMap::new();
    let started = Instant::now();
    let mut inspected = 0usize;
    let mut candidates: Vec<(String, NodeInfo)> = Vec::new();
    let mut document_ready = false;

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

            let mut text_leaves: Vec<NodeInfo> = Vec::new();
            for mut node in level {
                // Görünmeyen alt ağaçlar (arka plan sekmeleri, kapalı menüler) gürültüdür ve bütçeyi yer.
                // Etkin belgenin içindeki görünmeyen düğümler ise kaydırılınca görünecek içeriktir:
                // uzun bir sayfayı okumak için ajan 8-10 tur kaydırıp ekran görüntüsü alıyordu.
                // `offscreen` işaretiyle alınır (kapalı açılır menü seçenekleri hariç).
                let showing = node.states.iter().any(|s| s == "showing");
                if depth > 0 && !showing {
                    // Görünmeyen içerik bütçeyi yemesin: ağır sayfalarda (YouTube sonuçları) görünür
                    // elemanlara sıra gelmiyor, okuma OCR'a düşüyordu.
                    if !node.in_document
                        || matches!(node.role.as_str(), "menu" | "menu item" | "option")
                        || offscreen_seen >= MAX_OFFSCREEN_NODES
                    {
                        continue;
                    }
                    offscreen_seen += 1;
                    node.states.push("offscreen".to_string());
                }
                {
                    for (_, child) in &node.children {
                        parent_of.insert(child.clone(), node.path.clone());
                    }
                    doc_nodes.insert(
                        node.path.clone(),
                        (node.role.clone(), node.name.clone(), node.children.iter().map(|(_, c)| c.clone()).collect()),
                    );
                }
                let is_document = matches!(node.role.as_str(), "document web" | "document frame");
                let in_doc = node.in_document || is_document;
                // Belge hazırlığı filtreden bağımsız: belgenin içinde içerikli bir düğüm görüldüyse
                // hazırdır. Filtreli sonuca bakmak, belgede olmayan bir şeyi (açılır menü seçeneği,
                // henüz görünmemiş "Kaydedildi") arayan her sorguyu 2,5 sn bekletiyordu.
                if node.in_document && (!node.name.is_empty() || is_interactive_role(&node.role)) {
                    document_ready = true;
                }
                for (srv, child) in &node.children {
                    if *srv == service && visited.insert(child.clone()) {
                        frontier.push((child.clone(), in_doc));
                    }
                }
                if node.name.is_empty() && !is_interactive_role(&node.role) {
                    // Firefox paragraf/durum metnini ad olarak değil yalnız Text arayüzünden verir:
                    // form gönderildikten sonra beliren "Kaydedildi" hiç görünmüyor, ajan ekran
                    // görüntüsüne başvuruyordu (ölçüm 113644). Yaprak metin kapları okunur.
                    if node.in_document && node.children.is_empty() && is_text_container_role(&node.role) {
                        text_leaves.push(NodeInfo { in_document: in_doc, ..node });
                    }
                    continue;
                }
                let matches_query = filter_lower.as_ref().map_or(true, |q| {
                    node.name.to_lowercase().contains(q) || node.role.to_lowercase().contains(q)
                });
                if matches_query {
                    candidates.push((service.clone(), NodeInfo { in_document: in_doc, ..node }));
                }
            }
            let texts = join_all(
                text_leaves.iter().map(|n| atspi_bus::text_or_value(conn, &service, &n.path)),
            )
            .await;
            for (mut node, text) in text_leaves.into_iter().zip(texts) {
                let text: String = text.unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
                if text.is_empty() {
                    continue;
                }
                node.name = text.chars().take(200).collect();
                if let Some(entry) = doc_nodes.get_mut(&node.path) {
                    entry.1 = node.name.clone();
                }
                document_ready = true;
                let matches_query = filter_lower.as_ref().map_or(true, |q| node.name.to_lowercase().contains(q));
                if matches_query {
                    candidates.push((service.clone(), node));
                }
            }
            depth += 1;
        }
    }

    // Sayfa/belge içeriği önce: tarayıcıda araç çubuğu düğmeleri sayfanın önüne geçip çıktı
    // sınırını doldurmasın. Sıralama kararlı; her grupta ağaç sırası korunur.
    // Görünür olan her zaman önce: ekran dışı içerik sınırı doldurup görünür elemanları dışarıda
    // bırakmasın (sınır sorgusuz okumada küçüktür).
    candidates.sort_by_key(|(_, n)| (!n.in_document, n.states.iter().any(|s| s == "offscreen")));
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

    // Tablo hücresi bulunduysa satırın tamamı değer olarak eklenir: "GK-2546 kodlu ürünün stoğu"
    // gibi bir soru, hücreyi bulan tek sorguyla cevaplanır.
    let row_text = |cell_path: &str| -> Option<String> {
        let row = parent_of.get(cell_path)?;
        let (role, _, children) = doc_nodes.get(row)?;
        if !matches!(role.as_str(), "table row" | "row") {
            return None;
        }
        let cells: Vec<&str> = children
            .iter()
            .filter_map(|c| doc_nodes.get(c).map(|(_, n, _)| n.as_str()))
            .filter(|n| !n.is_empty())
            .collect();
        (cells.len() > 1).then(|| cells.join(" | "))
    };

    let container_of = |path: &str| -> Option<String> {
        let mut cur = parent_of.get(path)?;
        for _ in 0..6 {
            let (role, name, _) = doc_nodes.get(cur)?;
            if !name.is_empty()
                && matches!(
                    role.as_str(),
                    "list" | "list box" | "tree" | "table" | "tree table" | "panel" | "page tab" | "scroll pane"
                )
            {
                return Some(name.clone());
            }
            cur = parent_of.get(cur)?;
        }
        None
    };

    let mut elements = Vec::new();
    for ((service, node), (extents, description, value)) in candidates.into_iter().zip(details) {
        let container = if matches!(
            node.role.as_str(),
            "list item" | "tree item" | "table cell" | "cell" | "grid cell" | "row" | "table row"
        ) {
            container_of(&node.path)
        } else {
            None
        };
        let value = match value {
            Some(v) => Some(v),
            None if matches!(node.role.as_str(), "table cell" | "cell" | "grid cell") => row_text(&node.path),
            None => None,
        };
        let Some(([rx, ry, rw, rh], window_relative)) = extents else { continue };
        if rw <= 2 || rh <= 2 {
            continue;
        }
        let (sx, sy) = if window_relative {
            (win.x + rx, win.y + ry)
        } else {
            resolve_screen_coordinates(rx, ry, rw, rh, win)
        };
        // "Ekran dışı" geometriden karar verilir: sayfalar (YouTube) görünür içeriği de "showing"
        // durumu olmadan bildiriyor. Merkezi pencerenin dışındaysa ekran dışıdır.
        let mut states = node.states;
        states.retain(|s| s != "offscreen");
        let (cx, cy) = (sx + rw / 2, sy + rh / 2);
        if cx < win.x || cy < win.y || cx >= win.x + win.width || cy >= win.y + win.height {
            states.push("offscreen".to_string());
        }
        elements.push(UiElement {
            mark: elements.len() + 1,
            role: node.role,
            name: node.name,
            value,
            description,
            states,
            actions: vec!["click".to_string(), "dbus_do_action".to_string()],
            rel_bounds: [rx, ry, rw, rh],
            screen_bounds: [sx, sy, rw, rh],
            click_point: [sx + rw / 2, sy + rh / 2],
            dbus_service: service,
            dbus_path: node.path,
            container,
        });
        if elements.len() >= max_elements {
            break;
        }
    }
    (elements, document_ready, services)
}

/// Fareyi hareket ettirmeden doğrudan D-Bus `org.a11y.atspi.Action.DoAction(0)` ile kontrolü tetikler.
pub fn atspi_invoke_do_action(service: &str, path: &str, action_index: i32) -> Result<bool> {
    let conn = atspi_bus::connection()?;
    atspi_bus::block_on(atspi_bus::do_action(conn, service, path, action_index))
        .ok_or_else(|| anyhow!("AT-SPI DoAction başarısız: {} {}", service, path))
}

/// Olay güdümlü eleman önbelleği (yalnız olay izleyicisi çalışan uzun yaşayan süreçlerde).
/// Geçerlilik: ilgili uygulamalardan olay gelmemiş, pc-eye o arada hiçbir eylem yapmamış (eylem
/// dönemi), pencere yerinde ve okuma 2 sn'den taze. GTK4 etiket metni değişiminde olay
/// yayınlamadığı ölçüldü; dönem ve yaş sınırı bu açığı kapatır.
const TREE_CACHE_MAX_AGE: Duration = Duration::from_secs(2);

struct CachedTree {
    at: Instant,
    epoch: String,
    geometry: [i64; 4],
    generations: Vec<(String, u64)>,
    elements: Vec<UiElement>,
}

static TREE_CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, CachedTree>>> =
    std::sync::OnceLock::new();

fn tree_cache_key(win: &WindowInfo, query: Option<&str>, max: usize) -> String {
    format!("{}|{}|{}", win.address, query.unwrap_or(""), max)
}

fn cached_tree(key: &str, win: &WindowInfo) -> Option<Vec<UiElement>> {
    if a11y_events::cache_bypassed() {
        return None;
    }
    let cache = TREE_CACHE.get()?.lock().ok()?;
    let entry = cache.get(key)?;
    let fresh = entry.at.elapsed() < TREE_CACHE_MAX_AGE
        && entry.epoch == a11y_events::action_epoch()
        && entry.geometry == [win.x, win.y, win.width, win.height]
        && entry
            .generations
            .iter()
            .all(|(svc, g)| a11y_events::generation(svc) == Some(*g));
    fresh.then(|| entry.elements.clone())
}

fn store_tree(
    key: String,
    win: &WindowInfo,
    generations_before: Option<std::collections::HashMap<String, u64>>,
    services: &[String],
    elements: &[UiElement],
) {
    let Some(before) = generations_before else { return };
    let epoch = a11y_events::action_epoch();
    // Uygulama henüz AT-SPI'a kaydolmamışsa (yeni açılmış) izlenecek servis yoktur: hiçbir olay
    // bu kaydı geçersiz kılamaz ve boş/eksik sonuç 5 sn boyunca dönerdi. Böyle sonuç saklanmaz.
    if services.is_empty() || elements.is_empty() {
        return;
    }
    let generations = services
        .iter()
        .map(|s| (s.clone(), before.get(s).copied().unwrap_or(0)))
        .collect();
    let cache = TREE_CACHE.get_or_init(Default::default);
    if let Ok(mut map) = cache.lock() {
        map.retain(|_, v| v.at.elapsed() < TREE_CACHE_MAX_AGE);
        map.insert(
            key,
            CachedTree {
                at: Instant::now(),
                epoch,
                geometry: [win.x, win.y, win.width, win.height],
                generations,
                elements: elements.to_vec(),
            },
        );
    }
}

/// Bir elemanın snapshot'lar arası kararlı kimliği (AT-SPI: D-Bus yolu; OCR: rol + ad + konum).
pub fn element_key(e: &UiElement) -> String {
    if e.dbus_path.is_empty() {
        format!("{}|{}|{},{}", e.role, e.name, e.click_point[0], e.click_point[1])
    } else {
        format!("{}{}", e.dbus_service, e.dbus_path)
    }
}

/// Pencere başına "eleman kimliği → mark" kaydı. Dosyada tutulur ki daemon, MCP ve CLI aynı
/// numaraları versin (`click-ui --mark` ortak işaret dosyasından okur). Eleman var olduğu sürece
/// numarası değişmez: üstte yeni bir eleman belirince alttakilerin numarası kaymaz; ajan önceki
/// snapshot'taki numarayla güvenle tıklar ve delta yalnız gerçekten değişeni taşır.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct WindowMarks {
    next: usize,
    keys: std::collections::HashMap<String, usize>,
    touched: u64,
}

const MARK_REGISTRY_WINDOWS: usize = 24;
const MARK_REGISTRY_KEYS: usize = 4000;

fn stabilize_marks(window: &str, elements: &mut [UiElement]) {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock();
    let path = crate::eyes::vision::runtime_dir().join("marks_registry.json");
    let mut registry: std::collections::HashMap<String, WindowMarks> = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let entry = registry.entry(window.to_string()).or_default();
    if entry.keys.len() > MARK_REGISTRY_KEYS {
        *entry = WindowMarks::default();
    }
    for e in elements.iter_mut() {
        let key = element_key(e);
        e.mark = match entry.keys.get(&key) {
            Some(&m) => m,
            None => {
                entry.next += 1;
                entry.keys.insert(key, entry.next);
                entry.next
            }
        };
    }
    entry.touched = now_ms();
    if registry.len() > MARK_REGISTRY_WINDOWS {
        let mut by_age: Vec<(u64, String)> = registry.iter().map(|(k, v)| (v.touched, k.clone())).collect();
        by_age.sort();
        for (_, k) in by_age.into_iter().take(registry.len() - MARK_REGISTRY_WINDOWS) {
            registry.remove(&k);
        }
    }
    if let Ok(json) = serde_json::to_string(&registry) {
        crate::eyes::vision::atomic_write_str(&path, &json);
    }
}

/// Sınıf adının parçalarından biri tarayıcı adı mı. Alt dize araması "zenity"yi Zen tarayıcısı
/// sanıyordu: her okuma belge için 2,5 sn bekliyor, sihirbaz görevi 18 sn yerine 95 sn sürüyordu.
pub fn is_browser_class(class: &str) -> bool {
    is_browser_window(class)
}

fn found_named(elements: &[UiElement]) -> usize {
    elements.iter().filter(|e| !e.name.is_empty()).count()
}

fn is_browser_window(class: &str) -> bool {
    const BROWSERS: &[&str] = &[
        "firefox", "librewolf", "floorp", "zen", "zen-browser", "chromium", "chrome", "google-chrome",
        "brave", "brave-browser", "vivaldi", "msedge", "microsoft-edge", "waterfox", "mullvadbrowser",
    ];
    let c = class.to_ascii_lowercase();
    c.split(|ch: char| ch == '.' || ch == '_' || ch == ' ')
        .any(|part| BROWSERS.contains(&part) || part.starts_with("firefox") || part.starts_with("chromium"))
}

/// Bir metin kutusunun güncel içeriği (yazma sonrası doğrulama için).
pub fn atspi_read_text(service: &str, path: &str) -> Option<String> {
    let conn = atspi_bus::connection().ok()?;
    atspi_bus::block_on(atspi_bus::text_or_value(conn, service, path))
}

/// İmleçteki metne `text` ekler (seçim varsa onun yerine) ve geri okuyarak doğrular.
/// Klavye yolu açılır tamamlama penceresi olan alanlarda bozuluyordu (bkz. `type_text`).
pub fn atspi_insert_text(service: &str, path: &str, text: &str) -> bool {
    let Ok(conn) = atspi_bus::connection() else { return false };
    atspi_bus::block_on(async {
        let Some((current, caret, selection)) = atspi_bus::text_state(conn, service, path).await else {
            return false;
        };
        let chars: Vec<char> = current.chars().collect();
        let len = chars.len() as i32;
        let (start, end) = selection.unwrap_or((caret, caret));
        let (start, end) = (start.clamp(0, len) as usize, end.clamp(0, len) as usize);
        let mut new: String = chars[..start].iter().collect();
        new.push_str(text);
        new.extend(chars[end..].iter());
        if atspi_bus::set_text_contents(conn, service, path, &new).await != Some(true) {
            return false;
        }
        let back = atspi_bus::text_or_value(conn, service, path).await.unwrap_or_default();
        if back != new {
            // Yarım/yanlış uygulandıysa önceki içeriği geri koy: klavye yedeği iki kez yazmasın.
            let _ = atspi_bus::set_text_contents(conn, service, path, &current).await;
            return false;
        }
        let _ = atspi_bus::set_caret(conn, service, path, (start + text.chars().count()) as i32).await;
        true
    })
}

/// Görünür alanın dışındaki elemanı kaydırarak görünür yapar.
pub fn atspi_scroll_into_view(service: &str, path: &str) -> bool {
    let Ok(conn) = atspi_bus::connection() else { return false };
    atspi_bus::block_on(atspi_bus::scroll_to(conn, service, path))
}

/// Sayısal kontrolün (spin button, kaydırıcı) değerini `Value.CurrentValue` ile yazar ve geri okur.
/// Metin yazmak GTK4 spin button'da değeri işlemiyordu (Enter ya da odak kaybı gerekir).
pub fn atspi_set_value(service: &str, path: &str, value: f64) -> bool {
    let Ok(conn) = atspi_bus::connection() else { return false };
    atspi_bus::block_on(async {
        atspi_bus::set_current_value(conn, service, path, value).await
            && atspi_bus::current_value(conn, service, path)
                .await
                .map_or(false, |v| (v - value).abs() < 1e-6)
    })
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

/// Kendi adı olmayıp metnini Text arayüzünden veren kaplar (paragraf, durum çubuğu, uyarı).
fn is_text_container_role(role: &str) -> bool {
    matches!(
        role,
        "paragraph"
            | "status bar"
            | "statusbar"
            | "section"
            | "static"
            | "label"
            | "heading"
            | "notification"
            | "alert"
            | "list item"
            | "table cell"
            | "caption"
            | "block quote"
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
            | "switch"
            | "text box"
            | "menu button"
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
            container: None,
        }
    }

    #[test]
    fn browser_detection_is_by_name_not_substring() {
        for c in ["firefox", "org.mozilla.firefox", "firefox-esr", "zen", "chromium", "google-chrome", "brave-browser"] {
            assert!(is_browser_window(c), "{}", c);
        }
        for c in ["zenity", "org.kde.dolphin", "kitty", "org.gnome.Chromatic"] {
            assert!(!is_browser_window(c), "{}", c);
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
