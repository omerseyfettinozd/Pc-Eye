# PC-Eye (`v0.3.0` — Full White-Box OS Observability & Deterministic Director Engine)

**PC-Eye V3**, yapay zeka modellerinin (LLM / VLM ajanlarının) Linux (`Wayland` / `Hyprland`) işletim sistemini **kör ekran görüntüsü tahminleriyle (Black-Box)** değil, işletim sisteminin tüm iç katmanlarını **Tam Beyaz Kutu (Full White-Box)** olarak anlık izleyerek ve deterministik biçimde yöneterek kullanması için geliştirilmiş **saf Rust (`pure-Rust`)** tek-binary çekirdektir.

---

## 5 Göz (5-Eyes Observability) + 1 Yönetmen (Deterministic Director)

| Katman | Modül | V3.0 Yetenekleri |
| :--- | :--- | :--- |
| **Göz 1: Kompozitör & Uzay** | [`src/eyes/compositor.rs`](src/eyes/compositor.rs) | Doğrudan Hyprland UNIX soketi (`.socket.sock` & `.socket2.sock`, `<0.5ms`), monitör/workspace/pencere topolojisi, `layer-shell` (`waybar`, `rofi`, `dunst`), `AuthGuard` (`polkit`/`gpg` kilit tespiti), `ProtectedWindow` koruması, Hyprland kısayol (`binds`) keşfi |
| **Göz 2a: Semantik UI, Bellek-İçi OCR & Terminal Ekranı** | [`src/eyes/ui_atspi.rs`](src/eyes/ui_atspi.rs), [`src/eyes/ocr.rs`](src/eyes/ocr.rs) | Otomatik `org.a11y.Status.IsEnabled=true` aktivasyonu, `AT-SPI2` erişilebilirlik ağacı, **Pencere-İçi -> Global Koordinat Çözücü**, **64-bit State Bitmask & Value okuyucu**, D-Bus `Action.DoAction(0)` & `EditableText.SetTextContents`, **Sıfır-Disk Bellek-İçi OCR (`grim PPM -> tesseract eng+tur TSV` ifade gruplayıcı: Electron/oyun/Canvas pencerelerindeki tüm metinleri koordinatlarıyla çıkarır)**, ve **Terminal PTY + Görünür Ekran Satırları (`visible_lines`)** |
| **Göz 2b: Diferansiyel Vizyon & SoM** | [`src/eyes/vision.rs`](src/eyes/vision.rs) | Sıfır-disk (`grim -t ppm -` pipe) görüntü işleme, 64-bit `dHash`, kareler arası `pixel_diff_pct` değişim yüzdesi, bölgesel `zoom`, ve **Set-of-Marks (`--marks`)** numaralı kutu çizimi (AT-SPI2 + OCR + yüksek kontrastlı görsel kontrol kutusu dedektörü) |
| **Göz 3: Süreç Kümesi & GPU X-Ray** | [`src/eyes/process_xray.rs`](src/eyes/process_xray.rs) | Uygulama süreç soy ağacı (`AppCluster` + `cgroup`), `p50/p95/p99` spike istatistikleri, açık dosyalar (`/proc/<pid>/fd`), canlı `TCP/UDP` soket tablosu (`/proc/net/*`), Linux PSI (`/proc/pressure/*`), çift GPU telemetrisi (`NVIDIA RTX` + `AMD Radeon`) |
| **Göz 4: Birleşik Log, Coredump & Bildirim** | [`src/eyes/logs_notify.rs`](src/eyes/logs_notify.rs) | `systemd-journald` JSON logları + `pc-eye launch` uygulama `stdout/stderr` kayıtları (`app_logs/*.log`) + **`coredumpctl` süreç çökme röntgeni (`SIGSEGV`/`SIGABRT`)** + `dunstctl history` masaüstü bildirimlerinin tek kronolojik akışta birleştirilmesi |
| **Göz 5: Ses, Fiziksel PCM, WAV Kaydı & MPRIS** | [`src/eyes/audio_pipewire.rs`](src/eyes/audio_pipewire.rs) | `PipeWire` aktif ses akışları, **`pw-record` ile gerçek fiziksel PCM 16-bit ses sinyali (`rms_dbfs`, `peak_dbfs`, `audible_signal`)**, **Masaüstü Ses Kaydedici (`audio --op record --arg <ms>` -> `.wav`)**, **D-Bus MPRIS medya ve aktif tarayıcı URL (`xesam:url`) okuyucu**, ve ses/medya kontrolü |
| **Yönetmen (Director) & Kara Kutu Daemon** | [`src/director/`](src/director/), [`src/daemon/`](src/daemon/) | Saf-Rust Wayland `zwlr_virtual_pointer_v1` + `zwp_virtual_keyboard_v1` (dinamik `memfd` XKB keymap ile tam Türkçe/UTF-8 dizgi yazımı), **Zengin Çok-Formatlı Pano (`MIME`, `chromium/x-source-url`, `cliphist` geçmişi, `--file` ile görsel/dosya kopyalama, `--restore-id`)**, **Kesintisiz Arka Plan `pc-eye daemon` (`.socket2.sock` Kara Kutu kaydedicisi)**, Watchdog korumalı `freeze`/`thaw` ve çok-modlu `wait` (`visual_settle`, `log_match`, `ui_match`, `port_open`) |

---

## Kurulum ve Çalıştırma

```bash
cargo test
cargo build --release
install -m 755 target/release/pc-eye ~/.local/bin/pc-eye
```

### Temel CLI Komutları

```bash
# Sistem yetenek, OCR, AT-SPI2, GPU ve arka plan daemon kontrolü
pc-eye doctor

# Tek çağrıda birleşik masaüstü, sistem sağlığı (CPU/RAM/PSI/GPU), ses/MPRIS ve delta olay özeti
pc-eye context

# Hedef pencerenin AT-SPI2 veya Otomatik Bellek-İçi OCR (eng+tur) UI elemanları ve Terminal ekran satırları
pc-eye ui --window 0x55fd87a5cad0 --max-elements 20

# Set-of-Marks numaralı görsel yakalama + dHash + pixel_diff_pct
pc-eye vision --marks

# Süreç kümesi (AppCluster), açık dosyalar ve aktif ağ soketleri röntgeni
pc-eye xray --pid 1234

# Birleşik journald + uygulama stdout/stderr + coredumpctl çökmeleri + masaüstü bildirim geçmişi
pc-eye logs --limit 20

# PipeWire ses akışları + gerçek fiziksel PCM ses sinyali (dBFS) + MPRIS medya/URL + WAV ses kaydı
pc-eye audio
pc-eye audio --op record --arg 3000
pc-eye audio --op play-pause

# Semantik UI / OCR metni tıklama / doğrudan D-Bus DoAction / metin yazma
pc-eye click-ui --name "New Conversation"
pc-eye click-ui --mark 3

# Saf-Rust Wayland sanal fare ve klavye (Türkçe/UTF-8 destekli)
pc-eye input click --x 640 --y 360
pc-eye input type --text "Merhaba Dünya! ŞĞÜÖÇİ"
pc-eye input key --keys "Ctrl+Shift+P"

# Zengin Pano (MIME türleri, kaynak URL, cliphist geçmişi, görsel/dosya kopyalama)
pc-eye director clipboard
pc-eye director clipboard --file /path/to/image.png
pc-eye director clipboard --restore-id 16

# Hyprland kısayollarını keşfetme ve tetikleme
pc-eye director binds
pc-eye director use-bind --text "SUPER+Return"

# Kör sleep yerine deterministik çok-modlu bekleme (AT-SPI2 ve OCR destekli)
pc-eye wait visual_settle --timeout-ms 3000
pc-eye wait ui_match --match "Tamamlandı" --timeout-ms 5000
pc-eye wait port_open --match "8080" --timeout-ms 5000

# Canlı Web Gözlem Paneli (http://127.0.0.1:7777)
pc-eye serve --port 7777
```
