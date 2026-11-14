use crate::eyes::vision::parse_ppm_p6;
use crate::protocol::types::UiElement;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone)]
pub struct OcrWord {
    pub block_num: i32,
    pub par_num: i32,
    pub line_num: i32,
    pub word_num: i32,
    pub left: i64,
    pub top: i64,
    pub width: i64,
    pub height: i64,
    pub conf: f64,
    pub text: String,
}

/// Sistemde `eng` ve `tur` dil modellerinin bulunduğu `tessdata` dizinini çözümler.
pub fn resolve_tessdata_dir() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("HOME") {
        let local_tess = PathBuf::from(home).join(".local/share/pc-eye/tessdata");
        if local_tess.join("eng.traineddata").exists() {
            return Some(local_tess);
        }
    }
    let sys_tess = PathBuf::from("/usr/share/tessdata");
    // Boş tessdata dizini yüzünden OCR'ın hazır görünüp başarısız olmasını önlemek için model dosyasını doğrula.
    if sys_tess.join("eng.traineddata").exists() || sys_tess.join("tur.traineddata").exists() {
        return Some(sys_tess);
    }
    None
}

/// OCR motorunun (`tesseract` + `eng`/`tur` modelleri) hazır olup olmadığını kontrol eder.
pub fn is_ocr_available() -> bool {
    let bin_ok = Command::new("tesseract")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    bin_ok && resolve_tessdata_dir().is_some()
}

/// Belirtilen masaüstü bölgesini (`[x, y, w, h]`) sıfır-disk (`grim -t ppm -` -> `tesseract stdin stdout`)
/// ile tarar ve tıklanabilir metin ifadelerini (`UiElement`, `role = "ocr-text"`) döner.
pub fn ocr_region_elements(
    region: [i64; 4],
    query_filter: Option<&str>,
    max_elements: usize,
) -> Result<Vec<UiElement>> {
    let [rx, ry, rw, rh] = region;
    if rw <= 4 || rh <= 4 {
        return Ok(Vec::new());
    }

    let geom_arg = format!("{},{} {}x{}", rx, ry, rw, rh);
    let grim_out = Command::new("grim")
        .args(["-g", &geom_arg, "-t", "ppm", "-"])
        .output()?;
    if !grim_out.status.success() || grim_out.stdout.is_empty() {
        return Err(anyhow!("grim ile bölge yakalanamadı: {}", geom_arg));
    }

    ocr_ppm_bytes_to_elements(&grim_out.stdout, region, query_filter, max_elements)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct OcrCacheEntry {
    pub content_hash: u64,
    pub region: [i64; 4],
    pub elements: Vec<UiElement>,
}

/// Önbellek anahtarı piksellerin birebir aynı olduğunu garanti etmeli. Algısal dHash bilerek kabadır:
/// yerleşimi aynı, metni farklı iki ekran (değişen sayaç, yeni kod) aynı dHash'i verir ve eski metin döner.
fn content_hash(img: &image::RgbImage) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    img.dimensions().hash(&mut h);
    img.as_raw().hash(&mut h);
    h.finish()
}

fn load_ocr_cache(region: [i64; 4], hash: u64) -> Option<Vec<UiElement>> {
    let p = crate::eyes::vision::runtime_dir().join("ocr_cache.json");
    let raw = std::fs::read_to_string(p).ok()?;
    let entry: OcrCacheEntry = serde_json::from_str(&raw).ok()?;
    if entry.region == region && entry.content_hash == hash && !entry.elements.is_empty() {
        Some(entry.elements)
    } else {
        None
    }
}

fn save_ocr_cache(region: [i64; 4], hash: u64, elements: &[UiElement]) {
    let p = crate::eyes::vision::runtime_dir().join("ocr_cache.json");
    let entry = OcrCacheEntry {
        content_hash: hash,
        region,
        elements: elements.to_vec(),
    };
    if let Ok(json) = serde_json::to_string(&entry) {
        crate::eyes::vision::atomic_write_str(&p, &json);
    }
}

/// Bellek-içi `PPM` görüntü baytlarından (içerik hash'i önbellekli) `tesseract TSV` + İkon Füzyonu ile `UiElement` listesi üretir.
pub fn ocr_ppm_bytes_to_elements(
    ppm_bytes: &[u8],
    region: [i64; 4],
    query_filter: Option<&str>,
    max_elements: usize,
) -> Result<Vec<UiElement>> {
    let img = parse_ppm_p6(ppm_bytes)?;
    let hash = content_hash(&img);

    // 1. Pencere görüntüsü piksel piksel aynıysa tesseract'ı atla, önbellekten dön.
    let all_elements = if let Some(cached) = load_ocr_cache(region, hash) {
        cached
    } else {
        let ppm_w = img.width();
        let effective_scale = if region[2] > 0 && ppm_w > 0 {
            (ppm_w as f64) / (region[2] as f64)
        } else {
            1.0
        };

        let words = ocr_words(&img, 35.0)?;
        let phrases = group_words_into_phrases(&words, 32);

        let mut built = Vec::new();
        for phrase in phrases {
            let rel_x = ((phrase.left as f64) / effective_scale).round() as i64;
            let rel_y = ((phrase.top as f64) / effective_scale).round() as i64;
            let log_w = (((phrase.width as f64) / effective_scale).round() as i64).max(4);
            let log_h = (((phrase.height as f64) / effective_scale).round() as i64).max(4);

            let gx = region[0] + rel_x;
            let gy = region[1] + rel_y;
            let cx = gx + log_w / 2;
            let cy = gy + log_h / 2;

            let mark = built.len() + 1;
            built.push(UiElement {
                mark,
                role: "ocr-text".to_string(),
                name: phrase.text.clone(),
                value: Some(format!("conf:{:.0}%", phrase.conf)),
                description: Some("Tesseract OCR metin bloğu".to_string()),
                states: vec![
                    "showing".to_string(),
                    "visible".to_string(),
                    "enabled".to_string(),
                ],
                actions: vec!["click".to_string()],
                rel_bounds: [rel_x, rel_y, log_w, log_h],
                screen_bounds: [gx, gy, log_w, log_h],
                click_point: [cx, cy],
                dbus_service: String::new(),
                dbus_path: String::new(),
                container: None,
            });
        }

        // 2. Yazısız İkon/Buton Füzyonu: OCR metinleriyle çakışmayan yüksek kontrastlı kompakt ikon bölgelerini ekle
        let visual_boxes = crate::eyes::vision::detect_visual_control_boxes(
            &img,
            region,
            built.len() + 1,
            64,
        );
        for mut vbox in visual_boxes {
            let overlaps_text = built.iter().any(|t| {
                t.role == "ocr-text"
                    && t.name.chars().count() > 2
                    && boxes_overlap(t.screen_bounds, vbox.screen_bounds)
            });
            if !overlaps_text {
                let pos_tag = semantic_position_label(vbox.rel_bounds, region[2], region[3]);
                vbox.mark = built.len() + 1;
                vbox.role = "icon-button".to_string();
                vbox.name = format!("icon:{}#{}", pos_tag, vbox.mark);
                built.push(vbox);
            }
        }

        save_ocr_cache(region, hash, &built);
        built
    };

    let q_lower = query_filter.map(crate::eyes::fold);
    let mut filtered = Vec::new();
    for mut el in all_elements {
        if let Some(ref q) = q_lower {
            if !crate::eyes::fold(&el.name).contains(q.as_str()) && !el.role.to_lowercase().contains(q.as_str()) {
                continue;
            }
        }
        el.mark = filtered.len() + 1;
        filtered.push(el);
        if filtered.len() >= max_elements {
            break;
        }
    }

    Ok(filtered)
}

fn boxes_overlap(a: [i64; 4], b: [i64; 4]) -> bool {
    let ax2 = a[0] + a[2];
    let ay2 = a[1] + a[3];
    let bx2 = b[0] + b[2];
    let by2 = b[1] + b[3];
    !(ax2 < b[0] || bx2 < a[0] || ay2 < b[1] || by2 < a[1])
}

fn semantic_position_label(rel: [i64; 4], win_w: i64, win_h: i64) -> &'static str {
    let cx = rel[0] + rel[2] / 2;
    let cy = rel[1] + rel[3] / 2;
    let v = if cy < win_h / 6 {
        "top"
    } else if cy > (win_h * 5) / 6 {
        "bottom"
    } else {
        "mid"
    };
    let h = if cx < win_w / 4 {
        "left"
    } else if cx > (win_w * 3) / 4 {
        "right"
    } else {
        "center"
    };
    match (v, h) {
        ("top", "left") => "top-left",
        ("top", "right") => "top-right",
        ("top", _) => "top-bar",
        ("bottom", "left") => "bottom-left",
        ("bottom", "right") => "bottom-right",
        ("bottom", _) => "bottom-bar",
        ("mid", "left") => "sidebar-left",
        ("mid", "right") => "sidebar-right",
        _ => "center",
    }
}

/// Terminal veya metin pencereleri için görünür ekran satırlarını yukarıdan aşağıya düz metin olarak çıkarır.
pub fn ocr_region_lines(region: [i64; 4], max_lines: usize) -> Result<Vec<String>> {
    let [rx, ry, rw, rh] = region;
    if rw <= 4 || rh <= 4 {
        return Ok(Vec::new());
    }

    let geom_arg = format!("{},{} {}x{}", rx, ry, rw, rh);
    let grim_out = Command::new("grim")
        .args(["-g", &geom_arg, "-t", "ppm", "-"])
        .output()?;
    if !grim_out.status.success() || grim_out.stdout.is_empty() {
        return Ok(Vec::new());
    }

    let img = parse_ppm_p6(&grim_out.stdout)?;
    // Terminal satırları tesseract'ta kalır: sinir ağı küçük eş aralıklı yazıda karakter düşürüyordu
    // ("Dağıtım anahtarı: PVDYEG9L" → "Dağtm anahtar: PVDYEGL"); tesseract bu yazı tipinde kararlı.
    let words = ocr_words_with(&img, 30.0, ocr_single_band)?;
    Ok(reconstruct_lines(&words, max_lines))
}

fn tess_lang(tess_dir: &Path) -> String {
    let has_eng = tess_dir.join("eng.traineddata").exists();
    let has_tur = tess_dir.join("tur.traineddata").exists();
    // Bilinen diller yoksa anlamsız "afr" yerine varsayılan "eng" kullan.
    match (has_eng, has_tur) {
        (true, true) => "eng+tur".to_string(),
        (true, false) => "eng".to_string(),
        (false, true) => "tur".to_string(),
        (false, false) => "eng".to_string(),
    }
}

const BAND_OVERLAP_PX: u32 = 24;
const BAND_CUT_SEARCH_PX: u32 = 40;
const BAND_CACHE_MAX_ENTRIES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
struct BandSpec {
    core_y0: u32,
    core_y1: u32,
    ocr_y0: u32,
    ocr_y1: u32,
}

fn is_uniform_row(img: &image::RgbImage, y: u32) -> bool {
    let w = img.width() as usize;
    if w <= 1 {
        return true;
    }
    let row_start = (y as usize) * w * 3;
    let row = &img.as_raw()[row_start..row_start + w * 3];
    let first = &row[0..3];
    row.chunks_exact(3).skip(1).all(|px| px == first)
}

fn split_into_bands(img: &image::RgbImage) -> Vec<BandSpec> {
    let h = img.height();
    if h == 0 || img.width() == 0 {
        return Vec::new();
    }
    // Ölçüm (1600x900 metin yoğun sayfa, 16 çekirdek): 75 px / en çok 16 bant en hızlısı (0,7 sn) ve tek
    // parça OCR'a en yakın kelime kümesi; daha ince bantlarda satır kesimi kaybı artıyor.
    let bpx: u32 = std::env::var("PCEYE_OCR_BAND_PX").ok().and_then(|v| v.parse().ok()).unwrap_or(75);
    let kmax: u32 = std::env::var("PCEYE_OCR_BANDS").ok().and_then(|v| v.parse().ok()).unwrap_or(16);
    let k = (h / bpx.max(16)).clamp(1, kmax.max(1));
    let mut cuts = Vec::with_capacity((k + 1) as usize);
    cuts.push(0);

    // Satır ortasından kesip harf koparmamak için kesim noktasını yakın düz renkli satıra kaydır.
    for i in 1..k {
        let nominal = (i * h) / k;
        let prev = *cuts.last().unwrap_or(&0);
        let min_y = nominal.saturating_sub(BAND_CUT_SEARCH_PX).max(prev + 1);
        let max_y = (nominal + BAND_CUT_SEARCH_PX).min(h.saturating_sub(1));

        let mut chosen = nominal.clamp(prev + 1, h.saturating_sub(1));
        for delta in 0..=BAND_CUT_SEARCH_PX {
            if let Some(y_up) = nominal.checked_sub(delta) {
                if y_up >= min_y && y_up <= max_y && is_uniform_row(img, y_up) {
                    chosen = y_up;
                    break;
                }
            }
            let y_down = nominal + delta;
            if y_down >= min_y && y_down <= max_y && is_uniform_row(img, y_down) {
                chosen = y_down;
                break;
            }
        }
        cuts.push(chosen);
    }
    cuts.push(h);

    let mut bands = Vec::with_capacity(k as usize);
    for i in 0..(k as usize) {
        let core_y0 = cuts[i];
        let core_y1 = cuts[i + 1];
        let ocr_y0 = core_y0.saturating_sub(BAND_OVERLAP_PX);
        let ocr_y1 = (core_y1 + BAND_OVERLAP_PX).min(h);
        bands.push(BandSpec {
            core_y0,
            core_y1,
            ocr_y0,
            ocr_y1,
        });
    }
    bands
}

fn band_content_hash(w: u32, pixels: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    w.hash(&mut h);
    pixels.hash(&mut h);
    h.finish()
}

fn band_cache() -> &'static Mutex<HashMap<u64, Vec<OcrWord>>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, Vec<OcrWord>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn encode_ppm_p6(img: &image::RgbImage) -> Vec<u8> {
    let header = format!("P6\n{} {}\n255\n", img.width(), img.height());
    let raw = img.as_raw();
    let mut out = Vec::with_capacity(header.len() + raw.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(raw);
    out
}

fn ocr_single_band(band_img: &image::RgbImage) -> Result<Vec<OcrWord>> {
    let tsv = if let Some(tess_dir) = resolve_tessdata_dir() {
        let lang = tess_lang(&tess_dir);
        if let Some(Ok(tsv)) = crate::eyes::tess_ffi::with_engine(&tess_dir, &lang, |eng| {
            eng.tsv(band_img.as_raw(), band_img.width(), band_img.height())
        }) {
            tsv
        } else {
            let ppm = encode_ppm_p6(band_img);
            run_tesseract_tsv(&ppm)?
        }
    } else {
        let ppm = encode_ppm_p6(band_img);
        run_tesseract_tsv(&ppm)?
    };
    Ok(parse_tesseract_tsv(&tsv, 0.0))
}

fn merge_band_words(
    bands: &[BandSpec],
    band_results: &[Option<Vec<OcrWord>>],
    min_conf: f64,
) -> Vec<OcrWord> {
    let mut merged = Vec::new();
    // Örtüşme bölgesinde iki bandın da okuduğu kelimeleri yalnız çekirdek aralığına düşen bantta tut.
    for (band, words_opt) in bands.iter().zip(band_results.iter()) {
        let Some(words) = words_opt else { continue };
        let y_offset = band.ocr_y0 as i64;
        let core_y0 = band.core_y0 as i64;
        let core_y1 = band.core_y1 as i64;
        for w in words {
            if w.conf < min_conf {
                continue;
            }
            let abs_top = w.top + y_offset;
            let center_y = abs_top + w.height / 2;
            if center_y >= core_y0 && center_y < core_y1 {
                let mut shifted = w.clone();
                shifted.top = abs_top;
                merged.push(shifted);
            }
        }
    }
    merged
}

fn ocr_words_with<F>(
    img: &image::RgbImage,
    min_conf: f64,
    recognize_band: F,
) -> Result<Vec<OcrWord>>
where
    F: Fn(&image::RgbImage) -> Result<Vec<OcrWord>> + Sync,
{
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Ok(Vec::new());
    }

    let bands = split_into_bands(img);
    let row_bytes = (w as usize) * 3;
    let raw = img.as_raw();

    let mut band_results: Vec<Option<Vec<OcrWord>>> = vec![None; bands.len()];
    let mut pending: Vec<(usize, u64, image::RgbImage)> = Vec::new();

    {
        let cache = band_cache()
            .lock()
            .map_err(|_| anyhow!("OCR bant önbelleği kilitlenemedi"))?;
        for (idx, band) in bands.iter().enumerate() {
            let start = (band.ocr_y0 as usize) * row_bytes;
            let end = (band.ocr_y1 as usize) * row_bytes;
            let slice = &raw[start..end];
            let hash = band_content_hash(w, slice);
            if let Some(cached) = cache.get(&hash) {
                band_results[idx] = Some(cached.clone());
            } else {
                let band_h = band.ocr_y1 - band.ocr_y0;
                let band_img = image::RgbImage::from_raw(w, band_h, slice.to_vec())
                    .ok_or_else(|| anyhow!("bant görüntüsü oluşturulamadı"))?;
                pending.push((idx, hash, band_img));
            }
        }
    }

    if !pending.is_empty() {
        let recognized: Vec<Result<(usize, u64, Vec<OcrWord>)>> = std::thread::scope(|s| {
            let handles: Vec<_> = pending
                .iter()
                .map(|(idx, hash, band_img)| {
                    let recognize_ref = &recognize_band;
                    s.spawn(move || {
                        let words = recognize_ref(band_img)?;
                        Ok((*idx, *hash, words))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .map_err(|_| anyhow!("OCR bant iş parçacığı panikledi"))?
                })
                .collect()
        });

        let mut cache = band_cache()
            .lock()
            .map_err(|_| anyhow!("OCR bant önbelleği kilitlenemedi"))?;
        for item in recognized {
            let (idx, hash, words) = item?;
            if !cache.contains_key(&hash) && cache.len() >= BAND_CACHE_MAX_ENTRIES {
                cache.clear();
            }
            cache.insert(hash, words.clone());
            band_results[idx] = Some(words);
        }
    }

    Ok(merge_band_words(&bands, &band_results, min_conf))
}

fn ocr_words(img: &image::RgbImage, min_conf: f64) -> Result<Vec<OcrWord>> {
    if crate::eyes::ocr_neural::available() {
        if let Ok(words) = crate::eyes::ocr_neural::recognize(img) {
            return Ok(words.into_iter().filter(|w| w.conf >= min_conf).collect());
        }
    }
    ocr_words_with(img, min_conf, ocr_single_band)
}

fn run_tesseract_tsv(ppm_bytes: &[u8]) -> Result<String> {
    let mut cmd = Command::new("tesseract");
    cmd.env("OMP_THREAD_LIMIT", "1")
        .args(["stdin", "stdout"]);

    if let Some(tess_dir) = resolve_tessdata_dir() {
        let lang = tess_lang(&tess_dir);
        cmd.arg("--tessdata-dir").arg(&tess_dir);
        cmd.args(["-l", &lang]);
    }

    cmd.args(["--psm", "11", "-c", "tessedit_create_tsv=1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = cmd.spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(ppm_bytes);
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(anyhow!("tesseract TSV çalıştırılamadı"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Tesseract TSV çıktısını ayrıştırır ve güven skoru `min_conf` üstündeki kelimeleri döner.
pub fn parse_tesseract_tsv(tsv: &str, min_conf: f64) -> Vec<OcrWord> {
    let mut words = Vec::new();
    for (idx, line) in tsv.lines().enumerate() {
        if idx == 0 {
            continue; // başlık satırı
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 12 {
            continue;
        }
        let level: i32 = cols[0].parse().unwrap_or(0);
        if level != 5 {
            continue;
        }
        let conf: f64 = cols[10].parse().unwrap_or(-1.0);
        let text = cols[11].trim();
        if conf < min_conf || text.is_empty() {
            continue;
        }
        // Sadece tek noktalama karakteri olan düşük güvenli gürültüleri ele
        if text.len() == 1 && !text.chars().all(|c| c.is_alphanumeric()) && conf < 75.0 {
            continue;
        }

        let block_num = cols[2].parse().unwrap_or(0);
        let par_num = cols[3].parse().unwrap_or(0);
        let line_num = cols[4].parse().unwrap_or(0);
        let word_num = cols[5].parse().unwrap_or(0);
        let left = cols[6].parse().unwrap_or(0);
        let top = cols[7].parse().unwrap_or(0);
        let width = cols[8].parse().unwrap_or(0);
        let height = cols[9].parse().unwrap_or(0);

        if width <= 1 || height <= 1 {
            continue;
        }

        words.push(OcrWord {
            block_num,
            par_num,
            line_num,
            word_num,
            left,
            top,
            width,
            height,
            conf,
            text: text.to_string(),
        });
    }
    words
}

/// Aynı satırda (`block_num`, `par_num`, `line_num`) yan yana duran kelimeleri (`gap <= max_gap_px`)
/// anlamlı tıklanabilir ifadeler ("New Conversation", "Kaydet" vb.) halinde birleştirir.
pub fn group_words_into_phrases(words: &[OcrWord], max_gap_px: i64) -> Vec<OcrWord> {
    let mut phrases: Vec<OcrWord> = Vec::new();

    for w in words {
        if let Some(last) = phrases.last_mut() {
            let same_line = last.block_num == w.block_num
                && last.par_num == w.par_num
                && last.line_num == w.line_num;
            let prev_right = last.left + last.width;
            let gap = w.left - prev_right;
            let vertical_overlap = (last.top - w.top).abs() <= last.height.max(w.height);

            if same_line && vertical_overlap && gap >= -8 && gap <= max_gap_px {
                let new_left = last.left.min(w.left);
                let new_top = last.top.min(w.top);
                let new_right = (last.left + last.width).max(w.left + w.width);
                let new_bottom = (last.top + last.height).max(w.top + w.height);

                last.left = new_left;
                last.top = new_top;
                last.width = new_right - new_left;
                last.height = new_bottom - new_top;
                last.conf = (last.conf + w.conf) / 2.0;
                last.text.push(' ');
                last.text.push_str(&w.text);
                continue;
            }
        }
        phrases.push(w.clone());
    }

    phrases
}

/// Kelimeleri dikey koordinatlarına göre satır satır düz metin listesine dönüştürür.
pub fn reconstruct_lines(words: &[OcrWord], max_lines: usize) -> Vec<String> {
    let mut sorted = group_words_into_phrases(words, 48);
    // Kova sınırı (top / 10) aynı satırdaki ifadelerin sırasını bozmasın diye önce top ile sırala.
    sorted.sort_by_key(|p| (p.top, p.left));

    let mut clusters: Vec<(i64, Vec<(i64, String)>)> = Vec::new();
    for p in sorted {
        if let Some((last_y, items)) = clusters.last_mut() {
            if (p.top - *last_y).abs() <= 12 {
                items.push((p.left, p.text));
                continue;
            }
        }
        clusters.push((p.top, vec![(p.left, p.text)]));
    }

    let len = clusters.len();
    let skip = len.saturating_sub(max_lines);
    clusters
        .into_iter()
        .skip(skip)
        .map(|(_, mut items)| {
            items.sort_by_key(|(left, _)| *left);
            items
                .into_iter()
                .map(|(_, text)| text)
                .collect::<Vec<_>>()
                .join("  ")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_tsv_and_group_phrases() {
        let sample_tsv = "\
level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext
1\t1\t0\t0\t0\t0\t0\t0\t800\t300\t-1\t
5\t1\t2\t1\t1\t1\t100\t50\t35\t14\t95.0\tNew
5\t1\t2\t1\t1\t2\t142\t50\t90\t14\t96.0\tConversation
5\t1\t3\t1\t1\t1\t400\t50\t50\t14\t92.0\tKaydet
";
        let words = parse_tesseract_tsv(sample_tsv, 40.0);
        assert_eq!(words.len(), 3);

        let phrases = group_words_into_phrases(&words, 32);
        assert_eq!(phrases.len(), 2);
        assert_eq!(phrases[0].text, "New Conversation");
        assert_eq!(phrases[0].left, 100);
        assert_eq!(phrases[0].width, 132); // (142 + 90) - 100
        assert_eq!(phrases[1].text, "Kaydet");
    }

    #[test]
    fn cache_key_changes_when_only_text_pixels_change() {
        // Beyaz zemin üzerinde aynı yerde iki farklı "metin" şeridi: dHash aynı kalabilir, anahtar kalmamalı.
        let mut a = image::RgbImage::from_pixel(200, 60, image::Rgb([255, 255, 255]));
        let mut b = a.clone();
        for x in 20..120 {
            a.put_pixel(x, 30, image::Rgb([20, 20, 20]));
            b.put_pixel(x + 1, 30, image::Rgb([20, 20, 20]));
        }
        assert_eq!(
            crate::eyes::vision::compute_dhash64(&a),
            crate::eyes::vision::compute_dhash64(&b),
            "örnek, dHash çakışmasını temsil etmeli"
        );
        assert_ne!(content_hash(&a), content_hash(&b));
        assert_eq!(content_hash(&a), content_hash(&a.clone()));
    }

    #[test]
    fn reconstruct_lines_ignores_bucket_boundary() {
        let w = |block_num, left, top, text: &str| OcrWord {
            block_num,
            par_num: 1,
            line_num: 1,
            word_num: 1,
            left,
            top,
            width: 40,
            height: 14,
            conf: 95.0,
            text: text.to_string(),
        };
        let words = vec![w(1, 200, 19, "Sağ"), w(2, 50, 20, "Sol")];
        assert_eq!(reconstruct_lines(&words, 10), vec!["Sol  Sağ"]);
    }

    #[test]
    fn split_into_bands_covers_full_height_without_core_gaps_or_overlap() {
        for (h, expected_k) in [(60u32, 1usize), (300, 4), (900, 12), (1600, 16)] {
            let mut img = image::RgbImage::from_pixel(120, h, image::Rgb([240, 240, 240]));
            // Nominal kesimden 12 px aşağısı dışında tüm satırları tekdüze olmaktan çıkar.
            for y in 0..h {
                img.put_pixel(0, y, image::Rgb([10, 20, 30]));
            }
            if expected_k > 1 {
                let target_cut = (h / (expected_k as u32)) + 12;
                img.put_pixel(0, target_cut, image::Rgb([240, 240, 240]));
            }

            let bands = split_into_bands(&img);
            assert_eq!(bands.len(), expected_k);
            assert_eq!(bands.first().unwrap().core_y0, 0);
            assert_eq!(bands.last().unwrap().core_y1, h);

            if expected_k > 1 {
                assert_eq!(bands[0].core_y1, (h / (expected_k as u32)) + 12);
            }

            for (i, b) in bands.iter().enumerate() {
                assert!(b.core_y0 < b.core_y1);
                assert_eq!(b.ocr_y0, b.core_y0.saturating_sub(24));
                assert_eq!(b.ocr_y1, (b.core_y1 + 24).min(h));
                if i + 1 < bands.len() {
                    assert_eq!(b.core_y1, bands[i + 1].core_y0);
                    assert!(b.ocr_y1 > bands[i + 1].ocr_y0);
                }
            }
        }
    }

    #[test]
    fn core_filter_eliminates_duplicate_words_in_overlap() {
        let bands = vec![
            BandSpec {
                core_y0: 0,
                core_y1: 150,
                ocr_y0: 0,
                ocr_y1: 174,
            },
            BandSpec {
                core_y0: 150,
                core_y1: 300,
                ocr_y0: 126,
                ocr_y1: 300,
            },
        ];
        let mk_word = |top: i64, text: &str| OcrWord {
            block_num: 1,
            par_num: 1,
            line_num: 1,
            word_num: 1,
            left: 20,
            top,
            width: 50,
            height: 14,
            conf: 90.0,
            text: text.to_string(),
        };
        // Her iki kelime de [126, 174) örtüşme bölgesinde olduğu için iki bant tarafından da üretilir.
        let band_results = vec![
            Some(vec![mk_word(140, "UstSatir"), mk_word(152, "AltSatir")]),
            Some(vec![
                mk_word(140 - 126, "UstSatir"),
                mk_word(152 - 126, "AltSatir"),
            ]),
        ];

        let merged = merge_band_words(&bands, &band_results, 35.0);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].text, "UstSatir");
        assert_eq!(merged[0].top, 140);
        assert_eq!(merged[1].text, "AltSatir");
        assert_eq!(merged[1].top, 152);
    }

    #[test]
    fn band_cache_skips_ocr_on_unchanged_bands() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

        fn fake_ocr(band_img: &image::RgbImage) -> Result<Vec<OcrWord>> {
            CALL_COUNT.fetch_add(1, Ordering::SeqCst);
            let marker = band_img.get_pixel(0, 10)[0];
            Ok(vec![OcrWord {
                block_num: 1,
                par_num: 1,
                line_num: 1,
                word_num: 1,
                left: 10,
                top: 30,
                width: 40,
                height: 12,
                conf: 92.0,
                text: format!("bant-{}", marker),
            }])
        }

        // 150 px yükseklik → 75 px'lik iki bant (çekirdekler 0..75, 75..150; OCR aralığı ±24 px).
        let mut img = image::RgbImage::from_pixel(100, 150, image::Rgb([250, 250, 250]));
        for y in 0..75 {
            img.put_pixel(0, y, image::Rgb([11, y as u8, 77]));
        }
        for y in 75..150 {
            img.put_pixel(0, y, image::Rgb([22, y as u8, 88]));
        }

        let fake_fn: fn(&image::RgbImage) -> Result<Vec<OcrWord>> = fake_ocr;
        CALL_COUNT.store(0, Ordering::SeqCst);

        let first = ocr_words_with(&img, 35.0, fake_fn).unwrap();
        assert_eq!(CALL_COUNT.load(Ordering::SeqCst), 2);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].top, 30);
        assert_eq!(first[1].top, 51 + 30);

        // Aynı görüntüyle ikinci çağrı tamamen bant önbelleğinden dönmeli.
        let second = ocr_words_with(&img, 35.0, fake_fn).unwrap();
        assert_eq!(CALL_COUNT.load(Ordering::SeqCst), 2);
        assert_eq!(second.len(), 2);
        assert_eq!(second[1].top, first[1].top);

        // Yalnız alt bandın pikseli değiştiğinde sadece o bant yeniden OCR edilmeli.
        img.put_pixel(50, 130, image::Rgb([99, 99, 99]));
        let third = ocr_words_with(&img, 35.0, fake_fn).unwrap();
        assert_eq!(CALL_COUNT.load(Ordering::SeqCst), 3);
        assert_eq!(third.len(), 2);
    }
}

/// Geliştirme ölçümü: soğuk / sıcak / önbellekli / tek bant değişmiş OCR süreleri ve tek parça
/// tesseract'a göre kelime örtüşmesi (`pc-eye ocr-bench --file x.ppm`).
/// Geliştirme: sinir ağı ve tesseract çıktısını satır satır döker (`PCEYE_OCR_DUMP=1 pc-eye ocr-bench`).
pub fn dump(ppm: &[u8]) -> Result<()> {
    let img = crate::eyes::vision::parse_ppm_p6(ppm)?;
    for w in crate::eyes::ocr_neural::recognize(&img)? {
        println!("N {:>4},{:>4} {:>3.0} {}", w.left, w.top, w.conf, w.text);
    }
    for w in parse_tesseract_tsv(&run_tesseract_tsv(ppm)?, 35.0) {
        println!("T {:>4},{:>4} {:>3.0} {}", w.left, w.top, w.conf, w.text);
    }
    Ok(())
}

pub fn bench(ppm: &[u8]) -> Result<serde_json::Value> {
    let base = crate::eyes::vision::parse_ppm_p6(ppm)?;
    let (w, h) = base.dimensions();
    let t = std::time::Instant::now();
    let whole = parse_tesseract_tsv(&run_tesseract_tsv(ppm)?, 35.0);
    let whole_ms = t.elapsed().as_millis();
    let set = |v: &[OcrWord]| v.iter().map(|w| w.text.to_lowercase()).collect::<std::collections::HashSet<_>>();
    let a = set(&whole);
    // Sinir ağı ilklendirmesini runs döngüsünden önce ölç ki ms_soguk gerçek soğuk başlangıcı yansıtsın.
    let t0 = std::time::Instant::now();
    let neural = if crate::eyes::ocr_neural::available() {
        if let Ok(first) = crate::eyes::ocr_neural::recognize(&base) {
            let ms_soguk = t0.elapsed().as_millis();
            let t1 = std::time::Instant::now();
            let second = crate::eyes::ocr_neural::recognize(&base).unwrap_or_else(|_| first.clone());
            let ms_sicak = t1.elapsed().as_millis();
            let neural_words: std::collections::HashSet<String> = second
                .iter()
                .filter(|w| w.conf >= 35.0)
                .flat_map(|w| w.text.split_whitespace())
                .map(|s| s.to_lowercase())
                .collect();
            serde_json::json!({
                "available": true,
                "ms_soguk": ms_soguk,
                "ms_sicak": ms_sicak,
                "kelime": second.len(),
                "ortak_kelime_tek_parca": a.intersection(&neural_words).count(),
            })
        } else {
            serde_json::json!({ "available": false, "ms_soguk": 0, "ms_sicak": 0, "kelime": 0, "ortak_kelime_tek_parca": 0 })
        }
    } else {
        serde_json::json!({ "available": false, "ms_soguk": 0, "ms_sicak": 0, "kelime": 0, "ortak_kelime_tek_parca": 0 })
    };
    let mut runs = Vec::new();
    let mut last = Vec::new();
    for (i, label) in ["soguk", "sicak_tum_bantlar_degisti", "degismedi", "alt_bant_degisti"].iter().enumerate() {
        let mut img = base.clone();
        match i {
            1 => for y in 0..h { img.put_pixel(0, y, image::Rgb([1, 2, 3])); },
            3 => for x in 0..w.min(40) { img.put_pixel(x, h - 1, image::Rgb([9, 9, 9])); },
            _ => {}
        }
        if i == 2 {
            img = runs_img_cache(&base, h);
        }
        let t = std::time::Instant::now();
        last = ocr_words(&img, 35.0)?;
        runs.push(serde_json::json!({ "run": label, "ms": t.elapsed().as_millis(), "words": last.len() }));
    }
    let b = set(&last);
    Ok(serde_json::json!({
        "size": [w, h], "tek_parca_cli_ms": whole_ms, "tek_parca_kelime": whole.len(),
        "runs": runs, "ortak_kelime": a.intersection(&b).count(), "tek_parca_benzersiz": a.len(),
        "neural": neural,
    }))
}

/// 2. çalıştırmanın görüntüsü (tüm satırlarda ilk piksel değişmiş): "değişmedi" ölçümü aynısını tekrarlar.
fn runs_img_cache(base: &image::RgbImage, h: u32) -> image::RgbImage {
    let mut img = base.clone();
    for y in 0..h {
        img.put_pixel(0, y, image::Rgb([1, 2, 3]));
    }
    img
}
