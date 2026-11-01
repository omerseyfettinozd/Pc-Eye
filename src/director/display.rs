use crate::director::input::ActionReceipt;
use crate::eyes::compositor::{hypr_query_json, hypr_request_raw};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::thread;
use std::time::{Duration, Instant};

/// Sanal çıkışın kullanıcının ekranlarından uzaklığı (px). Bitişik olmayan çıkışa imleç
/// kayamaz; ajan sanal ekranda çalışırken kullanıcının faresi kendi ekranında kalır.
const ISOLATION_GAP: i64 = 1000;

fn monitors() -> Result<Vec<Value>> {
    Ok(hypr_query_json("monitors all")?.as_array().cloned().unwrap_or_default())
}

fn find_monitor(name: &str) -> Result<Option<Value>> {
    Ok(monitors()?.into_iter().find(|m| m["name"].as_str() == Some(name)))
}

fn monitor_summary(m: &Value) -> Value {
    json!({
        "name": m["name"],
        "width": m["width"],
        "height": m["height"],
        "refresh_rate": m["refreshRate"],
        "x": m["x"],
        "y": m["y"],
        "scale": m["scale"],
        "workspace_id": m["activeWorkspace"]["id"],
        "workspace_name": m["activeWorkspace"]["name"],
        "headless": is_headless(m),
    })
}

/// Headless çıkışların EDID'i yoktur: üretici/model/açıklama boş gelir.
fn is_headless(m: &Value) -> bool {
    m["name"].as_str().map_or(false, |n| n.starts_with("HEADLESS-"))
        || ["make", "model", "description"]
            .iter()
            .all(|k| m[*k].as_str().map_or(true, |v| v.trim().is_empty()))
}

/// `WxH` veya `WxH@R` biçimini doğrular; Hyprland mode dizesi olarak döner.
fn parse_mode(mode: &str) -> Result<String> {
    let (res, rate) = match mode.split_once('@') {
        Some((r, hz)) => (r, Some(hz)),
        None => (mode, None),
    };
    let (w, h) = res
        .split_once('x')
        .ok_or_else(|| anyhow!("mod `WxH[@R]` biçiminde olmalı: {}", mode))?;
    let w: u32 = w.trim().parse().map_err(|_| anyhow!("geçersiz genişlik: {}", w))?;
    let h: u32 = h.trim().parse().map_err(|_| anyhow!("geçersiz yükseklik: {}", h))?;
    if !(320..=7680).contains(&w) || !(240..=4320).contains(&h) {
        return Err(anyhow!("çözünürlük sınır dışı: {}x{}", w, h));
    }
    let hz: f64 = match rate {
        Some(r) => r.trim().parse().map_err(|_| anyhow!("geçersiz yenileme hızı: {}", r))?,
        None => 60.0,
    };
    Ok(format!("{}x{}@{}", w, h, hz))
}

/// Tüm diğer çıkışların sağına `ISOLATION_GAP` boşlukla yerleşen konum.
fn isolated_position(exclude: &str) -> Result<(i64, i64)> {
    let right = monitors()?
        .iter()
        .filter(|m| m["name"].as_str() != Some(exclude))
        .map(|m| m["x"].as_i64().unwrap_or(0) + m["width"].as_i64().unwrap_or(0))
        .max()
        .unwrap_or(0);
    Ok((right + ISOLATION_GAP, 0))
}

/// Çıkışın modunu/konumunu uygular: önce Lua (`hl.monitor`), olmazsa klasik `keyword monitor`.
fn apply_monitor_rule(name: &str, mode: &str, pos: (i64, i64), scale: f64) -> Result<()> {
    let lua = format!(
        "eval hl.monitor({{output=\"{}\", mode=\"{}\", position=\"{}x{}\", scale={}}})",
        name, mode, pos.0, pos.1, scale
    );
    if hypr_request_raw(&lua)?.trim() == "ok" {
        return Ok(());
    }
    let legacy = format!("keyword monitor {},{},{}x{},{}", name, mode, pos.0, pos.1, scale);
    let resp = hypr_request_raw(&legacy)?;
    if resp.trim() == "ok" {
        Ok(())
    } else {
        Err(anyhow!("monitör kuralı uygulanamadı ({}): {}", name, resp.trim()))
    }
}

fn wait_monitor<F: Fn(&Option<Value>) -> bool>(name: &str, done: F, timeout_ms: u64) -> Result<Option<Value>> {
    let start = Instant::now();
    loop {
        let m = find_monitor(name)?;
        if done(&m) || start.elapsed() >= Duration::from_millis(timeout_ms) {
            return Ok(m);
        }
        thread::sleep(Duration::from_millis(40));
    }
}

fn receipt(action: &str, target: &str, details: String) -> ActionReceipt {
    ActionReceipt {
        ok: true,
        action: format!("display_{}", action),
        target: target.to_string(),
        coordinates: None,
        details,
        verification: None,
    }
}

/// Sanal (headless) ekran yönetimi: `list | create | mode | remove`.
///
/// `create` kullanıcının ekranlarından yalıtılmış (bitişik olmayan) bir headless çıkış açar;
/// dönen `workspace_id`, `pc-eye launch --workspace` ile uygulamayı o ekrana göndermek içindir.
pub fn manage_display(
    action: &str,
    name: Option<&str>,
    mode: Option<&str>,
    position: Option<&str>,
    scale: Option<f64>,
) -> Result<ActionReceipt> {
    match action.to_ascii_lowercase().as_str() {
        "list" => {
            let list: Vec<Value> = monitors()?.iter().map(monitor_summary).collect();
            Ok(receipt("list", "all", serde_json::to_string(&list)?))
        }
        "create" => {
            if let Some(n) = name {
                if n.is_empty() || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                    return Err(anyhow!("çıkış adı yalnız harf, rakam, `-`, `_` içerebilir: {}", n));
                }
                if find_monitor(n)?.is_some() {
                    return Err(anyhow!("`{}` adlı çıkış zaten var", n));
                }
            }
            let before: Vec<String> = monitors()?
                .iter()
                .filter_map(|m| m["name"].as_str().map(String::from))
                .collect();
            // Çıkışın varsayılan çalışma alanı adlı olsun ("name:<çıkış>"): yoksa Hyprland ona ilk boş
            // numarayı (2, 3…) veriyor. Kullanıcı Super+3'e basınca kendi uygulaması (Steam) görünmez
            // çıkışa açıldı, Super+2 ajan oturumunun penceresini kullanıcının ekranına çekti.
            // Kalıcı değil: çıkış silinince boş çalışma alanı da kaybolur.
            if let Some(n) = name {
                let _ = hypr_request_raw(&format!(
                    "eval hl.workspace_rule({{ workspace = \"name:{}\", monitor = \"{}\", default = true }})",
                    n.to_ascii_lowercase(),
                    n
                ));
            }
            let cmd = match name {
                Some(n) => format!("output create headless {}", n),
                None => "output create headless".to_string(),
            };
            let resp = hypr_request_raw(&cmd)?;
            if resp.trim() != "ok" {
                return Err(anyhow!("headless çıkış açılamadı: {}", resp.trim()));
            }
            // Adsız istekte Hyprland `HEADLESS-N` atar; yeni çıkışı önceki listeyle farkından bul.
            let start = Instant::now();
            let created = loop {
                let found = monitors()?.into_iter().find(|m| {
                    let n = m["name"].as_str().unwrap_or("");
                    !before.iter().any(|b| b == n) && name.map_or(true, |want| want == n)
                });
                if let Some(m) = found {
                    break m;
                }
                if start.elapsed() >= Duration::from_millis(2000) {
                    return Err(anyhow!("headless çıkış 2 sn içinde görünmedi"));
                }
                thread::sleep(Duration::from_millis(40));
            };
            let out_name = created["name"].as_str().unwrap_or_default().to_string();
            configure(&out_name, mode, position, scale, "create")
        }
        "mode" => {
            let n = name.ok_or_else(|| anyhow!("mode için --name gerekli"))?;
            if find_monitor(n)?.is_none() {
                return Err(anyhow!("`{}` adlı çıkış yok", n));
            }
            configure(n, mode, position, scale, "mode")
        }
        "remove" => {
            let n = name.ok_or_else(|| anyhow!("remove için --name gerekli"))?;
            let m = find_monitor(n)?.ok_or_else(|| anyhow!("`{}` adlı çıkış yok", n))?;
            // Gerçek panelin kapatılması kullanıcıyı ekransız bırakır; yalnız sanal çıkış silinir.
            if !is_headless(&m) {
                return Err(anyhow!("`{}` fiziksel bir çıkış; yalnız headless çıkışlar kaldırılabilir", n));
            }
            let resp = hypr_request_raw(&format!("output remove {}", n))?;
            if resp.trim() != "ok" {
                return Err(anyhow!("çıkış kaldırılamadı: {}", resp.trim()));
            }
            wait_monitor(n, |m| m.is_none(), 2000)?;
            Ok(receipt("remove", n, format!("headless çıkış kaldırıldı: {}", n)))
        }
        other => Err(anyhow!("Bilinmeyen display aksiyonu: {} (list | create | mode | remove)", other)),
    }
}

fn configure(
    name: &str,
    mode: Option<&str>,
    position: Option<&str>,
    scale: Option<f64>,
    action: &str,
) -> Result<ActionReceipt> {
    let current = find_monitor(name)?.ok_or_else(|| anyhow!("`{}` kayboldu", name))?;
    let mode_str = match mode {
        Some(m) => parse_mode(m)?,
        None => format!(
            "{}x{}@{}",
            current["width"].as_i64().unwrap_or(1920),
            current["height"].as_i64().unwrap_or(1080),
            current["refreshRate"].as_f64().map(|r| r.round()).unwrap_or(60.0)
        ),
    };
    let pos = match position.map(str::trim) {
        None | Some("isolated") | Some("auto") => isolated_position(name)?,
        Some(p) => {
            let (x, y) = p
                .split_once(|c| c == 'x' || c == ',' || c == ' ')
                .ok_or_else(|| anyhow!("konum `X,Y` veya `isolated` olmalı: {}", p))?;
            (
                x.trim().parse().map_err(|_| anyhow!("geçersiz X: {}", x))?,
                y.trim().parse().map_err(|_| anyhow!("geçersiz Y: {}", y))?,
            )
        }
    };
    let scale = scale.unwrap_or(1.0);
    if !(0.25..=4.0).contains(&scale) {
        return Err(anyhow!("ölçek 0.25–4 aralığında olmalı: {}", scale));
    }
    apply_monitor_rule(name, &mode_str, pos, scale)?;
    let (w, h) = mode_str.split('@').next().and_then(|r| r.split_once('x')).unwrap();
    let (w, h) = (w.parse::<i64>().unwrap_or(0), h.parse::<i64>().unwrap_or(0));
    let m = wait_monitor(
        name,
        |m| {
            m.as_ref().map_or(false, |m| {
                m["width"].as_i64() == Some(w) && m["height"].as_i64() == Some(h) && m["x"].as_i64() == Some(pos.0)
            })
        },
        2000,
    )?
    .ok_or_else(|| anyhow!("`{}` kural sonrası kayboldu", name))?;
    Ok(receipt(action, name, serde_json::to_string(&monitor_summary(&m))?))
}
