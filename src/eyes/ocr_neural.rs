use crate::eyes::ocr::OcrWord;
use anyhow::{anyhow, Result};
use ndarray::Array4;
use ort::environment::Environment;
use ort::session::Session;
use ort::value::Tensor;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

struct Engine {
    _env: Environment,
    det: Session,
    // Tanıma oturumları paralel çalışır: tek oturuma 8 iş parçacığı vermek küçük partilerde ölçeklenmiyordu.
    rec: Vec<Mutex<Session>>,
    dict: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DetBox {
    left: i64,
    top: i64,
    width: i64,
    height: i64,
}

fn parse_dict(raw: &str) -> Vec<String> {
    let mut lines: Vec<String> = raw
        .split('\n')
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect();
    if matches!(lines.last(), Some(s) if s.is_empty()) {
        lines.pop();
    }
    lines
}

/// GPU OCR (CUDA): `~/.local/share/pc-eye/cuda/lib` (pip tekerleklerinden çıkarılmış CUDA 13 +
/// cuDNN 9) ve ONNX Runtime'ın CUDA derlemesi varsa. `PCEYE_OCR_GPU=0` kapatır.
fn cuda_lib_dir() -> Option<PathBuf> {
    if std::env::var("PCEYE_OCR_GPU").map_or(false, |v| v == "0") {
        return None;
    }
    let dir = PathBuf::from(std::env::var_os("HOME")?).join(".local/share/pc-eye/cuda/lib");
    ["libcudart.so.13", "libcublas.so.13", "libcublasLt.so.13", "libcurand.so.10", "libcudnn.so.9"]
        .iter()
        .all(|l| dir.join(l).is_file())
        .then_some(dir)
}

fn gpu_ort_lib() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("HOME")?).join(
        ".local/share/pc-eye/onnxruntime/onnxruntime-linux-x64-gpu_cuda13-1.30.1/lib/libonnxruntime.so.1.30.1",
    );
    p.is_file().then_some(p)
}

/// CUDA sağlayıcısının bağımlılıkları tam yollarıyla, bağımlılık sırasıyla RTLD_GLOBAL yüklenir:
/// `LD_LIBRARY_PATH` süreç başında okunduğu için sonradan ayarlamak işe yaramaz.
fn preload_cuda(dir: &Path) -> bool {
    let mut libs: Vec<PathBuf> = ["libcudart.so.13", "libcublasLt.so.13", "libcublas.so.13", "libcurand.so.10", "libnvrtc.so.13"]
        .iter()
        .map(|l| dir.join(l))
        .filter(|p| p.is_file())
        .collect();
    // cuDNN 9 alt kütüphanelerini kendisi dlopen eder; önce bağımsızlar, en son ana kütüphane.
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut cudnn: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().map_or(false, |n| {
                let n = n.to_string_lossy();
                n.starts_with("libcudnn") && n.ends_with(".so.9")
            }))
            .collect();
        cudnn.sort_by_key(|p| (p.file_name().map_or(false, |n| n == "libcudnn.so.9"), p.clone()));
        libs.extend(cudnn);
    }
    for lib in libs {
        let c = match std::ffi::CString::new(lib.as_os_str().as_encoded_bytes()) {
            Ok(c) => c,
            Err(_) => return false,
        };
        let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
        if h.is_null() && lib.file_name().map_or(false, |n| !n.to_string_lossy().starts_with("libcudnn_")) {
            return false;
        }
    }
    true
}

fn resolve_ort_lib_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PCEYE_ORT_LIB").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(p);
        return path.is_file().then_some(path);
    }
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(
        ".local/share/pc-eye/onnxruntime/onnxruntime-linux-x64-1.30.1/lib/libonnxruntime.so.1.30.1",
    );
    path.is_file().then_some(path)
}

fn resolve_models_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PCEYE_OCR_MODELS").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(p);
        return path.is_dir().then_some(path);
    }
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".local/share/pc-eye/models/ppocrv5");
    path.is_dir().then_some(path)
}

fn build_session(env: &Environment, path: &Path, threads: usize, gpu: bool) -> Result<Session> {
    let mut b = Session::builder(env).map_err(|e| anyhow!("{e}"))?;
    if gpu {
        b = b
            .with_execution_providers([ort::ep::CUDA::default()
                // Tanıma girdisinin genişliği her çağrıda değişir: kapsamlı cuDNN algoritma araması her
                // yeni şekilde yüzlerce ms harcar; sezgisel seçim yeter.
                .with_conv_algorithm_search(ort::ep::cuda::ConvAlgorithmSearch::Heuristic)
                .build()
                .error_on_failure()])
            .map_err(|e| anyhow!("{e}"))?;
    }
    b.with_intra_threads(threads)
        .map_err(|e| anyhow!("{e}"))?
        .commit_from_file(path)
        .map_err(|e| anyhow!("{e}"))
}

fn load_engine_inner() -> Result<Engine> {
    // Önce GPU; CUDA kütüphaneleri ya da sağlayıcı yüklenemezse CPU derlemesine düşülür.
    if let (Some(dir), Some(lib)) = (cuda_lib_dir(), gpu_ort_lib()) {
        if preload_cuda(&dir) {
            match load_engine_with(&lib, true) {
                Ok(e) => return Ok(e),
                Err(e) => {
                    if std::env::var_os("PCEYE_OCR_TRACE").is_some() {
                        eprintln!("[ocr] GPU açılamadı, CPU: {e}");
                    }
                }
            }
        }
    }
    let lib_path = resolve_ort_lib_path().ok_or_else(|| anyhow!("libonnxruntime bulunamadı"))?;
    load_engine_with(&lib_path, false)
}

fn load_engine_with(lib_path: &Path, gpu: bool) -> Result<Engine> {
    let models_dir = resolve_models_dir().ok_or_else(|| anyhow!("ppocrv5 model dizini bulunamadı"))?;

    let det_path = models_dir.join("pp-ocrv5_mobile_det.onnx");
    let rec_path = models_dir.join("latin_pp-ocrv5_mobile_rec.onnx");
    let dict_path = models_dir.join("ppocrv5_latin_dict.txt");

    if !det_path.is_file() || !rec_path.is_file() || !dict_path.is_file() {
        return Err(anyhow!("ppocrv5 model veya sözlük dosyası eksik"));
    }

    let raw_dict = std::fs::read_to_string(&dict_path)?;
    let dict = parse_dict(&raw_dict);
    if dict.is_empty() {
        return Err(anyhow!("ppocrv5 sözlüğü boş"));
    }

    // Ölçüm: 8'den fazla iş parçacığı ONNX Runtime eşzamanlılığında tersine yavaşlıyor.
    let threads = std::thread::available_parallelism()
        .map(|n| n.get() / 2)
        .unwrap_or(1)
        .clamp(1, 8);

    let env = ort::init_from(lib_path)
        .map_err(|e| anyhow!("{e}"))?
        .build()
        .map_err(|e| anyhow!("{e}"))?;

    let det = build_session(&env, &det_path, threads, gpu)?;
    let env_n = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse::<usize>().ok()).filter(|&n| n > 0).unwrap_or(d);
    let rec_sessions = env_n("PCEYE_OCR_REC_SESSIONS", 2);
    let rec_threads = env_n("PCEYE_OCR_REC_THREADS", (threads / 2).max(1));
    let rec = (0..rec_sessions)
        .map(|_| build_session(&env, &rec_path, rec_threads, gpu).map(Mutex::new))
        .collect::<Result<Vec<_>>>()?;

    Ok(Engine {
        _env: env,
        det,
        rec,
        dict,
    })
}

static LONG_LIVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Daemon ve MCP sunucusu çağırır. Motorun soğuk açılışı (~0,3–0,8 sn) tek seferlik CLI süreçlerinde her
/// çağrıda ödenir ve tıklama zamanlamasını kaydırıyordu (ölçüm: açılır liste görevleri 9/9 → 3/9); bu
/// süreçler tesseract'ta kalır. `PCEYE_OCR=neural` her süreçte açar, `PCEYE_OCR=tesseract` her yerde kapatır.
pub fn set_long_lived() {
    LONG_LIVED.store(true, std::sync::atomic::Ordering::Relaxed);
    // Motor (GPU'da CUDA bağlamı + cuDNN çekirdekleri ~4 sn, CPU'da ~1 sn) arka planda ısınır:
    // ilk OCR çağrısı açılışı beklemesin. Isınma bitmeden gelen çağrı aynı ilklendirmeyi bekler.
    if std::env::var("PCEYE_OCR").map_or(true, |v| v != "tesseract") && installed().is_some() {
        std::thread::spawn(|| {
            let _ = engine_cell();
            // Algılama ağının ilk çalıştırması da çekirdek seçimi yapar: boş bir kareyle tetiklenir.
            let _ = recognize(&image::RgbImage::from_pixel(640, 360, image::Rgb([255, 255, 255])));
        });
    }
}

fn engine_cell() -> Option<&'static Mutex<Engine>> {
    match std::env::var("PCEYE_OCR").ok().as_deref() {
        Some("tesseract") => return None,
        Some("neural") => {}
        _ if !LONG_LIVED.load(std::sync::atomic::Ordering::Relaxed) => return None,
        _ => {}
    }
    static CELL: OnceLock<Option<Mutex<Engine>>> = OnceLock::new();
    CELL.get_or_init(|| load_engine_inner().ok().map(Mutex::new))
        .as_ref()
}

/// Kütüphane ve modeller kurulu mu (yüklemeden; `doctor` için).
pub fn installed() -> Option<serde_json::Value> {
    let lib = resolve_ort_lib_path()?;
    let dir = resolve_models_dir()?;
    let files = ["pp-ocrv5_mobile_det.onnx", "latin_pp-ocrv5_mobile_rec.onnx", "ppocrv5_latin_dict.txt"];
    files.iter().all(|f| dir.join(f).is_file()).then(|| {
        serde_json::json!({ "onnxruntime": lib, "models": dir, "used_by": "daemon + MCP (PCEYE_OCR=neural|tesseract)" })
    })
}

pub fn available() -> bool {
    engine_cell().is_some()
}

fn det_input_size(src_w: u32, src_h: u32) -> (u32, u32) {
    let max_side = src_w.max(src_h) as f32;
    let scale = if max_side > 960.0 {
        960.0 / max_side
    } else {
        1.0
    };
    let round32 = |v: f32| (((v / 32.0).round() as u32) * 32).max(32);
    (
        round32((src_w as f32) * scale),
        round32((src_h as f32) * scale),
    )
}

fn prepare_det_tensor(img: &image::RgbImage, det_w: usize, det_h: usize) -> Array4<f32> {
    let (src_w, src_h) = img.dimensions();
    let sw = src_w as usize;
    let sh = src_h as usize;
    let raw = img.as_raw();
    let plane = det_h * det_w;
    let mut buf = vec![0.0_f32; 3 * plane];
    let (b_plane, rest) = buf.split_at_mut(plane);
    let (g_plane, r_plane) = rest.split_at_mut(plane);

    for dy in 0..det_h {
        let sy = ((dy * sh) / det_h).min(sh - 1);
        let src_row = sy * sw * 3;
        let dst_row = dy * det_w;
        for dx in 0..det_w {
            let sx = ((dx * sw) / det_w).min(sw - 1);
            let p = src_row + sx * 3;
            let r = raw[p] as f32 / 255.0;
            let g = raw[p + 1] as f32 / 255.0;
            let b = raw[p + 2] as f32 / 255.0;
            // PaddleOCR OpenCV (BGR) düzeninde eğitildiği için BGR kanal sırası kullanılır.
            b_plane[dst_row + dx] = (b - 0.485) / 0.229;
            g_plane[dst_row + dx] = (g - 0.456) / 0.224;
            r_plane[dst_row + dx] = (r - 0.406) / 0.225;
        }
    }

    Array4::from_shape_vec((1, 3, det_h, det_w), buf)
        .expect("det tensor boyutu buf uzunluğuyla eşleşmeli")
}

fn extract_det_boxes(
    prob: &[f32],
    map_w: usize,
    map_h: usize,
    src_w: u32,
    src_h: u32,
) -> Vec<DetBox> {
    if map_w == 0 || map_h == 0 || prob.len() < map_w * map_h || src_w == 0 || src_h == 0 {
        return Vec::new();
    }
    let thresh = 0.3_f32;
    let min_box_score = 0.6_f64;
    let unclip_ratio = 1.5_f32;
    let scale_x = (src_w as f32) / (map_w as f32);
    let scale_y = (src_h as f32) / (map_h as f32);

    let mut visited = vec![false; map_w * map_h];
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut boxes = Vec::new();

    for sy in 0..map_h {
        for sx in 0..map_w {
            let start_idx = sy * map_w + sx;
            if visited[start_idx] || prob[start_idx] <= thresh {
                continue;
            }

            visited[start_idx] = true;
            stack.push((sx, sy));

            let mut min_x = sx;
            let mut max_x = sx;
            let mut min_y = sy;
            let mut max_y = sy;
            let mut sum_prob = 0.0_f64;
            let mut count = 0_usize;

            while let Some((cx, cy)) = stack.pop() {
                let idx = cy * map_w + cx;
                sum_prob += prob[idx] as f64;
                count += 1;

                if cx < min_x {
                    min_x = cx;
                }
                if cx > max_x {
                    max_x = cx;
                }
                if cy < min_y {
                    min_y = cy;
                }
                if cy > max_y {
                    max_y = cy;
                }

                let y_lo = cy.saturating_sub(1);
                let y_hi = (cy + 1).min(map_h - 1);
                let x_lo = cx.saturating_sub(1);
                let x_hi = (cx + 1).min(map_w - 1);

                for ny in y_lo..=y_hi {
                    for nx in x_lo..=x_hi {
                        let n_idx = ny * map_w + nx;
                        if !visited[n_idx] && prob[n_idx] > thresh {
                            visited[n_idx] = true;
                            stack.push((nx, ny));
                        }
                    }
                }
            }

            let mean_prob = sum_prob / (count as f64);
            let bw = (max_x - min_x + 1) as f32;
            let bh = (max_y - min_y + 1) as f32;
            if mean_prob < min_box_score || bw < 3.0 || bh < 3.0 {
                continue;
            }

            let area = bw * bh;
            let perim = 2.0 * (bw + bh);
            let d = (area * unclip_ratio) / perim;

            let x0 = (((min_x as f32) - d) * scale_x).floor().max(0.0) as i64;
            let y0 = (((min_y as f32) - d) * scale_y).floor().max(0.0) as i64;
            let x1 = ((((max_x + 1) as f32) + d) * scale_x)
                .ceil()
                .min(src_w as f32) as i64;
            let y1 = ((((max_y + 1) as f32) + d) * scale_y)
                .ceil()
                .min(src_h as f32) as i64;

            let width = x1 - x0;
            let height = y1 - y0;
            if width > 0 && height > 0 {
                boxes.push(DetBox {
                    left: x0,
                    top: y0,
                    width,
                    height,
                });
            }
        }
    }

    boxes
}

fn rec_target_width(box_w: i64, box_h: i64) -> usize {
    let w = box_w.max(1) as f64;
    let h = box_h.max(1) as f64;
    ((48.0 * w / h).ceil() as usize).clamp(8, 3200)
}

fn fill_rec_crop(
    img: &image::RgbImage,
    b: DetBox,
    target_w: usize,
    batch_idx: usize,
    tensor: &mut Array4<f32>,
) {
    const REC_H: usize = 48;
    let img_w = img.width() as usize;
    let raw = img.as_raw();
    let x0 = b.left.max(0) as usize;
    let y0 = b.top.max(0) as usize;
    let cw = (b.width.max(1) as usize).min(img_w.saturating_sub(x0));
    let ch = (b.height.max(1) as usize).min((img.height() as usize).saturating_sub(y0));
    if cw == 0 || ch == 0 {
        return;
    }

    for ty in 0..REC_H {
        let fy = (((ty as f32) + 0.5) * (ch as f32) / (REC_H as f32) - 0.5)
            .clamp(0.0, (ch - 1) as f32);
        let sy0 = fy.floor() as usize;
        let sy1 = (sy0 + 1).min(ch - 1);
        let wy = fy - (sy0 as f32);
        let row0 = (y0 + sy0) * img_w;
        let row1 = (y0 + sy1) * img_w;

        for tx in 0..target_w {
            let fx = (((tx as f32) + 0.5) * (cw as f32) / (target_w as f32) - 0.5)
                .clamp(0.0, (cw - 1) as f32);
            let sx0 = fx.floor() as usize;
            let sx1 = (sx0 + 1).min(cw - 1);
            let wx = fx - (sx0 as f32);

            let i00 = (row0 + x0 + sx0) * 3;
            let i01 = (row0 + x0 + sx1) * 3;
            let i10 = (row1 + x0 + sx0) * 3;
            let i11 = (row1 + x0 + sx1) * 3;

            let w00 = (1.0 - wx) * (1.0 - wy);
            let w01 = wx * (1.0 - wy);
            let w10 = (1.0 - wx) * wy;
            let w11 = wx * wy;

            for (c_dst, c_src) in [(0usize, 2usize), (1, 1), (2, 0)] {
                let val = (raw[i00 + c_src] as f32) * w00
                    + (raw[i01 + c_src] as f32) * w01
                    + (raw[i10 + c_src] as f32) * w10
                    + (raw[i11 + c_src] as f32) * w11;
                tensor[[batch_idx, c_dst, ty, tx]] = (val / 255.0 - 0.5) / 0.5;
            }
        }
    }
}

fn ctc_decode(probs: &[f32], seq_len: usize, num_classes: usize, dict: &[String]) -> (String, f64) {
    if seq_len == 0 || num_classes == 0 || probs.len() < seq_len * num_classes {
        return (String::new(), 0.0);
    }

    let has_space_char = num_classes == dict.len() + 2;
    let mut text = String::new();
    let mut prob_sum = 0.0_f64;
    let mut kept = 0_usize;
    let mut prev_idx = 0_usize;

    for t in 0..seq_len {
        let row = &probs[t * num_classes..(t + 1) * num_classes];
        let mut best_idx = 0_usize;
        let mut best_prob = f32::NEG_INFINITY;
        for (c, &p) in row.iter().enumerate() {
            if p > best_prob {
                best_prob = p;
                best_idx = c;
            }
        }

        if best_idx != 0 && best_idx != prev_idx {
            let dict_idx = best_idx - 1;
            if let Some(ch) = dict.get(dict_idx) {
                text.push_str(ch);
                prob_sum += best_prob as f64;
                kept += 1;
            } else if has_space_char && dict_idx == dict.len() {
                text.push(' ');
                prob_sum += best_prob as f64;
                kept += 1;
            }
        }
        prev_idx = best_idx;
    }

    if kept == 0 {
        return (String::new(), 0.0);
    }
    (text, (prob_sum / (kept as f64)) * 100.0)
}

const REC_BATCH_MAX: usize = 16;
const REC_CACHE_MAX: usize = 8192;

fn rec_cache() -> &'static Mutex<std::collections::HashMap<u64, (String, f64)>> {
    static C: OnceLock<Mutex<std::collections::HashMap<u64, (String, f64)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Kutunun piksellerinin ve boyutunun hash'i (konumdan bağımsız: kayan metin de önbellekten gelir).
fn crop_hash(img: &image::RgbImage, b: DetBox) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let (w, ih) = (img.width() as i64, img.height() as i64);
    let (x0, y0) = (b.left.clamp(0, w), b.top.clamp(0, ih));
    let (x1, y1) = ((b.left + b.width).clamp(0, w), (b.top + b.height).clamp(0, ih));
    (x1 - x0, y1 - y0).hash(&mut h);
    let raw = img.as_raw();
    for y in y0..y1 {
        let s = ((y * w + x0) * 3) as usize;
        let e = ((y * w + x1) * 3) as usize;
        raw[s..e].hash(&mut h);
    }
    h.finish()
}

fn rec_batch(
    img: &image::RgbImage,
    chunk: &[(DetBox, usize)],
    sess: &Mutex<Session>,
    dict: &[String],
    tr: bool,
) -> Result<Vec<OcrWord>> {
    let batch_n = chunk.len();
    let w_max = chunk.iter().map(|&(_, tw)| tw).max().unwrap_or(8);
    let mut rec_arr = Array4::<f32>::zeros((batch_n, 3, 48, w_max));
    for (i, &(b, tw)) in chunk.iter().enumerate() {
        fill_rec_crop(img, b, tw, i, &mut rec_arr);
    }
    let tb = std::time::Instant::now();
    let rec_tensor = Tensor::from_array(rec_arr).map_err(|e| anyhow!("{e}"))?;
    let mut sess = sess.lock().map_err(|_| anyhow!("tanıma oturumu kilidi bozuldu"))?;
    let rec_out = sess.run(ort::inputs![rec_tensor]).map_err(|e| anyhow!("{e}"))?;
    let (rec_shape, rec_data) = rec_out[0].try_extract_tensor::<f32>().map_err(|e| anyhow!("{e}"))?;
    let rec_dims: &[i64] = rec_shape;
    if tr {
        eprintln!("  parti n={} wmax={} -> {:?} {} ms", batch_n, w_max, rec_dims, tb.elapsed().as_millis());
    }
    if rec_dims.len() != 3 {
        return Err(anyhow!("rec çıktı boyutu geçersiz: {:?}", rec_dims));
    }
    let out_n = usize::try_from(rec_dims[0]).map_err(|_| anyhow!("geçersiz rec batch"))?;
    let seq_len = usize::try_from(rec_dims[1]).map_err(|_| anyhow!("geçersiz rec adım"))?;
    let num_classes = usize::try_from(rec_dims[2]).map_err(|_| anyhow!("geçersiz rec sınıf"))?;
    let sample_stride = seq_len * num_classes;
    if out_n < batch_n || rec_data.len() < batch_n * sample_stride {
        return Err(anyhow!("rec çıktı verisi beklenenden kısa"));
    }
    let mut words = Vec::new();
    for (i, &(b, _)) in chunk.iter().enumerate() {
        let sample_probs = &rec_data[i * sample_stride..(i + 1) * sample_stride];
        let (raw_text, conf) = ctc_decode(sample_probs, seq_len, num_classes, dict);
        let text = raw_text.trim().to_string();
        // Tek karakterlik düşük güvenli sonuçlar çoğunlukla simge kenarından çıkan gürültü.
        if text.is_empty() || conf < 50.0 || (text.chars().count() == 1 && conf < 90.0) {
            continue;
        }
        words.push(OcrWord {
            block_num: 1,
            par_num: 1,
            line_num: 1,
            word_num: 1,
            left: b.left,
            top: b.top,
            width: b.width,
            height: b.height,
            conf,
            text,
        });
    }
    Ok(words)
}
const REC_BATCH_WIDTH_BUDGET: usize = 6400;

pub fn recognize(img: &image::RgbImage) -> Result<Vec<OcrWord>> {
    let (src_w, src_h) = img.dimensions();
    if src_w == 0 || src_h == 0 {
        return Ok(Vec::new());
    }

    let Some(lock) = engine_cell() else {
        return Err(anyhow!("sinir ağı OCR kullanılamıyor"));
    };

    let tr = std::env::var_os("PCEYE_OCR_TRACE").is_some();
    let t0 = std::time::Instant::now();
    let (det_w, det_h) = det_input_size(src_w, src_h);
    let det_arr = prepare_det_tensor(img, det_w as usize, det_h as usize);

    let mut guard = lock
        .lock()
        .map_err(|_| anyhow!("sinir ağı OCR kilidi bozuldu"))?;
    let eng = &mut *guard;

    if tr { eprintln!("hazırlık {} ms", t0.elapsed().as_millis()); }
    let t1 = std::time::Instant::now();
    let det_tensor = Tensor::from_array(det_arr).map_err(|e| anyhow!("{e}"))?;
    let det_out = eng
        .det
        .run(ort::inputs![det_tensor])
        .map_err(|e| anyhow!("{e}"))?;
    let (det_shape, det_data) = det_out[0]
        .try_extract_tensor::<f32>()
        .map_err(|e| anyhow!("{e}"))?;
    let det_dims: &[i64] = det_shape;
    if det_dims.len() != 4 {
        return Err(anyhow!("det çıktı boyutu geçersiz: {:?}", det_dims));
    }
    let map_h = usize::try_from(det_dims[2]).map_err(|_| anyhow!("geçersiz det harita yüksekliği"))?;
    let map_w = usize::try_from(det_dims[3]).map_err(|_| anyhow!("geçersiz det harita genişliği"))?;
    let boxes = extract_det_boxes(det_data, map_w, map_h, src_w, src_h);
    drop(det_out);
    if tr { eprintln!("algılama {} ms, {} kutu ({}x{})", t1.elapsed().as_millis(), boxes.len(), det_w, det_h); }
    let t2 = std::time::Instant::now();

    if boxes.is_empty() {
        return Ok(Vec::new());
    }

    let items: Vec<(DetBox, usize)> = boxes
        .into_iter()
        .map(|b| {
            let tw = rec_target_width(b.width, b.height);
            (b, tw)
        })
        .collect();
    // Kutu önbelleği: kutunun pikselleri birebir aynıysa tanıma (işin ~%90'ı) atlanır; ekranın yalnız bir
    // kısmı değiştiğinde maliyet algılama + değişen kutular olur.
    let mut cached_words = Vec::new();
    let mut keyed = Vec::with_capacity(items.len());
    {
        let cache = rec_cache().lock().unwrap_or_else(|e| e.into_inner());
        for (b, tw) in items {
            let key = crop_hash(img, b);
            match cache.get(&key) {
                Some((text, conf)) => cached_words.push(OcrWord {
                    block_num: 1, par_num: 1, line_num: 1, word_num: 1,
                    left: b.left, top: b.top, width: b.width, height: b.height,
                    conf: *conf, text: text.clone(),
                }),
                None => keyed.push(((b, tw), key)),
            }
        }
    }
    let mut items: Vec<(DetBox, usize)> = keyed.iter().map(|(it, _)| *it).collect();
    // Benzer genişlikteki kutuları aynı partide toplayarak sağdaki sıfır dolguyu en aza indir.
    items.sort_by_key(|&(_, tw)| tw);

    // Parti, içindeki en geniş kutuya doldurulur: bir uzun satır (ör. 2392 px URL) 10 kısa kutuyla aynı
    // partiye düşünce tek parti 1,2 sn sürüyordu. Parti genişlik bütçesiyle (n * wmax) sınırlanır.
    let mut batches: Vec<&[(DetBox, usize)]> = Vec::new();
    let mut start = 0;
    while start < items.len() {
        let mut end = start + 1;
        while end < items.len() && end - start < REC_BATCH_MAX && (end - start + 1) * items[end].1 <= REC_BATCH_WIDTH_BUDGET {
            end += 1;
        }
        batches.push(&items[start..end]);
        start = end;
    }

    // Partiler sırayla değil, oturum başına bir iş parçacığıyla paralel çözülür.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let collected: Mutex<Vec<OcrWord>> = Mutex::new(Vec::new());
    let first_err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let dict = &eng.dict;
    std::thread::scope(|sc| {
        for sess in eng.rec.iter() {
            let (next, collected, first_err, batches) = (&next, &collected, &first_err, &batches);
            sc.spawn(move || loop {
                let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(chunk) = batches.get(k) else { break };
                match rec_batch(img, chunk, sess, dict, tr) {
                    Ok(mut ws) => collected.lock().unwrap_or_else(|e| e.into_inner()).append(&mut ws),
                    Err(e) => {
                        first_err.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(e);
                        break;
                    }
                }
            });
        }
    });
    if let Some(e) = first_err.into_inner().unwrap_or_else(|e| e.into_inner()) {
        return Err(e);
    }
    let mut words = collected.into_inner().unwrap_or_else(|e| e.into_inner());
    {
        // Yeni tanınan kutular önbelleğe; düşük güven nedeniyle atılanlar boş metinle saklanır ki
        // bir sonraki okumada yeniden tanınmasın.
        let mut cache = rec_cache().lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() > REC_CACHE_MAX {
            cache.clear();
        }
        for ((b, _), key) in &keyed {
            let hit = words.iter().find(|w| w.left == b.left && w.top == b.top && w.width == b.width && w.height == b.height);
            cache.insert(*key, hit.map(|w| (w.text.clone(), w.conf)).unwrap_or_default());
        }
    }
    words.extend(cached_words.into_iter().filter(|w| !w.text.is_empty()));

    if tr { eprintln!("tanıma {} ms", t2.elapsed().as_millis()); }
    words.sort_by_key(|w| (w.top, w.left));
    for (idx, w) in words.iter_mut().enumerate() {
        w.block_num = (idx + 1) as i32;
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn det_input_size_rounds_to_32_and_never_upscales() {
        assert_eq!(det_input_size(1920, 1080), (960, 544));
        assert_eq!(det_input_size(640, 480), (640, 480));
        assert_eq!(det_input_size(20, 10), (32, 32));
        assert_eq!(det_input_size(400, 100), (416, 96));
    }

    #[test]
    fn extract_boxes_connected_components_filter_and_unclip() {
        let (w, h) = (24usize, 24usize);
        let mut prob = vec![0.0_f32; w * h];

        // 1) Geçerli 4x4 bileşen (ortalama 0.8 >= 0.6, kenar >= 3)
        for y in 4..8 {
            for x in 4..8 {
                prob[y * w + x] = 0.8;
            }
        }
        // 2) 8-komşulu çapraz bileşen (3x3 kutu)
        prob[14 * w + 14] = 0.85;
        prob[15 * w + 15] = 0.85;
        prob[16 * w + 16] = 0.85;

        // 3) Düşük güvenli 4x4 bileşen (eşik üstü 0.4 ama ortalama < 0.6 -> atılmalı)
        for y in 4..8 {
            for x in 14..18 {
                prob[y * w + x] = 0.4;
            }
        }
        // 4) İnce 2x5 bileşen (genişlik < 3 -> atılmalı)
        for y in 14..19 {
            for x in 4..6 {
                prob[y * w + x] = 0.9;
            }
        }

        let boxes = extract_det_boxes(&prob, w, h, 48, 48);
        assert_eq!(boxes.len(), 2);

        // 4x4 kutu için d = 16*1.5/16 = 1.5 -> [2.5, 9.5], ölçek 2x -> [5, 19] -> genişlik 14
        assert_eq!(
            boxes[0],
            DetBox {
                left: 5,
                top: 5,
                width: 14,
                height: 14,
            }
        );
        // 3x3 çapraz kutu için d = 9*1.5/12 = 1.125 -> [12.875, 18.125], ölçek 2x -> [25, 37]
        assert_eq!(
            boxes[1],
            DetBox {
                left: 25,
                top: 25,
                width: 12,
                height: 12,
            }
        );
    }

    #[test]
    fn ctc_decode_handles_blank_repeats_and_space_class() {
        let dict = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        // C = dict.len() + 2 = 5 (0=blank, 1='a', 2='b', 3='c', 4=' ')
        let c_with_space = 5;
        let steps = [
            (1usize, 0.9_f32),
            (1, 0.8),
            (0, 0.95),
            (1, 0.7),
            (4, 0.85),
            (2, 0.95),
        ];
        let mut probs = vec![0.0_f32; steps.len() * c_with_space];
        for (t, &(cls, p)) in steps.iter().enumerate() {
            probs[t * c_with_space + cls] = p;
        }
        let (text, conf) = ctc_decode(&probs, steps.len(), c_with_space, &dict);
        assert_eq!(text, "aa b");
        assert!((conf - 85.0).abs() < 1e-4);

        // Boşluk sınıfı olmayan durum (C = dict.len() + 1 = 4)
        let c_no_space = 4;
        let steps2 = [(0usize, 0.9_f32), (3, 0.8), (3, 0.9), (0, 0.99)];
        let mut probs2 = vec![0.0_f32; steps2.len() * c_no_space];
        for (t, &(cls, p)) in steps2.iter().enumerate() {
            probs2[t * c_no_space + cls] = p;
        }
        let (text2, conf2) = ctc_decode(&probs2, steps2.len(), c_no_space, &dict);
        assert_eq!(text2, "c");
        assert!((conf2 - 80.0).abs() < 1e-4);
    }

    #[test]
    fn parse_dict_keeps_inner_empty_and_drops_single_trailing_newline() {
        assert_eq!(
            parse_dict("a\r\nb\n\nc\n"),
            vec!["a", "b", "", "c"]
        );
        assert_eq!(parse_dict("a\nb\n\n"), vec!["a", "b", ""]);
    }

    #[test]
    #[ignore]
    fn neural_ocr_end_to_end_smoke() {
        assert!(available(), "PP-OCRv5 modelleri ve ONNX Runtime kurulu olmalı");
        let mut img = image::RgbImage::from_pixel(320, 80, image::Rgb([255, 255, 255]));
        for x in 20..200 {
            img.put_pixel(x, 40, image::Rgb([0, 0, 0]));
        }
        assert!(recognize(&img).is_ok());
    }
}
