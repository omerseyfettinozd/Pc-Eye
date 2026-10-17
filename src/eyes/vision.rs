use crate::daemon::ring_buffer::now_ms;
use crate::eyes::compositor::{capture_desktop_state, normalize_window_address};
use crate::eyes::ui_atspi::inspect_window_ui;
use crate::protocol::types::{UiElement, VisionReport};
use anyhow::{anyhow, Context, Result};
use image::{ImageFormat, Rgb, RgbImage};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LastFrameMeta {
    dhash: u64,
    timestamp_ms: u64,
    desktop_geometry: [i64; 4],
}

pub fn runtime_dir() -> PathBuf {
    let base = env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let dir = PathBuf::from(base).join("pc-eye");
    let _ = fs::create_dir_all(dir.join("captures"));
    dir
}

/// Linux VFS `rename(2)` garantisiyle yarış koşulsuz (lock-free atomic) dosya yazımı yapar.
pub fn atomic_write_str(target: &std::path::Path, content: &str) {
    let tmp = target.with_extension(format!("tmp.{}", std::process::id()));
    if fs::write(&tmp, content).is_ok() {
        let _ = fs::rename(&tmp, target);
    }
}

pub fn load_persisted_marks() -> Vec<UiElement> {
    let p = runtime_dir().join("last_marks.json");
    fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_persisted_marks(marks: &[UiElement]) {
    let p = runtime_dir().join("last_marks.json");
    if let Ok(json) = serde_json::to_string_pretty(marks) {
        atomic_write_str(&p, &json);
    }
}

/// Göz 2b: Ekran, Pencere, Bölge veya Zoom görüntüsü yakalar; dHash / Pixel-Diff hesaplar ve isteğe bağlı Set-of-Marks çizer.
pub fn capture_vision(
    window_addr: Option<&str>,
    region: Option<[i64; 4]>,
    zoom_point: Option<[i64; 2]>,
    draw_marks: bool,
) -> Result<VisionReport> {
    let ts = now_ms();
    let desktop = capture_desktop_state()?;

    if desktop.auth_guard_tripped {
        return Err(anyhow!(
            "AuthGuard aktif: {} — güvenlik nedeniyle ekran yakalama reddedildi",
            desktop.auth_guard_reason.unwrap_or_default()
        ));
    }

    let primary_mon = desktop.monitors.first().cloned();
    let mon_w = primary_mon.as_ref().map(|m| m.width).unwrap_or(2560);
    let mon_h = primary_mon.as_ref().map(|m| m.height).unwrap_or(1600);
    let scale = primary_mon.as_ref().map(|m| m.scale).unwrap_or(1.0);

    let (capture_mode, target_window, geom) = if let Some([zx, zy]) = zoom_point {
        let zw = 480i64;
        let zh = 320i64;
        let x = (zx - zw / 2).clamp(0, (mon_w - zw).max(0));
        let y = (zy - zh / 2).clamp(0, (mon_h - zh).max(0));
        (
            "zoom".to_string(),
            window_addr.map(normalize_window_address),
            [x, y, zw, zh],
        )
    } else if let Some(r) = region {
        let x = r[0].clamp(0, (mon_w - 10).max(0));
        let y = r[1].clamp(0, (mon_h - 10).max(0));
        let w = r[2].clamp(10, (mon_w - x).max(10));
        let h = r[3].clamp(10, (mon_h - y).max(10));
        ("region".to_string(), None, [x, y, w, h])
    } else if let Some(addr) = window_addr {
        let norm = normalize_window_address(addr);
        let win = desktop
            .windows
            .iter()
            .find(|w| w.address == norm)
            .ok_or_else(|| anyhow!("Pencere bulunamadı: {}", norm))?;
        let x = win.x.clamp(0, (mon_w - 10).max(0));
        let y = win.y.clamp(0, (mon_h - 10).max(0));
        let w = win.width.clamp(10, (mon_w - x).max(10));
        let h = win.height.clamp(10, (mon_h - y).max(10));
        ("window".to_string(), Some(norm), [x, y, w, h])
    } else {
        ("monitor".to_string(), None, [0, 0, mon_w, mon_h])
    };

    // In-memory PPM yakalama (grim -t ppm -) -> Sıfır disk I/O
    let mut img = grab_region_ppm(geom)?;

    // 64-bit dHash ve önceki kareyle görsel fark (pixel_diff_pct)
    let current_dhash = compute_dhash64(&img);
    let dhash_hex = format!("{:016x}", current_dhash);
    let (pixel_diff_pct, visual_stable) = evaluate_and_store_frame_diff(current_dhash, ts, geom);

    // Set-of-Marks (Numaralı kutular)
    let mut marks_legend = Vec::new();
    if draw_marks {
        // 1. Önce AT-SPI semantik elemanlarını dene
        if let Ok(ui_rep) = inspect_window_ui(target_window.as_deref(), None, 60) {
            marks_legend = ui_rep.elements;
        }
        // 2. Eğer AT-SPI boşsa (veya <2 eleman döndüyse) görüntü üzerindeki yüksek kontrastlı kontrol kutularını tespit et
        if marks_legend.len() < 2 {
            let visual_marks = detect_visual_control_boxes(&img, geom, marks_legend.len() + 1, 32);
            marks_legend.extend(visual_marks);
        }

        draw_marks_overlay(&mut img, geom, &marks_legend);
        save_persisted_marks(&marks_legend);
    }

    let out_dir = runtime_dir().join("captures");
    let file_path = out_dir.join(format!("capture_{}_{}.png", capture_mode, ts));
    img.save_with_format(&file_path, ImageFormat::Png)
        .with_context(|| format!("Görüntü kaydedilemedi: {}", file_path.display()))?;

    // Ayrıca sabit `latest.png` bağlantısını güncelle (Canlı Web Paneli için)
    let latest_path = out_dir.join("latest.png");
    let _ = fs::copy(&file_path, &latest_path);

    Ok(VisionReport {
        timestamp_ms: ts,
        capture_mode,
        target_window,
        file_path: file_path.to_string_lossy().to_string(),
        format: "png".to_string(),
        image_width: img.width(),
        image_height: img.height(),
        desktop_geometry: geom,
        scale,
        dhash_hex,
        pixel_diff_pct,
        visual_stable,
        marks_drawn: marks_legend.len(),
        marks_legend,
    })
}

/// `grim -g "x,y WxH" -t ppm -` ile sıkıştırmasız ham RGB belleği ~20ms içinde doğrudan RAM'e çeker.
pub fn grab_region_ppm(geom: [i64; 4]) -> Result<RgbImage> {
    let [x, y, w, h] = geom;
    let geom_arg = format!("{},{} {}x{}", x, y, w, h);
    let output = Command::new("grim")
        .args(["-g", &geom_arg, "-t", "ppm", "-"])
        .output()
        .context("grim çalıştırılamadı")?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("grim yakalama hatası: {}", err.trim()));
    }

    parse_ppm_p6(&output.stdout)
}

/// Ham P6 PPM ikili verisini sıfır kopya ile `RgbImage`'e dönüştürür.
pub fn parse_ppm_p6(bytes: &[u8]) -> Result<RgbImage> {
    // Format: P6\n<width> <height>\n<maxval>\n<rgb bytes>
    if bytes.len() < 15 || &bytes[0..2] != b"P6" {
        return Err(anyhow!("Geçersiz PPM P6 başlığı"));
    }
    let mut pos = 2;
    let mut tokens = Vec::with_capacity(3);

    while tokens.len() < 3 && pos < bytes.len() {
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos < bytes.len() && bytes[pos] == b'#' {
            while pos < bytes.len() && bytes[pos] != b'\n' {
                pos += 1;
            }
            continue;
        }
        let start = pos;
        while pos < bytes.len() && !bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if start < pos {
            let tok = std::str::from_utf8(&bytes[start..pos])?;
            tokens.push(tok.parse::<u32>()?);
        }
    }

    // Başlık sonundaki tek whitespace karakterini atla
    if pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
        pos += 1;
    }

    let width = *tokens.first().ok_or_else(|| anyhow!("PPM genişlik yok"))?;
    let height = *tokens.get(1).ok_or_else(|| anyhow!("PPM yükseklik yok"))?;
    let expected_len = (width as usize) * (height as usize) * 3;

    if bytes.len() < pos + expected_len {
        return Err(anyhow!(
            "PPM veri boyutu eksik: beklenen {}, gelen {}",
            expected_len,
            bytes.len().saturating_sub(pos)
        ));
    }

    let raw_rgb = bytes[pos..pos + expected_len].to_vec();
    RgbImage::from_raw(width, height, raw_rgb)
        .ok_or_else(|| anyhow!("RgbImage oluşturulamadı"))
}

/// 9x8 gri tonlama örneklemesiyle 64-bit Difference Hash (`dHash`) hesaplar (<0.2ms).
pub fn compute_dhash64(img: &RgbImage) -> u64 {
    let w = img.width().max(9);
    let h = img.height().max(8);
    let mut grid = [[0u8; 9]; 8];

    for (ry, row) in grid.iter_mut().enumerate() {
        let py = ((ry as u32) * img.height() / h).min(img.height().saturating_sub(1));
        for (rx, cell) in row.iter_mut().enumerate() {
            let px = ((rx as u32) * img.width() / w).min(img.width().saturating_sub(1));
            let p = img.get_pixel(px, py);
            *cell = ((p[0] as u32 * 77 + p[1] as u32 * 150 + p[2] as u32 * 29) >> 8) as u8;
        }
    }

    let mut hash = 0u64;
    let mut bit = 0;
    for row in &grid {
        for col in 0..8 {
            if row[col] > row[col + 1] {
                hash |= 1u64 << bit;
            }
            bit += 1;
        }
    }
    hash
}

fn evaluate_and_store_frame_diff(
    current_dhash: u64,
    ts: u64,
    geom: [i64; 4],
) -> (Option<f64>, bool) {
    let meta_path = runtime_dir().join("last_frame_meta.json");
    let prev: Option<LastFrameMeta> = fs::read_to_string(&meta_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());

    let new_meta = LastFrameMeta {
        dhash: current_dhash,
        timestamp_ms: ts,
        desktop_geometry: geom,
    };
    if let Ok(json) = serde_json::to_string(&new_meta) {
        atomic_write_str(&meta_path, &json);
    }

    if let Some(p) = prev {
        if p.desktop_geometry == geom {
            let hamming = (p.dhash ^ current_dhash).count_ones();
            let diff_pct = ((hamming as f64 / 64.0) * 100.0 * 100.0).round() / 100.0;
            let stable = hamming <= 2;
            return (Some(diff_pct), stable);
        }
    }
    (None, true)
}

/// AT-SPI ağacı olmayan (Canvas/Electron/Oyun) pencerelerde yüksek kontrastlı buton/ikon/kontrol bölgelerini tespit eder.
pub fn detect_visual_control_boxes(
    img: &RgbImage,
    geom: [i64; 4],
    start_mark: usize,
    max_boxes: usize,
) -> Vec<UiElement> {
    let w = img.width() as i64;
    let h = img.height() as i64;
    if w < 60 || h < 60 {
        return Vec::new();
    }

    // Ekranı 32x24 hücrelik ızgaraya bölüp yerel kenar (gradient) yoğunluğu yüksek olan bölgeleri grupla
    let cell_w = 40i64;
    let cell_h = 28i64;
    let cols = (w / cell_w).max(1);
    let rows = (h / cell_h).max(1);

    let mut candidates = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            let cx0 = c * cell_w;
            let cy0 = r * cell_h;
            let mut edge_score = 0u64;
            for dy in (2..cell_h - 2).step_by(4) {
                for dx in (2..cell_w - 2).step_by(4) {
                    let p1 = img.get_pixel((cx0 + dx) as u32, (cy0 + dy) as u32);
                    let p2 = img.get_pixel((cx0 + dx + 2) as u32, (cy0 + dy) as u32);
                    let p3 = img.get_pixel((cx0 + dx) as u32, (cy0 + dy + 2) as u32);
                    let diff = (p1[0] as i32 - p2[0] as i32).unsigned_abs()
                        + (p1[1] as i32 - p2[1] as i32).unsigned_abs()
                        + (p1[0] as i32 - p3[0] as i32).unsigned_abs();
                    if diff > 30 {
                        edge_score += diff as u64;
                    }
                }
            }
            if edge_score > 260 {
                candidates.push((edge_score, cx0, cy0, cell_w, cell_h));
            }
        }
    }

    // En belirgin bölgeleri seç ve birbirine çok yakın olanları birleştir (NMS)
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    let mut selected: Vec<[i64; 4]> = Vec::new();
    let mut elements = Vec::new();

    for (_, rx, ry, rw, rh) in candidates {
        if elements.len() >= max_boxes {
            break;
        }
        let overlaps = selected.iter().any(|b| {
            let dx = (b[0] - rx).abs();
            let dy = (b[1] - ry).abs();
            dx < cell_w * 2 && dy < cell_h * 2
        });
        if overlaps {
            continue;
        }
        let bw = rw.min(w - rx);
        let bh = rh.min(h - ry);
        selected.push([rx, ry, bw, bh]);

        let eff_scale = if geom[2] > 0 && w > 0 {
            (w as f64) / (geom[2] as f64)
        } else {
            1.0
        };
        let log_rx = ((rx as f64) / eff_scale).round() as i64;
        let log_ry = ((ry as f64) / eff_scale).round() as i64;
        let log_bw = (((bw as f64) / eff_scale).round() as i64).max(4);
        let log_bh = (((bh as f64) / eff_scale).round() as i64).max(4);

        let mark = start_mark + elements.len();
        let sx = geom[0] + log_rx;
        let sy = geom[1] + log_ry;
        let click_x = sx + log_bw / 2;
        let click_y = sy + log_bh / 2;

        elements.push(UiElement {
            mark,
            role: "visual-region".to_string(),
            name: format!("region_{}", mark),
            value: None,
            description: Some("Görsel kenar analizi ile tespit edilen etkileşim bölgesi".to_string()),
            states: vec!["visible".to_string()],
            actions: vec!["click".to_string()],
            rel_bounds: [log_rx, log_ry, log_bw, log_bh],
            screen_bounds: [sx, sy, log_bw, log_bh],
            click_point: [click_x, click_y],
            dbus_service: String::new(),
            dbus_path: String::new(),
        });
    }

    // Yukarıdan aşağıya, soldan sağa sırala ve mark numaralarını yeniden ata
    elements.sort_by_key(|e| (e.screen_bounds[1] / 32, e.screen_bounds[0]));
    for (idx, el) in elements.iter_mut().enumerate() {
        el.mark = start_mark + idx;
        if el.role == "visual-region" {
            el.name = format!("mark_{}", el.mark);
        }
    }

    elements
}

/// Saf Rust ile görüntü üzerine numaralı Set-of-Marks kutularını ve `[N]` etiketlerini çizer.
pub fn draw_marks_overlay(img: &mut RgbImage, geom: [i64; 4], marks: &[UiElement]) {
    let img_w = img.width() as i64;
    let img_h = img.height() as i64;
    let eff_scale = if geom[2] > 0 && img_w > 0 {
        (img_w as f64) / (geom[2] as f64)
    } else {
        1.0
    };

    for el in marks {
        let rx = (((el.screen_bounds[0] - geom[0]) as f64) * eff_scale).round() as i64;
        let ry = (((el.screen_bounds[1] - geom[1]) as f64) * eff_scale).round() as i64;
        let rw = ((el.screen_bounds[2] as f64) * eff_scale).round() as i64;
        let rh = ((el.screen_bounds[3] as f64) * eff_scale).round() as i64;

        if rx + rw <= 0 || ry + rh <= 0 || rx >= img_w || ry >= img_h {
            continue;
        }

        let x0 = rx.clamp(0, img_w - 1) as u32;
        let y0 = ry.clamp(0, img_h - 1) as u32;
        let x1 = (rx + rw - 1).clamp(0, img_w - 1) as u32;
        let y1 = (ry + rh - 1).clamp(0, img_h - 1) as u32;

        let color = if el.role == "visual-region" || el.role == "icon-button" {
            Rgb([255, 180, 0]) // Kehribar (görsel / ikon tespit)
        } else {
            Rgb([0, 230, 118]) // Zümrüt yeşili (AT-SPI / OCR semantik)
        };

        // 2px kalınlığında çerçeve çiz
        for t in 0..2 {
            let tx0 = (x0 + t).min(img_w as u32 - 1);
            let ty0 = (y0 + t).min(img_h as u32 - 1);
            let tx1 = x1.saturating_sub(t);
            let ty1 = y1.saturating_sub(t);
            for x in tx0..=tx1 {
                img.put_pixel(x, ty0, color);
                img.put_pixel(x, ty1, color);
            }
            for y in ty0..=ty1 {
                img.put_pixel(tx0, y, color);
                img.put_pixel(tx1, y, color);
            }
        }

        // Sol üst köşeye okunaklı numara rozeti (badge) çiz
        let label = el.mark.to_string();
        let badge_w = (label.len() as u32 * 6 + 4).min(img_w as u32 - x0);
        let badge_h = 11u32.min(img_h as u32 - y0);
        for by in y0..(y0 + badge_h) {
            for bx in x0..(x0 + badge_w) {
                img.put_pixel(bx, by, Rgb([18, 18, 24]));
            }
        }
        draw_bitmap_digits(img, x0 + 2, y0 + 2, &label, color);
    }
}

fn draw_bitmap_digits(img: &mut RgbImage, start_x: u32, start_y: u32, text: &str, color: Rgb<u8>) {
    for (idx, ch) in text.chars().enumerate() {
        let glyph = digit_glyph_5x7(ch);
        let gx = start_x + (idx as u32) * 6;
        for (row_idx, &row_bits) in glyph.iter().enumerate() {
            for col_idx in 0..5 {
                if (row_bits >> (4 - col_idx)) & 1 == 1 {
                    let px = gx + col_idx;
                    let py = start_y + row_idx as u32;
                    if px < img.width() && py < img.height() {
                        img.put_pixel(px, py, color);
                    }
                }
            }
        }
    }
}

fn digit_glyph_5x7(c: char) -> [u8; 7] {
    match c {
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        _ => [0b00000; 7],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ppm_parser_and_dhash_and_marks() {
        let header = b"P6\n16 16\n255\n";
        let mut data = header.to_vec();
        for i in 0..(16 * 16) {
            data.push((i % 255) as u8);
            data.push(((i * 3) % 255) as u8);
            data.push(((i * 7) % 255) as u8);
        }
        let mut img = parse_ppm_p6(&data).expect("PPM ayrıştırılabilmeli");
        assert_eq!(img.width(), 16);
        assert_eq!(img.height(), 16);

        let h1 = compute_dhash64(&img);
        let mark = UiElement {
            mark: 1,
            role: "button".to_string(),
            name: "Test".to_string(),
            value: None,
            description: None,
            states: vec![],
            actions: vec![],
            rel_bounds: [2, 2, 12, 12],
            screen_bounds: [2, 2, 12, 12],
            click_point: [8, 8],
            dbus_service: String::new(),
            dbus_path: String::new(),
        };
        draw_marks_overlay(&mut img, [0, 0, 16, 16], &[mark]);
        let h2 = compute_dhash64(&img);
        assert_ne!(h1, h2);
    }
}
