pub mod daemon;
pub mod director;
pub mod eyes;
pub mod mcp;
pub mod protocol;
pub mod web;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "pc-eye",
    version,
    about = "PC-Eye V2: Tam Beyaz Kutu (Full White-Box) İşletim Sistemi Gözlemlenebilirlik ve Kontrol Çekirdeği"
)]
struct Cli {
    /// Çıktı formatı (varsayılan olarak her zaman JSON döner, uyumluluk için kabul edilir)
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Sistem hazırlık ve yetenek tanılama raporu (Hyprland, Wayland protokolleri, AT-SPI2, PipeWire, GPU, grim)
    Doctor,

    /// Göz 1+3+4+5: Tek çağrıda tam masaüstü, sistem sağlığı (PSI/RAM/CPU/GPU), ses/MPRIS ve delta olay özeti
    Context {
        /// Sadece bu sıra numarasından sonraki olayları getir
        #[arg(long)]
        seq_since: Option<u64>,
    },

    /// Tek çağrılık, token-dostu durum: pencereler + hedef pencerenin etkileşimli elemanları + yeni olaylar
    Snapshot {
        /// Pencere adresi, başlığı veya sınıfı (boşsa aktif pencere)
        #[arg(long)]
        window: Option<String>,
        #[arg(long, default_value_t = 40)]
        max_elements: usize,
        /// Algısal ikon tahminlerini de ekle (varsayılan: metinli eleman varsa elenir)
        #[arg(long)]
        icons: bool,
        #[arg(long)]
        seq_since: Option<u64>,
        /// Önceki snapshot'ın `snap` kimliği: yalnız değişenler (daemon üzerinden çalışır)
        #[arg(long)]
        since: Option<u64>,
    },

    /// Göz 2a: Hedef pencerenin AT-SPI2 semantik UI ağacını (states ve value dahil) veya Terminal PTY durumunu okur
    Ui {
        /// Pencere adresi (ör. 0x55fd...). Boşsa aktif pencere
        #[arg(long)]
        window: Option<String>,
        /// İsim veya rol filtresi
        #[arg(long)]
        query: Option<String>,
        /// Maksimum eleman sayısı
        #[arg(long, default_value_t = 60)]
        max_elements: usize,
    },

    /// Göz 2b: Ekran, pencere, bölge veya zoom görüntüsü yakalar; dHash/pixel-diff ve Set-of-Marks üretir
    Vision {
        /// Hedef pencere adresi
        #[arg(long)]
        window: Option<String>,
        /// Kırpma bölgesi: "x,y,w,h" (--window ile pencereye göreli, tek başına ekran koordinatı)
        #[arg(long)]
        region: Option<String>,
        /// Yakınlaştırma merkezi: "x,y"
        #[arg(long)]
        zoom: Option<String>,
        /// Tıklanabilir bölgelerin üzerine numaralı Set-of-Marks kutuları çiz
        #[arg(long)]
        marks: bool,
        /// Kare dizisi (animasyon izlenebilirliği) modu: bu kadar kare yakala
        #[arg(long)]
        frames: Option<usize>,
        /// Kareler arası bekleme süresi (ms)
        #[arg(long, default_value_t = 100)]
        interval_ms: u64,
        /// Yalnızca kare dizisi modunda: SON kareyi bu yola yaz (varsayılan: zero-disk, yazılmaz)
        #[arg(long)]
        out: Option<String>,
    },

    /// Göz 3: Pencerenin veya PID'nin arkasındaki tüm süreç soy ağacını, açık dosyaları ve ağ soketlerini röntgenler
    Xray {
        /// Pencere adresi
        #[arg(long)]
        window: Option<String>,
        /// Kök süreç PID numarası
        #[arg(long)]
        pid: Option<i32>,
    },

    /// Eylem programı: beklentili adım dizisini tek çağrıda çalıştırır (JSON: {"window":..,"steps":[..]})
    Run {
        /// Program JSON'u; `-` ise standart girdiden okunur
        program: String,
        #[arg(long)]
        allow_protected: bool,
    },

    /// Göz 3: Penceresi olmayan süreçleri de ad/komut satırı parçasına veya dinlediği porta göre bulur
    Ps {
        /// Ad veya komut satırında geçen metin
        #[arg(long = "match")]
        query: Option<String>,
        /// Bu TCP/UDP portunu dinleyen süreçler
        #[arg(long)]
        port: Option<u16>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },

    /// Göz 4: Sistem journald loglarını, başlatılan uygulamaların stdout/stderr kayıtlarını ve masaüstü bildirimlerini döner
    Logs {
        /// Belirli bir PID için filtrele
        #[arg(long)]
        pid: Option<i32>,
        /// Maksimum satır sayısı
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },

    /// Göz 5: PipeWire ses grafiğini, gerçek fiziksel PCM ses sinyalini (RMS/Peak dBFS) ve MPRIS medya/tarayıcı URL durumunu okur veya kontrol eder
    Audio {
        /// Kontrol işlemi: play | pause | play-pause | next | previous | stop | set-volume | mute | unmute
        #[arg(long)]
        op: Option<String>,
        /// Kontrol argümanı (ör. 0.5 veya firefox)
        #[arg(long)]
        arg: Option<String>,
    },

    /// Semantik UI elemanına ismine veya Set-of-Marks numarasına göre doğrudan tıklar, D-Bus DoAction tetikler veya içine metin yazar
    ClickUi {
        /// Pencere adresi
        #[arg(long)]
        window: Option<String>,
        /// Kontrol adı
        #[arg(long)]
        name: Option<String>,
        /// Set-of-Marks numarası
        #[arg(long)]
        mark: Option<usize>,
        /// Fare düğmesi: left | right | middle
        #[arg(long, default_value = "left")]
        button: String,
        /// Fareyi hareket ettirmeden doğrudan D-Bus DoAction(0) kullan
        #[arg(long)]
        prefer_dbus: bool,
        /// Kontrol bir metin kutusuysa içine bu metni yaz
        #[arg(long)]
        set_text: Option<String>,
        /// Korumalı pencere kilidini aş
        #[arg(long)]
        allow_protected: bool,
    },

    /// Wayland yerel sanal fare ve dinamik XKB klavye enjeksiyonu
    Input {
        /// İşlem: click | move | drag | scroll | type | key
        op: String,
        #[arg(long)]
        x: Option<i64>,
        #[arg(long)]
        y: Option<i64>,
        #[arg(long)]
        x2: Option<i64>,
        #[arg(long)]
        y2: Option<i64>,
        #[arg(long, default_value_t = 0)]
        dx: i32,
        #[arg(long, default_value_t = -3)]
        dy: i32,
        #[arg(long, default_value = "left")]
        button: String,
        #[arg(long, default_value_t = 1)]
        clicks: u32,
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        keys: Option<String>,
        #[arg(long)]
        window: Option<String>,
        #[arg(long)]
        allow_protected: bool,
    },

    /// Hyprland IPC üzerinden pencere ve çalışma alanı yönetimi
    Window {
        /// İşlem: focus | close | workspace | movetoworkspace | fullscreen | float | move | resize
        /// (fullscreen --arg: on | max | off | toggle; --window verilirse o pencere)
        action: String,
        #[arg(long)]
        window: Option<String>,
        #[arg(long)]
        arg: Option<String>,
    },

    /// Sanal (headless) ekran: list | create | mode | remove — kullanıcının ekranından yalıtılmış çalışma alanı
    Display {
        /// İşlem: list | create | mode | remove
        action: String,
        /// Çıkış adı (create'te isteğe bağlı; yoksa HEADLESS-N)
        #[arg(long)]
        name: Option<String>,
        /// Çözünürlük: "WxH" veya "WxH@R" (ör. 1500x940@60)
        #[arg(long)]
        mode: Option<String>,
        /// Konum: "X,Y" veya "isolated" (varsayılan: diğer ekranların sağında 1000 px boşluk)
        #[arg(long)]
        position: Option<String>,
        #[arg(long)]
        scale: Option<f64>,
    },

    /// Uygulama başlatır, stdout/stderr logunu yakalar ve yeni pencerenin adresini döner
    Launch {
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long, default_value_t = 3000)]
        timeout_ms: u64,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },

    /// Çok-modlu deterministik senkronizasyon: visual_settle | log_match | ui_match | port_open | openwindow | closewindow | activewindow
    Wait {
        #[arg(default_value = "visual_settle")]
        event: String,
        #[arg(long)]
        r#match: Option<String>,
        #[arg(long)]
        window: Option<String>,
        #[arg(long, default_value_t = 5000)]
        timeout_ms: u64,
    },

    /// Yönetmen kontrolleri: freeze | thaw | clipboard | binds | use-bind | sequence
    Director {
        /// İşlem: freeze | thaw | clipboard | binds | use-bind | sequence
        op: String,
        #[arg(long)]
        pid: Option<i32>,
        #[arg(long, default_value_t = 500)]
        max_freeze_ms: u64,
        #[arg(long)]
        text: Option<String>,
        /// Panoya görsel (`image/png`) veya dosya (`text/uri-list`) kopyalamak için dosya yolu
        #[arg(long)]
        file: Option<String>,
        /// `cliphist` geçmişindeki bir öğeyi ID ile panoya geri yükle
        #[arg(long)]
        restore_id: Option<String>,
        #[arg(long)]
        steps_json: Option<String>,
        #[arg(long)]
        allow_protected: bool,
    },

    /// Kesintisiz arka plan Kara Kutu (Flight Recorder) olay ve telemetri servisi
    Daemon,

    /// Model Context Protocol (MCP) JSON-RPC 2.0 stdio sunucusunu başlatır
    Mcp,

    /// Canlı Web Gözlem Paneli ve HTTP API sunucusunu başlatır
    Serve {
        #[arg(long, default_value_t = 7777)]
        port: u16,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Doctor => {
            let hypr_sock = eyes::compositor::resolve_hyprland_socket(".socket.sock").ok();
            let hypr_event_sock = eyes::compositor::resolve_hyprland_socket(".socket2.sock").ok();
            let a11y_enabled = eyes::ui_atspi::ensure_atspi_enabled();
            let a11y_bus = eyes::ui_atspi::resolve_atspi_bus_address();
            let grim_ok = std::process::Command::new("grim")
                .arg("-h")
                .output()
                .is_ok();
            let pw_ok = std::process::Command::new("pw-dump")
                .arg("--version")
                .output()
                .is_ok();
            let ocr_ok = eyes::ocr::is_ocr_available();
            let tess_dir = eyes::ocr::resolve_tessdata_dir();
            let daemon_ok = daemon::server::ensure_background_daemon();
            let gpus = eyes::process_xray::read_gpu_telemetry();

            let report = json!({
                "status": "ready",
                "version": env!("CARGO_PKG_VERSION"),
                "daemon_running": daemon_ok,
                "hyprland_socket": hypr_sock,
                "hyprland_event_socket": hypr_event_sock,
                "atspi_enabled": a11y_enabled,
                "atspi_bus_address": a11y_bus,
                "ocr_available": ocr_ok,
                "tessdata_dir": tess_dir,
                "grim_available": grim_ok,
                "pipewire_available": pw_ok,
                "gpus_detected": gpus,
                "runtime_dir": eyes::vision::runtime_dir(),
            });
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Commands::Context { seq_since } => {
            let rep = daemon::server::build_context_report(seq_since)?;
            println!("{}", serde_json::to_string_pretty(&rep)?);
        }
        Commands::Snapshot { window, max_elements, icons, seq_since, since } => {
            // Daemon'da olay güdümlü önbellek ve snapshot geçmişi (delta) yaşar; yoksa yerelde.
            daemon::server::ensure_background_daemon();
            let args = serde_json::json!({
                "window": window, "max_elements": max_elements, "icons": icons,
                "seq_since": seq_since, "since": since,
            });
            let snap = match daemon::rpc::request("snapshot", args) {
                Some(reply) => reply.map_err(|e| anyhow::anyhow!(e))?,
                None => eyes::snapshot::build_snapshot(window.as_deref(), max_elements, icons, seq_since, since)?,
            };
            println!("{}", serde_json::to_string_pretty(&snap)?);
        }
        Commands::Ui {
            window,
            query,
            max_elements,
        } => {
            daemon::server::ensure_background_daemon();
            let args = serde_json::json!({ "window": window, "query": query, "max_elements": max_elements });
            let rep = match daemon::rpc::request("ui", args) {
                Some(reply) => reply.map_err(|e| anyhow::anyhow!(e))?,
                None => serde_json::to_value(eyes::ui_atspi::inspect_window_ui(
                    window.as_deref(),
                    query.as_deref(),
                    max_elements,
                )?)?,
            };
            println!("{}", serde_json::to_string_pretty(&rep)?);
        }
        Commands::Vision {
            window,
            region,
            zoom,
            marks,
            frames,
            interval_ms,
            out,
        } => {
            let reg = region.as_deref().and_then(parse_four_ints);
            let zm = zoom.as_deref().and_then(parse_two_ints);
            if let Some(n) = frames {
                let rep = eyes::vision::capture_frame_sequence(
                    window.as_deref(),
                    reg,
                    zm,
                    n,
                    interval_ms,
                    out.as_deref(),
                )?;
                println!("{}", serde_json::to_string_pretty(&rep)?);
            } else {
                let rep = eyes::vision::capture_vision(window.as_deref(), reg, zm, marks)?;
                println!("{}", serde_json::to_string_pretty(&rep)?);
            }
        }
        Commands::Xray { window, pid } => {
            let rep = daemon::server::build_xray_for_target(window.as_deref(), pid)?;
            println!("{}", serde_json::to_string_pretty(&rep)?);
        }
        Commands::Run { program, allow_protected } => {
            let text = if program == "-" {
                let mut buf = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                buf
            } else {
                program
            };
            let parsed: serde_json::Value = serde_json::from_str(&text)?;
            let out = director::program::run_program(&parsed, allow_protected)?;
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        Commands::Ps { query, port, limit } => {
            let found = eyes::process_xray::find_processes(query.as_deref(), port, limit)?;
            println!("{}", serde_json::to_string_pretty(&found)?);
        }
        Commands::Logs { pid, limit } => {
            let pids = pid.map(|p| vec![p]);
            let logs = eyes::logs_notify::query_recent_journal_logs(pids.as_deref(), limit)?;
            println!("{}", serde_json::to_string_pretty(&logs)?);
        }
        Commands::Audio { op, arg } => {
            if let Some(operation) = op {
                let rec =
                    eyes::audio_pipewire::control_audio_and_media(&operation, arg.as_deref())?;
                println!("{}", serde_json::to_string_pretty(&rec)?);
            } else {
                let rep = eyes::audio_pipewire::capture_audio_report()?;
                println!("{}", serde_json::to_string_pretty(&rep)?);
            }
        }
        Commands::ClickUi {
            window,
            name,
            mark,
            button,
            prefer_dbus,
            set_text,
            allow_protected,
        } => {
            let rec = director::input::click_ui_element_ext(
                window.as_deref(),
                name.as_deref(),
                mark,
                &button,
                prefer_dbus,
                set_text.as_deref(),
                allow_protected,
            )?;
            println!("{}", serde_json::to_string_pretty(&rec)?);
        }
        Commands::Input {
            op,
            x,
            y,
            x2,
            y2,
            dx,
            dy,
            button,
            clicks,
            text,
            keys,
            window,
            allow_protected,
        } => {
            let rec = match op.as_str() {
                "click" => director::input::click_at(x, y, &button, clicks, allow_protected)?,
                "move" => director::input::move_cursor(
                    x.ok_or_else(|| anyhow::anyhow!("--x gerekli"))?,
                    y.ok_or_else(|| anyhow::anyhow!("--y gerekli"))?,
                )?,
                "drag" => director::input::drag_mouse(
                    x.ok_or_else(|| anyhow::anyhow!("--x gerekli"))?,
                    y.ok_or_else(|| anyhow::anyhow!("--y gerekli"))?,
                    x2.ok_or_else(|| anyhow::anyhow!("--x2 gerekli"))?,
                    y2.ok_or_else(|| anyhow::anyhow!("--y2 gerekli"))?,
                    &button,
                    allow_protected,
                )?,
                "scroll" => director::input::scroll_wheel(x, y, dx, dy, allow_protected)?,
                "type" => director::input::type_text(
                    text.as_deref()
                        .ok_or_else(|| anyhow::anyhow!("--text gerekli"))?,
                    window.as_deref(),
                    allow_protected,
                )?,
                "key" => director::input::press_key_combo(
                    keys.as_deref()
                        .or(text.as_deref())
                        .ok_or_else(|| anyhow::anyhow!("--keys gerekli"))?,
                    window.as_deref(),
                    allow_protected,
                )?,
                other => return Err(anyhow::anyhow!("Bilinmeyen input işlemi: {}", other)),
            };
            println!("{}", serde_json::to_string_pretty(&rec)?);
        }
        Commands::Window {
            action,
            window,
            arg,
        } => {
            let rec =
                director::process_ctl::manage_window(&action, window.as_deref(), arg.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&rec)?);
        }
        Commands::Display {
            action,
            name,
            mode,
            position,
            scale,
        } => {
            let rec = director::display::manage_display(
                &action,
                name.as_deref(),
                mode.as_deref(),
                position.as_deref(),
                scale,
            )?;
            println!("{}", serde_json::to_string_pretty(&rec)?);
        }
        Commands::Launch {
            workspace,
            timeout_ms,
            command,
        } => {
            let rec = director::process_ctl::launch_application(
                &command,
                workspace.as_deref(),
                timeout_ms,
            )?;
            println!("{}", serde_json::to_string_pretty(&rec)?);
        }
        Commands::Wait {
            event,
            r#match,
            window,
            timeout_ms,
        } => {
            let rep = director::wait::wait_for_condition(
                &event,
                r#match.as_deref(),
                window.as_deref(),
                timeout_ms,
            )?;
            println!("{}", serde_json::to_string_pretty(&rep)?);
        }
        Commands::Director {
            op,
            pid,
            max_freeze_ms,
            text,
            file,
            restore_id,
            steps_json,
            allow_protected,
        } => match op.as_str() {
            "freeze" => {
                let p = pid.ok_or_else(|| anyhow::anyhow!("--pid gerekli"))?;
                let rec = director::process_ctl::freeze_process_safe(p, max_freeze_ms)?;
                println!("{}", serde_json::to_string_pretty(&rec)?);
            }
            "thaw" => {
                let p = pid.ok_or_else(|| anyhow::anyhow!("--pid gerekli"))?;
                let rec = director::process_ctl::thaw_process(p)?;
                println!("{}", serde_json::to_string_pretty(&rec)?);
            }
            "clipboard" => {
                let val = director::process_ctl::clipboard_action_ext(
                    text.as_deref(),
                    file.as_deref(),
                    restore_id.as_deref(),
                )?;
                println!("{}", serde_json::to_string_pretty(&val)?);
            }
            "binds" => {
                let binds = eyes::compositor::list_hyprland_binds()?;
                println!("{}", serde_json::to_string_pretty(&binds)?);
            }
            "use-bind" | "use_bind" => {
                let combo = text.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("--text içinde kısayol adı gerekli (ör. SUPER+Return)")
                })?;
                let rec = director::process_ctl::trigger_hyprland_bind(combo)?;
                println!("{}", serde_json::to_string_pretty(&rec)?);
            }
            "sequence" => {
                let raw = steps_json
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("--steps-json gerekli"))?;
                let steps: Vec<director::sequence::SequenceStep> = serde_json::from_str(raw)?;
                let rep = director::sequence::execute_sequence(&steps, allow_protected)?;
                println!("{}", serde_json::to_string_pretty(&rep)?);
            }
            other => return Err(anyhow::anyhow!("Bilinmeyen director işlemi: {}", other)),
        },
        Commands::Daemon => {
            daemon::server::run_daemon_forever()?;
        }
        Commands::Mcp => {
            mcp::run_mcp_stdio_server()?;
        }
        Commands::Serve { port } => {
            web::run_web_dashboard(port)?;
        }
    }

    Ok(())
}

fn parse_four_ints(s: &str) -> Option<[i64; 4]> {
    let parts: Vec<i64> = s
        .split(|c: char| c == ',' || c == 'x' || c == ' ')
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse::<i64>().ok())
        .collect();
    if parts.len() == 4 {
        Some([parts[0], parts[1], parts[2], parts[3]])
    } else {
        None
    }
}

fn parse_two_ints(s: &str) -> Option<[i64; 2]> {
    let parts: Vec<i64> = s
        .split(|c: char| c == ',' || c == ' ')
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse::<i64>().ok())
        .collect();
    if parts.len() == 2 {
        Some([parts[0], parts[1]])
    } else {
        None
    }
}
