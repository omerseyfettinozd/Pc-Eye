//! Eylem programı (`pc_eye_run`): beklentili adım dizisi tek çağrıda.
//!
//! Ajan her adımda modele dönüp "şimdi ne yapayım" diye sormak yerine bir kez planlar: adımları
//! ve her adımdan sonra görmesi gerekeni (`expect`) yazar. Yürütücü adımları sırayla çalıştırır,
//! her birini doğrular ve ilk sapmada durup o anki kompakt durumu döner. Ölçüm: form doldurma
//! 7–14 tur sürüyordu; program ile bir planlama + bir doğrulama turu.

use crate::director::input::{click_ui_element_ext, press_key_combo, type_text};
use crate::director::wait::wait_for_condition;
use crate::eyes::a11y_events::with_fresh_reads;
use crate::eyes::ui_atspi::inspect_window_ui;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_EXPECT_MS: u64 = 3000;
const MAX_STEPS: usize = 40;

/// Hedef: `"Gönder"` (ad) ya da `{"mark": 7}` / `{"name": "Gönder"}`.
fn target(v: &Value) -> Result<(Option<String>, Option<usize>)> {
    match v {
        Value::String(name) => Ok((Some(name.clone()), None)),
        Value::Object(o) => Ok((
            o.get("name").and_then(Value::as_str).map(String::from),
            o.get("mark").and_then(Value::as_u64).map(|m| m as usize),
        )),
        _ => Err(anyhow!("hedef ad (metin) ya da {{\"mark\": N}} olmalı")),
    }
    .and_then(|(n, m)| {
        if n.is_none() && m.is_none() {
            Err(anyhow!("hedefte name ya da mark yok"))
        } else {
            Ok((n, m))
        }
    })
}

fn describe(name: &Option<String>, mark: Option<usize>) -> String {
    match (name, mark) {
        (Some(n), _) => n.clone(),
        (None, Some(m)) => format!("#{}", m),
        _ => String::new(),
    }
}

/// Metin görünene ya da kaybolana kadar taze okumayla bekler.
fn wait_text(window: Option<&str>, text: &str, gone: bool, timeout_ms: u64) -> Result<u64> {
    let start = Instant::now();
    if !gone {
        wait_for_condition("ui_match", Some(text), window, timeout_ms)?;
        return Ok(start.elapsed().as_millis() as u64);
    }
    let needle = text.to_lowercase();
    while start.elapsed() < Duration::from_millis(timeout_ms) {
        let present = with_fresh_reads(|| inspect_window_ui(window, Some(text), 10))
            .map(|ui| ui.elements.iter().any(|e| e.name.to_lowercase().contains(&needle)))
            .unwrap_or(true);
        if !present {
            return Ok(start.elapsed().as_millis() as u64);
        }
        thread::sleep(Duration::from_millis(150));
    }
    Err(anyhow!("`{}` {} ms içinde kaybolmadı", text, timeout_ms))
}

/// Adımın beklentisi: `"Kaydedildi"` | `{"text": ".."}` | `{"gone": ".."}` (+ `timeout_ms`).
fn check_expect(window: Option<&str>, step: &Value) -> Result<Option<String>> {
    let Some(exp) = step.get("expect") else { return Ok(None) };
    let timeout = step
        .get("timeout_ms")
        .or_else(|| exp.get("timeout_ms"))
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_EXPECT_MS);
    let (text, gone) = match exp {
        Value::String(t) => (t.as_str(), false),
        Value::Object(o) => match (o.get("text").and_then(Value::as_str), o.get("gone").and_then(Value::as_str)) {
            (Some(t), _) => (t, false),
            (None, Some(g)) => (g, true),
            _ => return Err(anyhow!("expect: text ya da gone gerekli")),
        },
        _ => return Err(anyhow!("expect metin ya da nesne olmalı")),
    };
    let ms = wait_text(window, text, gone, timeout)?;
    Ok(Some(format!("{} `{}` ({} ms)", if gone { "kayboldu" } else { "göründü" }, text, ms)))
}

fn run_step(window: Option<&str>, step: &Value, allow_protected: bool) -> Result<(String, String, String)> {
    let note_of = |r: &crate::director::input::ActionReceipt| match &r.verification {
        Some(v) if !v.visible_change => "görünür değişiklik yok".to_string(),
        _ => "ok".to_string(),
    };
    if let Some(t) = step.get("click") {
        let (name, mark) = target(t)?;
        let r = click_ui_element_ext(window, name.as_deref(), mark, "left", false, None, allow_protected)?;
        return Ok(("click".into(), describe(&name, mark), note_of(&r)));
    }
    if let Some(t) = step.get("set") {
        let (name, mark) = target(t)?;
        let text = step.get("text").and_then(Value::as_str).ok_or_else(|| anyhow!("set için text gerekli"))?;
        let r = click_ui_element_ext(window, name.as_deref(), mark, "left", false, Some(text), allow_protected)?;
        return Ok(("set".into(), describe(&name, mark), note_of(&r)));
    }
    if let Some(t) = step.get("select") {
        let (name, mark) = target(t)?;
        let option = step.get("option").and_then(Value::as_str).ok_or_else(|| anyhow!("select için option gerekli"))?;
        click_ui_element_ext(window, name.as_deref(), mark, "left", false, None, allow_protected)?;
        // Açılır liste ayrı bir açılır yüzeyde belirir; seçenek görünene kadar bekle.
        wait_for_condition("ui_match", Some(option), window, 2000)?;
        let r = click_ui_element_ext(window, Some(option), None, "left", false, None, allow_protected)?;
        return Ok(("select".into(), format!("{} → {}", describe(&name, mark), option), note_of(&r)));
    }
    if let Some(text) = step.get("type").and_then(Value::as_str) {
        type_text(text, window, allow_protected)?;
        return Ok(("type".into(), String::new(), "ok".into()));
    }
    if let Some(keys) = step.get("key").and_then(Value::as_str) {
        press_key_combo(keys, window, allow_protected)?;
        return Ok(("key".into(), keys.to_string(), "ok".into()));
    }
    if let Some(text) = step.get("wait").and_then(Value::as_str) {
        let timeout = step.get("timeout_ms").and_then(Value::as_u64).unwrap_or(DEFAULT_EXPECT_MS);
        let ms = wait_text(window, text, false, timeout)?;
        return Ok(("wait".into(), text.to_string(), format!("{} ms", ms)));
    }
    Err(anyhow!("bilinmeyen adım: click | set | select | type | key | wait bekleniyor"))
}

/// Programı yürütür. Dönüş: adım özetleri, ilk hata (varsa) ve son kompakt durum.
pub fn run_program(program: &Value, allow_protected: bool) -> Result<Value> {
    let window = program.get("window").and_then(Value::as_str);
    let steps = program
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("program: steps dizisi gerekli"))?;
    if steps.len() > MAX_STEPS {
        return Err(anyhow!("en fazla {} adım", MAX_STEPS));
    }

    let started = Instant::now();
    let mut results = Vec::new();
    let mut failure = None;
    for (i, step) in steps.iter().enumerate() {
        let step_started = Instant::now();
        let outcome = run_step(window, step, allow_protected)
            .and_then(|(op, target, note)| Ok((op, target, note, check_expect(window, step)?)));
        let ms = step_started.elapsed().as_millis() as u64;
        match outcome {
            Ok((op, target, note, expect)) => {
                let mut r = json!({ "step": i + 1, "op": op, "ok": true, "ms": ms });
                if !target.is_empty() {
                    r["target"] = json!(target);
                }
                if note != "ok" {
                    r["note"] = json!(note);
                }
                if let Some(e) = expect {
                    r["expect"] = json!(e);
                }
                results.push(r);
            }
            Err(e) => {
                results.push(json!({ "step": i + 1, "ok": false, "ms": ms, "error": e.to_string() }));
                failure = Some(i + 1);
                break;
            }
        }
    }

    let mut out = json!({
        "ok": failure.is_none(),
        "completed": results.iter().filter(|r| r["ok"] == true).count(),
        "total": steps.len(),
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "steps": results,
    });
    if let Some(i) = failure {
        out["failed_step"] = json!(i);
        out["hint"] = json!("Program durdu; aşağıdaki durumdan devam et (kalan adımları yeni programla gönder).");
    }
    // Son durum her zaman eklenir: ajanın ayrı bir snapshot turuna ihtiyacı kalmasın.
    if let Ok(snap) = crate::eyes::snapshot::build_snapshot(window, 40, false, None, None) {
        out["after"] = snap;
    }
    Ok(out)
}
