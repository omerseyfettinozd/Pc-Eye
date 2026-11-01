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

Probe katmanı: %50 → %100 (11 görev, 33/33). Aşağıdaki iki bölüm başlangıçtaki
durumu anlatır; listelenen hataların hepsi `feat/eval-harness` dalında düzeltildi.

Bilinen sınırlar: tesseract tuval metninde ~10 denemede 1 karakter karıştırır (ajan
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
