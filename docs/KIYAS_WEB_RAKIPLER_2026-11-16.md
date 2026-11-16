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
| JS diyalogları (alert/confirm/prompt) | CDP `Page.handleJavaScriptDialog` | `dialog accept/dismiss` | yok, sayfa kilitlenirdi | otomatik yanıt + kayıt, `dialogs` ile politika; eylem sonucunda kendiliğinden görünür |
| Konsol ve JS hataları | `read_console_messages` | `console`, `errors` | yok | `console` (pattern, level, since); eylem sırasında çıkan hata eylem sonucuna eklenir |
| Ağ istekleri | `read_network_requests` | `network requests` | yok | `network` (pattern, failed, since); gövde ve başlık kaydedilmez |
| Dosya yükleme | `file_upload` | `upload` | yok | `upload` (ref ya da ad, `path`/`paths`; toplam 700 KB) |
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

## Bilinen sınırlar (henüz kapatılmadı)

- Yükleme 700 KB ile sınırlı (native messaging çerçevesi 1 MiB); parçalı aktarım yok.
- Çerçeve içi elemanlarda `real` (gerçek işaretçi) tıklaması, `text` ve `wait` yalnız üst çerçevede.
- Kancalar sayfa yüklendikten sonra kurulursa (geçici kurulum) daha önceki konsol kayıtları yoktur.
- Çerez/depolama okuma, sürükle-bırak, indirme takibi (agent-browser'da var) eklenmedi.
- Ajan katmanı (model ile uçtan uca) bu değişikliklerle ölçülmedi; yalnız probe katmanı koşuldu.
- Chrome'da çalışmaz: kancalar Firefox'a özgü `wrappedJSObject`/`exportFunction` kullanır.
