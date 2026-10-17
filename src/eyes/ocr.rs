use crate::eyes::vision::parse_ppm_p6;
use crate::protocol::types::UiElement;
use anyhow::{anyhow, Result};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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
    if sys_tess.exists() {
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

/// Bellek-içi `PPM` görüntü baytlarından `tesseract TSV` çıktısı üretir ve `UiElement` listesine dönüştürür.
pub fn ocr_ppm_bytes_to_elements(
    ppm_bytes: &[u8],
    region: [i64; 4],
    query_filter: Option<&str>,
    max_elements: usize,
) -> Result<Vec<UiElement>> {
    let img = parse_ppm_p6(ppm_bytes)?;
    let ppm_w = img.width();
    let effective_scale = if region[2] > 0 && ppm_w > 0 {
        (ppm_w as f64) / (region[2] as f64)
    } else {
        1.0
    };

    let tsv = run_tesseract_tsv(ppm_bytes)?;
    let words = parse_tesseract_tsv(&tsv, 35.0);
    let phrases = group_words_into_phrases(&words, 32);

    let q_lower = query_filter.map(|q| q.to_lowercase());
    let mut elements = Vec::new();

    for phrase in phrases {
        if let Some(ref q) = q_lower {
            if !phrase.text.to_lowercase().contains(q) {
                continue;
            }
        }

        let rel_x = ((phrase.left as f64) / effective_scale).round() as i64;
        let rel_y = ((phrase.top as f64) / effective_scale).round() as i64;
        let log_w = (((phrase.width as f64) / effective_scale).round() as i64).max(4);
        let log_h = (((phrase.height as f64) / effective_scale).round() as i64).max(4);

        let gx = region[0] + rel_x;
        let gy = region[1] + rel_y;
        let cx = gx + log_w / 2;
        let cy = gy + log_h / 2;

        let mark = elements.len() + 1;
        elements.push(UiElement {
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
        });

        if elements.len() >= max_elements {
            break;
        }
    }

    Ok(elements)
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

    let tsv = run_tesseract_tsv(&grim_out.stdout)?;
    let words = parse_tesseract_tsv(&tsv, 30.0);
    Ok(reconstruct_lines(&words, max_lines))
}

fn run_tesseract_tsv(ppm_bytes: &[u8]) -> Result<String> {
    let mut cmd = Command::new("tesseract");
    cmd.args(["stdin", "stdout"]);

    if let Some(tess_dir) = resolve_tessdata_dir() {
        cmd.arg("--tessdata-dir").arg(&tess_dir);
        let has_eng = tess_dir.join("eng.traineddata").exists();
        let has_tur = tess_dir.join("tur.traineddata").exists();
        let lang = match (has_eng, has_tur) {
            (true, true) => "eng+tur",
            (true, false) => "eng",
            (false, true) => "tur",
            (false, false) => "afr",
        };
        cmd.args(["-l", lang]);
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
    let phrases = group_words_into_phrases(words, 48);
    let mut sorted = phrases;
    sorted.sort_by_key(|p| (p.top / 10, p.left));

    let mut lines: Vec<(i64, String)> = Vec::new();
    for p in sorted {
        if let Some((last_y, last_line)) = lines.last_mut() {
            if (p.top - *last_y).abs() <= 12 {
                last_line.push_str("  ");
                last_line.push_str(&p.text);
                continue;
            }
        }
        lines.push((p.top, p.text));
    }

    let len = lines.len();
    let skip = len.saturating_sub(max_lines);
    lines.into_iter().skip(skip).map(|(_, s)| s).collect()
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
}
