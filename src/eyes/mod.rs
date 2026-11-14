pub mod a11y_events;
pub mod atspi_bus;
pub mod audio_pipewire;
pub mod calibrate;
pub mod compositor;
pub mod logs_notify;
pub mod media;
pub mod ocr;
pub mod ocr_neural;
pub mod process_xray;
pub mod snapshot;
pub mod tess_ffi;
pub mod ui_atspi;
pub mod vision;

/// Ad eşleştirme için Türkçe-güvenli katlama: küçük harf, `İ` → `i` (U+0307 atılır), `ı` → `i`.
/// OCR modeli "İstanbul"u "istanbul" okuyabiliyor; `to_lowercase` tek başına "i̇stanbul" verip eşleşmiyordu.
pub fn fold(s: &str) -> String {
    s.to_lowercase().replace('\u{307}', "").replace('ı', "i")
}
