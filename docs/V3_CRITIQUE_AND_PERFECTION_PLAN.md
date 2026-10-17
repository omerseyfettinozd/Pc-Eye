# PC-Eye V2.1 -> V3.0: Acımasız Öz-Eleştiri, Derin Mimari İnceleme ve 10/10 Mükemmellik Planı

Bu doküman, mevcut **PC-Eye (`v0.2.1`)** sisteminin gerçek dünya yapay zeka ajan senaryolarında (LLM / VLM masaüstü kontrolü, hata ayıklama, oyun/Canvas otomasyonu, sistem izleme) karşılaşabileceği **tüm kör noktaları, mimari darboğazları ve eksiklikleri** acımasızca eleştirmek ve sistemi **9.2/10 seviyesinden 10/10 (Tam Kusursuzluk) seviyesine** taşıyacak **V3.0 Mühendislik Planını** tanımlamak için hazırlanmıştır.

---

## BÖLÜM 1: Mevcut Sistemin (`v0.2.1`) Acımasız Eleştirisi ve Kör Nokta Analizi

### Eleştiri 1: "AT-SPI2 Körlüğü" — Electron, Oyun ve Özel Arayüzlerde Semantik Çöküş
- **Mevcut Durum (`src/eyes/ui_atspi.rs` & `src/eyes/vision.rs`):**
  - `v0.2.1`, `org.a11y.Status.IsEnabled = true` ile GTK4, Qt6 ve Firefox pencerelerinde `AT-SPI2` ağacını okuyabiliyor.
  - **Ancak gerçek dünyada:** Linux masaüstündeki uygulamaların büyük bir kısmı (VS Code / Antigravity gibi Electron uygulamaları, Steam, oyunlar, PDF okuyucular, özel çizim/Canvas araçları) `AT-SPI2` ağacına ya hiçbir şey vermez (`element_count: 0`) ya da sadece dış çerçeveyi verir.
  - `v0.2.1` bu durumda `detect_visual_control_boxes` ile yüksek kontrastlı kutular buluyor ama **o kutuların içinde ne yazdığını bilmiyor!**
  - **Sonuç:** AI modeli `pc-eye ui` çağırdığında `0` eleman alıyor ve mecbur kalarak `pc-eye vision` ile 2560x1600 piksellik dev ekran görüntüsünü (binlerce Vision token'ı ve 3-5 saniye model gecikmesi) modele yüklemek zorunda kalıyor.
- **Nasıl Değiştirilmeli (V3 Çözümü — Sıfır-Disk Bellek-İçi OCR Semantik Köprüsü):**
  - `grim -t ppm -` ile alınan bellek içi görüntü, diske hiç yazılmadan doğrudan `tesseract stdin stdout --tessdata-dir ~/.local/share/pc-eye/tessdata -l eng+tur --psm 11 -c tessedit_create_tsv=1` boru hattına (pipe) verilmeli.
  - Çıkan TSV içindeki kelimeler (`level == 5`, `conf >= 40.0`) hem tekil kelime hem de aynı satırda yan yana duran **ifade/buton grupları** (`"New Conversation"`, `"File"`, `"Kaydet"`, `"İptal"`) olarak birleştirilmeli ve Hyprland pencere ofseti + monitör ölçeği (`scale`) ile çarpılarak gerçek ekran koordinatlarıyla (`screen_bounds`, `click_point`) `UiElement` (`role: "ocr-text"`) olarak döndürülmeli.
  - Böylece **AT-SPI2 hiç çalışmasa bile** AI modeli:
    1. `pc-eye ui` dediğinde penceredeki tüm yazıları ve koordinatlarını JSON olarak okur (0 görüntü token'ı!).
    2. `pc-eye click-ui --name "New Conversation"` dediğinde OCR ile metnin koordinatını bulup doğrudan tıklar!
    3. `pc-eye wait ui_match --match "Tamamlandı"` dediğinde oyun veya Canvas ekranında bile o yazının çıkmasını deterministik olarak bekler!

---

### Eleştiri 2: Terminal Pencerelerinde "Süreç Var Ama Ekran Metni Yok" Sorunu
- **Mevcut Durum (`src/eyes/ui_atspi.rs` -> `inspect_terminal_pty`):**
  - `v0.2.1`, bir `kitty` veya `alacritty` penceresine bakınca `/proc` üzerinden içinde çalışan komutu (`foreground_cmd`), PID'sini ve çalışma klasörünü (`cwd`) buluyor.
  - **Ancak eksiklik:** Terminalin içinde o an ekranda yazan çıktıyı (`cargo build` hatası, `python` traceback'i, `pacman` sorusu, `htop` ekranı) metin olarak okuyamıyor!
- **Nasıl Değiştirilmeli (V3 Çözümü — Çok-Katmanlı Terminal Ekran Okuyucu):**
  - `TerminalContext` yapısına `visible_lines: Vec<String>` alanı eklenmeli.
  - **Katman 1:** Eğer `kitty` uzaktan kontrol soketi (`/tmp/kitty*` veya `/run/user/1000/kitty*`) varsa `kitty @ get-text` ile doğrudan ham metin alınır.
  - **Katman 2:** Soket yoksa, sadece o terminal penceresinin koordinatları (`grim -g "x,y wxh" -t ppm -`) bellek-içi OCR satır birleştiricisinden geçirilerek terminal ekranındaki tüm satırlar `visible_lines` dizisi olarak JSON içinde döndürülür!

---

### Eleştiri 3: İki Çağrı Arasında Kaybolan Olaylar ve Çöken Süreçler (`coredumpctl` Eksikliği)
- **Mevcut Durum (`src/daemon/server.rs` & `src/eyes/logs_notify.rs`):**
  - `v0.2.1` yalnızca AI modeli bir komut çağırdığında o anki durumu okuyor. İki çağrı arasında bir uygulama açılıp `SIGSEGV` / `SIGABRT` ile 100 milisaniye içinde çökerse, AI modeli pencereyi hiç görmüyor ve neden çöktüğünü anlamıyor.
- **Nasıl Değiştirilmeli (V3 Çözümü — `pc-eye daemon` Kara Kutu Kaydedicisi + `coredumpctl` Entegrasyonu):**
  - **1. `coredumpctl` Çökme Röntgeni:** `logs_notify.rs` içine `read_coredump_entries` eklenmeli (`coredumpctl list --json=short -n 10`). Sistemde çöken her süreç (`SIGSEGV=11`, `SIGABRT=6`, `SIGBUS=7` vb.), çökme zamanı (`timestamp_ms`), PID'si, çalıştırılabilir dosya yolu (`exe`) ve sinyal adıyla birlikte doğrudan `pc-eye logs` ve `pc-eye context` içinde `source: "coredump"`, `level: "crit"` olarak raporlanmalı!
  - **2. Gerçek Zamanlı Arka Plan Daemon (`pc-eye daemon`):** Hyprland `.socket2.sock` olay soketini kesintisiz dinleyen, açılan/kapanan her pencereyi ve çalışma alanı değişimini anında `/run/user/1000/pc-eye/telemetry_ring.json` halka belleğine kaydeden ultra-hafif arka plan守护 (daemon) modu eklenmeli ve `pc-eye doctor` / `pc-eye context` çağrıldığında daemon çalışmıyorsa otomatik olarak arka planda (`spawn_background_daemon_if_needed`) başlatılmalı!

---

### Eleştiri 4: Pano (Clipboard) Körlüğü — Sadece Düz Metin Okuma
- **Mevcut Durum (`src/director/process_ctl.rs` -> `clipboard_action`):**
  - Sadece `wl-paste -n` ve `wl-copy` ile düz metin okuyor/yazıyor.
  - **Eksiklik:** Kullanıcının panosunda bir resim (`image/png`), kopyalanmış bir dosya (`text/uri-list`), tarayıcıdan kopyalanmış bir metnin kaynak URL'si (`chromium/x-source-url`) veya `cliphist` geçmişindeki önceki 10 kopyalama kaydı var ama AI bunların hiçbirini göremiyor! Ayrıca AI bir resmi veya dosyayı panoya kopyalayıp (`image/png` veya `text/uri-list`) bir uygulamaya `Ctrl+V` ile yapıştıramıyor.
- **Nasıl Değiştirilmeli (V3 Çözümü — Zengin Çok-Formatlı Pano ve `cliphist` Geçmişi):**
  - `ClipboardReport` yapısı oluşturulmalı:
    - `mime_types: Vec<String>` (`wl-paste --list-types`)
    - `current_text: Option<String>`
    - `source_url: Option<String>` (`wl-paste -t chromium/x-source-url`)
    - `has_image: bool` (`image/png` veya `image/jpeg` var mı)
    - `history: Vec<ClipboardHistoryItem>` (`cliphist list` üzerinden son 15 pano öğesi: ID ve özet)
  - Pano yazma (`clipboard` aksiyonu) genişletilmeli:
    - Metin yazma (`--text "..."`)
    - Dosya/Görsel panoya kopyalama (`--file "/path/to/img.png"` -> `wl-copy --type image/png < file` veya `text/uri-list`)
    - `cliphist` geçmişinden ID ile geri yükleme (`--restore-id 16`).

---

### Eleştiri 5: Ses Sisteminde "Sinyal Var Ama Kayıt Yok" Eksikliği
- **Mevcut Durum (`src/eyes/audio_pipewire.rs`):**
  - `v0.2.1`, 50ms'lik PCM örneklemeyle sesin fiziksel şiddetini (`rms_dbfs`, `peak_dbfs`) ve `MPRIS` bilgilerini veriyor.
  - **Eksiklik:** AI modeli bilgisayarda çalan sesi (örneğin bir uyarı sesini, videodaki konuşmayı veya oyundaki diyaloğu) bir `.wav` dosyası olarak kaydedip multimodal olarak dinleyemiyor.
- **Nasıl Değiştirilmeli (V3 Çözümü — Masaüstü Ses Yakalayıcı `record-wav`):**
  - `pc-eye audio --op record --arg 3000` (veya MCP `pc_eye_audio` içinde `op: "record"`, `arg: "3000"`) eklendiğinde, `@DEFAULT_AUDIO_SINK@` çıkışını belirtilen milisaniye boyunca `/run/user/1000/pc-eye/audio_capture.wav` dosyasına kaydedip dosya yolunu, süresini ve dBFS sinyal istatistiğini döndürmeli.

---

### Eleştiri 6: XWayland (`X11`) Pencerelerinde İç Özellik Körlüğü
- **Mevcut Durum (`src/eyes/compositor.rs`):**
  - `xwayland: true` bayrağını gösteriyor ama X11 penceresinin `WM_TRANSIENT_FOR` (alt diyalog mu?), `_NET_WM_STATE` (modal mı, sticky mi, demands_attention mı?) veya `WM_WINDOW_ROLE` özelliklerini okumuyor.
- **Nasıl Değiştirilmeli (V3 Çözümü — XWayland / X11 Özellik Zenginleştirici):**
  - XWayland pencereleri için `xprop` tabanlı hızlı özellik okuyucu (`inspect_xwayland_properties`) eklenerek pencere detayları zenginleştirilmeli.

---

## BÖLÜM 2: V3.0 Uygulama Planı (Adım Adım İcra)

| Faz | Hedef Modül | Yapılacak Mühendislik İşi | Doğrulama Yöntemi |
| :--- | :--- | :--- | :--- |
| **Faz 1** | `src/eyes/ocr.rs` *(Yeni Modül)* & `src/eyes/ui_atspi.rs` | Bellek-içi `PPM -> tesseract TSV` (`eng+tur`) OCR motoru, satır/ifade gruplayıcı (`phrase grouping`), HiDPI koordinat çözücü. `inspect_window_ui`, `click_ui_element_ext` ve `wait` (`ui_match`) içine otomatik OCR geri dönüşü (fallback) ve `--ocr` bayrağı entegrasyonu. | `Antigravity` (AT-SPI2 vermeyen Electron penceresi) üzerinde `pc-eye ui` çalıştırıp tüm buton/sekme metinlerinin koordinatlarıyla geldiğini doğrulama. |
| **Faz 2** | `src/eyes/ui_atspi.rs` (`inspect_terminal_pty`) | Açık terminal pencereleri için `kitty` soket okuyucu + pencere bazlı bellek-içi OCR satır çıkarıcı (`visible_lines: Vec<String>`). | Açık terminal üzerinde `TerminalContext.visible_lines` çıktısını test etme. |
| **Faz 3** | `src/daemon/server.rs` & `src/eyes/logs_notify.rs` | 1) `coredumpctl list --json=short` çökme dedektörünü `logs` ve `context` içine entegre etme (`source: "coredump"`).<br>2) `pc-eye daemon` arka plan `.socket2.sock` kesintisiz olay dinleyicisini ve otomatik başlatıcısını (`ensure_daemon_running`) yazma. | `pc-eye logs` içinde gerçek `dotnet` coredump kayıtlarının ve arka plan daemon durumunun (`daemon_running: true`) görünmesini doğrulama. |
| **Faz 4** | `src/director/process_ctl.rs` & `src/eyes/audio_pipewire.rs` | 1) Zengin Pano (`ClipboardReport`: `mime_types`, `source_url`, `has_image`, `cliphist` geçmişi, `--file` ile görsel/dosya kopyalama, `--restore-id`).<br>2) `pc-eye audio --op record --arg <ms>` ile masaüstü sesini `.wav` olarak yakalama. | `pc-eye director clipboard` ve `pc-eye audio --op record --arg 500` komutlarını canlı test etme. |
| **Faz 5** | `tests/integration_tests.rs`, `SKILL.md`, `README.md` | Yeni V3 yetenekleri için birim ve entegrasyon testlerini ekleme, `~/.local/bin/pc-eye` binary'sini ve `SKILL.md` dokümanını güncelleme, `0` uyarı ile git commit alma. | `cargo test` (tüm birim + entegrasyon testleri %100 başarılı). |
