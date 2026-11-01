//! Eylem programı (`pc_eye_run`): beklentili adım dizisi tek çağrıda.
//!
//! Ajan her adımda modele dönüp "şimdi ne yapayım" diye sormak yerine bir kez planlar: adımları
//! ve her adımdan sonra görmesi gerekeni (`expect`) yazar. Yürütücü adımları sırayla çalıştırır,
//! her birini doğrular ve ilk sapmada durup o anki kompakt durumu döner. Ölçüm: form doldurma
//! 7–14 tur sürüyordu; program ile bir planlama + bir doğrulama turu.

use crate::director::input::{click_ui_element_ext, press_key_combo, type_text};
use crate::director::recipes;
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

/// Makbuzdaki `[mark] ad (rol)` hedefinden eleman adı: tarif mark'la değil adla saklanır.
fn receipt_name(target: &str) -> Option<String> {
    let rest = target.split_once("] ").map(|(_, r)| r)?;
    let name = rest.rsplit_once(" (").map_or(rest, |(n, _)| n).trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Adımın tarife yazılacak hali: mark yerine çözülmüş ad (çözülemezse `None`: iz kopar).
fn portable_target(name: &Option<String>, r: &crate::director::input::ActionReceipt) -> Option<String> {
    name.clone().or_else(|| receipt_name(&r.target))
}

struct StepOutcome {
    op: String,
    target: String,
    note: String,
    /// Tarif için taşınabilir adım; `None` ise bu program tarif olarak saklanmaz.
    portable: Option<Value>,
}

fn run_step(window: Option<&str>, step: &Value, allow_protected: bool) -> Result<StepOutcome> {
    let note_of = |r: &crate::director::input::ActionReceipt| match &r.verification {
        Some(v) if !v.visible_change => "görünür değişiklik yok".to_string(),
        _ => "ok".to_string(),
    };
    let done = |op: &str, target: String, note: String, portable: Option<Value>| StepOutcome {
        op: op.into(),
        target,
        note,
        portable,
    };
    if let Some(t) = step.get("click") {
        let (name, mark) = target(t)?;
        let r = click_ui_element_ext(window, name.as_deref(), mark, "left", false, None, allow_protected)?;
        let portable = portable_target(&name, &r).map(|n| json!({ "click": n }));
        return Ok(done("click", describe(&name, mark), note_of(&r), portable));
    }
    if let Some(t) = step.get("set") {
        let (name, mark) = target(t)?;
        let text = step.get("text").and_then(Value::as_str).ok_or_else(|| anyhow!("set için text gerekli"))?;
        let r = click_ui_element_ext(window, name.as_deref(), mark, "left", false, Some(text), allow_protected)?;
        let portable = portable_target(&name, &r).map(|n| json!({ "set": n, "text": text }));
        return Ok(done("set", describe(&name, mark), note_of(&r), portable));
    }
    if let Some(t) = step.get("select") {
        let (name, mark) = target(t)?;
        let option = step.get("option").and_then(Value::as_str).ok_or_else(|| anyhow!("select için option gerekli"))?;
        // Ad bir etikete denk geliyorsa (GTK4 DropDown'un adı seçili değeridir) yanındaki seçici açılır.
        let chooser = match (&name, mark) {
            (Some(n), None) => crate::director::input::resolve_chooser(window, n),
            _ => None,
        };
        let opened = match chooser {
            Some(m) => click_ui_element_ext(window, None, Some(m), "left", false, None, allow_protected)?,
            None => click_ui_element_ext(window, name.as_deref(), mark, "left", false, None, allow_protected)?,
        };
        // Açılır liste ayrı bir açılır yüzeyde belirir; seçenek görünene kadar bekle.
        wait_for_condition("ui_match", Some(option), window, 2000)?;
        let r = click_ui_element_ext(window, Some(option), None, "left", false, None, allow_protected)?;
        let portable = portable_target(&name, &opened).map(|n| json!({ "select": n, "option": option }));
        return Ok(done("select", format!("{} → {}", describe(&name, mark), option), note_of(&r), portable));
    }
    if let Some(text) = step.get("type").and_then(Value::as_str) {
        type_text(text, window, allow_protected)?;
        crate::director::input::settle_after_input();
        return Ok(done("type", String::new(), "ok".into(), Some(json!({ "type": text }))));
    }
    if let Some(keys) = step.get("key").and_then(Value::as_str) {
        press_key_combo(keys, window, allow_protected)?;
        crate::director::input::settle_after_input();
        return Ok(done("key", keys.to_string(), "ok".into(), Some(json!({ "key": keys }))));
    }
    if let Some(text) = step.get("wait").and_then(Value::as_str) {
        let timeout = step.get("timeout_ms").and_then(Value::as_u64).unwrap_or(DEFAULT_EXPECT_MS);
        let ms = wait_text(window, text, false, timeout)?;
        return Ok(done("wait", text.to_string(), format!("{} ms", ms), Some(json!({ "wait": text }))));
    }
    Err(anyhow!("bilinmeyen adım: click | set | select | type | key | wait bekleniyor"))
}

/// Programı yürütür. Dönüş: adım özetleri, ilk hata (varsa) ve son kompakt durum.
pub fn run_program(program: &Value, allow_protected: bool) -> Result<Value> {
    let window = program.get("window").and_then(Value::as_str);
    // `recipe` + `args`: kayıtlı bir akışı yeniden oynat.
    let replay = match program.get("recipe").and_then(Value::as_str) {
        Some(key) => {
            let recipe = recipes::find(key)?;
            let empty = serde_json::Map::new();
            let args = program.get("args").and_then(Value::as_object).unwrap_or(&empty);
            let steps = recipes::instantiate(&recipe, args)?;
            Some((recipe["id"].as_str().unwrap_or(key).to_string(), steps))
        }
        None => None,
    };
    let steps = match &replay {
        Some((_, steps)) => steps,
        None => program
            .get("steps")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("program: steps (ya da recipe) gerekli"))?,
    };
    if steps.len() > MAX_STEPS {
        return Err(anyhow!("en fazla {} adım", MAX_STEPS));
    }

    let started = Instant::now();
    let mut results = Vec::new();
    let mut failure = None;
    let mut portable: Option<Vec<Value>> = Some(Vec::new());
    for (i, step) in steps.iter().enumerate() {
        let step_started = Instant::now();
        let outcome = run_step(window, step, allow_protected)
            .and_then(|o| Ok((o, check_expect(window, step)?)));
        let ms = step_started.elapsed().as_millis() as u64;
        match outcome {
            Ok((o, expect)) => {
                let mut r = json!({ "step": i + 1, "op": o.op, "ok": true, "ms": ms });
                if !o.target.is_empty() {
                    r["target"] = json!(o.target);
                }
                if o.note != "ok" {
                    r["note"] = json!(o.note);
                }
                if let Some(e) = expect {
                    r["expect"] = json!(e);
                }
                results.push(r);
                // Iskalamış olabilecek bir tıklama tarife girmez.
                portable = match (portable, o.portable, o.note.starts_with("görünür değişiklik yok")) {
                    (Some(mut acc), Some(mut p), false) => {
                        for k in ["expect", "timeout_ms"] {
                            if let Some(v) = step.get(k) {
                                p[k] = v.clone();
                            }
                        }
                        acc.push(p);
                        Some(acc)
                    }
                    _ => None,
                };
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
        if failure.is_none() {
            learn(&snap, replay.as_ref(), portable);
        }
        out["after"] = snap;
    }
    if let Some((id, _)) = &replay {
        out["recipe"] = json!(id);
    }
    Ok(out)
}

/// Başarılı programı tarif hafızasına işler (ya da oynatılan tarifin kullanımını sayar).
fn learn(after: &Value, replay: Option<&(String, Vec<Value>)>, portable: Option<Vec<Value>>) {
    let target = &after["target"];
    let (Some(address), Some(class)) = (target["address"].as_str(), target["class"].as_str()) else {
        return;
    };
    match (replay, portable) {
        (Some((id, concrete)), _) => recipes::note_replay(address, class, id, concrete.clone()),
        (None, Some(steps)) => {
            let names: Vec<String> = target["elements"]
                .as_array()
                .map(|a| a.iter().filter_map(|e| e["name"].as_str().map(String::from)).collect())
                .unwrap_or_default();
            recipes::record(address, class, &steps, &names);
        }
        (None, None) => recipes::break_trail(),
    }
}

#[cfg(test)]
mod tests {
    use super::receipt_name;

    #[test]
    fn receipt_name_strips_mark_and_role() {
        assert_eq!(receipt_name("[7] Gönder (push button)").as_deref(), Some("Gönder"));
        assert_eq!(receipt_name("[3] Ad (Soyad) (entry)").as_deref(), Some("Ad (Soyad)"));
        assert_eq!(receipt_name("[3]  (entry)"), None);
    }
}
