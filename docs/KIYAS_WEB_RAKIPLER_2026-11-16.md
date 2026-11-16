# Web kullanımında rakip incelemesi ve kapatılan eksikler (2026-11-16)

Yöntem: diskteki rakip paketleri REA ile açıldı (çalıştırılmadan, statik). İncelenenler:

| Rakip | Sürüm | Açılan parça |
| --- | --- | --- |
| Claude in Chrome (Chrome eklentisi) | 1.0.107 | `accessibility-tree.js` içerik betiği (tamamı okundu), araç tanımları ve kullandığı CDP yöntemleri (`ShortcutExecutionService` paketi) |
| agent-browser (Vercel, Rust ikilisi) | 0.26.0 | Paketle gelen komut başvurusu ve README. İkilinin dize taraması REA'da zaman aşımına düştü; ikili içi doğrulanmadı |
| Playwright | 1.62.1 | Yalnız bilinen davranışı (aria anlık görüntüsü, tıklanabilirlik denetimi) ölçüt alındı; paket açılmadı |

## Rakiplerde olup bizde olmayanlar ve durum

| Yetenek | Claude in Chrome | agent-browser | pc-eye önce | pc-eye şimdi |
| --- | --- | --- | --- | --- |
| JS diyalogları (alert/confirm/prompt) | CDP `Page.handleJavaScriptDialog` | `dialog accept/dismiss` | yok, sayfa kilitlenirdi | varsayılan ret + kayıt; kabul için eyleme `dialog: "accept"` / `dialog_text`; eylem sonucunda kendiliğinden görünür |
| Konsol ve JS hataları | `read_console_messages` | `console`, `errors` | yok | `console` (pattern, level, since); eylem sırasında çıkan hata eylem sonucuna eklenir |
| Ağ istekleri | `read_network_requests` | `network requests` | yok | `network` (pattern, failed, since); gövde ve başlık kaydedilmez |
| Dosya yükleme | `file_upload` | `upload` | yok | `upload` (ref ya da ad, `path`/`paths`; toplam 32 MB, parçalı) |
| Çapraz kaynak iframe | `all_frames` içerik betiği | `frame` | yalnız aynı kaynak | özet içinde `frame fN` bölümü, ref `e5f12`, adla eylem çalışır |
| Özet kapsamı | `ref_id`, `depth` | `-s` seçici, `-d` | query, max, viewport | + `scope` (ref ya da CSS seçici) |
| Durum bayrakları | yok (yalnız rol/ad/href/type) | aria durumları | checked, expanded, disabled, required, invalid | + selected, pressed, current, popup, collapsed |
| CSS ile maskelenen alan gizliliği | `text-security` denetimi | yok | yalnız password ve autocomplete | + `-webkit-text-security` |
| Tıklama hedefi örtülü mü | yok (koordinat tıklar) | Playwright tabanlı yolda var | yok | `covered_by` uyarısı (tıklama yine yapılır) |
| Okunur sayfa metni | `get_page_text` (makale öncelikli) | `get text` | tüm gövde | ana içerik öncelikli, `query` ile yalnız eşleşen satırlar |

Rakiplerde olmayıp bizde olan: diyalog ve JS hatasının ajan sormadan eylem sonucuna eklenmesi
(rakiplerde ayrı araç çağrısı gerekir), tek çağrıda adla eylem ve çok adımlı `run`, yalıtılmış oturum.

## Ölçüm

`python3 evals/run_evals.py probe --browser bridge --session <ad> --tag bridge --repeat 3`
(yalıtılmış ajan oturumu, gerçek Firefox; başarı sunucuya giden gövdeden denetlenir):
9 görev, 27/27. Yeni görevler: `web-dialog-policy`, `web-console-network`, `web-upload`,
`web-snapshot-scope`, `web-cross-frame`. Sonuç: `evals/results/20261116-114315-webhard-bridge-probe.json`.

### Ajan katmanı (Haiku, 6 görev × 3, yalıtılmış oturum)

| Koşu | Başarı | Not |
| --- | --- | --- |
| İlk (`20261116-115354-webag-bridge-agent.json`) | 10/15 | Diyalog görevi 1/3: varsayılan "kabul" yüzünden ajan politikayı ayarlamadan tıklayınca silme onaylandı. Konsol görevi 0/3: görev beklentisi yanlıştı (ajan doğru yanıtlamıştı) |
| Düzeltme sonrası (`20261116-115923-webag-bridge-agent.json`) | 18/18, toplam $0,034 | Varsayılan ret, eyleme özel `dialog: "accept"`; `web-dialog-accept` görevi eklendi |

Ölçümün bulduğu hatalar: yıkıcı onayın görülmeden kabul edilmesi (varsayılan değişti) ve
`dialogs`/`console`/`network` çıktısının başındaki boş `url: | title:` satırı.
Probe tekrarında `hard-web-long` bir kez 340 ms ile süre bütçesini aştı (makine kullanıcı
etkinliğiyle meşguldü); ardışık 5 tekrarda 27,5 ms medyanla 5/5.

## Başa baş ajan kıyası (2026-11-16, Haiku 5.5)

Aynı model, aynı 12 görev, aynı sayfalar; başarı sunucuya giden gövdeden ya da yanıttan denetlenir.
Sayfa üç araç setinde de ÖNCEDEN açıktır (rakipler başsız Chrome'a CDP ile bağlanır, gezinme turu harcamaz).
Araç setleri dönüşümlü koşuldu (tur başına sırayla), her görev en az 3 koşu.
`python3 evals/run_evals.py agent --toolset web|playwright-mcp|agent-browser --tag bridge`,
toplama: `python3 evals/compare_toolsets.py <sonuçlar>`.

| Görev (başarı · medyan süre · koşu başı maliyet) | pc-eye (web profili) | Playwright MCP 0.0.85 | agent-browser 0.26.0 |
| --- | --- | --- | --- |
| `hard-web-long` | 3/3 · 9.0 sn · $0.0010 | 3/3 · 9.9 sn · $0.0030 | 3/3 · 10.8 sn · $0.0027 |
| `hard-web-validation` | 3/3 · 13.0 sn · $0.0016 | 3/3 · 15.3 sn · $0.0022 | 3/3 · 51.3 sn · $0.0020 |
| `web-console-network` | 6/6 · 9.7 sn · $0.0016 | 3/3 · 14.4 sn · $0.0031 | 7/7 · 10.1 sn · $0.0015 |
| `web-cross-frame` | 3/3 · 10.7 sn · $0.0014 | 3/3 · 17.1 sn · $0.0016 | 1/3 · 51.8 sn · $0.0056 |
| `web-dialog-accept` | 3/3 · 9.9 sn · $0.0014 | 3/3 · 12.7 sn · $0.0021 | 3/3 · 47.8 sn · $0.0030 |
| `web-dialog-policy` | 3/3 · 9.7 sn · $0.0014 | 3/3 · 12.5 sn · $0.0021 | 3/3 · 47.8 sn · $0.0024 |
| `web-drag-drop` | 3/3 · 11.1 sn · $0.0016 | 3/3 · 14.8 sn · $0.0018 | 2/3 · 30.3 sn · $0.0045 |
| `web-form-fill` | 3/3 · 11.5 sn · $0.0015 | 3/3 · 14.1 sn · $0.0020 | 3/3 · 13.2 sn · $0.0017 |
| `web-snapshot-scope` | 3/3 · 9.4 sn · $0.0011 | 3/3 · 10.1 sn · $0.0013 | 3/3 · 11.2 sn · $0.0018 |
| `web-table-read` | 3/3 · 8.7 sn · $0.0011 | 3/3 · 9.4 sn · $0.0012 | 3/3 · 8.8 sn · $0.0011 |
| `web-upload` | 3/3 · 9.6 sn · $0.0012 | 2/2 · 17.1 sn · $0.0020 | 3/3 · 10.2 sn · $0.0016 |
| `web-upload-large` | 3/3 · 10.0 sn · $0.0012 | 3/3 · 14.9 sn · $0.0020 | 3/3 · 11.3 sn · $0.0016 |

| Toplam | pc-eye | Playwright MCP | agent-browser |
| --- | --- | --- | --- |
| Başarı | 39/39 | 35/35 | 37/40 |
| Görev medyanlarının toplamı | 122,2 sn | 162,3 sn | 304,5 sn |
| Koşu başı ortalama maliyet | $0,0014 | $0,0020 | $0,0024 |
| En iyi olduğu görev (başarı / hız / maliyet, 12 üzerinden) | 12 / 12 / 11 | 12 / 0 / 0 | 10 / 0 / 1 |

pc-eye başarıda (Playwright ile birlikte %100), 12 görevin 12'sinde hızda, 11'inde maliyette en iyi.
`web-console-network` maliyetinde agent-browser ile berabere sayılır ($0,0015–0,0016; koşu başı
gürültü ±$0,0003, çoğu önbellek yazımından).

İlk kıyasta (düzeltmelerden önce) pc-eye hızda 10/12, maliyette 11/12 idi; dökümlerin gösterdiği nedenler
ve düzeltmeleri: tahmini metni bekleyen `wait` 5 sn harcıyordu (sayfa durulunca erken dönüş), eylem
sonrası doğrulama çağrıları (eylem artık değişen satırları döndürür), dosya girişinin `button` görünmesi
(`file` rolü), sorguda tablo satırının sütun adsız gelmesi (`Stok: 12`), başlık sorgusunun bölümü
getirmemesi, bırakma hedefinin özette görünmemesi.

Sınırlar: görevler ve sayfalar bizim yazdığımız yerel test sayfalarıdır (dış, tarafsız bir web
ölçütü değil); tek model (Haiku 5.5); Claude in Chrome bu düzenekte koşturulamadı (kullanıcının Chrome'u
ve oturumu gerekir); agent-browser'ın kurulu 0.26.0 sürümü ölçüldü (güncel 0.43.0 değil); agent-browser
Bash üzerinden CLI olarak, pc-eye yalnız `pc_eye_browser` aracıyla (web profili) koşuldu.

## Dört araç setli kıyas: Claude in Chrome dahil (2026-11-16 gece, Haiku 5.5)

24 görev: 13 yerel sayfa + 11 dış deneme sitesi (the-internet.herokuapp.com, yerel yansıdan; site
yavaş ve dalgalı olduğu için süre ölçümü yansıdan yapılır, `PCEYE_EXT_LIVE=1` canlı). Her araç seti 3 tur.
Claude in Chrome `claude --chrome` ile kullanıcının gerçek Chrome'unda koşuldu; sayfayı önceden açamaz,
her görevde sekme grubu kurma/gezinme/kapatma için ~3 ek çağrı harcar. Eklenti bağlantısı kararsızdı:
"extension is not connected" geçen 16 koşu tabloya alınmadı (aracın yeteneği değil, bağlantı).

| Görev (başarı · medyan süre · koşu başı maliyet) | pc-eye (web) | Playwright MCP | agent-browser | Claude in Chrome |
| --- | --- | --- | --- | --- |
| `ext-add-remove` | 3/3 · 10.4 sn · $0.0015 | 3/3 · 13.5 sn · $0.0022 | 3/3 · 17.3 sn · $0.0013 | 1/1 · 26.7 sn · $0.0028 |
| `ext-drag` | 3/3 · 10.6 sn · $0.0013 | 3/3 · 14.4 sn · $0.0017 | 3/3 · 14.7 sn · $0.0022 | 3/3 · 27.5 sn · $0.0029 |
| `ext-dynamic-controls` | 3/3 · 13.2 sn · $0.0017 | 3/3 · 17.9 sn · $0.0019 | 1/3 · 14.6 sn · $0.0015 | 2/2 · 30.1 sn · $0.0028 |
| `ext-dynamic-loading` | 3/3 · 18.6 sn · $0.0014 | 3/3 · 16.9 sn · $0.0017 | 3/3 · 17.2 sn · $0.0013 | 3/3 · 32.8 sn · $0.0029 |
| `ext-hover` | 3/3 · 9.4 sn · $0.0012 | 3/3 · 14.0 sn · $0.0019 | 3/3 · 19.7 sn · $0.0027 | 3/3 · 22.5 sn · $0.0029 |
| `ext-js-confirm` | 3/3 · 15.5 sn · $0.0012 | 3/3 · 13.4 sn · $0.0017 | 3/3 · 51.1 sn · $0.0024 | 0/2 · 96.9 sn · $0.0033 |
| `ext-js-prompt` | 3/3 · 12.1 sn · $0.0013 | 3/3 · 21.8 sn · $0.0018 | 3/3 · 85.0 sn · $0.0027 | 0/3 · 80.9 sn · $0.0033 |
| `ext-large` | 3/3 · 10.4 sn · $0.0016 | 3/3 · 18.8 sn · $0.0019 | 3/3 · 13.5 sn · $0.0021 | 2/2 · 40.0 sn · $0.0033 |
| `ext-nested-frames` | 3/3 · 11.9 sn · $0.0015 | 3/3 · 11.0 sn · $0.0011 | 3/3 · 9.0 sn · $0.0011 | 3/3 · 20.9 sn · $0.0023 |
| `ext-table` | 3/3 · 6.6 sn · $0.0010 | 3/3 · 10.8 sn · $0.0015 | 3/3 · 10.9 sn · $0.0014 | 2/2 · 31.9 sn · $0.0019 |
| `ext-upload` | 3/3 · 13.5 sn · $0.0016 | 3/3 · 32.0 sn · $0.0027 | 3/3 · 14.3 sn · $0.0014 | 0/2 · 47.9 sn · $0.0029 |
| `hard-web-long` | 3/3 · 9.4 sn · $0.0010 | 3/3 · 9.6 sn · $0.0029 | 3/3 · 10.2 sn · $0.0027 | 3/3 · 21.0 sn · $0.0022 |
| `hard-web-validation` | 3/3 · 11.1 sn · $0.0016 | 3/3 · 18.0 sn · $0.0022 | 3/3 · 57.4 sn · $0.0018 | 3/3 · 41.4 sn · $0.0045 |
| `web-console-network` | 3/3 · 10.7 sn · $0.0014 | 3/3 · 15.8 sn · $0.0025 | 3/3 · 10.8 sn · $0.0015 | 1/1 · 59.1 sn · $0.0029 |
| `web-cross-frame` | 3/3 · 10.3 sn · $0.0013 | 3/3 · 20.3 sn · $0.0015 | 0/3 · 41.9 sn · $0.0054 | 2/2 · 31.7 sn · $0.0032 |
| `web-dialog-accept` | 3/3 · 10.3 sn · $0.0012 | 3/3 · 15.4 sn · $0.0022 | 3/3 · 90.1 sn · $0.0034 | 1/3 · 138.7 sn · $0.0039 |
| `web-dialog-policy` | 3/3 · 10.6 sn · $0.0014 | 3/3 · 23.0 sn · $0.0021 | 2/3 · 49.1 sn · $0.0018 | 2/3 · 85.7 sn · $0.0037 |
| `web-drag-drop` | 3/3 · 14.0 sn · $0.0015 | 3/3 · 12.8 sn · $0.0019 | 2/3 · 26.0 sn · $0.0029 | 2/2 · 31.9 sn · $0.0030 |
| `web-form-fill` | 3/3 · 9.8 sn · $0.0015 | 3/3 · 12.1 sn · $0.0019 | 2/3 · 16.1 sn · $0.0012 | 1/1 · 46.7 sn · $0.0040 |
| `web-forms-extra` | 3/3 · 21.0 sn · $0.0026 | 3/3 · 16.1 sn · $0.0026 | 2/3 · 65.7 sn · $0.0043 | 2/2 · 51.6 sn · $0.0042 |
| `web-snapshot-scope` | 3/3 · 7.3 sn · $0.0010 | 3/3 · 10.5 sn · $0.0014 | 2/3 · 15.8 sn · $0.0012 | 2/2 · 30.3 sn · $0.0027 |
| `web-table-read` | 3/3 · 9.4 sn · $0.0010 | 3/3 · 9.3 sn · $0.0011 | 3/3 · 13.4 sn · $0.0012 | 3/3 · 20.4 sn · $0.0018 |
| `web-upload` | 3/3 · 8.9 sn · $0.0012 | 3/3 · 17.9 sn · $0.0021 | 2/3 · 28.6 sn · $0.0010 | 0/2 · 27.0 sn · $0.0032 |
| `web-upload-large` | 3/3 · 12.0 sn · $0.0012 | 3/3 · 16.1 sn · $0.0021 | 2/3 · 31.3 sn · $0.0011 | 0/3 · 36.1 sn · $0.0034 |

| Toplam | pc-eye | Playwright MCP | agent-browser | Claude in Chrome |
| --- | --- | --- | --- | --- |
| Başarı | 72/72 | 72/72 | 60/72 | 41/56 |
| Görev medyanlarının toplamı | 277 sn | 382 sn | 724 sn | 1080 sn |
| Koşu başı ortalama maliyet | $0,0014 | $0,0020 | $0,0021 | $0,0031 |
| En iyi olduğu görev (başarı / hız / maliyet, 24 üzerinden) | 24 / 18 / 16 | 24 / 5 / 1 | 15 / 1 / 7 | 17 / 0 / 0 |

Claude in Chrome'un gerçek zayıflıkları: JS onay/istem kutusu açılınca sayfa donar ("renderer may be
frozen"), tıklama ve okuma 30–45 sn zaman aşımına düşer (diyalog görevleri 0/8, 66–157 sn); dosya
yükleme 0/7. agent-browser'ın 12 başarısızlığının 7'si son turdaki 180 sn zaman aşımı (nedeni doğrulanmadı).

pc-eye'ın yavaş kaldığı 6 görev dökümlerden incelendi ve düzeltildi (commit 296d57e): eylem yükleme
göstergesi başlatırsa sonucu bekler, düz metinli sayfa (çerçeve kümesi) boş özet vermez, kaydırıcıya
ok tuşu/doğrudan değer, `wait` eşleşen satırı döndürür. Doğrulama 18/18; araç çağrısı iç içe çerçevede
2–6 → 1, yüklemeli sayfada 3–4 → 1. **Bu düzeltmelerden sonra dörtlü kıyas yeniden koşulmadı**: o gece
model API'si çok dalgalıydı (aynı görev 4–139 sn), süre karşılaştırması güvenilir olmazdı.

### Claude in Chrome'dan alınanlar (REA ile `ShortcutExecutionService` ve `accessibility-tree.js` okundu)

select hatasında mevcut seçenekler; checkbox karışık durum + doğrulama uyarısı; date/time/color/range
alanlarına doğrudan değer; ref + dy ile en yakın kaydırılabilir kap; geniş ana içerik seçici listesi ve
satır sonunda kesme; bağlantı adreslerinde gizli sorgu parametrelerini maskeleme (token=REDACTED);
kutusu olmayan hedefe tıklamada uyarı (commit 5ac2245).

## Bilinen sınırlar (henüz kapatılmadı)

- Yükleme toplam 32 MB ile sınırlı (parçalı aktarım).
- Çerçeve içi elemanlarda `real` (gerçek işaretçi) tıklaması, `text` ve `wait` yalnız üst çerçevede.
- Kancalar sayfa yüklendikten sonra kurulursa (geçici kurulum) daha önceki konsol kayıtları yoktur.
- Çerez/depolama okuma ve indirme takibi (agent-browser'da var) eklenmedi; sürükle-bırak yapay olaylarla çalışır, tarayıcının yerel sürükleme oturumunu başlatmaz.
- Chrome'da çalışmaz: kancalar Firefox'a özgü `wrappedJSObject`/`exportFunction` kullanır.
