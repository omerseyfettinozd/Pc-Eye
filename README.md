# PC-Eye (`v0.2.0` — Full White-Box OS Observability & Director Engine)

**PC-Eye**, yapay zeka modellerinin (LLM / VLM ajanlarının) Linux (`Wayland` / `Hyprland`) işletim sistemini **kör ekran görüntüsü tahminleriyle (Black-Box)** değil, işletim sisteminin tüm iç katmanlarını **Tam Beyaz Kutu (Full White-Box)** olarak anlık izleyerek ve deterministik biçimde yöneterek kullanması için geliştirilmiş **saf Rust (`pure-Rust`)** tek-binary çekirdektir.

---

## 5 Göz (5-Eyes Observability) + 1 Yönetmen (Deterministic Director)

| Katman | Modül | Yetenekler |
| :--- | :--- | :--- |
| **Göz 1: Kompozitör & Uzay** | [`src/eyes/compositor.rs`](src/eyes/compositor.rs) | Doğrudan Hyprland UNIX soketi (`.socket.sock` & `.socket2.sock`, `<0.5ms`), monitör/workspace/pencere topolojisi, `layer-shell` (`waybar`, `rofi`, `dunst`), `AuthGuard` (`polkit`/`gpg` kilit tespiti), `ProtectedWindow` koruması, Hyprland kısayol (`binds`) keşfi |
| **Göz 2a: Semantik UI & PTY** | [`src/eyes/ui_atspi.rs`](src/eyes/ui_atspi.rs) | Otomatik `org.a11y.Status.IsEnabled=true` aktivasyonu, `AT-SPI2` erişilebilirlik ağacı, **Pencere-İçi -> Global Koordinat Çözücü**, **64-bit State Bitmask (`checked`, `focused`, `editable`, `enabled`, `selected`)**, **Text & Value okuyucu**, D-Bus `Action.DoAction(0)` & `EditableText.SetTextContents`, ve **Terminal PTY Context (`foreground_pid`, `foreground_cmd`, `cwd`, `pty_path`)** |
| **Göz 2b: Diferansiyel Vizyon & SoM** | [`src/eyes/vision.rs`](src/eyes/vision.rs) | Sıfır-disk (`grim -t ppm -` pipe) görüntü işleme, 64-bit `dHash`, kareler arası `pixel_diff_pct` değişim yüzdesi, bölgesel `zoom`, ve **Set-of-Marks (`--marks`)** numaralı kutu çizimi (AT-SPI2 + yüksek kontrastlı görsel kontrol kutusu dedektörü) |
| **Göz 3: Süreç Kümesi & GPU X-Ray** | [`src/eyes/process_xray.rs`](src/eyes/process_xray.rs) | Uygulama süreç soy ağacı (`AppCluster` + `cgroup`), `p50/p95/p99` spike istatistikleri, açık dosyalar (`/proc/<pid>/fd`), canlı `TCP/UDP` soket tablosu (`/proc/net/*`), Linux PSI (`/proc/pressure/*`), çift GPU telemetrisi (`NVIDIA RTX` + `AMD Radeon`) |
| **Göz 4: Birleşik Log & Bildirim** | [`src/eyes/logs_notify.rs`](src/eyes/logs_notify.rs) | `systemd-journald` JSON logları + `pc-eye launch` ile başlatılan uygulamaların `stdout/stderr` kayıtları (`/run/user/1000/pc-eye/app_logs/*.log`) + `dunstctl history` masaüstü bildirimlerinin tek kronolojik akışta birleştirilmesi |
| **Göz 5: Ses, Fiziksel PCM & MPRIS** | [`src/eyes/audio_pipewire.rs`](src/eyes/audio_pipewire.rs) | `PipeWire` aktif ses akışları, **`pw-record` ile gerçek fiziksel PCM 16-bit ses sinyali (`rms_dbfs`, `peak_dbfs`, `is_physically_silent`)**, **D-Bus MPRIS medya ve aktif tarayıcı URL (`xesam:url`) okuyucu**, ve ses/medya kontrolü (`play-pause`, `set-volume`, `mute`) |
| **Yönetmen (Director)** | [`src/director/`](src/director/) | Saf-Rust Wayland `zwlr_virtual_pointer_v1` + `zwp_virtual_keyboard_v1` (dinamik `memfd` XKB keymap ile tam Türkçe/UTF-8 dizgi yazımı, `wtype`/`ydotool` gerektirmez), Watchdog korumalı `freeze`/`thaw` (`SIGSTOP`/`SIGCONT` + `FreezeDenylist`), atomik `sequence` yürütücü, ve çok-modlu `wait` (`visual_settle`, `log_match`, `ui_match`, `port_open`) |

---

## Kurulum ve Çalıştırma

```bash
cargo test
cargo build --release
cp target/release/pc-eye ~/.local/bin/pc-eye
```

### Temel CLI Komutları

```bash
# Sistem yetenek ve hazırlık kontrolü
pc-eye doctor

# Tek çağrıda birleşik masaüstü, sistem sağlığı (CPU/RAM/PSI/GPU), ses/MPRIS ve delta olay özeti
pc-eye context

# Hedef pencerenin AT-SPI2 UI elemanları (states/value dahil) ve Terminal PTY durumu
pc-eye ui --query "Kaydet"

# Set-of-Marks numaralı görsel yakalama + dHash + pixel_diff_pct
pc-eye vision --marks

# Süreç kümesi (AppCluster), açık dosyalar ve aktif ağ soketleri röntgeni
pc-eye xray --pid 1234

# Birleşik journald + uygulama stdout/stderr + masaüstü bildirim geçmişi
pc-eye logs --limit 20

# PipeWire ses akışları + gerçek fiziksel PCM ses sinyali (dBFS) + MPRIS medya/URL
pc-eye audio
pc-eye audio --op play-pause

# Semantik UI tıklama / doğrudan D-Bus DoAction / metin yazma
pc-eye click-ui --name "Save" --prefer-dbus
pc-eye click-ui --mark 3

# Saf-Rust Wayland sanal fare ve klavye (Türkçe/UTF-8 destekli)
pc-eye input click --x 640 --y 360
pc-eye input type --text "Merhaba Dünya! ŞĞÜÖÇİ"
pc-eye input key --keys "Ctrl+Shift+P"

# Hyprland kısayollarını keşfetme ve tetikleme
pc-eye director binds
pc-eye director use-bind --text "SUPER+Return"

# Kör sleep yerine deterministik çok-modlu bekleme
pc-eye wait visual_settle --timeout-ms 3000
pc-eye wait port_open --match "8080" --timeout-ms 5000

# Canlı Web Gözlem Paneli (http://127.0.0.1:7777)
pc-eye serve --port 7777
```

---

## MCP Sunucusu ve AI Ajan Entegrasyonu

`pc-eye mcp` komutu, **JSON-RPC 2.0 `stdio` Model Context Protocol** sunucusunu başlatır ve 12 aracı (`then="context"|"ui"|"vision"|"vision_marks"` aksiyon+gözlem füzyonu ile) doğrudan AI modellerine açar:

- `pc_eye_context`
- `pc_eye_ui`
- `pc_eye_vision`
- `pc_eye_xray`
- `pc_eye_logs`
- `pc_eye_audio`
- `pc_eye_click_ui`
- `pc_eye_input`
- `pc_eye_window`
- `pc_eye_launch`
- `pc_eye_wait`
- `pc_eye_director`
