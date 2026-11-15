# Pc-Eye V4 Mimarisi: Hız × Kalite × Maliyet, En Uç Hedef

Durum: öneri (2026-11-01). Hedef: hız, kalite ve maliyetin üçünde birden piyasadaki her
bilgisayar kullanan ajandan iyi olmak. Bu iddia ancak aynı görevlerde, rakiplerle yan yana
ölçülünce söylenir (bkz. §7). Her faz ölçüm setiyle kabul edilir.

## 1. Yaklaşım: yeniden yazma değil, sıcak yolu söküp takma

Ölçüm, yavaşlığın tasarımın geneline değil birkaç sıcak noktaya yığıldığını gösterdi.
Bugün düzeltilen hatalar (GTK4 koordinatı, OCR önbelleği, coredump sırası, uygulama
karışması…) ölçüm setiyle kanıtlı; sıfırdan yazmak bunları yeniden üretir. Sıcak yol
parça parça değiştirilir. Ölçüm seti her parçanın hem hızı artırdığını hem de bir şeyi
bozmadığını kanıtlar.

| Sıcak nokta (2026-11-01 ölçümü) | Şimdi | Kök neden |
| --- | --- | --- |
| AT-SPI ağacı | GTK ~400 ms, Firefox 1,4–1,9 s (85 düğümde kesilip sayfa içeriğine ulaşamıyor) | Özellik başına ayrı `busctl` süreci, 3,6 ms × yüzlerce |
| OCR | 1200×600: 1,2 s; Electron penceresinin snapshot'ı: 5,8 s | Her çağrıda CPU `tesseract` süreci, tüm pencere |
| Ekran yakalama | 36 ms | Her çağrıda `grim` süreci |
| Ajan taban maliyeti | görev başı ~18K girdi token | Claude Code sistem istemi + 3,8K token araç şeması |
| Tarayıcı | sayfa içeriği yok | Masaüstü erişilebilirlik ağacı web için yanlış kanal |
| Oturum paylaşımı | imleç, odak ve klavye kullanıcıyla ortak | Ajan, kullanıcının Hyprland oturumunda sanal bir çıkışta çalışıyor |

## 2. Hedefler

| Metrik | Şimdi | Hedef |
| --- | --- | --- |
| Durum okuma (snapshot) p50 | 0,4–1,9 s | < 20 ms (bellekten) |
| Eylem + doğrulama | ~0,4–1 s | < 100 ms (kare bazlı "uygulama durdu" sinyali) |
| Pencere OCR | 1,2–5,8 s | < 100 ms GPU, yalnız değişen karolar |
| Tarayıcı sayfa özeti | yok | < 30 ms, ~1–2K token |
| Rutin görev başı LLM çağrısı | 2–9 tur | ≤ 1 (tarifli görevde 0) |
| Rutin görev maliyeti | $0.035 | < $0.002 |
| Başarı | 7 görevde %100 | OSWorld alt kümesi + kendi zor görevlerimizde rakiplerin üstünde |
| Kullanıcıya etki | ortak imleç ve odak | sıfır: ayrı oturum, paralel ajanlar |

## 3. Katmanlar

```
 ┌ Ajan çalışma zamanı ─────────────────────────────────────────────────────────┐
 │ tarif oynatıcı (0 LLM) → yerel küçük model (yönlendirme, basit adım, $0)      │
 │ → bulut modeli: PLANLA (tek çağrı, beklentili eylem programı)                  │
 │ → yerel ÇALIŞTIR + DOĞRULA → yalnız sapmada yeniden planla                     │
 │ MCP adaptörü: Claude Code / Codex için                                         │
 └──────────────────────────────────────────────────────────────────────────────┘
              │ tek JSON-RPC protokolü (unix soketi, ~1 ms)
 ┌ Dünya modeli: pc-eyed (olay güdümlü, bellekte) ──────────────────────────────┐
 │ erişilebilirlik: zbus + AT-SPI Cache + olay aboneliği                          │
 │ tarayıcı: Firefox eklentisi → DOM özetleri (değişimde itilir)                  │
 │ görüntü: kompozitörden doğrudan tampon + hasar → karo OCR (GPU) + yerel VLM    │
 │ arka plan: journald akışı, coredump, inotify, systemd/NM, PipeWire             │
 └──────────────────────────────────────────────────────────────────────────────┘
              │
 ┌ Oturumlar ────────────────────────────────────────────────────────────────────┐
 │ Kullanıcı: günlük Hyprland (dokunulmaz; yalnız kullanıcının oturumu gereken     │
 │            işler: kendi Firefox'u, açık belgeleri)                              │
 │ Ajan(lar): pc-eye-wm, ajana özel Rust (Smithay) kompozitörü, oturum başına      │
 │            ayrı imleç/odak/klavye, istenirse izlemek için pencere içinde         │
 └──────────────────────────────────────────────────────────────────────────────┘
```

### 3.1 pc-eye-wm: ajana özel kompozitör (en uç parça)

Hazır kompozitörler insan için tasarlanmış. Ajan onlara dışarıdan, protokol protokol
bağlanıyor: screencopy, sanal fare, IPC. pc-eye-wm, Smithay ile yazılan ve ajanı
birinci sınıf kullanıcı sayan bir Wayland kompozitörü (+ XWayland):

- **Kopyasız görüntü**: her yüzeyin tamponuna ve hasar bölgesine doğrudan erişir. Ekran
  yakalama diye ayrı bir adım kalmaz, OCR ve doğrulama değişen pikselleri anında alır.
- **Kare doğruluğunda bekleme**: kompozitör hangi istemcinin ne zaman kare gönderdiğini
  bilir. "Uygulama durdu", piksel yoklamasıyla değil commit/frame-callback sessizliğiyle
  anlaşılır. `visual_settle` tahmin olmaktan çıkar.
- **Girdiyi kaynağında verme**: sanal fare ve klavye protokolleri gerekmez. Olay doğrudan
  hedef yüzeye verilir, imleç kullanıcınınkinden bağımsızdır.
- **Oturum başına yalıtım**: her ajanın kendi çıkışı, odağı, panosu ve AT-SPI veri yolu.
  Birden çok ajan paralel çalışır, kullanıcıyı hiç etkilemez. Ajanın başlattığı uygulamalar
  isteğe bağlı olarak bubblewrap ile kum havuzuna alınabilir.
- **Çözünürlük bağımsızlığı**: uzun bir sayfa tek seferde 1920×4000 yüzeye çizilip okunabilir.
- **İzleme**: kullanıcı ajanın oturumunu Hyprland'de bir pencere olarak izleyebilir veya devralabilir.

Ara basamak (F4): pc-eye-wm hazır olana dek ajan oturumu, ayrı bir headless Hyprland
veya Sway örneğidir. Yalıtım kazancının çoğunu hemen verir; pc-eye-wm onu sonradan yerinden eder.

Kullanıcının oturumu Hyprland'de kalır. Kompozitör katmanı bir arayüzün arkasına alınır;
Sway/KDE arka uçları sonradan eklenebilir.

### 3.2 Dünya modeli daemon'u

- **Erişilebilirlik**: `busctl` yerine `zbus`. Ağaç `org.a11y.atspi.Cache.GetItems` ile tek
  çağrıda alınır (GTK4, Qt ve at-spi2-atk köprüsünde var; uygulama bazında doğrulanacak).
  Konumlar eşzamanlı `GetExtents` ile gelir. `state-changed`, `children-changed` ve
  `text-changed` olaylarıyla ağaç yerinde güncellenir. Okumada düğüm sınırı yoktur;
  sınır snapshot'ın token bütçesinde uygulanır.
- **Tarayıcı**: §3.3.
- **Görüntü ve OCR**: pencere karolara bölünür, yalnız hash'i değişen karolar okunur ve
  karo bazında önbelleğe alınır. Arka uç sırası: GPU ONNX (RTX 5070) → `ocrs` (saf Rust
  CPU) → tesseract. Metin kanalları (AT-SPI, DOM, OCR) boş kaldığında ikon/canvas için
  yerel bir görsel grounding modeli (UI-TARS / OmniParser sınıfı, 8 GB VRAM'e sığan
  nicemlenmiş sürüm) devreye girer. Marjinal maliyeti 0'dır. AGPL bileşenler gömülmez,
  ayrı süreç olarak çalışır.
- **Arka plan**: `journalctl` süreci yerine journal'ı canlı izleme. coredump, inotify,
  systemd birim durumu, NetworkManager ve PipeWire sinyalleri aynı `seq` halka belleğine
  akar. İki çağrı arasında olay kaybolmaz.

### 3.3 Tarayıcı: Firefox eklentisi + native messaging

Firefox kalır. `Pc-Eye Bridge` eklentisi her sekmede aria-snapshot benzeri bir özet tutar:
`[e12] button "Gönder"`, `[e7] textbox "Ad Soyad" = ""`, tablolar düz satırlar halinde.
Özet `MutationObserver` ile değişimde güncellenip daemon'a itilir. Eylemler numara (`ref`)
iledir: tıkla, yaz, seç, kaydır, sekme işlemleri.

Aynı eklenti iki yerde çalışır:
- kullanıcının günlük Firefox'unda: giriş yapılmış oturumlar,
- ajan oturumundaki ayrı profilde: yalıtılmış işler.

Güvenlik: varsayılan salt okuma; banka ve şifre yöneticisi siteleri yasak listesinde;
şifre ve kart alanları özete girmez; her eylem deftere yazılır.

Dağıtım: AMO'da listelenmeyen ücretsiz imza; geliştirmede `about:debugging` ile geçici yükleme.
BiDi, ajan profili için isteğe bağlı ikinci yoldur.

### 3.4 Sorgu/eylem API'si

- **Delta snapshot**: `snapshot(seq_since)` yalnız değişen pencereleri ve elemanları döner.
- **Beklentili eylem**: `click(ref, expect={text|state|window|url})`. Daemon koşulu
  olaylardan bekler ve sonucu aynı çağrıda döner.
- **Eylem programı**: beklentili eylem dizisi tek çağrıda yürütülür (UFO²'nin spekülatif
  çoklu eylemi). İlk sapmada durur ve o anki delta durumu döner. Dallanma (`if-visible`)
  ve tekrar (`for-each-row`) desteklenir. Model bir kez planlar, çalıştırma yereldedir.
- **Kompakt şema**: araç tanımları kısa tutulur (bugün 3,8K token); ayrıntı SKILL.md'de.

### 3.5 Ajan çalışma zamanı: planla, yerelde çalıştır, doğrula

Maliyeti rakiplerin altına indiren katman. Rakiplerin çoğu her adımda modele ekran
görüntüsü gönderip bir sonraki tıklamayı sorar: N adım, N büyük çağrı. Biz:

1. **Tarif eşleşmesi** (Agent Workflow Memory): başarılı geçmiş yürütmelerden değerleri yer
   tutucuya çevrilmiş iş akışları. Eşleşen görev modele hiç gitmeden programla oynatılır.
2. **Yerel model**: RTX 5070'te küçük bir dil modeli niyet sınıflandırma, tarif seçme ve
   basit tek adımlı işleri yapar. Maliyeti $0'dır.
3. **Bulut planlayıcı**: kalan işte model kompakt delta durumu görüp tek çağrıda bir eylem
   programı yazar. Ucuz model (Haiku) önce denenir.
4. **Yerel yürütücü**: program beklentilerle çalışır. Sapma olursa yalnız sapma bağlamıyla
   yeniden planlanır; iki sapmadan sonra güçlü modele yükseltilir.
5. **İstem önbelleği**: kısa, sabit sistem istemi + araç şeması önbellekte durur.
   Görev başı taban 18K'dan ~2–3K'ya iner.

Başarılı her yürütme tarif adayıdır. Sistem kullanıldıkça ucuzlar ve hızlanır.

### 3.6 Güvenlik

Ajan varsayılan olarak kendi yalıtılmış oturumunda çalışır, kullanıcının oturumuna yalnız
gerektiğinde ve görünür biçimde dokunur. Diğer önlemler:
- AuthGuard ve korumalı pencere (var),
- eylem sınıfları: okuma / geri alınabilir / geri alınamaz,
- geri alınamaz eylemde onay,
- kalıcı eylem defteri,
- tarayıcı site yasak listesi,
- MCP kabuk kısıtı (var).

## 4. Fazlar

Sıra, "kazanç ÷ emek" ve bağımlılıklara göredir. Her faz, ölçüm setinde hedef metriği
tutturup gerileme yoksa kabul edilir ve kendi yeni ölçüm görevlerini getirir.

| Faz | İş | Kabul ölçütü |
| --- | --- | --- |
| F0 ✅ | Ölçüm seti + 9 hata düzeltmesi + snapshot (bugün) | ajan %100, $0.74 / 21 görev |
| F1a ✅ | `zbus` + seviye seviye paralel okuma, görünmeyen alt ağaç eleme, belge önceliği, tarayıcı belge beklemesi | GTK 298→32 ms, Firefox 1311→40 ms, sayfa içeriği görünür; ajan 27/27 |
| F1b ✅ | Olay aboneliği + olay/eylem dönemi/yaş ile geçersiz kılınan önbellek; daemon sorgu soketi (build + oturum kontrolü); delta snapshot | CLI snapshot ~8 ms, MCP tekrar ~6 ms, değişmeyen delta 136 bayt. Kalan: ajan delta'yı kendiliğinden kullanmıyor → kararlı mark numaraları + otomatik delta F2'ye; GTK4 etiket değişiminde olay yayınlamıyor (eylem dönemiyle kapatıldı) |
| F2 ✅ | Eylem programı (`pc_eye_run`, expect/gone), kararlı mark'lar, MCP otomatik delta, kısa şema | Form 9–11 tur → 4 tur (hedef ≤2 tutmadı: ajan önce snapshot alıyor, sonra Gönder'i ayrı tıklıyor); şema 12,9 → 8,8 KB (~2,2K token, hedef 1,5K tutmadı); toplam $1.11 → $0.74 |
| F3 ✅ | Haiku ölçümü, tarif hafızası (kayıt, snapshot önerisi, `pc_eye_run` / `pc-eye recipe run` ile oynatma), Haiku alt ajanı (`agents/pc-eye.md`) | Haiku 30/30, $0.063 (Sonnet F2: 26/27, $0.74); rutin görev $0.003–0.010 (hedef < $0.005 metin görevlerinde tuttu, formlarda ~$0.008); tarifli görev CLI'de 0 LLM, ajanda 1 çağrı. Yerel model yönlendirmesi ve istem tabanının küçültülmesi F6'ya kaldı |
| F4 ✅ | Ajan oturumu v1: kullanıcının oturumundaki görünmez headless çıkışta iç içe Hyprland (yalnız headless arka uç Hyprland'de yok); `pc-eye session start|stop|list`, `PC_EYE_SESSION` (CLI/MCP/daemon, ilk çağrıda otomatik açılış), oturuma özel çalışma dizini | açılış ~0,5 s; 2 paralel ajan 60/60, kullanıcı imleci/odağı 0/60 değişti; X11-only uygulamalar oturumda açılmaz (DISPLAY temizlenir) |
| F4.5 ✅ | Zor görev seti (8) + rakip kıyası (`--toolset pixels`, `LEADERBOARD.md`) + gerçek site (YouTube) + medya durumu + oturum ses yalıtımı + adlı çalışma alanları | pc-eye+Haiku %100 $0.010 / pc-eye+Sonnet %100 $0.067 / piksel+Sonnet %86 $0.096; aynı build adil kıyas: Haiku 21,7 s / Sonnet 24,2 s'e karşı piksel 18,9 s (piksel %90, $0.075); 20+ algı/eylem hatası düzeltildi; kalan hız farkı Dolphin/sihirbaz/Kate |
| F5 ✅ | Firefox eklentisi (`extension/`) + native host + `pc_eye_browser` (ref ya da ad ile işlem, tek çağrılık `run`, gerçek imleç yedeği, arka plan sekmesi, XPI paketi); ölçüm setinde `--browser bridge` kipi | sayfa özeti 15–39 ms (hedef < 30 ms; AT-SPI 1,5–2,5 sn idi); probe: form 2858 → 824 ms, uzun liste 143 → 30 ms; ajan (Haiku, 6 tarayıcı görevi): 18/18 %100, medyan 9,0 s (AT-SPI 11,9 s), aynı maliyet. AMO imzalı 0.1.1 günlük Firefox'a kalıcı kurulu (2026-11-15); `run` aynı sayfada yalnız değişen satırları döner (`after: diff|full|none`; küçük formlarda token nötr). Kalan: `file://` desteklenmez |
| F6a ✅ | Süreç içi tesseract (dlopen, sıcak motor havuzu) + örtüşen paralel bantlar + bant önbelleği | 1600x900 metin yoğun sayfa 2610 → 720 ms, değişmeyen 2 ms; probe 29/29, terminal-read 1202 → 363 ms. Tesseract tavanı ~0,7 sn: < 100 ms için sinir ağı OCR'ı gerekir |
| F6b ✅ | PP-OCRv5 sinir ağı OCR'ı (ONNX Runtime CPU, dinamik yükleme; daemon + MCP'de), kutu önbelleği, Türkçe-güvenli ad eşleştirme. **Yerel dil/görsel modeller iptal** (2026-11-15, kullanıcı kararı): ucuz bulut modeli (Haiku ~$0.003–0.005/görev, %100) zaten yeterli; 8 GB VRAM'e sığan modeller güvenilirlik katmaz | ilk okuma ~610 ms, metin aynıysa ~70 ms, bölge değişince ~165 ms (< 100 ms hedefi tüm pencere ilk okumada tutmadı; GPU yürütücüsü CUDA kütüphaneleri ister — **ertelendi**: CUDA 12 + cuDNN + onnxruntime-gpu ~1,5–2 GB indirme, kullanıcı internet düzelince onaylayacak). Ajan (Haiku): OCR görevleri 12/12, ayarlar 34,1 → 21,1 s, sihirbaz 23,7 → 21,5 s; **GPU (2026-11-15):** CUDA 13 + cuDNN 9 (pip tekerlekleri, ~1,7 GB) ile sıcak okuma 78 → 20 ms, sayfa değişince 121 → 70 ms; açılış ~3,9 sn arka planda ısınır; ajan OCR görevleri 12/12 (uçtan uca süre model turlarına bağlı, değişmedi) |
| F7 ◐ | pc-eye-wm (Smithay, `wm/`): tamponlar commit anında okunur, kare sessizliğiyle bekleme, kaynağında girdi; pc-eye Hyprland IPC'sini taklit eden `wm_backend` ile değişmeden çalışır (`--backend wm`) | wm IPC: tıkla + bekle + yakala 88 ms (hedef < 100 ✅); yakalama 15 → 4 ms; ölçüm seti wm 31/31 (Hyprland oturumu 31/31), çoğu görev 1,1–1,5x hızlı, kullanıcıya sızıntı 0; GTK4/Qt/Firefox/kitty çalışıyor. Ajan (Haiku, 20 görev × 3): wm 60/60, 994 sn, 5,98 M token, $0,239 — Hyprland oturumu 60/60, 1279 sn, 5,48 M, $0,233 (süre farkının çoğu indirme beklemesi görevi; o hariç ~%10; maliyet eşit). **XWayland** ✅ (X11 GTK formu: tıklama, Türkçe yazım, gönder). **Varsayılan arka uç** ✅ (2026-11-15; `PC_EYE_BACKEND=hyprland` geri döner). **dmabuf** ✅: render düğümünde EGL/GLES, GPU tamponları içe alınıp okunur; kitty/Firefox/Electron GPU'da çizer (GTK4 cairo'da: GPU çizicisi imleci her karede soldurup pencereyi hiç durultmuyor); terminal-read 496 → 253 ms. Kalan: AT-SPI veri yolu yalıtımı |
| F9 ✅ | Eylem gecikmesi: adla tıklamada gereksiz tam pencere OCR'ı kaldırıldı, süreç kümesi önbelleği, kalibrasyon ağaç okumasız, wm `state` önbelleği, bekleyenler kare gelir gelmez yanıtlanır, tarif kaydı arka planda, metin imleci değişim sayılmaz; doğrulama ve durgunluk **wm içinde** (`snapshot_region` / `diff_region`, görüntü aktarılmaz; satır memcmp, opak satır kopyası); `PCEYE_SPANS=1` | MCP click_ui ~560 → **kararlı durumda 82–97 ms (medyan ~93; hedef < 100 ✅)**, pencere açıldıktan sonraki ilk tıklamalar 110–130 ms; D-Bus tıklama 478 → 63 ms; ölçüm seti toplamı Hyprland oturumu 52,1 → wm 31,7 sn |
| F10 ◐ | Dış rakip kıyası: koşucuda `--toolset wayland-mcp` (kaynaktan derlenmiş rakip, aynı model/görev/yalıtım); `evals/LEADERBOARD.md` F10 bölümü | Haiku, 20 görev × 3: **pc-eye 60/60, 1271 s, $0.0037/görev** · wayland-mcp 45/60, 2325 s, $0.0127 · ekran görüntüsü 37/60, 1503 s, $0.0054 — üç ölçütte birden önde. Kıyas sırasında bulunan iki pc-eye hatası düzeltildi (başlıktaki yol korumayı tetikliyordu; wm yazımı Qt tamamlayıcısıyla yarışıyordu). Kalan: OSWorld alt kümesi, ikinci dış rakip, Sonnet ile tekrar |
| F8 ✅ | Arka plan akışları: journald (uyarı+, coredump), inotify (İndirilenler/Masaüstü + yapılandırma), systemd (iş sonucu + çalışırken çöken servis), NetworkManager → tek 8192'lik seq halkası; RPC/CLI/MCP `events`, `wait event` | iki çağrı arası olay kaybı 0 (40 olay, 8 ms, kesintisiz seq, dropped 0); çöken servis 1,6 s'de; ajan 'indirme bitene kadar bekle' 4/4 ~13 s $0.0015; probe 31/31 |
| Sürekli | Rakip kıyası (§7) | her fazdan sonra tablo güncellenir |

## 5. Riskler

- **pc-eye-wm büyük iş**: aylar sürebilir; GTK, Qt, Electron, Firefox ve XWayland
  uyumluluğu ayrı ayrı test edilmeli. F4 ara basamağı sayesinde ona bağımlı kalmayız.
- **Yerel modeller**: 8 GB VRAM'e sığma, OCR ile aynı anda çalışma ve doğruluk. Bulut
  yedeği her zaman açık.
- **AT-SPI Cache desteği** uygulamaya göre değişir; olmayanda eşzamanlı tekil çağrılara düşülür.
- **Hyprland güncellemeleri** kırılma getirebilir; ölçüm seti her güncellemeden sonra koşulur.

## 6. Bağımlılıklar (lisans)

`zbus` (MIT/Apache-2.0), `smithay` (MIT), `wayland-client` / `wayland-protocols` (MIT),
`ort` / ONNX Runtime (MIT/Apache-2.0), `ocrs` (MIT/Apache-2.0). Model ağırlıklarının
lisansı model bazında kontrol edilir.

## 7. "Herkesten iyi" nasıl kanıtlanır

Aynı görevler, aynı makine, üç metrik: başarı, duvar saati süresi, görev başı $.

- **Görevler**: kendi ölçüm setimiz (masaüstü, arka plan, tarayıcı, çok adımlı) +
  `cua-bench` üzerinden OSWorld-Verified'dan Linux'ta koşturulabilen bir alt küme.
- **Rakipler**: Anthropic computer use (referans uygulama), OpenAI CUA, UI-TARS Desktop,
  Agent S3; Linux MCP akranlarından en az ikisi (ör. linux-desktop-mcp, computer-use-linux).
- **Çıktı**: `evals/LEADERBOARD.md`. "Rakiplerden iyi" ancak bu tabloda üç metrikte birden
  öndeysek söylenir.
