# PC-Eye Ölçüm Seti

"Daha iyi oldu" demeden önce ölçmek için. Her görev rastgele değerlerle kurulur
(ezber olmasın) ve başarı, ajanın "yaptım" demesinden değil **gerçek durumdan**
denetlenir: test uygulamasının yazdığı çıktı ya da cevabın doğru değerle eşleşmesi.

Sahneler kullanıcının ekranından yalıtılmış `PCEYE-EVAL` sanal ekranında açılır.
Her görevden sonra açılan süreçler kapatılır; imleç ve odak kullanıcıya geri verilir.

## Katmanlar

| Katman | Ne ölçer | Maliyet |
| --- | --- | --- |
| `probe` | Araç seviyesi: bilinen hedefle `pc-eye` CLI adımları. Algı/eylem doğruluğu ve gecikme. | Ücretsiz, ~1 dk |
| `agent` | Uçtan uca: `claude -p` ajanı yalnız `pc_eye_*` araçlarıyla görevi doğal dille çözer. Başarı, tur, araç çağrısı, token, $. | Ücretli (`--budget` görev başı sınır) |

```bash
python3 evals/run_evals.py list
python3 evals/run_evals.py probe --repeat 3
python3 evals/run_evals.py agent --model claude-sonnet-5-5 --repeat 3
python3 evals/run_evals.py agent --task ui-form-fill --tag perception:ocr
python3 -m unittest discover -s evals      # koşucunun kendi testleri
```

Sonuçlar `evals/results/<zaman>-<katman>.json` dosyasına yazılır; rapor aynı katmanın
bir önceki sonucuyla görev görev karşılaştırır. Ajan dökümleri (`results/transcripts/`)
git dışıdır. `--seed` aynı rastgele değerlerle yeniden koşar.

> `agent` katmanı gerçek fare/klavye olayları üretir. Ajana yalnız sanal ekranda
> çalışması söylenir ama girdi enjeksiyonu küreseldir; koşarken bilgisayarı
> kullanmamak sonuçları daha güvenilir kılar.

## Görev ekleme

`tasks/<id>.json`:

```json
{
  "id": "ui-click-button",
  "scene": "buttons",
  "tags": ["perception:atspi", "action:click"],
  "prompt": "\"{title}\" başlıklı pencerede \"{target}\" düğmesine tıkla.",
  "probe": [
    {"args": ["click-ui", "--window", "{window}", "--name", "{target}"], "settle_ms": 300}
  ],
  "check": {"type": "out_json", "expect": {"clicked": "{target}"}}
}
```

- `scene`: `run_evals.py` içindeki `SCENES` (`buttons`, `form`, `canvas`, `terminal`,
  `port`, `journal`, `crash`). Sahne `{ad}` değişkenlerini üretir; yeni sahne bir
  fonksiyon ve gerekirse `fixtures/eval_app.py` içinde bir GTK4 görünümüdür.
- `probe` adımları: `expect_contains` (çıktıda geçmesi gereken metinler), `settle_ms`.
- `check`: `out_json` (uygulama çıktısındaki alanlar) veya `answer` (ajanın son
  `CEVAP:` satırı; liste verilirse alternatiflerden biri).

## Ölçüm geçmişi

Ajan katmanı, Sonnet 5.5 ×3, aynı 7 görev:

| Koşu | Değişiklik | Başarı | Maliyet | Medyan girdi token | Toplam tur |
| --- | --- | --- | --- | --- | --- |
| 020555 | başlangıç | 20/21 | $2.19 | 72.6K | 130 |
| 031039 | koordinat, click doğrulama, OCR önbelleği, başlıkla pencere, `ps`, launch kısıtı, coredump | 21/21 | $1.04 | 26.6K | 72 |
| 031813 | + `snapshot` | 20/21 | $0.77 | 26.6K | 70 |
| 032721 | snapshot'a OCR güveni geri, pencereye göreli zoom/region | 21/21 | $0.74 | 25.9K | 63 |
| 042455 | F1: yerel zbus, paralel AT-SPI okuma, Ctrl düzeltmesi (+2 tarayıcı görevi) | 27/27 (aynı 7: 21/21) | $0.85 (aynı 7: $0.63) | 24.6K | 85 (aynı 7: 68) |

| 045024 | F1b: olay güdümlü önbellek, daemon soketi, delta snapshot | 27/27 | $1.11 | 24.8K | 91 |
| 054034 | F2: eylem programı, kararlı mark'lar, otomatik delta, kısa şema | 27/27 | $0.81 | 20.3K | 70 |
| 074248 | F2 düzeltmeleri: mark yolunda doğrulama, select ipucu | 26/27 (1 OCR: 8→S) | $0.74 | 20.2K | 62 |

| 115603 | F3: Firefox metin okuma düzeltmesi, tarif önerisi ana ipucunda (+tarif görevi) | 30/30 (Haiku) | $0.063 (Haiku) | 27.9K | 101 |

| 130530 | F4: ajan oturumu (iç içe Hyprland), 2 paralel koşucu | 60/60 (Haiku, 2×30) | $0.115 (2×) | 28.8K | — |

**F4, ajan oturumu (`--session`):** görevler kullanıcının oturumunda değil, ajana ayrılmış iç içe bir
Hyprland'da koşar; koşucu her görevden önce/sonra kullanıcının imlecini ve aktif penceresini karşılaştırır
(`user_disturbed`). İki koşucu aynı anda (`--session a1` / `--session a2`), her biri tüm set:
60/60 başarı, **kullanıcı imleci/odağı 0/60 koşuda değişti**, 60 koşu 419 s duvar saatinde (tek koşucu
30 koşu ~360 s; görev başı süre paralellikle uzamadı). Probe katmanı oturumda 21/21 (bir koşuda
bilinen seyrek `ui-click-miss-detect` kararsızlığı; kullanıcı oturumunda da 6'da 1 görülüyor).
İlk paralel deneme (125142, sonuç dosyaları duruyor) iki hata buldu: aynı anda açılan iki oturum aynı
iç örneği sahipleniyordu (biri kapanınca öteki gitti) ve iç örnek kapanınca pc-eye **kullanıcının
Hyprland'ına düşüyordu** (ajan kullanıcının pencerelerini görmeye başladı). İkisi de düzeltildi:
örnek, başlatılırken verilen etiketle `/proc/<pid>/environ`'dan tanınır; oturum modunda başka örneğe
düşülmez ve MCP kapanmış oturum için açık hata döner.

```bash
python3 evals/run_evals.py agent --session a1 --model claude-haiku-5-5
```

## Zor görev seti ve rakip kıyası (2026-11-01)

`--tag hard`: 8 gerçekçi, çok adımlı görev (iki uygulama arası kopyalama, arka plan + GUI,
kenar çubuklu ayarlar, doğrulama hatasından toparlanma, 150 satırlık sayfa, art arda diyaloglar,
Dolphin'de dosya taşıma, Kate'te Farklı Kaydet). Kıyas tablosu: [`LEADERBOARD.md`](LEADERBOARD.md)
(`python3 evals/leaderboard.py <sonuç dosyaları>`). Rakip mimarisi `--toolset pixels`: aynı model
yalnız ekran görüntüsü + fare/klavye (Anthropic computer use / OpenAI CUA yaklaşımı).

Sonuç (150617, ortak 7 görev): pc-eye+Haiku %95 / $0.006 görev başı; pc-eye+Sonnet %90 / $0.062;
piksel tabanı+Sonnet %86 / $0.096. Maliyet ve başarıda öndeyiz, medyan sürede (17,1 s'e karşı
19–23 s) gerideydik; en büyük fark zenity sihirbazıydı (18 s'e karşı 95 s) ve nedeni bulundu:
sınıf adı "zenity" alt dize aramasında Zen tarayıcısı sanılıyor, her okuma belge için 2,5 sn
bekliyordu (düzeltildi: okuma 2,6 sn → 8–65 ms).

Zor setin bulduğu hatalar (hepsi düzeltildi): sanal klavyede 14./15. benzersiz karakter
Backspace/Tab fiziksel koduna düşüyordu (Qt karakteri yutuyor ya da siliyordu; Kate 33 tur → 8);
GTK4 spin button değeri işlenmiyordu; GTK4 açılır listesinin adı seçili değeri; libadwaita
diyaloğunda AT-SPI konumu (+26, +36) kayık (OCR ile otomatik kalibrasyon); iç Hyprland'ın ilk GPU
tamponu ara sıra ayrılamıyor (yeniden deneme); kaçan tıklamayı önceki konumdaki üzerine gelme
vurgusu gizliyordu; Dolphin'de "Yerler" paneli ile klasör içeriği ayırt edilemiyordu (`in` alanı);
tuş/yazma sonrası uygulamanın tepkisi beklenmiyordu (Ctrl+L ardından yazılan yol başka yere gidiyordu).

Düzeltmelerden sonra (181842 Sonnet, 184021 Haiku; aynı 7 ortak görev): pc-eye+Haiku 21/21 (%100),
medyan 24,1 s, $0.010; pc-eye+Sonnet 21/21, 26,6 s, $0.067; piksel tabanı 12/14 (%86), 17,1 s, $0.096.
Başarı ve maliyette öndeyiz; medyan sürede hâlâ geride. `PC_EYE_TRACE` izine göre görev süresinin
~%70'i model turları; araç tarafında tıklama doğrulaması 1065 → ~390 ms'e indi (ajan içinde
`then: snapshot` ile birlikte medyan 783 ms). Sıradaki hız işi: tur sayısı (Dolphin 17, sihirbaz 15).

Tur azaltma turu (191943 Sonnet, 194220 Haiku): vision görüntüyü her zaman döndürür, `then: snapshot`
delta ve kapanan pencerede sonraki pencereye geçer, `clicks: 2` / `open`, aynı adlı adaylarda pencere
başlığıyla eşleşen kap, kaymalı pencerede düğmelere D-Bus, yazma klavye önce + bozulursa AT-SPI onarımı
(Dolphin adres çubuğu tamamlama penceresi ~120 karakter yutuyordu). Sihirbaz 15 → 9–10 tur (Sonnet 60 →
23 s), Dolphin Sonnet 33 → 10 tur / 68 → 27 s (194220-g8s), Haiku tüm set 24/24, $0.116.

**Gerçek site (`--tag live`):** `live-youtube-play` — YouTube'da arama, video açma; başarı tarayıcının
MPRIS durumundan (gerçekten "Playing" mi) denetlenir. Haiku 3/3 (biri Shorts), ~45 s, $0.011.
Ajan oturumundaki uygulamaların sesi oturumun boş ses çıkışına gider (kullanıcının hoparlörüne değil);
snapshot `media` alanı oynatma durumunu verir.

**Model kıyası (F3, aynı makine, aynı görevler ×3):**

| | Sonnet 5.5 (074248, F2) | Haiku 5.5 (115603, F3) |
| --- | --- | --- |
| Başarı | 26/27 | 30/30 |
| Toplam | $0.74 (27 koşu) | $0.063 (30 koşu) |
| Görev başı medyan | ~$0.02–0.12 | ~$0.003–0.010 |

Form görevleri ×3 (120431 Haiku / 120811 Sonnet, F3 sonrası): web formu 4 tur ($0.009 / $0.118),
GTK formu 4 tur ($0.008 / $0.091), **tarifli web formu** Sonnet 3/3 koşuda `snapshot → run(recipe)`
(3 tur, $0.110), Haiku 6 koşunun 4'ünde tarifi seçti. Model olmadan: `pc-eye recipe run` aynı formu
~6 s'de doldurup gönderiyor, $0 (`recipe-replay-cli`). Sonnet'te maliyetin çoğu sabit taban
(sistem istemi + şema); tarif tur sayısını 4 → 3'e indiriyor, asıl kaldıraç model seçimi.

F3'te bulunan hatalar: Firefox'ta `GetText(0, 500)` metin 500 karakterden kısaysa hata döndürüyordu,
yani giriş değerleri, `set_text` geri okuması ve sayfa metni **hiç** okunmuyordu (F1a'daki
"SetTextContents sahte başarı" bulgusunun bir kısmı buydu). `role="status"` gibi adsız metin kapları
listelenmiyordu: ajan gönderim sonrası "Kaydedildi"yi göremeyip ekran görüntüsü alıyordu (web formu
Haiku'da 8 → 4 tur). GTK form sahnesine de gönderim onayı eklendi (gerçek formlar onay gösterir;
önce ajan sonucu günlüklerde arıyordu). Eşzamanlı build uyuşmazlığında sahipsiz daemon'lar birikiyordu
(5 adet); artık `flock` ile tek örnek.

F2 form görevleri: web formu 10 tur/99K → 4 tur/39K, masaüstü formu 9 tur/80K → 4 tur/34K
(her koşuda aynı; varyans kayboldu). En basit görevin tabanı 18,4K → 14,5K token (kısa şema).

F1 okuma süreleri: GTK formu 298 → 32 ms, Firefox sayfası 1311 → 40 ms (form ve tablo artık görünür).
F1b: CLI snapshot 32 → ~8 ms, MCP tekrar 42 → ~6 ms, değişmeyen delta 4,1 KB → 136 bayt.
045024'teki maliyet artışı F1b'den değil: ajan delta'yı hiç kullanmadı; form görevleri koşudan
koşuya 7–14 tur oynuyor (70K–167K token) ve bir koşuda `then: "context"` tek başına 38 KB döndü
(düzeltildi: artık kompakt snapshot, 2,5 KB). Görev başı 3 tekrarla ±%25 fark gürültü sınırında;
maliyet iddiaları için daha çok tekrar gerekir.
Tarayıcı: tablo okuma 2 tur; form doldurma 10 tur ($0.15) → F2'nin (eylem programı) hedefi.

Probe katmanı: %50 → %100 (11 görev, 33/33). Aşağıdaki iki bölüm başlangıçtaki
durumu anlatır; listelenen hataların hepsi `feat/eval-harness` dalında düzeltildi.

Bilinen sınırlar: tesseract tuval metninde koda göre değişen sıklıkta karakter karıştırır ya da boşluk ekler (bazı seed'lerde 3'te 2) (ajan
`conf` düşükse yakınlaştırıp doğrular); `click-ui` doğrulaması eylem başına ~400 ms ekler;
Electron gibi AT-SPI'sız pencerelerde tam pencere OCR'ı birkaç saniye sürer.

## İlk ölçüm (2026-11-01, probe ×3)

| Görev | Sonuç | Neden |
| --- | --- | --- |
| crash-signal, logs-journal, xray-port, terminal-read | 3/3 | — |
| ui-click-button | 0/3 | GTK4 elemanlarının hepsi pencerenin sol üst köşesinde raporlanıyor (göreli konum eklenmiyor); tıklama yanlış yere gidiyor. |
| ui-click-button-dbus | 0/3 | `--prefer-dbus` adı eşleşen ilk elemanı (eylemi olmayan `grid cell`) seçip koordinata düşüyor. |
| ui-form-fill | 0/3 | Aynı koordinat hatası; açılır liste açılamıyor. |
| ocr-canvas-read | 0/3 | OCR önbelleği bölge + 64-bit dHash ile anahtarlı: yerleşimi aynı, metni farklı ekran **eski metni** döndürüyor (`src/eyes/ocr.rs`). |

## İlk ajan ölçümü (2026-11-01, Sonnet 5.5 ×3, $2.19)

| Görev | Geçti | Medyan tur | Medyan girdi token |
| --- | --- | --- | --- |
| crash-signal, logs-journal | 3/3 | 2 | ~23K |
| terminal-read | 3/3 | 4 | 63K |
| ocr-canvas-read | 3/3 | 5 | 73K |
| xray-port | 3/3 | 5 | 143K |
| ui-form-fill | 3/3 | 16 | 220K |
| ui-click-button | 2/3 | 9 | 236K |

Başarı %95, ama araç hataları başarıda değil **maliyette** görünüyor: ajan bozuk
koordinatları ve OCR'ı ekran görüntüsüyle (36 base64 çağrısı) telafi ediyor; UI
görevleri metin görevlerinin ~10 katı token harcıyor. Dökümlerden:

- Tek başarısızlık: `click-ui` boşluğa tıkladığı halde `ok: true` döndü, ajan doğrulamadan
  "tamam" dedi. Eylem sonrası doğrulama yok.
- `window` parametresi yalnız adres kabul ediyor; başlık verilince `0x` eklenip
  "Pencere bulunamadı" dönüyor (5/21 koşuda 10 boşa çağrı).
- Pencereli olmayan süreci arayacak araç yok; ajan 3/3 koşuda `pc_eye_launch` ile
  `sh -c "pgrep …; ss -ltnp"` çalıştırdı. `launch` fiilen sınırsız kabuk: "yalnız pc-eye
  araçları" kısıtı bununla delinir.
- Argümansız `vision` tam masaüstünü (kullanıcının ekranı dahil) yakalıyor.

Ayrıca: `ui_atspi.rs` uygulamayı `pid_matches || name_matches` ile seçiyor; aynı adlı
başka bir uygulamanın (ikinci pencere/örnek) elemanları sonuca karışıyor. OCR'ın sabit
ızgarada ürettiği onlarca `icon-button` elemanı da gürültü.
