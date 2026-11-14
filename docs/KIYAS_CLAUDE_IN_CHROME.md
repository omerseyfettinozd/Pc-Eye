# pc-eye tarayıcı köprüsü vs Claude in Chrome (2026-11-15)

Aynı makine, aynı yerel sayfalar (`evals/fixtures/web` şablonları, sabit değerler), aynı dört görev.
Başarı sunucuya giden form gövdesinden / okunan değerden denetlendi. Claude in Chrome tarafı Claude'un
kendi araçlarıyla (`read_page`, `find`, `form_input`, `get_page_text`, `browser_batch`) bir ajanın yapacağı
gibi yürütüldü; pc-eye tarafı `pc-eye browser` (MCP `pc_eye_browser` ile aynı çekirdek).

| Görev | pc-eye: çağrı · dönen veri | Claude in Chrome: çağrı (tur) · dönen veri | Not |
| --- | --- | --- | --- |
| Form (ad, bülten, şehir, gönder) | 2 · ~2,0 KB | 3 · ~1,5 KB | Chrome'un etkileşimli özetinde metin kutusu adsız (`textbox [ref_1]`), onay kutusu `"on"`; etiket için ek `find` gerekir. pc-eye satırı `[e1] textbox "Ad Soyad"`, `[e4] checkbox "Bültene abone ol"` |
| Tablo (Cetvel fiyatı) | 2 · 0,34 KB | 2 · ~0,5 KB | `find` hücreyi bulur ama satırı vermez; değer için tüm sayfa metni okunur |
| 150 satırlık listede kod → stok | 2 · 0,33 KB | 2 · **~3,5 KB** | Chrome'da `find` değeri vermedi, `get_page_text` tüm listeyi döktü (~10x veri). pc-eye `query` ile yalnız eşleşen satırı döndürür |
| Doğrulamalı form (hata → düzelt) | 3 · ~2,5 KB | 3 · ~1,3 KB | Chrome'da adsız kutular için yine `find` |

Süre: pc-eye araç süreleri 71 ms (tablo) – 1,1 s (iki gönderimli doğrulama) ölçüldü. Claude in Chrome'un
araç gecikmesi bu kurulumda ayrıca ölçülemedi (çağrılar modelin turlarıyla iç içe).

Sonuç: Başarı ikisinde de 4/4. pc-eye özetleri anlamca daha zengin (etiketli alanlar, `row "a | b | c"`
tablo satırları, `query` süzgeci) ve uzun sayfada belirgin daha az token döndürür; Claude in Chrome'un
`browser_batch`'i tek turda çok eylem yaptığından tur sayısı denk. pc-eye'ın ek üstünlükleri: kullanıcının
ekranına dokunmayan yalıtılmış ajan oturumu, masaüstü uygulamaları ve arka plan durumu aynı araç setinde,
parola/kart alanlarının değerini hiç döndürmemesi, eylem defteri. Claude in Chrome'un üstünlüğü:
kullanıcının gerçek Chrome'u ve oturumları üzerinde kurulumsuz çalışması, ekran görüntüsü + koordinat yedeği.

Ölçümde çıkan pc-eye eksiği: form sonucu `run` çıktısı son sayfa özetini de döndürdüğü için ~2 KB; kısa
sonuç modu (yalnız değişen satırlar) ileride token'ı yarıya indirir.
