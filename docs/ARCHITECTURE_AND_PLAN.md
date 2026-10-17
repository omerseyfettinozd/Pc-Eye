# PC-Eye (V2) — Tam Beyaz Kutu (Full White-Box) İşletim Sistemi Gözlemlenebilirlik ve Kontrol Mimarisi

> **Tarih:** 17 Ekim 2026  
> **Hedef Ortam:** Linux (CachyOS Kernel 7.2+) · Wayland (`Hyprland 0.56+`) · `PipeWire` · `systemd-journald` · `AT-SPI2`  
> **Dil ve Dağıtım:** Saf Rust (Tek Binary: `pc-eye` — Arka Plan Daemon + CLI + MCP Server + Canlı Web/SSE Sunucusu)  
> **Mimari Felsefe:** Rowl Eye'daki *"Full-Observability + Director QA"* mimarisinin işletim sistemi seviyesine uyarlanması.

---

## 1. Yönetici Özeti ve Problem Tanımı

Standart "Computer Use" (bilgisayar kullanan AI) araçları bilgisayara **siyah kutu (black-box)** veya en iyi ihtimalle **gri kutu (grey-box)** olarak yaklaşır: ekran görüntüsü alır, piksellerden buton koordinatı tahmin eder ve fare tıklaması gönderir. Arka planda hangi sürecin çöktüğünü, hangi ağ soketinin zaman aşımına uğradığını, hangi dosya kilidinin beklendiğini, ses çıkıp çıkmadığını veya pencere içi erişilebilirlik ağacının gerçek durumunu bilmez.

**PC-Eye**, işletim sistemini AI modelleri için **%100 Beyaz Kutu (White-Box)** haline getiren, düşük gecikmeli (`<5ms` semantik sorgu, `~5-10 MB` RAM) bir Rust çekirdeğidir.

---

## 2. Mimari Öz-Eleştiri ve V2 Tasarım Kararları (Kilitli Kurallar)

Canlı sistem (CachyOS + Hyprland 0.56.2) üzerinde yapılan ön testlerde tespit edilen **6 kritik tuzak** ve bunların V2 mimarisindeki kesin çözümleri:

| # | Tespit Edilen Tuzak / Hata | PC-Eye V2 Kesin Çözümü |
| :--- | :--- | :--- |
| **K1** | **AT-SPI2 Kapalı Gelme & `(0,0)` Koordinat Hatası:** Hyprland'de `org.a11y.Status.IsEnabled` varsayılan olarak `false` olduğu için Firefox ve Electron ağaç yayınlamıyor. Ayrıca Wayland pencereleri kendi ekran koordinatlarını bilmediği için AT-SPI `(0,0)` pencere-içi koordinat dönüyor. | Daemon başlangıcında D-Bus üzerinden `IsEnabled=true` ve `ScreenReaderEnabled=true` zorlanır. **Coordinate Space Resolver** katmanı AT-SPI'dan gelen göreli `(rel_x, rel_y)` koordinatlarını Hyprland IPC'deki pencere adresi/PID'si üzerinden `window.at + rel` ve `monitor.scale` ile gerçek ekran koordinatına dönüştürür. |
| **K2** | **Terminal & Canvas Körlüğü:** Terminaller (Kitty, Alacritty, Foot) ve GPU-Canvas uygulamaları AT-SPI ağacı üretmez; ekran görüntüsünden OCR yapmak ise hem yavaş hem hatalıdır. | **Üçlü Algı Hunisi (Tri-Modal Perception):** (1) GTK/Qt/Browser için AT-SPI2 ağacı, (2) Terminaller için doğrudan alt-süreç PTY/kabuk durumu ve `/proc/<pid>/fd` + pano/buffer okuma, (3) Canvas/Oyunlar için `grim`/Wayland screencopy + numaralı **Set-of-Marks** görsel kutulama ve `zoom` (bölgesel kayıpsız kırpma). |
| **K3** | **Tekil PID Yanılgısı:** Firefox veya Electron gibi çok süreçli uygulamalarda pencere PID'si sadece kök süreçtir; ağ bağlantısını veya hatayı alt süreçler (`child PID` / `cgroup`) üretir. | **Süreç Soy Ağacı & Cgroup Röntgeni:** Pencere PID'sinden yola çıkılarak tüm alt süreç ağacı (`children` + `/sys/fs/cgroup`) tek bir **"Uygulama Kümesi (App Cluster)"** olarak izlenir. Ağ bağlantıları yavaş `/proc/net/tcp` metninden değil, çekirdek **`NETLINK_SOCK_DIAG`**/hızlı `/proc/<pid>/fd` inode eşlemesiyle çekilir. |
| **K4** | **Ekran Yakalama Disk I/O Darboğazı:** Her piksel kontrolünde diske PNG yazmak SSD ve CPU'yu yorar. | **Bellek İçi (In-Memory) Görüntü & Pixel-Diff:** Görüntüler doğrudan bellek tamponuna (`stdout` pipe / `shm` / `$XDG_RUNTIME_DIR` tmpfs) alınır; iki kare arasındaki değişim (`perceptual hash` & `pixel_diff_ratio`) RAM'de **1–2 ms** içinde hesaplanır. |
| **K5** | **Açık Uçlu `SIGSTOP` Masaüstü Kilitlenmesi:** D-Bus, PipeWire veya Clipboard kilidi tutan bir GUI sürecini süresiz dondurmak tüm masaüstünü kilitleyebilir. | **Watchdog Korumalı Freeze & Denylist:** `Hyprland`, `pipewire`, `wireplumber`, `dbus-daemon`, `waybar`, `xdg-desktop-portal*` ve `pc-eye` kesin yasaklı listededir (asla dondurulamaz). Hedef süreç dondurma işlemi varsayılan olarak `<100ms` atomik snapshot alır veya zorunlu `max_freeze_ms` (ör. maks 5000ms) otomatik çözücü (`SIGCONT` watchdog) ile çalışır. |
| **K6** | **Token Tufanı (Context Bloat) & Ayna Etkisi:** Tüm ağacı ve süreçleri her seferinde dökmek 20.000+ token harcar; AI kendi kontrol paneline veya şifre ekranına tıklayabilir. | **Monotonik `seq` ve Delta (Diff) Protokolü + Auth Guard:** Her olay monotonik bir `seq` numarası alır; `seq_since` parametresiyle AI sadece son adımdan beri değişenleri alır. `polkit`/`pinentry`/`hyprlock` aktifken girdi reddedilir; AI'nın kendi penceresi (`antigravity` vb.) korumalı (`protected`) işaretlenir. |

---

## 3. Sistem Mimarisi ve Modül Yapısı

Proje `/home/chaple/Belgeler/Pc-Eye` altında modüler bir Rust çalışma alanı (Cargo paketi) olarak inşa edilecektir:

```text
/home/chaple/Belgeler/Pc-Eye/
├── Cargo.toml
├── docs/
│   └── ARCHITECTURE_AND_PLAN.md
├── src/
│   ├── main.rs                 # CLI + Daemon + MCP stdio + HTTP/SSE giriş noktası
│   ├── protocol/               # Ortak JSON şemaları, seq/delta tipleri, hata sözleşmeleri
│   │   ├── mod.rs
│   │   └── types.rs
│   ├── daemon/                 # Arka plan servisi, UNIX soket sunucusu ve durum yöneticisi
│   │   ├── mod.rs
│   │   ├── server.rs           # $XDG_RUNTIME_DIR/pc-eye/daemon.sock + HTTP/Web
│   │   └── ring_buffer.rs      # Sabit boyutlu (Zero-Alloc) Halka Bellek yapısı
│   ├── eyes/                   # 5 Gözlemlenebilirlik Katmanı (Gözler)
│   │   ├── mod.rs
│   │   ├── compositor.rs       # Göz 1: Hyprland IPC (.socket.sock & .socket2.sock event stream)
│   │   ├── ui_atspi.rs         # Göz 2a: AT-SPI2 D-Bus ağacı + Coordinate Space Resolver
│   │   ├── vision.rs           # Göz 2b: In-memory ekran/pencere/bölge/zoom yakalama + Set-of-Marks + Pixel-Diff
│   │   ├── process_xray.rs     # Göz 3: Süreç ağacı, FD, Socket/Port, 60sn CPU/RAM/IO p50/p95/p99
│   │   ├── logs_notify.rs      # Göz 4: journald + dmesg + D-Bus bildirimler + yönetilen süreç stderr
│   │   └── audio_pipewire.rs   # Göz 5: PipeWire düğümleri, aktif ses akışları, ses/mikrofon durumu
│   ├── director/               # Kontrol, Girdi Enjeksiyonu ve Güvenlik Katmanı
│   │   ├── mod.rs
│   │   ├── input.rs            # Fare (Hyprland movecursor + Wayland click/scroll) & Klavye (XKB/wtype/hyprctl)
│   │   ├── process_ctl.rs      # Uygulama başlatma (openwindow takipli), güvenli Freeze/Thaw, sinyal yönetimi
│   │   ├── sequence.rs         # Atomik çok-adımlı aksiyon yürütücü + beklenmedik durum kesicisi
│   │   └── guard.rs            # Auth-Guard (polkit/pinentry), Protected-Window ve Denylist denetimi
│   ├── mcp/                    # Model Context Protocol (JSON-RPC 2.0 stdio) sunucusu
│   │   └── mod.rs
│   └── web/                    # Canlı Gözlem Paneli (SSE + Gömülü Tek-Sayfa Dashboard)
│       └── mod.rs
└── tests/                      # Entegrasyon, sözleşme ve halka bellek birim testleri
```

---

## 4. Veri Akışı ve Halka Bellek (Ring Buffer) Tasarımı

Arka planda çalışan `pc-eye daemon`, hiçbir AI çağrısı yokken bile çok düşük maliyetle şu **4 Halka Belleği (`RingBuffer<T>`)** canlı tutar:

1. **`SystemTelemetryRing` (Son 120 örnek — 500ms aralıkla 60 saniye):**
   - Sistem geneli CPU, RAM, Swap, LoadAvg, PSI (Pressure Stall Information — `/proc/pressure/{cpu,memory,io}`).
   - Açık her pencerenin süreç kümesi (`AppCluster`) için CPU %, RSS RAM (MB), Okuma/Yazma I/O hızı ve thread sayısı.
   - **Otomatik Hesaplanan Metrikler:** `p50`, `p95`, `p99` değerleri ve anlık sıçrama (`spike_detected: true`) bayrakları.
2. **`CompositorEventRing` (Son 512 Hyprland olayı):**
   - Hyprland `.socket2.sock` üzerinden gerçek zamanlı akan `openwindow`, `closewindow`, `activewindow`, `workspace`, `Screencast`, `urgent` olayları (`seq` numaralı ve milisaniye damgalı).
3. **`LogAndNotificationRing` (Son 1000 kayıt):**
   - `systemd-journald` (`PRIORITY <= 4` uyarı/hata veya pencere PID'leriyle eşleşen loglar), D-Bus masaüstü bildirimleri ve `pc-eye launch` ile başlatılan uygulamaların `stdout/stderr` satırları.
4. **`FrameHistoryRing` (Son 8 karenin hafif 64-bit dHash özeti + son tam kare):**
   - Ekranın görsel olarak durulup durulmadığını (`is_visual_stable`) ve bir tıklamanın ekranda ne kadar görsel değişim yarattığını (`visual_change_pct`) anında ölçer.

---

## 5. AI Araç Sözleşmesi (MCP & CLI Komut Seti)

Hem MCP sunucusu hem de CLI (`pc-eye <komut>`) birebir aynı 12 temel aracı sunar. Tüm aksiyon araçları `then="context" | "ui" | "screenshot" | "xray"` parametresi alarak **tek turda (single round-trip)** hem aksiyonu yapar hem sonucu döner:

### A. Gözlem Araçları (Read-Only)
1. **`pc_eye_context` (`pc-eye context [--seq-since N]`):**
   - Tek çağrıda token-dostu (~300–600 token) tam sistem özeti: Monitörler, çalışma alanları, pencereler (adres, başlık, PID, geometri), odaktaki pencere, sistem yükü/PSI özeti, aktif ses çıkaran uygulamalar ve `seq_since` verilmişse sadece o zamandan beri değişen olaylar/loglar.
2. **`pc_eye_ui` (`pc-eye ui [--window ADDR] [--query METİN]`):**
   - Hedef pencerenin AT-SPI2 erişilebilirlik ağacını okur, ekran koordinatlarını Hyprland ofsetiyle düzelterek tıklanabilir/yazılabilir elemanları (`id`, `role`, `name`, `value`, `states`, `screen_bounds: [x,y,w,h]`) döner. Terminal ise terminal içeriğini/durumunu döner.
3. **`pc_eye_vision` (`pc-eye vision [--window ADDR | --region X,Y,W,H | --zoom X,Y] [--marks]`):**
   - Tam ekran, tek pencere veya kırpılmış bölge görüntüsü alır. `--marks` açıkken tıklanabilir UI elemanlarının üzerine numaralı kutucuklar (**Set-of-Marks**) çizer ve JSON lejantını döner. Önceki kareyle `pixel_diff_pct` farkını raporlar.
4. **`pc_eye_xray` (`pc-eye xray --window ADDR | --pid PID`):**
   - **Derin Arka Plan Röntgeni:** Pencerenin arkasındaki süreç ağacını (`cgroup` + alt PID'ler), son 60 saniyelik CPU/RAM/IO `p50/p95/p99` istatistiklerini, açık dosyalarını (`fd`), ağ bağlantılarını (TCP/UDP port ve hedef IP), thread durumlarını (`R/S/D/Z`) ve o sürece ait son log/hata satırlarını döner.
5. **`pc_eye_logs` (`pc-eye logs [--pid PID] [--since-seq N] [--level warn|err]`):**
   - Halka tamponundaki uygulama `stderr` çıktılarını, `journald` hatalarını ve masaüstü bildirimlerini filtreleyerek döner.
6. **`pc_eye_audio` (`pc-eye audio`):**
   - PipeWire ses grafiğini, hangi uygulamanın ses çaldığını veya mikrofon kullandığını, ses seviyesini/durumunu döner.

### B. Kontrol ve Yönetmenlik Araçları (Director & Action)
7. **`pc_eye_click_ui` (`pc-eye click-ui --name "Kaydet" | --mark 4 [--window ADDR] [--then ui]`):**
   - Piksel tahmin etmeden doğrudan UI elemanının adına, rolüne veya Set-of-Marks numarasına tıklar.
8. **`pc_eye_input` (`pc-eye input <click|move|drag|scroll|type|key> ... [--then context]`):**
   - Milimetrik koordinat tıklaması, sürükle-bırak, tekerlek kaydırma, Unicode-güvenli metin yazma ve kısayol tuşu gönderme.
9. **`pc_eye_window` (`pc-eye window <focus|move|resize|close|workspace|fullscreen|float> ...`):**
   - Hyprland IPC üzerinden milisaniyelik pencere ve çalışma alanı yönetimi.
10. **`pc_eye_launch` (`pc-eye launch [--workspace N] -- <komut>`):**
    - Uygulamayı başlatır, `stdout/stderr` akışını kendi `LogRingBuffer` tamponuna bağlar, Hyprland `openwindow` olayını bekler ve yeni pencerenin adresini + PID'sini döner.
11. **`pc_eye_wait` (`pc-eye wait <window_open|window_close|title_change|visual_settle|log_match> ...`):**
    - Rastgele `sleep` beklemek yerine Hyprland soket olayını, ekran piksellerinin durulmasını veya logda belirli bir satırın belirmesini deterministik olarak bekler.
12. **`pc_eye_director` (`pc-eye director <sequence|freeze|thaw|clipboard> ...`):**
    - Çok adımlı atomik aksiyon dizisi (`sequence`), Watchdog korumalı süreç dondurma/çözme (`freeze`/`thaw`) ve pano (`clipboard`) kontrolü.

---

## 6. Fazlı Uygulama Planı (Adım Adım İnşa ve Doğrulama)

Her faz tamamlandığında gerçek sistem üzerinde otomatik testlerle (`cargo test` + canlı Hyprland duman testleri) doğrulanacaktır:

### Faz 1: Çekirdek İskelet, Protokol, Halka Bellek ve Hyprland IPC Gözü (`Göz 1`)
- Cargo projesinin oluşturulması (`tokio`, `serde`, `serde_json`, `clap`, `anyhow`, `libc`, `nix`).
- Jenerik, thread-safe `RingBuffer<T>` ve monotonik `seq` / `delta` altyapısının kurulması (`src/daemon/ring_buffer.rs`).
- Hyprland `.socket.sock` (komut/sorgu) ve `.socket2.sock` (canlı olay akışı) istemcisinin yazılması (`src/eyes/compositor.rs`).
- **Doğrulama Kapısı 1:** `pc-eye context` komutunun açık pencereleri, monitörleri, aktif pencereyi ve imleç konumunu `<5ms` içinde JSON olarak döndürmesi; birim testlerinin %100 yeşil geçmesi.

### Faz 2: Derin Arka Plan Röntgeni (`Göz 3`), Log/Bildirim (`Göz 4`) ve Ses Gözü (`Göz 5`)
- `/proc` okuyucu ve `AppCluster` (kök PID + alt süreç ağacı + `cgroup`) toplayıcısının yazılması (`src/eyes/process_xray.rs`).
- 60 saniyelik CPU/RAM/IO/PSI `p50/p95/p99` halka telemetri hesaplayıcısının, açık dosya (`fd`) ve aktif ağ soketi (TCP/UDP/UNIX) çözücüsünün eklenmesi.
- `journalctl` JSON akış okuyucusu, yönetilen süreç `stdout/stderr` yakalayıcısı (`src/eyes/logs_notify.rs`) ve `pw-dump` / `wpctl` tabanlı PipeWire ses/medya gözünün (`src/eyes/audio_pipewire.rs`) entegrasyonu.
- **Doğrulama Kapısı 2:** `pc-eye xray --pid <firefox_pid>` çağrıldığında Firefox'un tüm alt süreçlerinin, açık ağ bağlantılarının, son loglarının ve `p50/p95/p99` bellek/CPU metriklerinin eksiksiz gelmesi.

### Faz 3: Semantik UI Ağacı (`Göz 2a`) ve Görsel Göz + Set-of-Marks (`Göz 2b`)
- D-Bus üzerinden `org.a11y.Status.IsEnabled = true` aktivasyonu ve AT-SPI2 ağaç okuyucusunun (`src/eyes/ui_atspi.rs`) yazılması.
- **Coordinate Space Resolver:** AT-SPI pencere-içi koordinatlarının Hyprland pencere geometrisiyle birleştirilmesi + terminal pencereleri için PTY/süreç durumu algılayıcısı.
- Bellek içi görüntü yakalama (`grim` / toplevel yakalama), hızlı `dHash` / `pixel_diff` hesaplama, bölgesel `zoom` ve saf Rust (`image` + `imageproc` / `ab_glyph`) ile harici ImageMagick'e bile ihtiyaç duymadan görüntü üzerine numaralı **Set-of-Marks** kutucukları çizilmesi (`src/eyes/vision.rs`).
- **Doğrulama Kapısı 3:** `pc-eye ui` ve `pc-eye vision --marks` komutlarının hem koordinat listesini hem de işaretlenmiş görüntüyü hatasız üretmesi.

### Faz 4: Girdi Enjeksiyonu, Director Kontrolleri ve Güvenlik Duvarı
- Hyprland `movecursor` + Wayland sanal işaretçi/klavye (veya `hyprctl` / `wtype` / native uinput/wayland wire) ile tıklama, sürükleme, kaydırma ve Türkçe/Unicode güvenli metin yazma (`src/director/input.rs`).
- `pc-eye launch` (pencere açılana kadar bekleyen ve `stderr` yakalayan başlatıcı), `pc-eye wait` (olay/görsel durulma bekleyici), `sequence` ve Watchdog korumalı `freeze`/`thaw` (`src/director/process_ctl.rs`).
- `AuthGuard` (`polkit`/`pinentry` koruması) ve `ProtectedWindow` (`antigravity` / `pc-eye` öz-koruma) güvenlik katmanı (`src/director/guard.rs`).
- **Doğrulama Kapısı 4:** İzole bir test penceresini başlatma, içindeki UI elemanını bulup tıklama, çıktısını/logunu okuma ve kapatma döngüsünün uçtan uca otonom çalışması.

### Faz 5: Daemon, MCP Sunucusu ve Canlı Web Gözlem Paneli
- Arka plan servisi (`pc-eye daemon`), hızlı CLI istemcisi, standart **MCP (`stdio` JSON-RPC 2.0)** sunucusu (`src/mcp/mod.rs`) ve Antigravity/Claude entegrasyonu.
- Tarayıcıdan açıldığında AI'nın bilgisayarı nasıl gördüğünü (canlı pencere haritası, halka bellek grafikleri, akan loglar, ses barları ve son Set-of-Marks görüntüsü) gerçek zamanlı gösteren gömülü **Web Dashboard (`http://127.0.0.1:7777`)**.
- **Doğrulama Kapısı 5:** Tüm birim ve entegrasyon testlerinin %100 yeşil geçmesi, MCP `tools/list` ve `tools/call` sözleşme testlerinin doğrulanması.
