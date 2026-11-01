//! Tarif hafızası: başarılı eylem dizilerini yeniden oynatılabilir hale getirir.
//!
//! Ajanın bir kez çözdüğü iş (formu doldur, gönder) bir dahaki sefere tek çağrı olmalı; sabit
//! işler hiç modele gitmeden `pc-eye recipe run` ile (ör. bir Hyprland kısayolundan) oynatılabilir.
//!
//! - Kayıt: başarılı `pc_eye_run` programları ve aynı pencereye hemen ardından gelen başarılı
//!   `click_ui` eylemleri bir "iz" oluşturur; iz iki adımı geçince tarif olarak saklanır.
//! - Hedefler mark yerine eleman adıyla saklanır (mark'lar oturuma bağlıdır).
//! - Yazılan değerler diske yazılmaz: `set`/`select`/`type` değerleri parametre olur.
//! - Pencere başlığı değil sınıf + gerekli eleman adları eşleşir: aynı form farklı başlıkla da tanınır.

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_RECIPES: usize = 200;
/// Bir eylemin önceki ize eklenmesi için azami ara (ajan turu + araç süresi).
const TRAIL_GAP: Duration = Duration::from_secs(120);
const MAX_TRAIL_STEPS: usize = 40;
/// Snapshot'ta gösterilen en fazla tarif sayısı.
const MAX_SUGGESTED: usize = 3;

/// `PC_EYE_RECIPES` (dosya yolu) ölçüm setinin her görevi temiz hafızayla başlatması içindir.
pub fn store_path() -> PathBuf {
    if let Ok(p) = std::env::var("PC_EYE_RECIPES") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share"));
    base.join("pc-eye").join("recipes.json")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn load() -> Vec<Value> {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("recipes").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

fn save_all(mut recipes: Vec<Value>) -> Result<()> {
    if recipes.len() > MAX_RECIPES {
        // En uzun süredir kullanılmayanlar düşer.
        recipes.sort_by_key(|r| std::cmp::Reverse(r["last_used"].as_u64().unwrap_or(0)));
        recipes.truncate(MAX_RECIPES);
    }
    let path = store_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(&json!({ "version": 1, "recipes": recipes }))?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// FNV-1a: kimlik Rust sürümünden bağımsız kalsın (DefaultHasher kararlı değildir).
fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

fn step_target(step: &Value) -> Option<(&'static str, &str)> {
    for op in ["click", "set", "select"] {
        if let Some(t) = step.get(op).and_then(Value::as_str) {
            return Some((op, t));
        }
    }
    None
}

/// Somut adımlardan (gerçek değerlerle) parametreli tarif adımları çıkarır.
/// Dönüş: (adımlar, parametre adları sırasıyla).
pub fn parametrize(steps: &[Value]) -> (Vec<Value>, Vec<String>) {
    let mut params: Vec<String> = Vec::new();
    let mut values: Vec<(String, String)> = Vec::new();
    let fresh = |base: &str, params: &mut Vec<String>| {
        let mut name = base.trim().to_string();
        if name.is_empty() {
            name = "text".into();
        }
        let mut n = 2;
        let root = name.clone();
        while params.contains(&name) {
            name = format!("{}_{}", root, n);
            n += 1;
        }
        params.push(name.clone());
        name
    };

    let mut out = Vec::new();
    for step in steps {
        let mut s = step.clone();
        let obj = s.as_object_mut().expect("adım nesnedir");
        let field = if obj.contains_key("set") {
            Some(("set", "text"))
        } else if obj.contains_key("select") {
            Some(("select", "option"))
        } else if obj.contains_key("type") {
            Some(("type", "type"))
        } else {
            None
        };
        if let Some((op, field)) = field {
            if let Some(val) = obj.get(field).and_then(Value::as_str).map(String::from) {
                let base = if op == "type" { "text".to_string() } else { obj[op].as_str().unwrap_or("").to_string() };
                let p = fresh(&base, &mut params);
                obj.insert(field.into(), json!(format!("{{{}}}", p)));
                values.push((p, val));
            }
        }
        out.push(s);
    }
    // Beklenen metin girilen değeri içeriyorsa ("Kaydedildi: Ali") o da parametreye bağlanır.
    for s in out.iter_mut() {
        for key in ["expect", "wait"] {
            if let Some(text) = s.get(key).and_then(Value::as_str).map(String::from) {
                s[key] = json!(bind_values(&text, &values));
            } else if let Some(o) = s.get_mut(key).and_then(Value::as_object_mut) {
                for k in ["text", "gone"] {
                    if let Some(t) = o.get(k).and_then(Value::as_str).map(String::from) {
                        o.insert(k.into(), json!(bind_values(&t, &values)));
                    }
                }
            }
        }
    }
    (out, params)
}

fn bind_values(text: &str, values: &[(String, String)]) -> String {
    let mut t = text.to_string();
    for (p, v) in values {
        if v.chars().count() >= 2 && t.contains(v.as_str()) {
            t = t.replace(v.as_str(), &format!("{{{}}}", p));
        }
    }
    t
}

/// Parametreleri yerine koyar; eksik parametre hata verir.
pub fn instantiate(recipe: &Value, args: &Map<String, Value>) -> Result<Vec<Value>> {
    let params: Vec<String> = recipe["params"]
        .as_array()
        .map(|a| a.iter().filter_map(|p| p.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let missing: Vec<&String> = params.iter().filter(|p| !args.contains_key(p.as_str())).collect();
    if !missing.is_empty() {
        return Err(anyhow!(
            "tarif `{}` için eksik args: {:?} (beklenen: {:?})",
            recipe["id"].as_str().unwrap_or("?"),
            missing,
            params
        ));
    }
    let subst = |s: &str| {
        let mut t = s.to_string();
        for p in &params {
            let v = match &args[p.as_str()] {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            t = t.replace(&format!("{{{}}}", p), &v);
        }
        t
    };
    fn walk(v: &Value, f: &dyn Fn(&str) -> String) -> Value {
        match v {
            Value::String(s) => Value::String(f(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| walk(x, f)).collect()),
            Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), walk(x, f))).collect()),
            other => other.clone(),
        }
    }
    let steps = recipe["steps"].as_array().ok_or_else(|| anyhow!("tarifte adım yok"))?;
    Ok(steps.iter().map(|s| walk(s, &subst)).collect())
}

/// "set Ad Soyad → click Gönder": ajanın tarifin işine uyup uymadığına karar vermesi için.
pub fn describe_steps(steps: &[Value]) -> String {
    steps
        .iter()
        .map(|s| match step_target(s) {
            Some((op, t)) => format!("{} {}", op, t),
            None => s
                .as_object()
                .and_then(|o| o.iter().find(|(k, _)| !matches!(k.as_str(), "expect" | "timeout_ms")))
                .map(|(k, v)| format!("{} {}", k, v.as_str().unwrap_or("")))
                .unwrap_or_default(),
        })
        .collect::<Vec<_>>()
        .join(" → ")
}

fn skeleton(class: &str, steps: &[Value]) -> String {
    let mut s = class.to_lowercase();
    for step in steps {
        s.push('|');
        s.push_str(&serde_json::to_string(step).unwrap_or_default());
    }
    s
}

/// Tarifin uygulanabilmesi için pencerede bulunması gereken adlar. Tercihen eylemden sonra da
/// görünen hedefler (form alanları, düğmeler): açılır liste seçenekleri gibi geçici elemanlar
/// gerekli sayılırsa tarif bir daha hiç önerilmez.
fn required_names(steps: &[Value], after_names: &[String]) -> Vec<String> {
    let lower: Vec<String> = after_names.iter().map(|n| n.to_lowercase()).collect();
    let mut targets: Vec<String> = Vec::new();
    for s in steps {
        if let Some((_, t)) = step_target(s) {
            if !t.contains('{') && !targets.iter().any(|x| x == t) {
                targets.push(t.to_string());
            }
        }
    }
    let visible: Vec<String> = targets
        .iter()
        .filter(|t| {
            let t = t.to_lowercase();
            lower.iter().any(|n| n.contains(&t))
        })
        .cloned()
        .collect();
    if !visible.is_empty() {
        return visible;
    }
    // Pencere kapandıysa: değer giren hedefler kalıcı alanlardır.
    steps
        .iter()
        .filter(|s| s.get("set").is_some() || s.get("select").is_some())
        .filter_map(|s| step_target(s).map(|(_, t)| t.to_string()))
        .collect()
}

/// Tarifi saklar ya da aynısı varsa görülme sayısını artırır. Dönüş: kimlik.
fn upsert(class: &str, concrete: &[Value], after_names: &[String]) -> Result<String> {
    let (steps, params) = parametrize(concrete);
    let id = format!("r{:07x}", fnv1a(&skeleton(class, &steps)) & 0x0fff_ffff);
    let mut all = load();
    let t = now();
    if let Some(r) = all.iter_mut().find(|r| r["id"] == id.as_str()) {
        r["seen"] = json!(r["seen"].as_u64().unwrap_or(1) + 1);
        r["last_used"] = json!(t);
    } else {
        all.push(json!({
            "id": id,
            "name": format!("{}: {}", class, describe_steps(&steps)),
            "class": class,
            "requires": required_names(&steps, after_names),
            "params": params,
            "steps": steps,
            "created": t,
            "last_used": t,
            "seen": 1,
            "runs": 0,
        }));
    }
    save_all(all)?;
    Ok(id)
}

fn remove_if_unused(id: &str) {
    let mut all = load();
    let before = all.len();
    all.retain(|r| !(r["id"] == id && r["runs"].as_u64().unwrap_or(0) == 0 && r["seen"].as_u64().unwrap_or(1) <= 1));
    if all.len() != before {
        let _ = save_all(all);
    }
}

// ---------------------------------------------------------------------------
// İz: aynı pencereye art arda gelen başarılı eylemler (süreç içi)
// ---------------------------------------------------------------------------

struct Trail {
    address: String,
    class: String,
    steps: Vec<Value>,
    at: Instant,
    /// Bu izin son kaydettiği tarif; iz uzayınca kısa hali (yalnız bu izden geldiyse) silinir.
    saved: Option<String>,
    /// İz bir tarifin oynatılmasıyla başladı: o tarif silinmez.
    from_recipe: bool,
}

static TRAIL: Mutex<Option<Trail>> = Mutex::new(None);

/// Başarılı eylemleri ize ekler; iz en az iki adımsa tarif olarak saklar.
/// `after_names`: eylemden sonra pencerede görünen eleman adları.
pub fn record(address: &str, class: &str, steps: &[Value], after_names: &[String]) -> Option<String> {
    if address.is_empty() || steps.is_empty() {
        return None;
    }
    let mut guard = TRAIL.lock().ok()?;
    let continues = guard
        .as_ref()
        .map_or(false, |t| t.address == address && t.at.elapsed() < TRAIL_GAP);
    if !continues {
        *guard = Some(Trail {
            address: address.into(),
            class: class.into(),
            steps: Vec::new(),
            at: Instant::now(),
            saved: None,
            from_recipe: false,
        });
    }
    let trail = guard.as_mut()?;
    trail.steps.extend(steps.iter().cloned());
    trail.at = Instant::now();
    if trail.steps.len() > MAX_TRAIL_STEPS || trail.steps.len() < 2 {
        return None;
    }
    let id = upsert(&trail.class, &trail.steps, after_names).ok()?;
    if let Some(old) = trail.saved.replace(id.clone()) {
        if old != id && !trail.from_recipe {
            remove_if_unused(&old);
        }
    }
    trail.from_recipe = false;
    Some(id)
}

/// Bir tarif oynatıldı: iz o tarifin somut adımlarıyla başlar ki ardından gelen eylemler onu uzatsın.
pub fn note_replay(address: &str, class: &str, id: &str, concrete: Vec<Value>) {
    if let Ok(mut g) = TRAIL.lock() {
        *g = Some(Trail {
            address: address.into(),
            class: class.into(),
            steps: concrete,
            at: Instant::now(),
            saved: Some(id.into()),
            from_recipe: true,
        });
    }
    let mut all = load();
    if let Some(r) = all.iter_mut().find(|r| r["id"] == id) {
        r["runs"] = json!(r["runs"].as_u64().unwrap_or(0) + 1);
        r["last_used"] = json!(now());
        let _ = save_all(all);
    }
}

/// Koordinatla tıklama gibi yeniden üretilemeyen bir eylem izi koparır.
pub fn break_trail() {
    if let Ok(mut g) = TRAIL.lock() {
        *g = None;
    }
}

/// Kimlik ya da ad (parça) ile tarif bulur.
pub fn find(key: &str) -> Result<Value> {
    let all = load();
    if let Some(r) = all.iter().find(|r| r["id"] == key) {
        return Ok(r.clone());
    }
    let k = key.to_lowercase();
    let hits: Vec<&Value> = all
        .iter()
        .filter(|r| r["name"].as_str().map_or(false, |n| n.to_lowercase().contains(&k)))
        .collect();
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(anyhow!("tarif bulunamadı: {}", key)),
        many => Err(anyhow!(
            "`{}` birden çok tarife uyuyor: {}",
            key,
            many.iter().map(|r| r["id"].as_str().unwrap_or("?")).collect::<Vec<_>>().join(", ")
        )),
    }
}

pub fn rename(id: &str, name: &str) -> Result<()> {
    let mut all = load();
    let r = all.iter_mut().find(|r| r["id"] == id).ok_or_else(|| anyhow!("tarif bulunamadı: {}", id))?;
    r["name"] = json!(name);
    save_all(all)
}

pub fn remove(id: &str) -> Result<()> {
    let mut all = load();
    let before = all.len();
    all.retain(|r| r["id"] != id);
    if all.len() == before {
        return Err(anyhow!("tarif bulunamadı: {}", id));
    }
    save_all(all)
}

/// Bu pencereye uyan tarifler (sınıf + gerekli adların hepsi görünür).
pub fn applicable(class: &str, element_names: &[String]) -> Vec<Value> {
    let names: Vec<String> = element_names.iter().map(|n| n.to_lowercase()).collect();
    let mut hits: Vec<Value> = load()
        .into_iter()
        .filter(|r| r["class"].as_str().map_or(false, |c| c.eq_ignore_ascii_case(class)))
        .filter(|r| {
            let req = r["requires"].as_array().cloned().unwrap_or_default();
            !req.is_empty()
                && req.iter().all(|q| {
                    let q = q.as_str().unwrap_or("").to_lowercase();
                    names.iter().any(|n| n.contains(&q))
                })
        })
        .collect();
    hits.sort_by_key(|r| {
        std::cmp::Reverse((r["runs"].as_u64().unwrap_or(0) + r["seen"].as_u64().unwrap_or(0), r["last_used"].as_u64().unwrap_or(0)))
    });
    hits.truncate(MAX_SUGGESTED);
    hits
}

/// Tam snapshot'a bu pencereye uyan tarifleri ekler. Ajan isteğe bağlı parametre kullanmadığı
/// için tarifler ayrı bir araçla değil, zaten aldığı ilk görüntüde önerilir.
pub fn annotate_snapshot(snap: &mut Value) {
    let target = &snap["target"];
    let (Some(class), Some(elements)) = (target["class"].as_str(), target["elements"].as_array()) else {
        return;
    };
    let names: Vec<String> = elements.iter().filter_map(|e| e["name"].as_str().map(String::from)).collect();
    let hits = applicable(class, &names);
    if hits.is_empty() {
        return;
    }
    snap["recipes"] = json!(hits
        .iter()
        .map(|r| json!({
            "recipe": r["id"],
            "does": describe_steps(r["steps"].as_array().map(Vec::as_slice).unwrap_or(&[])),
            "args": r["params"],
        }))
        .collect::<Vec<_>>());
    snap["recipes_hint"] =
        json!("A recipe replays a past successful flow in one call: pc_eye_run {\"recipe\":ID,\"args\":{..},\"window\":..}. Use it only if `does` matches the task.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form_steps() -> Vec<Value> {
        vec![
            json!({"set": "Ad Soyad", "text": "Ali Veli"}),
            json!({"click": "Bültene abone ol"}),
            json!({"select": "Şehir", "option": "İzmir"}),
            json!({"click": "Gönder", "expect": "Kaydedildi: Ali Veli"}),
        ]
    }

    #[test]
    fn values_become_params_and_never_stay_in_steps() {
        let (steps, params) = parametrize(&form_steps());
        assert_eq!(params, vec!["Ad Soyad", "Şehir"]);
        let text = serde_json::to_string(&steps).unwrap();
        assert!(!text.contains("Ali Veli") && !text.contains("İzmir"), "{}", text);
        assert_eq!(steps[3]["expect"], "Kaydedildi: {Ad Soyad}");
    }

    #[test]
    fn instantiate_round_trips_and_reports_missing() {
        let (steps, params) = parametrize(&form_steps());
        let recipe = json!({"id": "r1", "steps": steps, "params": params});
        let mut args = Map::new();
        args.insert("Ad Soyad".into(), json!("Ayşe Kaya"));
        assert!(instantiate(&recipe, &args).unwrap_err().to_string().contains("Şehir"));
        args.insert("Şehir".into(), json!("Bursa"));
        let out = instantiate(&recipe, &args).unwrap();
        assert_eq!(out[0]["text"], "Ayşe Kaya");
        assert_eq!(out[2]["option"], "Bursa");
        assert_eq!(out[3]["expect"], "Kaydedildi: Ayşe Kaya");
    }

    #[test]
    fn transient_targets_are_not_required() {
        // Açılır liste seçeneğine doğrudan tıklanmış: eylemden sonra görünmüyor → gerekli değil.
        let steps = vec![json!({"set": "Ad", "text": "{Ad}"}), json!({"click": "İzmir"}), json!({"click": "Gönder"})];
        let req = required_names(&steps, &["Ad".into(), "Gönder".into(), "Kaydedildi".into()]);
        assert_eq!(req, vec!["Ad", "Gönder"]);
    }

    #[test]
    fn repeated_params_get_suffixes() {
        let (_, params) = parametrize(&[json!({"type": "a"}), json!({"type": "b"})]);
        assert_eq!(params, vec!["text", "text_2"]);
    }

    #[test]
    fn store_trail_and_applicability() {
        let dir = std::env::temp_dir().join(format!("pceye-recipes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("PC_EYE_RECIPES", dir.join("r.json"));
        let after: Vec<String> = ["Ad Soyad", "Bültene abone ol", "Şehir", "Gönder"].iter().map(|s| s.to_string()).collect();

        // Program doldurdu, ardından ajan Gönder'e ayrıca tıkladı: tek tarif, kısa hali silinir.
        let steps = form_steps();
        let short = record("0xabc", "firefox", &steps[..3], &after).unwrap();
        let long = record("0xabc", "firefox", &steps[3..], &after).unwrap();
        assert_ne!(short, long);
        let all = load();
        assert_eq!(all.len(), 1, "{:?}", all);
        assert_eq!(all[0]["id"], long.as_str());

        assert_eq!(applicable("Firefox", &after).len(), 1);
        assert!(applicable("firefox", &after[..2]).is_empty());
        assert!(applicable("kitty", &after).is_empty());

        // Aynı akış başka değerlerle: yeni tarif değil, görülme sayısı.
        break_trail();
        let mut again = form_steps();
        again[0]["text"] = json!("Can Er");
        again[3]["expect"] = json!("Kaydedildi: Can Er");
        record("0xdef", "firefox", &again, &after).unwrap();
        let all = load();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0]["seen"], 2);
        std::env::remove_var("PC_EYE_RECIPES");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
