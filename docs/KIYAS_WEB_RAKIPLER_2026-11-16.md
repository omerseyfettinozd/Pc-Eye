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

## Bilinen sınırlar (henüz kapatılmadı)

- Yükleme toplam 32 MB ile sınırlı (parçalı aktarım).
- Çerçeve içi elemanlarda `real` (gerçek işaretçi) tıklaması, `text` ve `wait` yalnız üst çerçevede.
- Kancalar sayfa yüklendikten sonra kurulursa (geçici kurulum) daha önceki konsol kayıtları yoktur.
- Çerez/depolama okuma ve indirme takibi (agent-browser'da var) eklenmedi; sürükle-bırak yapay olaylarla çalışır, tarayıcının yerel sürükleme oturumunu başlatmaz.
- Chrome'da çalışmaz: kancalar Firefox'a özgü `wrappedJSObject`/`exportFunction` kullanır.
