//! Firefox WebExtension (MV2) native messaging köprüsü ve Uzak Hata Ayıklama (RDP) başlatıcısı.
//!
//! Mimaride Firefox eklentisi `pc-eye browser-host` alt sürecini stdin/stdout borularıyla başlatır;
//! bu süreç `<runtime_dir>/browser.sock` Unix soketini açarak CLI ve MCP sorgularını eklentiye
//! çoğullar. AT-SPI erişilebilirlik ağacının yetersiz kaldığı karmaşık web uygulamalarında
//! doğrudan DOM ve sekme denetimi sağlar.

use crate::eyes::vision::runtime_dir;
use crate::session::active_name;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const NATIVE_APP_NAME: &str = "pc_eye_bridge";
const EXTENSION_ID: &str = "pc-eye-bridge@pc-eye.local";

/// Firefox native messaging ana makineden eklentiye en çok 1 MiB çerçeve kabul eder.
const MAX_OUTBOUND_FRAME: usize = 1024 * 1024;
/// Büyük DOM snapshot'larına izin vermek için eklentiden gelen üst sınır 64 MiB tutulur.
const MAX_INBOUND_FRAME: usize = 64 * 1024 * 1024;
const MAX_CLIENT_LINE: usize = 1024 * 1024;
const ACTION_LOG_ROTATE_BYTES: u64 = 2 * 1024 * 1024;

const EXT_MANIFEST_JSON: &str = include_str!("../extension/manifest.json");
const EXT_BACKGROUND_JS: &str = include_str!("../extension/background.js");
const EXT_CONTENT_JS: &str = include_str!("../extension/content.js");

/// Eklentiyle aynı yerleşik yasak liste desenleri: finans, parola kasası ve kimlik sağlayıcıları.
pub(crate) const BUILTIN_DENY_PATTERNS: &[&str] = &[
    "bank",
    "banka",
    "paypal",
    "wise.com",
    "binance",
    "coinbase",
    "1password",
    "bitwarden",
    "lastpass",
    "keepass",
    "e-devlet",
    "turkiye.gov.tr",
    "accounts.google.com",
    "login.microsoftonline.com",
];

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct FilterConfig {
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub allow: Vec<String>,
}

impl FilterConfig {
    pub(crate) fn from_path(path: &Path) -> Self {
        let Ok(raw) = fs::read_to_string(path) else {
            return Self::default();
        };
        Self::from_json_str(&raw)
    }

    pub(crate) fn from_json_str(raw: &str) -> Self {
        let Ok(parsed) = serde_json::from_str::<FilterConfig>(raw) else {
            return Self::default();
        };
        let clean = |list: Vec<String>| -> Vec<String> {
            list.into_iter()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        };
        Self {
            deny: clean(parsed.deny),
            allow: clean(parsed.allow),
        }
    }

    pub(crate) fn to_config_message(&self) -> Value {
        json!({
            "id": 0,
            "op": "config",
            "args": {
                "deny": self.deny,
                "allow": self.allow,
            }
        })
    }

    pub(crate) fn is_host_denied(&self, host: &str) -> bool {
        let h = host.trim().to_lowercase();
        if h.is_empty() {
            return false;
        }
        if self.allow.iter().any(|pat| h.contains(pat.as_str())) {
            return false;
        }
        BUILTIN_DENY_PATTERNS.iter().any(|pat| h.contains(pat))
            || self.deny.iter().any(|pat| h.contains(pat.as_str()))
    }
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("browser.sock")
}

fn info_path_in(dir: &Path) -> PathBuf {
    dir.join("browser.json")
}

fn launch_info_path_in(dir: &Path) -> PathBuf {
    dir.join("browser_launch.json")
}

fn action_log_path_in(dir: &Path) -> PathBuf {
    dir.join("browser_actions.log")
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME ortam değişkeni tanımlı değil"))
}

fn config_file_path() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("pc-eye/browser.json"));
        }
    }
    Ok(home_dir()?.join(".config/pc-eye/browser.json"))
}

fn extension_install_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".local/share/pc-eye/extension"))
}

fn host_wrapper_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".local/share/pc-eye/browser-host.sh"))
}

fn xpi_output_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".local/share/pc-eye/pc-eye-bridge.xpi"))
}

fn native_manifest_paths() -> Result<[PathBuf; 2]> {
    let home = home_dir()?;
    let file = format!("{}.json", NATIVE_APP_NAME);
    Ok([
        home.join(".mozilla/native-messaging-hosts").join(&file),
        home.join(".config/mozilla/native-messaging-hosts").join(&file),
    ])
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Native messaging çerçevesi yazar (4 bayt LE uzunluk + UTF-8 JSON).
pub(crate) fn write_frame<W: Write>(writer: &mut W, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value).context("çerçeve JSON kodlanamadı")?;
    if bytes.len() > MAX_OUTBOUND_FRAME {
        bail!(
            "eklentiye giden ileti 1 MiB sınırını aşıyor ({} bayt)",
            bytes.len()
        );
    }
    let len = (bytes.len() as u32).to_le_bytes();
    writer.write_all(&len).context("çerçeve uzunluğu yazılamadı")?;
    writer.write_all(&bytes).context("çerçeve gövdesi yazılamadı")?;
    writer.flush().context("çerçeve tamponu boşaltılamadı")?;
    Ok(())
}

/// Native messaging çerçevesi okur. Temiz EOF durumunda `Ok(None)` döner.
pub(crate) fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Value>> {
    let mut hdr = [0u8; 4];
    let mut read_hdr = 0usize;
    while read_hdr < 4 {
        match reader.read(&mut hdr[read_hdr..]) {
            Ok(0) if read_hdr == 0 => return Ok(None),
            Ok(0) => bail!("eklenti çerçeve başlığı yarım kaldı"),
            Ok(n) => read_hdr += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("eklenti çerçeve başlığı okunamadı"),
        }
    }
    let len = u32::from_le_bytes(hdr) as usize;
    if len == 0 {
        bail!("eklentiden boş çerçeve alındı");
    }
    if len > MAX_INBOUND_FRAME {
        bail!(
            "eklentiden gelen çerçeve 64 MiB sınırını aşıyor ({} bayt)",
            len
        );
    }
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .context("eklenti çerçeve gövdesi okunamadı")?;
    let val: Value =
        serde_json::from_slice(&buf).context("eklenti çerçevesi geçerli JSON değil")?;
    Ok(Some(val))
}

/// Aynı oturumda ikinci bir Firefox soketi devralmışsa çıkışta yeni soketi silmemek için inode kontrolü.
struct SocketGuard {
    path: PathBuf,
    inode: Option<u64>,
    running: Arc<AtomicBool>,
    ready_cvar: Arc<Condvar>,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        self.ready_cvar.notify_all();
        // Başka sürecin devraldığı sokete sahte bağlantı açmamak için önce inode doğrulanır.
        if let Some(expected_ino) = self.inode {
            if let Ok(meta) = fs::symlink_metadata(&self.path) {
                if meta.ino() == expected_ino {
                    let _ = UnixStream::connect(&self.path);
                    let _ = fs::remove_file(&self.path);
                }
            }
        }
    }
}

struct HostState<W: Write + Send + 'static> {
    writer: Mutex<W>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Value>>>,
    next_id: AtomicU64,
    log_lock: Mutex<()>,
    runtime_dir: PathBuf,
    ready: Mutex<bool>,
    ready_cvar: Arc<Condvar>,
    running: Arc<AtomicBool>,
}

impl<W: Write + Send + 'static> HostState<W> {
    fn mark_ready(&self) {
        if let Ok(mut guard) = self.ready.lock() {
            *guard = true;
            self.ready_cvar.notify_all();
        }
    }

    // hello alınıp config gönderilene dek istemciyi en çok belirtilen süre bekletir.
    fn wait_ready(&self, timeout: Duration) -> bool {
        let Ok(mut guard) = self.ready.lock() else {
            return false;
        };
        let deadline = Instant::now() + timeout;
        while !*guard {
            if !self.running.load(Ordering::SeqCst) {
                return false;
            }
            let Some(rem) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            if rem.is_zero() {
                return false;
            }
            let Ok((next_guard, wait_res)) = self.ready_cvar.wait_timeout(guard, rem) else {
                return false;
            };
            guard = next_guard;
            if wait_res.timed_out() && !*guard {
                return false;
            }
        }
        true
    }

    fn send_to_extension(&self, msg: &Value) -> Result<()> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| anyhow!("eklenti yazıcı kilidi bozuldu"))?;
        write_frame(&mut *guard, msg)
    }

    fn append_action_log(&self, op: &str, args: &Value, ok: bool, elapsed_ms: u64) {
        let Ok(_guard) = self.log_lock.lock() else {
            return;
        };
        let path = action_log_path_in(&self.runtime_dir);
        if let Ok(meta) = fs::metadata(&path) {
            if meta.len() > ACTION_LOG_ROTATE_BYTES {
                let rotated = self.runtime_dir.join("browser_actions.log.1");
                let _ = fs::rename(&path, rotated);
            }
        }
        // Günlük dosyası parola/token sızıntısına karşı yalnızca sahipçe okunabilir açılır.
        let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
        else {
            return;
        };
        let entry = json!({
            "t": now_unix_ms(),
            "op": op,
            "args": sanitize_args_for_log(op, args),
            "ok": ok,
            "ms": elapsed_ms,
        });
        let _ = writeln!(file, "{}", entry);
    }
}

// Hassas alanları (text/code) uzunlukla maskeler; navigate URL'sinden sorgu ve parça kısmını kırpar.
fn sanitize_args_for_log(op: &str, args: &Value) -> Value {
    let mut cloned = args.clone();
    let Some(obj) = cloned.as_object_mut() else {
        return cloned;
    };
    let secret_key = match op {
        "type" => Some("text"),
        "eval" => Some("code"),
        _ => None,
    };
    if let Some(key) = secret_key {
        if let Some(val_str) = obj.get(key).and_then(|v| v.as_str()) {
            let masked = format!("<gizli uzunluk={}>", val_str.chars().count());
            obj.insert(key.to_string(), Value::String(masked));
        }
    } else if op == "navigate" {
        if let Some(url_str) = obj.get("url").and_then(|v| v.as_str()) {
            let end = url_str
                .find(|c| c == '?' || c == '#')
                .unwrap_or(url_str.len());
            obj.insert("url".to_string(), Value::String(url_str[..end].to_string()));
        }
    }
    cloned
}

/// Firefox'un başlattığı native messaging ana makinesi: stdin/stdout'ta eklentiyle, browser.sock'ta istemcilerle konuşur. Eklenti kopunca (stdin EOF) döner.
pub fn run_native_host() -> anyhow::Result<()> {
    let dir = runtime_dir();
    let filter_cfg = config_file_path()
        .map(|p| FilterConfig::from_path(&p))
        .unwrap_or_default();
    run_host_with_io(
        std::io::stdin().lock(),
        std::io::stdout(),
        &dir,
        &filter_cfg,
    )
}

pub(crate) fn run_host_with_io<R, W>(
    mut reader: R,
    writer: W,
    dir: &Path,
    filter_cfg: &FilterConfig,
) -> Result<()>
where
    R: Read,
    W: Write + Send + 'static,
{
    fs::create_dir_all(dir).context("çalışma dizini oluşturulamadı")?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .context("çalışma dizini izinleri 0700 yapılamadı")?;
    // TOCTOU penceresini önlemek için soket geçici adla 0600 açılıp atomik rename ile taşınır.
    let sock_path = dir.join("browser.sock");
    let tmp_sock = dir.join(format!(
        ".browser.sock.{}.{}",
        std::process::id(),
        now_unix_ms()
    ));
    let _ = fs::remove_file(&tmp_sock);
    let listener =
        UnixListener::bind(&tmp_sock).context("browser.sock dinlemeye açılamadı")?;
    if let Err(e) = fs::set_permissions(&tmp_sock, fs::Permissions::from_mode(0o600)) {
        let _ = fs::remove_file(&tmp_sock);
        return Err(e).context("browser.sock izinleri 0600 yapılamadı");
    }
    if let Err(e) = fs::rename(&tmp_sock, &sock_path) {
        let _ = fs::remove_file(&tmp_sock);
        return Err(e).context("browser.sock yerine taşınamadı");
    }
    let inode = fs::symlink_metadata(&sock_path).map(|m| m.ino()).ok();
    let running = Arc::new(AtomicBool::new(true));
    let ready_cvar = Arc::new(Condvar::new());
    let _socket_guard = SocketGuard {
        path: sock_path,
        inode,
        running: Arc::clone(&running),
        ready_cvar: Arc::clone(&ready_cvar),
    };

    let state = Arc::new(HostState {
        writer: Mutex::new(writer),
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        log_lock: Mutex::new(()),
        runtime_dir: dir.to_path_buf(),
        ready: Mutex::new(false),
        ready_cvar,
        running: Arc::clone(&running),
    });

    let accept_state = Arc::clone(&state);
    let accept_running = Arc::clone(&running);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if !accept_running.load(Ordering::SeqCst) {
                break;
            }
            let st = Arc::clone(&accept_state);
            thread::spawn(move || handle_client(stream, st));
        }
    });

    loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(e) => {
                eprintln!("pc-eye browser-host okuma hatası: {}", e);
                if let Ok(mut map) = state.pending.lock() {
                    map.clear();
                }
                return Err(e);
            }
        };

        if frame.get("event").and_then(|v| v.as_str()) == Some("hello") {
            let info = json!({
                "pid": std::process::id(),
                "version": frame.get("version").cloned().unwrap_or(Value::Null),
                "ua": frame.get("ua").cloned().unwrap_or(Value::Null),
                "since_unix": now_unix_secs(),
                "session": active_name(),
            });
            let info_file = info_path_in(dir);
            if let Ok(serialized) = serde_json::to_string_pretty(&info) {
                let _ = fs::write(&info_file, serialized);
            }
            let cfg_msg = filter_cfg.to_config_message();
            if let Err(e) = state.send_to_extension(&cfg_msg) {
                eprintln!("pc-eye browser-host config gönderilemedi: {}", e);
            } else {
                state.mark_ready();
            }
            continue;
        }

        if let Some(id) = frame.get("id").and_then(|v| v.as_u64()) {
            let sender = state
                .pending
                .lock()
                .ok()
                .and_then(|mut map| map.remove(&id));
            if let Some(tx) = sender {
                let _ = tx.send(frame);
            }
        }
    }

    if let Ok(mut map) = state.pending.lock() {
        map.clear();
    }
    Ok(())
}

fn handle_client<W: Write + Send + 'static>(stream: UnixStream, state: Arc<HostState<W>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let Ok(mut client_writer) = stream.try_clone() else {
        return;
    };

    let mut line = String::new();
    let mut limited = BufReader::new(stream.take((MAX_CLIENT_LINE + 1) as u64));
    if let Err(e) = limited.read_line(&mut line) {
        let reply = json!({ "ok": false, "error": format!("istek okunamadı: {}", e) });
        let _ = writeln!(client_writer, "{}", reply);
        return;
    }
    if line.len() > MAX_CLIENT_LINE {
        let reply = json!({ "ok": false, "error": "istek 1 MiB sınırını aşıyor" });
        let _ = writeln!(client_writer, "{}", reply);
        return;
    }

    let req: Value = match serde_json::from_str(&line) {
        Ok(v) => v,
        Err(e) => {
            let reply = json!({ "ok": false, "error": format!("geçersiz JSON istek: {}", e) });
            let _ = writeln!(client_writer, "{}", reply);
            return;
        }
    };

    let Some(op) = req.get("op").and_then(|v| v.as_str()).map(str::to_string) else {
        let reply = json!({ "ok": false, "error": "istekte 'op' alanı eksik" });
        let _ = writeln!(client_writer, "{}", reply);
        return;
    };
    // Boş op veya iç yapılandırma iletisi olan config istemciden kabul edilmez.
    if op.trim().is_empty() {
        let reply = json!({ "ok": false, "error": "istekte 'op' alanı boş" });
        let _ = writeln!(client_writer, "{}", reply);
        return;
    }
    if op == "config" {
        let reply = json!({ "ok": false, "error": "'config' işlemi istemciden gönderilemez" });
        let _ = writeln!(client_writer, "{}", reply);
        return;
    }
    if !state.wait_ready(Duration::from_secs(3)) {
        let reply = json!({ "ok": false, "error": "eklenti henüz hazır değil" });
        let _ = writeln!(client_writer, "{}", reply);
        return;
    }
    let args = req.get("args").cloned().unwrap_or_else(|| json!({}));
    let timeout_ms = req
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(12_000);

    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    if let Ok(mut map) = state.pending.lock() {
        map.insert(id, tx);
    } else {
        let reply = json!({ "ok": false, "error": "iç durum kilidi hatası" });
        let _ = writeln!(client_writer, "{}", reply);
        return;
    }

    let started = Instant::now();
    let outbound = json!({
        "id": id,
        "op": op,
        "args": args,
    });

    if let Err(e) = state.send_to_extension(&outbound) {
        if let Ok(mut map) = state.pending.lock() {
            map.remove(&id);
        }
        let elapsed = started.elapsed().as_millis() as u64;
        state.append_action_log(&op, &args, false, elapsed);
        let reply = json!({ "ok": false, "error": e.to_string() });
        let _ = writeln!(client_writer, "{}", reply);
        return;
    }

    let reply = match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
        Ok(ext_msg) => {
            let ok = ext_msg.get("ok").and_then(|v| v.as_bool()) == Some(true);
            let elapsed = started.elapsed().as_millis() as u64;
            state.append_action_log(&op, &args, ok, elapsed);
            if ok {
                json!({
                    "ok": true,
                    "result": ext_msg.get("result").cloned().unwrap_or(Value::Null),
                })
            } else {
                let err_str = ext_msg
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tarayıcı eklentisi hatası");
                json!({ "ok": false, "error": err_str })
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if let Ok(mut map) = state.pending.lock() {
                map.remove(&id);
            }
            let elapsed = started.elapsed().as_millis() as u64;
            state.append_action_log(&op, &args, false, elapsed);
            json!({
                "ok": false,
                "error": format!("tarayıcı {} ms içinde yanıt vermedi (op: {})", timeout_ms, op),
            })
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            if let Ok(mut map) = state.pending.lock() {
                map.remove(&id);
            }
            let elapsed = started.elapsed().as_millis() as u64;
            state.append_action_log(&op, &args, false, elapsed);
            json!({
                "ok": false,
                "error": "tarayıcı eklentisi bağlantısı kapandı",
            })
        }
    };

    let _ = writeln!(client_writer, "{}", reply);
    let _ = client_writer.flush();
}

/// Köprüye tek istek. Soket yoksa/bağlanılamıyorsa anlaşılır hata: "tarayıcı köprüsü bağlı değil: `pc-eye browser launch` ile ajan Firefox'unu açın ya da eklentiyi yükleyin (`pc-eye browser install`)".
pub fn request(
    op: &str,
    args: serde_json::Value,
    timeout: std::time::Duration,
) -> anyhow::Result<serde_json::Value> {
    request_at(&socket_path(), op, args, timeout)
}

pub(crate) fn request_at(
    sock: &Path,
    op: &str,
    args: Value,
    timeout: Duration,
) -> Result<Value> {
    let mut stream = UnixStream::connect(sock).map_err(|_| {
        anyhow!(
            "tarayıcı köprüsü bağlı değil: `pc-eye browser launch` ile ajan Firefox'unu açın ya da eklentiyi yükleyin (`pc-eye browser install`)"
        )
    })?;
    let read_wait = timeout
        .checked_add(Duration::from_secs(2))
        .unwrap_or(timeout);
    stream
        .set_read_timeout(Some(read_wait))
        .context("soket okuma zaman aşımı ayarlanamadı")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .context("soket yazma zaman aşımı ayarlanamadı")?;

    let payload = json!({
        "op": op,
        "args": args,
        "timeout_ms": timeout.as_millis() as u64,
    });
    writeln!(stream, "{}", payload).context("istek köprü soketine yazılamadı")?;
    stream.flush().context("köprü soketi tamponu boşaltılamadı")?;

    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("köprüden yanıt okunamadı")?;
    if line.trim().is_empty() {
        bail!("tarayıcı köprüsü bağlantıyı yanıt vermeden kapattı");
    }

    let reply: Value =
        serde_json::from_str(&line).context("köprü yanıtı geçerli JSON değil")?;
    if reply.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
    }
    let err_msg = reply
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("bilinmeyen tarayıcı köprüsü hatası");
    Err(anyhow!("{}", err_msg))
}

/// URL içinden küçük harfli ana makine adını (`şema://kullanıcı@HOST:port/yol`) ayrıştırır.
pub(crate) fn extract_url_host(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let after_scheme = if let Some((scheme, rest)) = trimmed.split_once("://") {
        if scheme.is_empty()
            || !scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            return String::new();
        }
        if scheme.eq_ignore_ascii_case("file") && rest.starts_with('/') {
            return String::new();
        }
        rest
    } else {
        let lower = trimmed.to_ascii_lowercase();
        if [
            "about:",
            "data:",
            "file:",
            "view-source:",
            "mailto:",
            "moz-extension:",
            "javascript:",
        ]
        .iter()
        .any(|p| lower.starts_with(p))
        {
            return String::new();
        }
        trimmed
    };

    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    let host = if host_port.starts_with('[') {
        if let Some(end) = host_port.find(']') {
            &host_port[..=end]
        } else {
            host_port
        }
    } else {
        host_port.split(':').next().unwrap_or(host_port)
    };
    host.trim().to_lowercase()
}

fn check_navigate_allowed_with_cfg(url: &str, cfg: &FilterConfig) -> Result<()> {
    let trimmed = url.trim();
    if trimmed
        .get(..5)
        .map_or(false, |s| s.eq_ignore_ascii_case("file:"))
    {
        bail!("file:// sayfaları köprüyle açılamaz ve okunamaz; dosyayı http üzerinden sunun (ör. `python3 -m http.server` ile dizini sunup http://127.0.0.1:<port>/... açın)");
    }
    let host = extract_url_host(url);
    if !host.is_empty() && cfg.is_host_denied(&host) {
        bail!("site yasak listesinde: {}", host);
    }
    Ok(())
}

fn check_navigate_allowed(url: &str) -> Result<()> {
    let cfg = config_file_path()
        .map(|p| FilterConfig::from_path(&p))
        .unwrap_or_default();
    check_navigate_allowed_with_cfg(url, &cfg)
}

/// CLI ve MCP'nin tek giriş noktası. Yerel op'lar: "install", "pack", "launch", "attach", "status", "stop"; diğer her op köprüye iletilir (zaman aşımı: wait→timeout_ms+3 sn, navigate/back/forward/reload→25 sn, diğer→12 sn).
pub fn command(op: &str, args: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
    if op.trim().is_empty() {
        bail!("'op' alanı boş olamaz");
    }
    if op == "config" {
        bail!("'config' işlemi dışarıdan çağrılamaz");
    }
    match op {
        "install" => cmd_install(),
        "pack" => cmd_pack(),
        "launch" => cmd_launch(args),
        "attach" => cmd_attach(args),
        "status" => cmd_status(),
        "stop" => cmd_stop(),
        "run" => cmd_run(args),
        other => {
            // `name` verilip `ref` verilmediyse eleman adla bulunur: ajan snapshot + eylem için
            // iki tur harcamaz, ölçüm probe'ları da sabit argümanla çalışabilir.
            let resolved;
            let args = if args.get("ref").map_or(true, |v| v.is_null())
                && args.get("name").and_then(|v| v.as_str()).is_some()
                && REF_OPS.contains(&other)
            {
                resolved = resolve_name_arg(other, args)?;
                &resolved
            } else {
                args
            };
            if other == "click" && args.get("real").and_then(|v| v.as_bool()) == Some(true) {
                return cmd_real_click(args);
            }
            if other == "navigate" {
                if let Some(url) = args.get("url").and_then(|v| v.as_str()) {
                    check_navigate_allowed(url)?;
                }
            }
            let timeout = match other {
                "wait" => {
                    let base_ms = args
                        .get("timeout_ms")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(5_000);
                    Duration::from_millis(base_ms.saturating_add(3_000))
                }
                "navigate" | "back" | "forward" | "reload" => Duration::from_secs(25),
                _ => Duration::from_secs(12),
            };
            request(other, args.clone(), timeout)
        }
    }
}

/// Çok adımlı program: `steps` dizisindeki her adım (`{"op":"type","name":"Ad","text":".."}`) sırayla
/// yürütülür, ilk hatada durulur; sonunda isteğe bağlı `wait` ve sayfanın son özeti döner. Ajan formu
/// adım adım ayrı çağrılarla doldurunca tur ve token harcıyordu (masaüstündeki `pc_eye_run` karşılığı).
fn cmd_run(args: &Value) -> Result<Value> {
    let steps = args
        .get("steps")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("run: `steps` dizisi gerekli"))?;
    let tab = args.get("tab").filter(|v| !v.is_null()).cloned();
    let mode = args.get("after").and_then(|v| v.as_str()).unwrap_or("diff");
    if !matches!(mode, "diff" | "full" | "none") {
        bail!("run: `after` diff|full|none olmalı");
    }
    // Fark modu için adımlardan önceki özet (DOM'dan ~20 ms).
    let before = if mode == "diff" { Some(run_after_snapshot(tab.as_ref())) } else { None };
    let mut done = Vec::new();
    let mut failed: Option<usize> = None;
    for (i, step) in steps.iter().enumerate() {
        let op = step.get("op").and_then(|v| v.as_str()).unwrap_or_default();
        if matches!(op, "run" | "launch" | "stop" | "install" | "attach" | "pack" | "") {
            bail!("run: {}. adımda geçersiz op `{}`", i + 1, op);
        }
        let mut a = step.clone();
        if let (Some(obj), Some(t)) = (a.as_object_mut(), tab.as_ref()) {
            obj.entry("tab").or_insert_with(|| t.clone());
        }
        match command(op, &a) {
            Ok(r) => done.push(json!({ "op": op, "ok": true, "result": compact_step_result(&r) })),
            Err(e) => {
                done.push(json!({ "op": op, "ok": false, "error": e.to_string() }));
                failed = Some(i);
                break;
            }
        }
    }
    let mut out = match failed {
        Some(i) => json!({ "ok": false, "completed": i, "failed_step": i + 1, "steps": done }),
        None => json!({ "ok": true, "completed": steps.len(), "steps": done }),
    };
    // Hata olunca sayfanın tamamı gerekir (ajan neyin yanlış gittiğini görmeli).
    let mode = if failed.is_some() && mode == "none" { "full" } else { mode };
    if mode != "none" {
        let after = run_after_snapshot(tab.as_ref());
        out["after"] = match before {
            Some(b) if failed.is_none() => diff_snapshot(&b, after),
            _ => after,
        };
    }
    Ok(out)
}

/// Aynı sayfada kalındıysa yalnız yeni/değişen satırlar (`changed_lines`) ve kaybolan satır sayısı
/// döner: form dolduran bir `run` her seferinde 60 satırlık özeti tekrar göndermesin. Sayfa
/// değiştiyse (URL) ya da fark özetin yarısından büyükse tam özet döner.
fn diff_snapshot(before: &Value, after: Value) -> Value {
    let lines = |v: &Value| -> Vec<String> {
        v.get("lines")
            .and_then(|l| l.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    let (b, a) = (lines(before), lines(&after));
    if a.is_empty() || before.get("url") != after.get("url") {
        return after;
    }
    let bset: std::collections::HashSet<&String> = b.iter().collect();
    let aset: std::collections::HashSet<&String> = a.iter().collect();
    let added: Vec<&String> = a.iter().filter(|l| !bset.contains(l)).collect();
    let removed = b.iter().filter(|l| !aset.contains(l)).count();
    if added.len() * 2 > a.len() {
        return after;
    }
    let mut out = after.clone();
    if let Some(o) = out.as_object_mut() {
        o.remove("lines");
        o.insert("changed_lines".to_string(), json!(added));
        o.insert("removed_lines".to_string(), json!(removed));
        o.insert("unchanged_lines".to_string(), json!(a.len() - added.len()));
    }
    out
}

/// Adım sonucunun ajana gereken kısmı (tam özet yalnız en sonda bir kez döner).
fn compact_step_result(r: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for k in ["changed", "navigated", "value", "matched", "url", "real"] {
        if let Some(v) = r.get(k) {
            out.insert(k.to_string(), v.clone());
        }
    }
    Value::Object(out)
}

fn run_after_snapshot(tab: Option<&Value>) -> Value {
    let mut a = json!({ "max": 60 });
    if let Some(t) = tab {
        a["tab"] = t.clone();
    }
    request("snapshot", a, Duration::from_secs(12)).unwrap_or(Value::Null)
}

/// Ref ile çalışan sayfa op'ları (`name` ile de çağrılabilir).
const REF_OPS: &[&str] = &["click", "type", "select", "check", "hover", "focus", "rect", "scroll", "text"];

fn resolve_name_arg(op: &str, args: &Value) -> Result<Value> {
    let name = args.get("name").and_then(|v| v.as_str()).unwrap_or_default();
    let mut snap_args = json!({ "query": name, "max": 60 });
    if let Some(tab) = args.get("tab").filter(|v| !v.is_null()) {
        snap_args["tab"] = tab.clone();
    }
    let snap = request("snapshot", snap_args, Duration::from_secs(12))?;
    let lines: Vec<&str> = snap
        .get("lines")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|l| l.as_str()).collect())
        .unwrap_or_default();
    let r = pick_ref_by_name(&lines, name, op)
        .ok_or_else(|| anyhow!("`{}` adlı eleman bulunamadı (snapshot ile adları görün)", name))?;
    let mut out = args.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.remove("name");
        obj.insert("ref".to_string(), json!(r));
    }
    Ok(out)
}

/// Türkçe büyük/küçük harf katlaması: `İ` → `i` + U+0307 olur, `I` → `ı` olmalıydı.
fn fold_name(s: &str) -> String {
    s.to_lowercase().replace('\u{307}', "").replace('ı', "i").trim().to_string()
}

/// Özet satırlarından (`[e12] button "Gönder"`) ada en uygun ref'i seçer: tam ad > içerme,
/// aynı derecede op'a uygun rol (type → metin kutusu, select → açılır liste, check → onay kutusu).
pub(crate) fn pick_ref_by_name(lines: &[&str], name: &str, op: &str) -> Option<String> {
    let want = fold_name(name);
    if want.is_empty() {
        return None;
    }
    let fits = |role: &str| -> bool {
        match op {
            "type" => matches!(role, "textbox" | "searchbox" | "combobox" | "textarea" | "spinbutton"),
            "select" => matches!(role, "combobox" | "listbox"),
            "check" => matches!(role, "checkbox" | "radio" | "switch"),
            _ => true,
        }
    };
    let mut best: Option<((u8, u8, usize), String)> = None;
    for (i, line) in lines.iter().enumerate() {
        let Some(rest) = line.strip_prefix("[e") else { continue };
        let Some((num, rest)) = rest.split_once(']') else { continue };
        let rest = rest.trim_start();
        let (role, rest) = rest.split_once(' ').unwrap_or((rest, ""));
        let label = rest
            .strip_prefix('"')
            .and_then(|r| r.split_once('"'))
            .map(|(l, _)| l)
            .unwrap_or("");
        let l = fold_name(label);
        let name_rank = if l == want {
            0
        } else if l.contains(&want) {
            1
        } else {
            continue;
        };
        let role_rank = if fits(role) { 0 } else { 1 };
        let key = (role_rank, name_rank, i);
        if best.as_ref().map_or(true, |(k, _)| key < *k) {
            best = Some((key, format!("e{}", num)));
        }
    }
    best.map(|(_, r)| r)
}

fn cmd_install() -> Result<Value> {
    let ext_dir = extension_install_dir()?;
    fs::create_dir_all(&ext_dir).context("eklenti dizini oluşturulamadı")?;
    fs::write(ext_dir.join("manifest.json"), EXT_MANIFEST_JSON)
        .context("manifest.json yazılamadı")?;
    fs::write(ext_dir.join("background.js"), EXT_BACKGROUND_JS)
        .context("background.js yazılamadı")?;
    fs::write(ext_dir.join("content.js"), EXT_CONTENT_JS)
        .context("content.js yazılamadı")?;

    let exe = std::env::current_exe().context("çalışan ikili yolu alınamadı")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let wrapper = host_wrapper_path()?;
    if let Some(parent) = wrapper.parent() {
        fs::create_dir_all(parent).context("sarmalayıcı üst dizini oluşturulamadı")?;
    }
    let wrapper_script = format!("#!/bin/sh\nexec \"{}\" browser-host \"$@\"\n", exe.display());
    fs::write(&wrapper, wrapper_script).context("browser-host.sh yazılamadı")?;
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))
        .context("browser-host.sh çalıştırma izni verilemedi")?;

    let host_manifest = json!({
        "name": NATIVE_APP_NAME,
        "description": "Pc-Eye browser bridge",
        "path": wrapper.to_string_lossy(),
        "type": "stdio",
        "allowed_extensions": [EXTENSION_ID],
    });
    let manifest_pretty = serde_json::to_string_pretty(&host_manifest)?;
    let paths = native_manifest_paths()?;
    let mut written_paths = Vec::with_capacity(paths.len());
    for path in &paths {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("{} oluşturulamadı", parent.display()))?;
        }
        fs::write(path, &manifest_pretty)
            .with_context(|| format!("{} yazılamadı", path.display()))?;
        written_paths.push(path.to_string_lossy().into_owned());
    }

    Ok(json!({
        "extension_dir": ext_dir.to_string_lossy(),
        "host_manifest": written_paths,
        "note": format!(
            "Günlük Firefox'a yüklemek için: about:debugging → Bu Firefox → Geçici Eklenti Yükle → {}/manifest.json",
            ext_dir.display()
        ),
    }))
}

/// Ajan Firefox profili için ilk çalışma kapılarını kapatan ve RDP'yi açan user.js içeriği.
pub(crate) fn firefox_user_js(port: u16) -> String {
    format!(
        concat!(
            "user_pref(\"devtools.debugger.remote-enabled\", true);\n",
            "user_pref(\"devtools.chrome.enabled\", true);\n",
            "user_pref(\"devtools.debugger.prompt-connection\", false);\n",
            "user_pref(\"devtools.debugger.remote-port\", {});\n",
            "user_pref(\"browser.shell.checkDefaultBrowser\", false);\n",
            "user_pref(\"browser.aboutwelcome.enabled\", false);\n",
            "user_pref(\"datareporting.policy.dataSubmissionEnabled\", false);\n",
            "user_pref(\"toolkit.telemetry.reportingpolicy.firstRun\", false);\n",
            "user_pref(\"browser.startup.homepage_override.mstone\", \"ignore\");\n",
            "user_pref(\"startup.homepage_welcome_url\", \"\");\n",
            "user_pref(\"browser.tabs.warnOnClose\", false);\n",
            "user_pref(\"browser.sessionstore.resume_from_crash\", false);\n",
            "user_pref(\"app.update.auto\", false);\n",
            "user_pref(\"extensions.autoDisableScopes\", 0);\n",
            "user_pref(\"accessibility.force_disabled\", 0);\n",
            "user_pref(\"media.autoplay.default\", 0);\n",
            "user_pref(\"media.autoplay.blocking_policy\", 0);\n",
            "user_pref(\"media.autoplay.block-webaudio\", false);\n",
            "user_pref(\"dom.disable_open_during_load\", false);\n",
            "user_pref(\"browser.translations.automaticallyPopup\", false);\n",
            "user_pref(\"signon.rememberSignons\", false);\n",
            "user_pref(\"browser.contentblocking.category\", \"standard\");\n",
            "user_pref(\"extensions.pocket.enabled\", false);\n",
            "user_pref(\"browser.newtabpage.enabled\", false);\n",
            "user_pref(\"browser.startup.page\", 0);\n",
        ),
        port
    )
}

fn allocate_free_port() -> Result<u16> {
    let listener =
        TcpListener::bind("127.0.0.1:0").context("boş yerel TCP portu bulunamadı")?;
    let port = listener.local_addr()?.port();
    Ok(port)
}

fn wait_for_tcp_port(port: u16, timeout: Duration) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
            let _ = stream.shutdown(Shutdown::Both);
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HostInfoStamp {
    pid: u64,
    since_unix: u64,
}

fn read_host_info_stamp(dir: &Path) -> Option<HostInfoStamp> {
    let raw = fs::read_to_string(info_path_in(dir)).ok()?;
    let val: Value = serde_json::from_str(&raw).ok()?;
    Some(HostInfoStamp {
        pid: val.get("pid")?.as_u64()?,
        since_unix: val.get("since_unix")?.as_u64()?,
    })
}

fn is_pid_alive(pid: u32) -> bool {
    let raw_pid = pid as libc::pid_t;
    let res = unsafe { libc::kill(raw_pid, 0) };
    if res != 0 {
        let err = std::io::Error::last_os_error().raw_os_error();
        return err == Some(libc::EPERM);
    }
    if let Ok(stat) = fs::read_to_string(format!("/proc/{}/stat", pid)) {
        if let Some((_, after_comm)) = stat.rsplit_once(')') {
            if after_comm.trim_start().starts_with('Z') {
                return false;
            }
        }
    }
    true
}

// Önce SIGTERM gönderir, süreç 2 sn içinde çıkmazsa SIGKILL ile sonlandırır.
fn stop_pid_gracefully(pid: u32, mut is_alive: impl FnMut() -> bool) {
    let raw_pid = pid as libc::pid_t;
    if raw_pid <= 0 {
        return;
    }
    if unsafe { libc::kill(raw_pid, libc::SIGTERM) } != 0 {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !is_alive() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    unsafe {
        libc::kill(raw_pid, libc::SIGKILL);
    }
    let _ = is_alive();
}

/// launch hata yollarında başlatılan Firefox sürecinin sahipsiz kalmaması için koruma.
struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn disarm(&mut self) -> Option<Child> {
        self.child.take()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            stop_pid_gracefully(pid, || matches!(child.try_wait(), Ok(None)));
            let _ = child.wait();
        }
    }
}

fn cmd_launch(args: &Value) -> Result<Value> {
    let launch_url = args
        .get("url")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(ref url) = launch_url {
        // '-' ile başlayan değerlerin firefox bayrağı olarak yorumlanmasını önler.
        if url.starts_with('-') {
            bail!("geçersiz URL ('-' ile başlayamaz): {}", url);
        }
        check_navigate_allowed(url)?;
    }

    cmd_install()?;

    let rdir = runtime_dir();
    let prev_info = read_host_info_stamp(&rdir);

    let profile_dir = args
        .get("profile")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| rdir.join("firefox-profile"));
    fs::create_dir_all(&profile_dir).context("Firefox profil dizini oluşturulamadı")?;

    let port = match args.get("port").and_then(|v| v.as_u64()) {
        Some(p) if (1..=65535).contains(&p) => p as u16,
        Some(p) => bail!("geçersiz port numarası: {}", p),
        None => allocate_free_port()?,
    };

    let user_js = firefox_user_js(port);
    fs::write(profile_dir.join("user.js"), user_js).context("user.js yazılamadı")?;

    // Eklenti kurulmadan açılan ilk sayfaya içerik betiği girmediği için boş sayfa ile başlanır.
    let mut cmd = Command::new("firefox");
    cmd.arg("--new-instance")
        .arg("--profile")
        .arg(&profile_dir)
        .arg("--start-debugger-server")
        .arg(port.to_string())
        .arg("about:blank");

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);

    let child = cmd.spawn().context("firefox süreci başlatılamadı")?;
    let pid = child.id();
    let mut guard = ChildGuard { child: Some(child) };

    if !wait_for_tcp_port(port, Duration::from_secs(15)) {
        bail!(
            "Firefox hata ayıklama sunucusu 15 sn içinde {} portunu açmadı",
            port
        );
    }

    let attach_args = json!({ "port": port });
    let mut last_attach_err = None;
    for _ in 0..10 {
        match cmd_attach(&attach_args) {
            Ok(_) => {
                last_attach_err = None;
                break;
            }
            Err(e) => {
                last_attach_err = Some(e);
                thread::sleep(Duration::from_millis(250));
            }
        }
    }
    if let Some(err) = last_attach_err {
        return Err(err).context("Firefox RDP üzerinden eklenti yüklenemedi");
    }

    let ping_deadline = Instant::now() + Duration::from_secs(10);
    let mut ping_res: Option<Value> = None;
    while Instant::now() < ping_deadline {
        // Ping'in eski bir köprüden değil yeni başlatılan Firefox'tan geldiği doğrulanır.
        let is_new_bridge = match (&prev_info, read_host_info_stamp(&rdir)) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(prev), Some(curr)) => &curr != prev && curr.since_unix >= prev.since_unix,
        };
        if is_new_bridge {
            if let Ok(res) = request("ping", json!({}), Duration::from_secs(2)) {
                ping_res = Some(res);
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }

    let Some(ping_val) = ping_res else {
        bail!("Firefox köprüsü 10 sn içinde bağlanmadı");
    };

    // Kurulum tamamlandı; arka planda tarayıcı kapanınca zombi kalmaması için beklenir.
    if let Some(mut child) = guard.disarm() {
        thread::spawn(move || {
            let _ = child.wait();
        });
    }

    let launch_record = json!({
        "pid": pid,
        "port": port,
        "profile": profile_dir.to_string_lossy(),
    });
    if let Ok(serialized) = serde_json::to_string_pretty(&launch_record) {
        let _ = fs::write(launch_info_path_in(&rdir), serialized);
    }

    let mut out = json!({
        "pid": pid,
        "port": port,
        "profile": profile_dir.to_string_lossy(),
        "bridge": true,
        "ping": ping_val,
    });
    if let Some(url) = launch_url {
        let nav_res = command("navigate", &json!({ "url": url }))?;
        out["navigated"] = nav_res;
    }
    Ok(out)
}

fn read_rdp_packet<R: Read>(reader: &mut R) -> Result<Value> {
    let mut len_buf = Vec::with_capacity(12);
    loop {
        let mut byte = [0u8; 1];
        reader
            .read_exact(&mut byte)
            .context("RDP paket uzunluğu okunamadı")?;
        if byte[0] == b':' {
            break;
        }
        if !byte[0].is_ascii_digit() || len_buf.len() >= 16 {
            bail!("geçersiz RDP paket başlığı");
        }
        len_buf.push(byte[0]);
    }
    let len_str = std::str::from_utf8(&len_buf).context("RDP uzunluk başlığı UTF-8 değil")?;
    let len: usize = len_str.parse().context("RDP paket uzunluğu sayı değil")?;
    if len == 0 || len > 16 * 1024 * 1024 {
        bail!("geçersiz RDP paket boyutu: {}", len);
    }
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .context("RDP paket gövdesi okunamadı")?;
    let val: Value =
        serde_json::from_slice(&payload).context("RDP paketi geçerli JSON değil")?;
    Ok(val)
}

fn write_rdp_packet<W: Write>(writer: &mut W, msg: &Value) -> Result<()> {
    let payload = serde_json::to_vec(msg)?;
    write!(writer, "{}:", payload.len()).context("RDP uzunluk başlığı yazılamadı")?;
    writer
        .write_all(&payload)
        .context("RDP paket gövdesi yazılamadı")?;
    writer.flush().context("RDP soketi flush edilemedi")?;
    Ok(())
}

fn cmd_attach(args: &Value) -> Result<Value> {
    let port = args
        .get("port")
        .and_then(|v| v.as_u64())
        .filter(|p| (1..=65535).contains(p))
        .ok_or_else(|| anyhow!("attach için geçerli bir 'port' argümanı gerekli"))?
        as u16;

    let addon_dir = match args.get("addon").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => PathBuf::from(s),
        _ => {
            let dir = extension_install_dir()?;
            if !dir.join("manifest.json").is_file() {
                cmd_install()?;
            }
            dir
        }
    };
    let addon_abs = addon_dir
        .canonicalize()
        .with_context(|| format!("eklenti dizini bulunamadı: {}", addon_dir.display()))?;

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .with_context(|| format!("Firefox RDP portuna ({}) bağlanılamadı", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    // Sunucu bağlanınca root aktöründen bir selamlama paketi gönderir.
    let _greeting = read_rdp_packet(&mut stream)?;

    write_rdp_packet(&mut stream, &json!({ "to": "root", "type": "getRoot" }))?;

    let addons_actor = loop {
        let pkt = read_rdp_packet(&mut stream)?;
        if pkt.get("from").and_then(|v| v.as_str()) == Some("root") {
            if let Some(err) = pkt.get("error").and_then(|v| v.as_str()) {
                let msg = pkt
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                bail!("Firefox getRoot hatası: {} {}", err, msg);
            }
            if let Some(actor) = pkt.get("addonsActor").and_then(|v| v.as_str()) {
                break actor.to_string();
            }
        }
    };

    write_rdp_packet(
        &mut stream,
        &json!({
            "to": addons_actor,
            "type": "installTemporaryAddon",
            "addonPath": addon_abs.to_string_lossy(),
            "openDevTools": false,
        }),
    )?;

    loop {
        let pkt = read_rdp_packet(&mut stream)?;
        if pkt.get("from").and_then(|v| v.as_str()) != Some(addons_actor.as_str()) {
            continue;
        }
        if let Some(err) = pkt.get("error").and_then(|v| v.as_str()) {
            let msg = pkt
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            bail!("Firefox geçici eklenti kurulum hatası: {} ({})", err, msg);
        }
        let addon_val = pkt
            .get("addon")
            .cloned()
            .unwrap_or_else(|| json!({ "id": EXTENSION_ID }));
        return Ok(json!({
            "installed": true,
            "addon": addon_val,
        }));
    }
}

// Yalnız launch kaydındaki Firefox sürecini /proc/<pid>/comm doğrulamasıyla sonlandırır.
fn cmd_stop() -> Result<Value> {
    let dir = runtime_dir();
    let path = launch_info_path_in(&dir);
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({
                "stopped": false,
                "pid": Value::Null,
                "reason": "kayıtlı ajan Firefox oturumu yok",
            }));
        }
        Err(e) => return Err(e).context("browser_launch.json okunamadı"),
    };

    let _ = fs::remove_file(&path);
    let val: Value = serde_json::from_str(&raw).context("browser_launch.json geçerli JSON değil")?;
    let pid = val
        .get("pid")
        .and_then(|v| v.as_u64())
        .filter(|&p| p > 0 && p <= i32::MAX as u64)
        .ok_or_else(|| anyhow!("browser_launch.json içinde geçerli pid yok"))? as u32;

    let Ok(comm) = fs::read_to_string(format!("/proc/{}/comm", pid)) else {
        return Ok(json!({
            "stopped": false,
            "pid": pid,
            "reason": "süreç artık çalışmıyor",
        }));
    };
    if !comm.trim().to_lowercase().contains("firefox") {
        return Ok(json!({
            "stopped": false,
            "pid": pid,
            "reason": "PID artık bir firefox sürecine ait değil",
        }));
    }

    stop_pid_gracefully(pid, || is_pid_alive(pid));
    Ok(json!({
        "stopped": true,
        "pid": pid,
    }))
}

fn cmd_status() -> Result<Value> {
    let dir = runtime_dir();
    let sock = dir.join("browser.sock");
    let connected = request_at(&sock, "ping", json!({}), Duration::from_secs(2)).is_ok();
    let info = fs::read_to_string(info_path_in(&dir))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(Value::Null);
    let launch = fs::read_to_string(launch_info_path_in(&dir))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(Value::Null);
    let ext_installed = extension_install_dir()
        .map(|d| d.join("manifest.json").is_file())
        .unwrap_or(false);
    let manifests: Vec<String> = native_manifest_paths()
        .map(|arr| {
            arr.into_iter()
                .filter(|p| p.is_file())
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();

    Ok(json!({
        "socket": sock.to_string_lossy(),
        "connected": connected,
        "info": info,
        "launch": launch,
        "extension_installed": ext_installed,
        "host_manifests": manifests,
    }))
}

pub(crate) fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

pub(crate) fn build_stored_zip(entries: &[(&str, &[u8])]) -> Result<Vec<u8>> {
    if entries.len() > u16::MAX as usize {
        bail!("ZIP girdi sayısı 65535 sınırını aşıyor");
    }
    let mut out = Vec::new();
    let mut central = Vec::with_capacity(entries.len());

    for &(name, data) in entries {
        let name_bytes = name.as_bytes();
        let name_len = u16::try_from(name_bytes.len())
            .map_err(|_| anyhow!("ZIP dosya adı çok uzun: {}", name))?;
        let data_len = u32::try_from(data.len())
            .map_err(|_| anyhow!("ZIP dosya boyutu 4 GiB sınırını aşıyor: {}", name))?;
        let local_offset = u32::try_from(out.len())
            .map_err(|_| anyhow!("ZIP arşivi 4 GiB sınırını aşıyor"))?;
        let crc = crc32_ieee(data);

        // Yerel dosya başlığı (method 0, zaman alanları 0).
        out.extend_from_slice(&0x0403_4B50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&data_len.to_le_bytes());
        out.extend_from_slice(&data_len.to_le_bytes());
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(data);

        central.push((name_bytes, name_len, data_len, crc, local_offset));
    }

    let cd_offset = u32::try_from(out.len())
        .map_err(|_| anyhow!("ZIP arşivi 4 GiB sınırını aşıyor"))?;
    for (name_bytes, name_len, data_len, crc, local_offset) in central {
        out.extend_from_slice(&0x0201_4B50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&data_len.to_le_bytes());
        out.extend_from_slice(&data_len.to_le_bytes());
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&local_offset.to_le_bytes());
        out.extend_from_slice(name_bytes);
    }

    let cd_size = u32::try_from(out.len() - cd_offset as usize)
        .map_err(|_| anyhow!("ZIP merkezi dizini 4 GiB sınırını aşıyor"))?;
    let entry_count = entries.len() as u16;
    out.extend_from_slice(&0x0605_4B50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&entry_count.to_le_bytes());
    out.extend_from_slice(&entry_count.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    Ok(out)
}

fn cmd_pack() -> Result<Value> {
    let path = xpi_output_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("XPI üst dizini oluşturulamadı")?;
    }
    let bytes = build_stored_zip(&[
        ("manifest.json", EXT_MANIFEST_JSON.as_bytes()),
        ("background.js", EXT_BACKGROUND_JS.as_bytes()),
        ("content.js", EXT_CONTENT_JS.as_bytes()),
    ])?;
    fs::write(&path, &bytes).context("pc-eye-bridge.xpi yazılamadı")?;

    Ok(json!({
        "xpi": path.to_string_lossy(),
        "bytes": bytes.len(),
        "sign": "İmzalamak için AMO API anahtarlarınızla: npx web-ext sign --channel unlisted --source-dir ~/.local/share/pc-eye/extension --api-key $AMO_JWT_ISSUER --api-secret $AMO_JWT_SECRET",
    }))
}

pub(crate) fn screen_point(win_xy: (i64, i64), rect: &Value) -> Option<(i64, i64)> {
    let num = |k: &str| rect.get(k)?.as_f64().filter(|v| v.is_finite());
    let cx = num("cx")?;
    let cy = num("cy")?;
    let chrome_x = num("chrome_x")?;
    let chrome_y = num("chrome_y")?;
    let x = (win_xy.0 as f64 + chrome_x + cx).round() as i64;
    let y = (win_xy.1 as f64 + chrome_y + cy).round() as i64;
    Some((x, y))
}

fn parse_ppid_from_stat(stat: &str) -> Option<u32> {
    // comm alanında boşluk/parantez olabileceği için son ')' sonrası alanlar okunur.
    let (_, after_comm) = stat.rsplit_once(')')?;
    let ppid: u32 = after_comm.split_whitespace().nth(1)?.parse().ok()?;
    (ppid > 0).then_some(ppid)
}

fn collect_ancestor_pids(start_pid: u32, max_steps: usize) -> Vec<u32> {
    let mut pids = Vec::with_capacity(max_steps + 1);
    let mut cur = start_pid;
    for _ in 0..=max_steps {
        if cur == 0 {
            break;
        }
        pids.push(cur);
        let Ok(stat) = fs::read_to_string(format!("/proc/{}/stat", cur)) else {
            break;
        };
        let Some(ppid) = parse_ppid_from_stat(&stat) else {
            break;
        };
        if ppid == cur || ppid <= 1 {
            break;
        }
        cur = ppid;
    }
    pids
}

fn pick_window_for_chain<'a>(
    ancestor_pids: &[u32],
    windows: &'a [crate::protocol::types::WindowInfo],
) -> Option<&'a crate::protocol::types::WindowInfo> {
    for &pid in ancestor_pids {
        let best = windows
            .iter()
            .filter(|w| w.mapped && !w.hidden && w.pid > 0 && w.pid as u32 == pid)
            .max_by_key(|w| (w.focused, w.width.max(0).saturating_mul(w.height.max(0))));
        if best.is_some() {
            return best;
        }
    }
    None
}

fn find_agent_firefox_window() -> Result<crate::protocol::types::WindowInfo> {
    let rdir = runtime_dir();
    let host_pid = fs::read_to_string(info_path_in(&rdir))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("pid").and_then(|p| p.as_u64()))
        .filter(|&p| p > 0 && p <= u32::MAX as u64)
        .map(|p| p as u32)
        .ok_or_else(|| anyhow!("ajan Firefox penceresi bulunamadı"))?;

    let ancestors = collect_ancestor_pids(host_pid, 8);
    let desk = crate::eyes::compositor::capture_desktop_state()?;
    pick_window_for_chain(&ancestors, &desk.windows)
        .cloned()
        .ok_or_else(|| anyhow!("ajan Firefox penceresi bulunamadı"))
}

fn cmd_real_click(args: &Value) -> Result<Value> {
    let tab = args.get("tab").filter(|v| !v.is_null());
    if let Some(tab_val) = tab {
        request("activate", json!({ "tab": tab_val }), Duration::from_secs(12))?;
    }

    let mut rect_args = json!({
        "ref": args.get("ref").cloned().unwrap_or(Value::Null),
    });
    if let Some(tab_val) = tab {
        rect_args["tab"] = tab_val.clone();
    }
    let rect = request("rect", rect_args, Duration::from_secs(12))?;

    let win = find_agent_firefox_window()?;
    let (x, y) = screen_point((win.x, win.y), &rect)
        .ok_or_else(|| anyhow!("rect yanıtında geçerli koordinat yok: {}", rect))?;

    let receipt = crate::director::input::click_at(Some(x), Some(y), "left", 1, false)?;
    let receipt_val = serde_json::to_value(&receipt).context("ActionReceipt JSON kodlanamadı")?;

    let tabs_val = request("tabs", json!({}), Duration::from_secs(12)).ok();
    let active_tab = tabs_val.as_ref().and_then(|v| v.as_array()).and_then(|arr| {
        tab.and_then(|tv| {
            arr.iter().find(|t| {
                t.get("active").and_then(|a| a.as_bool()) == Some(true)
                    && t.get("id") == Some(tv)
            })
        })
        .or_else(|| {
            arr.iter()
                .find(|t| t.get("active").and_then(|a| a.as_bool()) == Some(true))
        })
        .or_else(|| arr.first())
    });
    let url = active_tab
        .and_then(|t| t.get("url"))
        .cloned()
        .unwrap_or(Value::Null);
    let title = active_tab
        .and_then(|t| t.get("title"))
        .cloned()
        .unwrap_or(Value::Null);

    Ok(json!({
        "ok": true,
        "real": true,
        "x": x,
        "y": y,
        "window": win.address,
        "receipt": receipt_val,
        "url": url,
        "title": title,
        "dpr": rect.get("dpr").cloned().unwrap_or(Value::Null),
        "zoom": rect.get("zoom").cloned().unwrap_or(Value::Null),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_diff_returns_only_changed_lines_on_same_page() {
        let before = json!({"url": "http://x/", "lines": ["[e1] textbox \"Ad\"", "[e2] button \"Gönder\"", "[e3] text \"a\"", "[e4] text \"b\""]});
        let after = json!({"url": "http://x/", "lines": ["[e1] textbox \"Ad\" = \"Ali\"", "[e2] button \"Gönder\"", "[e3] text \"a\"", "[e4] text \"b\""]});
        let d = diff_snapshot(&before, after);
        assert!(d.get("lines").is_none());
        assert_eq!(d["changed_lines"], json!(["[e1] textbox \"Ad\" = \"Ali\""]));
        assert_eq!(d["removed_lines"], json!(1));
        assert_eq!(d["unchanged_lines"], json!(3));
        // Sayfa değişince tam özet.
        let nav = json!({"url": "http://x/ok", "lines": ["[e9] text \"Kaydedildi\""]});
        assert!(diff_snapshot(&before, nav).get("lines").is_some());
    }

    #[test]
    fn name_resolves_to_ref_by_role_and_turkish_case() {
        let lines = [
            "heading(1) \"Sipariş formu\"",
            "[e1] textbox \"Ad Soyad\"",
            "[e3] combobox \"Şehir\" = \"Ankara\" {Ankara|İzmir}",
            "[e4] checkbox \"Bültene abone ol\"",
            "[e5] button \"Gönder\"",
            "[e6] link \"Gönderim koşulları\" -> /kosul",
            "[e7] label \"İptal\"",
            "[e8] button \"İptal et\"",
        ];
        assert_eq!(pick_ref_by_name(&lines, "ad soyad", "type").as_deref(), Some("e1"));
        assert_eq!(pick_ref_by_name(&lines, "Şehir", "select").as_deref(), Some("e3"));
        assert_eq!(pick_ref_by_name(&lines, "bültene", "check").as_deref(), Some("e4"));
        // Tam ad içermeden önce gelir.
        assert_eq!(pick_ref_by_name(&lines, "Gönder", "click").as_deref(), Some("e5"));
        assert_eq!(pick_ref_by_name(&lines, "iptal", "click").as_deref(), Some("e7"));
        assert!(pick_ref_by_name(&lines, "Yok", "click").is_none());
        assert!(pick_ref_by_name(&lines, "Sipariş", "click").is_none());
    }
    use std::io::Cursor;

    #[test]
    fn frame_encode_decode_roundtrip() {
        let original = json!({
            "id": 77,
            "op": "snapshot",
            "args": {
                "max": 150,
                "query": "Gönder düğmesi",
                "viewport": true
            }
        });
        let mut buf = Vec::new();
        write_frame(&mut buf, &original).expect("çerçeve yazılmalı");
        assert!(buf.len() > 4);
        let declared_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        assert_eq!(declared_len + 4, buf.len());

        let mut cursor = Cursor::new(buf);
        let decoded = read_frame(&mut cursor)
            .expect("çerçeve okunmalı")
            .expect("EOF olmamalı");
        assert_eq!(decoded, original);

        let eof = read_frame(&mut cursor).expect("temiz EOF dönmeli");
        assert!(eof.is_none());
    }

    #[test]
    fn rejects_oversized_frames() {
        let huge_text = "x".repeat(MAX_OUTBOUND_FRAME + 64);
        let outbound = json!({ "id": 1, "op": "eval", "args": { "code": huge_text } });
        let mut out_buf = Vec::new();
        let err = write_frame(&mut out_buf, &outbound).expect_err("1 MiB üstü reddedilmeli");
        assert!(err.to_string().contains("1 MiB"));
        assert!(out_buf.is_empty());

        let invalid_inbound_len = ((MAX_INBOUND_FRAME as u32) + 1).to_le_bytes();
        let mut in_cursor = Cursor::new(invalid_inbound_len.to_vec());
        let in_err = read_frame(&mut in_cursor).expect_err("64 MiB üstü reddedilmeli");
        assert!(in_err.to_string().contains("64 MiB"));
    }

    #[test]
    fn deny_allow_configuration() {
        let missing = FilterConfig::from_path(Path::new("/tmp/pc-eye-nonexistent-cfg-99999.json"));
        assert!(missing.deny.is_empty());
        assert!(missing.allow.is_empty());
        assert!(missing.is_host_denied("sub.paypal.com"));
        assert!(missing.is_host_denied("giris.turkiye.gov.tr"));
        assert!(!missing.is_host_denied("example.org"));

        let cfg = FilterConfig::from_json_str(
            r#"{"deny": [" Example-Corp.local ", ""], "allow": ["sandbox.paypal.com"]}"#,
        );
        assert_eq!(cfg.deny, vec!["example-corp.local"]);
        assert_eq!(cfg.allow, vec!["sandbox.paypal.com"]);
        assert!(!cfg.is_host_denied("sandbox.paypal.com"));
        assert!(cfg.is_host_denied("www.paypal.com"));
        assert!(cfg.is_host_denied("intranet.example-corp.local"));

        let msg = cfg.to_config_message();
        assert_eq!(msg["id"], 0);
        assert_eq!(msg["op"], "config");
        assert_eq!(msg["args"]["deny"], json!(["example-corp.local"]));
        assert_eq!(msg["args"]["allow"], json!(["sandbox.paypal.com"]));
    }

    #[test]
    fn user_js_generation() {
        let js = firefox_user_js(44556);
        assert!(js.contains("user_pref(\"devtools.debugger.remote-enabled\", true);"));
        assert!(js.contains("user_pref(\"devtools.chrome.enabled\", true);"));
        assert!(js.contains("user_pref(\"devtools.debugger.prompt-connection\", false);"));
        assert!(js.contains("user_pref(\"devtools.debugger.remote-port\", 44556);"));
        assert!(js.contains("user_pref(\"browser.shell.checkDefaultBrowser\", false);"));
        assert!(js.contains("user_pref(\"browser.aboutwelcome.enabled\", false);"));
        assert!(js.contains("user_pref(\"extensions.autoDisableScopes\", 0);"));
        assert!(js.contains("user_pref(\"accessibility.force_disabled\", 0);"));
        assert!(js.contains("user_pref(\"media.autoplay.default\", 0);"));
        assert!(js.contains("user_pref(\"media.autoplay.blocking_policy\", 0);"));
        assert!(js.contains("user_pref(\"media.autoplay.block-webaudio\", false);"));
        assert!(js.contains("user_pref(\"dom.disable_open_during_load\", false);"));
        assert!(js.contains("user_pref(\"browser.translations.automaticallyPopup\", false);"));
        assert!(js.contains("user_pref(\"signon.rememberSignons\", false);"));
        assert!(js.contains("user_pref(\"browser.contentblocking.category\", \"standard\");"));
        assert!(js.contains("user_pref(\"extensions.pocket.enabled\", false);"));
        assert!(js.contains("user_pref(\"browser.newtabpage.enabled\", false);"));
        assert!(js.contains("user_pref(\"browser.startup.page\", 0);"));
    }

    #[test]
    fn end_to_end_native_host_bridge() {
        let temp_dir = std::env::temp_dir().join(format!(
            "pc-eye-browser-e2e-{}-{}",
            std::process::id(),
            now_unix_ms()
        ));
        fs::create_dir_all(&temp_dir).expect("geçici dizin açılmalı");

        let (host_in_read, mut ext_out_write) = UnixStream::pair().expect("boru 1 açılmalı");
        let (mut ext_in_read, host_out_write) = UnixStream::pair().expect("boru 2 açılmalı");

        let filter_cfg = FilterConfig::from_json_str(r#"{"deny":["yasak.yerel"],"allow":[]}"#);
        let host_dir = temp_dir.clone();
        let host_handle = thread::spawn(move || {
            run_host_with_io(host_in_read, host_out_write, &host_dir, &filter_cfg)
        });

        // Sahte eklenti, ana iş parçacığı doğrulamalarını bitirene dek bağlantıyı açık tutar:
        // erken kapanırsa ana makine çıkıp soketi siler ve izin denetimleri yarışır.
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let ext_handle = thread::spawn(move || {
            write_frame(
                &mut ext_out_write,
                &json!({
                    "event": "hello",
                    "version": "1.0.0",
                    "ua": "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0"
                }),
            )
            .expect("hello yazılmalı");

            let cfg_frame = read_frame(&mut ext_in_read)
                .expect("config okunmalı")
                .expect("config çerçevesi gelmeli");
            assert_eq!(cfg_frame["id"], 0);
            assert_eq!(cfg_frame["op"], "config");
            assert_eq!(cfg_frame["args"]["deny"], json!(["yasak.yerel"]));

            let req1 = read_frame(&mut ext_in_read)
                .expect("istek 1 okunmalı")
                .expect("istek 1 gelmeli");
            assert_eq!(req1["op"], "tabs");
            let id1 = req1["id"].as_u64().expect("id olmalı");
            write_frame(
                &mut ext_out_write,
                &json!({
                    "id": id1,
                    "ok": true,
                    "result": [{"id": 10, "window": 1, "active": true, "title": "Pc-Eye", "url": "https://example.com", "status": "complete"}]
                }),
            )
            .expect("yanıt 1 yazılmalı");

            let req2 = read_frame(&mut ext_in_read)
                .expect("istek 2 okunmalı")
                .expect("istek 2 gelmeli");
            assert_eq!(req2["op"], "type");
            let id2 = req2["id"].as_u64().expect("id2 olmalı");
            write_frame(
                &mut ext_out_write,
                &json!({
                    "id": id2,
                    "ok": true,
                    "result": {"ok": true, "url": "https://example.com", "title": "Pc-Eye", "version": 2, "navigated": false, "changed": true}
                }),
            )
            .expect("yanıt 2 yazılmalı");

            let _ = done_rx.recv_timeout(Duration::from_secs(10));
            drop(ext_out_write);
        });

        let sock_path = temp_dir.join("browser.sock");
        for _ in 0..50 {
            if sock_path.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(sock_path.exists(), "browser.sock oluşturulmalı");

        let tabs = request_at(&sock_path, "tabs", json!({}), Duration::from_secs(3))
            .expect("tabs isteği başarılı olmalı");
        assert_eq!(tabs[0]["title"], "Pc-Eye");

        let long_input = "a".repeat(260);
        let type_res = request_at(
            &sock_path,
            "type",
            json!({ "ref": "e7", "text": long_input }),
            Duration::from_secs(3),
        )
        .expect("type isteği başarılı olmalı");
        assert_eq!(type_res["changed"], true);

        let info_raw = fs::read_to_string(temp_dir.join("browser.json"))
            .expect("browser.json yazılmış olmalı");
        let info: Value = serde_json::from_str(&info_raw).expect("browser.json geçerli olmalı");
        assert_eq!(info["version"], "1.0.0");

        let dir_mode = fs::metadata(&temp_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        let sock_mode = fs::metadata(&sock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(sock_mode, 0o600);
        let log_path = temp_dir.join("browser_actions.log");
        let log_mode = fs::metadata(&log_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(log_mode, 0o600);

        let log_raw = fs::read_to_string(&log_path).expect("eylem defteri yazılmış olmalı");
        let lines: Vec<&str> = log_raw.lines().collect();
        assert_eq!(lines.len(), 2);
        let second_log: Value = serde_json::from_str(lines[1]).expect("log JSON olmalı");
        assert_eq!(second_log["args"]["text"], "<gizli uzunluk=260>");

        let cfg_err = request_at(&sock_path, "config", json!({"allow":["."]}), Duration::from_secs(2))
            .expect_err("config op soketten reddedilmeli");
        assert!(cfg_err.to_string().contains("config"));

        let _ = done_tx.send(());
        ext_handle.join().expect("sahte eklenti iş parçacığı bitmeli");
        host_handle
            .join()
            .expect("host iş parçacığı paniklememeli")
            .expect("host temiz dönmeli");
        assert!(!sock_path.exists(), "çıkışta browser.sock silinmeli");

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn url_host_extraction_and_navigate_filter() {
        assert_eq!(
            extract_url_host("https://user:pass@Sub.PayPal.COM:8443/login?a=1#frag"),
            "sub.paypal.com"
        );
        assert_eq!(extract_url_host("turkiye.gov.tr/giris"), "turkiye.gov.tr");
        assert_eq!(extract_url_host("http://[::1]:8080/test"), "[::1]");
        assert_eq!(extract_url_host("about:blank"), "");
        assert_eq!(extract_url_host("file:///etc/passwd"), "");

        let cfg = FilterConfig::from_json_str(r#"{"deny":["ozel.yasak"],"allow":["sandbox.paypal.com"]}"#);
        let err = check_navigate_allowed_with_cfg("https://www.paypal.com/signin", &cfg)
            .expect_err("yasaklı host reddedilmeli");
        assert_eq!(err.to_string(), "site yasak listesinde: www.paypal.com");
        assert!(check_navigate_allowed_with_cfg("https://sandbox.paypal.com/test", &cfg).is_ok());
        assert!(check_navigate_allowed_with_cfg("https://ozel.yasak:9000/a", &cfg).is_err());
    }

    #[test]
    fn sanitize_args_masks_secrets_and_strips_navigate_url() {
        let masked_type = sanitize_args_for_log("type", &json!({ "ref": "e1", "text": "gizli123" }));
        assert_eq!(masked_type["text"], "<gizli uzunluk=8>");

        let masked_eval = sanitize_args_for_log("eval", &json!({ "code": "abc" }));
        assert_eq!(masked_eval["code"], "<gizli uzunluk=3>");

        let stripped_nav = sanitize_args_for_log(
            "navigate",
            &json!({ "url": "https://example.com/auth?token=secret#hash" }),
        );
        assert_eq!(stripped_nav["url"], "https://example.com/auth");
    }

    #[test]
    fn rejects_disallowed_commands_and_unready_wait() {
        assert!(command("", &json!({})).is_err());
        assert!(command("   ", &json!({})).is_err());
        assert!(command("config", &json!({ "allow": ["."] })).is_err());
        assert!(command("launch", &json!({ "url": "--headless" })).is_err());

        let state = HostState {
            writer: Mutex::new(Vec::<u8>::new()),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            log_lock: Mutex::new(()),
            runtime_dir: PathBuf::from("/tmp"),
            ready: Mutex::new(false),
            ready_cvar: Arc::new(Condvar::new()),
            running: Arc::new(AtomicBool::new(true)),
        };
        assert!(!state.wait_ready(Duration::from_millis(20)));
        state.mark_ready();
        assert!(state.wait_ready(Duration::from_millis(20)));
    }

    #[test]
    fn screen_point_and_window_selection() {
        let rect = json!({
            "x": 10.0,
            "y": 20.0,
            "w": 100.0,
            "h": 30.0,
            "cx": 60.4,
            "cy": 35.6,
            "chrome_x": 0,
            "chrome_y": 85,
            "dpr": 2.0,
            "zoom": 1.25
        });
        assert_eq!(screen_point((100, 50), &rect), Some((160, 171)));
        assert_eq!(screen_point((0, 0), &json!({ "cx": 10, "cy": 20 })), None);
        assert_eq!(
            screen_point(
                (0, 0),
                &json!({ "cx": "bad", "cy": 20, "chrome_x": 0, "chrome_y": 0 })
            ),
            None
        );

        assert_eq!(
            parse_ppid_from_stat("5432 (Web Content (1)) S 1234 5432 5432 0 -1 0"),
            Some(1234)
        );
        assert_eq!(parse_ppid_from_stat("bozuk"), None);

        let mk_win = |addr: &str, pid: i32, w: i64, h: i64, focused: bool, mapped: bool, hidden: bool| {
            crate::protocol::types::WindowInfo {
                address: addr.to_string(),
                class: "firefox".to_string(),
                title: "Firefox".to_string(),
                pid,
                workspace_id: 1,
                workspace_name: "1".to_string(),
                monitor: 0,
                x: 10,
                y: 20,
                width: w,
                height: h,
                floating: false,
                fullscreen: 0,
                xwayland: false,
                mapped,
                hidden,
                focused,
                protected_window: false,
            }
        };

        let windows = vec![
            mk_win("0xhidden", 100, 1920, 1080, true, true, true),
            mk_win("0xsmall_focused", 100, 800, 600, true, true, false),
            mk_win("0xlarge_unfocused", 100, 1600, 900, false, true, false),
            mk_win("0xparent", 90, 1920, 1080, true, true, false),
        ];
        assert_eq!(
            pick_window_for_chain(&[200, 100, 90], &windows).map(|w| w.address.as_str()),
            Some("0xsmall_focused")
        );

        let unfocused_windows = vec![
            mk_win("0xsmall", 100, 800, 600, false, true, false),
            mk_win("0xlarge", 100, 1600, 900, false, true, false),
        ];
        assert_eq!(
            pick_window_for_chain(&[200, 100], &unfocused_windows).map(|w| w.address.as_str()),
            Some("0xlarge")
        );
        assert!(pick_window_for_chain(&[999], &unfocused_windows).is_none());
    }

    #[test]
    fn navigate_rejects_file_scheme_early() {
        let err = command("navigate", &json!({ "url": "file:///etc/passwd" }))
            .expect_err("file:// URL köprüye gitmeden reddedilmeli");
        assert_eq!(
            err.to_string(),
            "file:// sayfaları köprüyle açılamaz ve okunamaz; dosyayı http üzerinden sunun (ör. `python3 -m http.server` ile dizini sunup http://127.0.0.1:<port>/... açın)"
        );
        assert!(command("navigate", &json!({ "url": "  FILE:///tmp/index.html " })).is_err());
        assert!(check_navigate_allowed_with_cfg("üğü", &FilterConfig::default()).is_ok());
    }

    #[test]
    fn crc32_and_xpi_zip_generation() {
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_ieee(b""), 0);

        let sources = [
            ("manifest.json", EXT_MANIFEST_JSON.as_bytes()),
            ("background.js", EXT_BACKGROUND_JS.as_bytes()),
            ("content.js", EXT_CONTENT_JS.as_bytes()),
        ];
        let zip = build_stored_zip(&sources).expect("ZIP oluşturulmalı");
        assert!(zip.len() >= 22);

        let eocd = &zip[zip.len() - 22..];
        assert_eq!(&eocd[0..4], &0x0605_4B50u32.to_le_bytes());
        let count_disk = u16::from_le_bytes([eocd[8], eocd[9]]);
        let count_total = u16::from_le_bytes([eocd[10], eocd[11]]);
        assert_eq!(count_disk, 3);
        assert_eq!(count_total, 3);

        let cd_size = u32::from_le_bytes([eocd[12], eocd[13], eocd[14], eocd[15]]) as usize;
        let cd_offset = u32::from_le_bytes([eocd[16], eocd[17], eocd[18], eocd[19]]) as usize;
        assert_eq!(cd_offset + cd_size, zip.len() - 22);

        let mut pos = cd_offset;
        for &(expected_name, expected_data) in &sources {
            assert_eq!(&zip[pos..pos + 4], &0x0201_4B50u32.to_le_bytes());
            let method = u16::from_le_bytes([zip[pos + 10], zip[pos + 11]]);
            assert_eq!(method, 0);
            let crc = u32::from_le_bytes([zip[pos + 16], zip[pos + 17], zip[pos + 18], zip[pos + 19]]);
            let comp_sz =
                u32::from_le_bytes([zip[pos + 20], zip[pos + 21], zip[pos + 22], zip[pos + 23]])
                    as usize;
            let uncomp_sz =
                u32::from_le_bytes([zip[pos + 24], zip[pos + 25], zip[pos + 26], zip[pos + 27]])
                    as usize;
            let name_len = u16::from_le_bytes([zip[pos + 28], zip[pos + 29]]) as usize;
            let extra_len = u16::from_le_bytes([zip[pos + 30], zip[pos + 31]]) as usize;
            let comment_len = u16::from_le_bytes([zip[pos + 32], zip[pos + 33]]) as usize;
            let local_off =
                u32::from_le_bytes([zip[pos + 42], zip[pos + 43], zip[pos + 44], zip[pos + 45]])
                    as usize;

            let name = std::str::from_utf8(&zip[pos + 46..pos + 46 + name_len]).unwrap();
            assert_eq!(name, expected_name);
            assert_eq!(comp_sz, expected_data.len());
            assert_eq!(uncomp_sz, expected_data.len());
            assert_eq!(crc, crc32_ieee(expected_data));

            assert_eq!(&zip[local_off..local_off + 4], &0x0403_4B50u32.to_le_bytes());
            let l_name_len = u16::from_le_bytes([zip[local_off + 26], zip[local_off + 27]]) as usize;
            let l_extra_len =
                u16::from_le_bytes([zip[local_off + 28], zip[local_off + 29]]) as usize;
            let data_start = local_off + 30 + l_name_len + l_extra_len;
            assert_eq!(&zip[data_start..data_start + comp_sz], expected_data);

            pos += 46 + name_len + extra_len + comment_len;
        }
        assert_eq!(pos, cd_offset + cd_size);
    }
}
