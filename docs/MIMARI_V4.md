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
| F3 | Ajan çalışma zamanı: Haiku ölçümü, model yönlendirme, planla-çalıştır-doğrula, istem önbelleği, tarif hafızası | rutin görev < $0.005; tarifli görev 0 LLM çağrısı |
| F4 | Ajan oturumu v1: ayrı headless Hyprland/Sway örneği | kullanıcı imleci ve odağı hiç değişmez; 2 paralel ajan |
| F5 | Firefox eklentisi + native host + tarayıcı görevleri | sayfa özeti < 30 ms; tarayıcı görevlerinde %95+ |
| F6 | Karo OCR + GPU arka ucu + yerel grounding modeli + yerel küçük dil modeli | pencere OCR < 100 ms; ikon/canvas görevleri; rutin görev < $0.002 |
| F7 | pc-eye-wm (Smithay): kopyasız tampon, kare doğruluğunda bekleme, kaynağında girdi | eylem + doğrulama < 100 ms; F4'ün yerini alır |
| F8 | Arka plan akışları: journald, inotify, systemd/NM | iki çağrı arası olay kaybı 0 |
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
