# Pc-Eye V4 Mimarisi: Hız × Kalite × Maliyet

Durum: öneri (2026-11-01). Yargıç: `evals/` ölçüm seti. Her faz ölçümle kabul edilir.

## 1. Neden baştan yazmıyoruz

Ölçüm, yavaşlığın tasarımın geneline değil üç sıcak noktaya yığıldığını gösterdi.
Bugün düzelttiğimiz hatalar (GTK4 koordinatı, OCR önbelleği, coredump sırası, uygulama
karışması…) ölçüm setiyle kanıtlı. Sıfırdan yazmak bunları yeniden üretir. Yöntem:
çalışan çekirdeği koru, sıcak yolu parça parça değiştir, her adımı ölçüm seti onaylasın.

| Sıcak nokta (2026-11-01 ölçümü) | Şimdi | Kök neden |
| --- | --- | --- |
| AT-SPI ağacı | GTK ~400 ms, Firefox 1,4–1,9 s (üstelik 85 düğümde kesilip sayfa içeriğine ulaşamıyor) | Özellik başına ayrı `busctl` süreci, 3,6 ms × yüzlerce |
| OCR | 1200×600: 1,2 s; Electron penceresi snapshot: 5,8 s | Her çağrıda CPU `tesseract` süreci, tüm pencere |
| Ekran yakalama | 36 ms | Her çağrıda `grim` süreci |
| Ajan taban maliyeti | görev başı ~18K girdi token | Claude Code sistem istemi + 3,8K token araç şeması |
| Tarayıcı | sayfa içeriği yok, sekmeler karışık | Masaüstü erişilebilirlik ağacı web için yanlış kanal |

Korunacaklar: tek Rust binary, Hyprland IPC, Wayland sanal girdi, sanal ekran, AuthGuard,
`snapshot` sözleşmesi, `click-ui` doğrulaması, ölçüm seti.

## 2. Hedefler (ölçülebilir)

| Metrik | Şimdi | V4 hedefi |
| --- | --- | --- |
| `snapshot` p50, AT-SPI uygulaması | 0,4–1,9 s | < 50 ms |
| Pencere OCR | 1,2–5,8 s | < 150 ms (GPU), yalnız değişen bölgeler |
| Tarayıcı sayfa özeti | yok | < 30 ms, sayfa başına ~1–2K token |
| Rutin görev maliyeti | $0.035 (Sonnet, 21 görev $0.74) | < $0.005 |
| Başarı | 7 görevde %100 | daha zor görevlerde (tarayıcı, çok adımlı) %95+ |

## 3. Katmanlar

```
 Ajan katmanı   tarif oynatma (0 token) → ucuz model → güçlü model (yalnız başarısızlıkta)
      │          hafif ajan döngüsü (API + önbellekli kısa istem)  |  MCP (Claude Code, Codex)
      ▼
 Sorgu/eylem    tek JSON-RPC protokolü, unix soketi (~1 ms)
 API            snapshot + delta(seq) · eylem + beklenen sonuç + toplu eylem · güvenlik katmanı
      ▲
 Dünya modeli   pc-eyed: olay güdümlü, bellekte, her zaman güncel
 (daemon)       ├ kompozitör: socket2 olayları (var)
                ├ erişilebilirlik: yerel D-Bus (zbus) + AT-SPI Cache toplu okuma + olay aboneliği
                ├ görüntü: Wayland screencopy, hasar bölgeleri, kare karo hash'leri
                ├ OCR: yalnız değişen karolar; GPU (ONNX) arka ucu, tesseract yedek
                ├ tarayıcı: Firefox eklentisi + native messaging, DOM özetini değişimde iter
                └ arka plan: journald akışı, coredump, inotify, systemd/NetworkManager sinyalleri
```

### 3.1 Dünya modeli daemon'u

Bugün her MCP çağrısı dünyayı baştan okuyor. V4'te `pc-eyed` dünyayı olaylarla güncel tutar.
Sorgu bellekten okunur.

- **Erişilebilirlik**: `busctl` yerine `zbus` (saf Rust D-Bus). Ağaç `org.a11y.atspi.Cache.GetItems`
  ile tek çağrıda alınır (GTK4, Qt ve at-spi2-atk köprüsünde var; uygulama bazında doğrulanacak).
  Konumlar eşzamanlı `GetExtents` çağrılarıyla gelir. `state-changed`, `children-changed` ve
  `text-changed` olaylarıyla ağaç yerinde güncellenir. 85 düğüm sınırı kalkar: sınır
  token bütçesine göre snapshot'ta uygulanır, okumada değil.
- **Görüntü**: `grim` yerine Wayland screencopy (wlr veya ext-image-copy-capture). Hasar
  bölgeleri, değişmeyen kareyi yeniden işlemekten kurtarır. Doğrulama ve `visual_settle`
  bu akıştan beslenir.
- **OCR**: pencere 64×64'lük karolara bölünür. Yalnız hash'i değişen karolar yeniden
  okunur, sonuçlar karo bazında önbelleğe alınır. Arka uç sırası: GPU ONNX (algılama +
  tanıma, RTX 5070) → `ocrs` (saf Rust CPU) → tesseract. Lisanslar uygun olan modeller
  seçilir; AGPL bileşen gömülmez.
- **Arka plan**: `journalctl` süreci yerine journal'ı canlı izleme. coredump, inotify,
  systemd birim durumu ve NetworkManager sinyalleri aynı `seq` halka belleğine akar.
  İki çağrı arasında olan hiçbir şey kaybolmaz.

### 3.2 Tarayıcı: Firefox eklentisi + native messaging

Firefox kalır. `Pc-Eye Bridge` eklentisi her sekmede aria-snapshot benzeri bir özet tutar:
`[e12] button "Gönder"`, `[e7] textbox "Ad Soyad" = ""`, tablolar düz satırlar halinde.
Özet `MutationObserver` ile değişimde güncellenip native host üzerinden daemon'a itilir.
Eylemler numara (`ref`) iledir: tıkla, yaz, seç, kaydır, sekme aç/kapat/geç. Koordinat tahmini yoktur.

Güvenlik:
- varsayılan olarak salt okuma,
- banka ve şifre yöneticisi siteleri yasak listesinde,
- şifre ve kart alanları özete girmez,
- her eylem deftere yazılır.

Dağıtım: AMO'da listelenmeyen ücretsiz imza (kullanıcı bir kez API anahtarı oluşturur).
Geliştirme sırasında `about:debugging` ile geçici yükleme.

BiDi (uzaktan kontrol protokolü) yalnız ajanın yalıtılmış profili için isteğe bağlı ikinci
yoldur. Günlük profilde açık bir port, yereldeki her sürece tarayıcının kontrolünü verir.

### 3.3 Sorgu/eylem API'si

- **Delta snapshot**: `snapshot(seq_since)` yalnız değişen pencereleri ve elemanları döner.
  Ajan her turda tüm dünyayı değil farkı okur.
- **Beklentili eylem**: `click(ref, expect={text|state|window|url})`. Daemon koşulu olaylardan
  bekler (kör uyku yok) ve sonucu aynı çağrıda döner. Görsel doğrulama zaten var; bu, onu
  anlamsal hale getirir.
- **Toplu eylem**: UFO²'nin spekülatif çoklu eylemi. Ajan "şunu yaz, şunu seç, Gönder'e bas,
  'Kaydedildi'yi bekle" adımlarını tek çağrıda gönderir; ilk beklenmeyen durumda dizi durur
  ve durum döner. Form doldurma 7 turdan 2'ye iner.
- **Kompakt şema**: araç tanımları kısa tutulur (bugün 3,8K token). Ayrıntılı kullanım SKILL.md'de kalır.

### 3.4 Ajan katmanı ve maliyet

1. **Tarif hafızası** (Agent Workflow Memory): başarılı dökümlerden değerleri yer tutucuya
   çeviren iş akışları çıkarılır. Eşleşen bir görev modele hiç gitmeden toplu eylemle oynatılır.
2. **Model yönlendirme**: rutin adımlarda ucuz model (Haiku). Beklenti tutmazsa ya da
   belirsizlik varsa güçlü modele yükseltme. Hangi modelin hangi görevde yettiğine ölçüm seti karar verir.
3. **Hafif ajan döngüsü**: sık işler için Claude Code yerine API üzerinde kısa sistem
   istemi + yalnız pc-eye araçları + istem önbelleği. Görev başı taban 18K'dan birkaç bine iner.
   MCP, Claude Code ve Codex için kalır.

### 3.5 Güvenlik

AuthGuard ve korumalı pencere (var). Ek olarak:
- eylem sınıfları: okuma / geri alınabilir / geri alınamaz,
- geri alınamaz eylemde onay,
- kalıcı eylem defteri,
- tarayıcı site yasak listesi,
- MCP kabuk kısıtı (var).

## 4. Fazlar

Her faz, ölçüm setinde hedef metriği tutturup gerileme yoksa kabul edilir. Her fazda o
faza özgü yeni ölçüm görevleri eklenir.

| Faz | İş | Kabul ölçütü |
| --- | --- | --- |
| F1 | `zbus` + AT-SPI Cache + eşzamanlı konum; 85 düğüm sınırı kalkar | GTK snapshot < 50 ms; Firefox sayfa içeriği görünür |
| F2 | MCP/CLI sorguları daemon'a; delta snapshot; beklentili ve toplu eylem | form doldurma turu ≤ 3; tekrar eden sorgu < 5 ms |
| F3 | Firefox eklentisi + native host + tarayıcı ölçüm görevleri (form, tablo, sekme) | sayfa özeti < 30 ms; tarayıcı görevlerinde %95+ |
| F4 | screencopy + karo OCR + GPU arka ucu | pencere OCR < 150 ms; OCR doğruluğu ≥ tesseract |
| F5 | Haiku ölçümü, model yönlendirme, hafif ajan döngüsü, tarif hafızası | rutin görev < $0.005; tarifli görev 0 token |
| F6 | journald akışı, inotify, systemd/NM sinyalleri | iki çağrı arası olay kaybı 0 |

Önerilen başlangıç F1 + F5'in "Haiku ölçümü" kısmı: biri en büyük gecikme kazancı, diğeri
neredeyse bedava ve en büyük maliyet sinyali.

## 5. Yeni bağımlılıklar (lisans)

`zbus` (MIT/Apache-2.0), `wayland-client` + `wayland-protocols(-wlr)` (MIT), `ort` /
ONNX Runtime (MIT/Apache-2.0) veya `ocrs` (MIT/Apache-2.0). Model ağırlıklarının
lisansı ayrıca kontrol edilir.
