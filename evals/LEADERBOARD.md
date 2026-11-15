# Kıyas tablosu

## F10: dış rakip kıyası (2026-11-15)

Aynı makine, aynı model (claude-haiku-5-5), aynı 20 görev × 3 tekrar, rastgele değerler; başarı
uygulamanın gerçek durumundan denetlenir. Her koşu kullanıcının ekranından yalıtılmış oturumda
(sızıntı: üç sistemde de 0/60).

- **pc-eye**: bugünkü sürüm, varsayılan pc-eye-wm oturumu.
- **wayland-mcp** v0.5.2 ([alex-oleshkevich/wayland-mcp](https://github.com/alex-oleshkevich/wayland-mcp),
  kaynaktan derlendi; erişilebilirlik ağacı + OCR + wlroots girdi/yakalama, 49 araç): iç içe Hyprland
  oturumunda, "host" hedefi o oturum. OCR modelleri yazarın HF deposundan (varsayılan S3 sunucusu 403).
- **ekran görüntüsü + koordinat**: computer use / CUA mimarisi; model yalnız ekran görüntüsü görür,
  fare ve klavyeyle koordinata göre çalışır (pc-eye'nin vision + input araçlarıyla).

| Sistem | Başarı | Toplam süre | Medyan | Görev başı girdi token | Görev başı $ |
| --- | --- | --- | --- | --- | --- |
| **pc-eye** | **60/60** | **1271 s** | **14.7 s** | **86K** | **$0.0037** |
| wayland-mcp | 45/60 | 2325 s | 26.8 s | 528K | $0.0127 |
| ekran görüntüsü + koordinat | 37/60 | 1503 s | 20.0 s | 121K | $0.0054 |

Ayrım: **GUI görevleri (16)** pc-eye 48/48 · wayland-mcp 43/48 · ekran görüntüsü 37/48.
**Arka plan görevleri (4: servis günlüğü, port, çöken süreç, indirme beklemesi)** pc-eye 12/12 ·
wayland-mcp 2/12 · ekran görüntüsü 0/12 — rakiplerde bu bilgileri okuyan araç yok (gerçek yetenek farkı).

Sınırlar: görevleri biz tasarladık (ev sahibi avantajı olabilir); tek dış rakip; OSWorld alt kümesi
henüz koşulmadı; ekran görüntüsü tabanı Anthropic'in referans uygulaması değil, aynı mimarinin
pc-eye araçlarıyla taklidi.

| Görev | wayland-mcp | pc-eye | ekran görüntüsü |
| --- | --- | --- | --- |
| crash-signal | 0/3  18.2s | 3/3  14.5s | 0/3  18.3s |
| download-wait | 2/3  55.6s | 3/3  14.7s | 0/3  90.3s |
| hard-cross-app | 0/3  18.4s | 3/3  15.0s | 2/3  31.8s |
| hard-dolphin-move | 3/3  75.2s | 3/3  52.8s | 3/3  34.8s |
| hard-kate-save-as | 3/3  31.6s | 3/3  20.0s | 3/3  20.8s |
| hard-process-form | 3/3  21.6s | 3/3  12.0s | 0/3  15.5s |
| hard-settings | 1/3  41.6s | 3/3  22.6s | 0/3  37.0s |
| hard-web-long | 3/3  41.7s | 3/3  12.5s | 3/3  21.1s |
| hard-web-validation | 3/3  39.0s | 3/3  18.6s | 3/3  23.7s |
| hard-wizard | 3/3  35.9s | 3/3  29.3s | 3/3  27.3s |
| live-youtube-play | 3/3 190.1s | 3/3  66.6s | 1/3  55.1s |
| logs-journal | 0/3  18.0s | 3/3  10.1s | 0/3  12.7s |
| ocr-canvas-read | 3/3   7.7s | 3/3  12.3s | 3/3   6.7s |
| recipe-web-form | 3/3  27.4s | 3/3  14.1s | 3/3  22.5s |
| terminal-read | 3/3  12.0s | 3/3   9.6s | 3/3   8.4s |
| ui-click-button | 3/3  12.3s | 3/3  13.1s | 3/3   9.6s |
| ui-form-fill | 3/3  32.3s | 3/3  15.5s | 1/3  24.7s |
| web-form-fill | 3/3  31.8s | 3/3  27.5s | 3/3  20.8s |
| web-table-read | 3/3  10.3s | 3/3   8.8s | 3/3   7.7s |
| xray-port | 0/3  13.7s | 3/3  10.3s | 0/3  13.7s |

Kaynak dosyalar: 20261115-205624-f10r-wayland-mcp-agent.json, 20261115-224353-f10q-agent.json,
20261115-220120-f10x-pixels-agent.json


Aynı makine (CachyOS + Hyprland), aynı görevler, rastgele değerler; başarı uygulamanın gerçek
durumundan denetlenir. Görevler ajana ayrılmış iç içe Hyprland oturumunda koşar.
Ortak görevler (7): hard-cross-app, hard-dolphin-move, hard-kate-save-as, hard-settings, hard-web-long, hard-web-validation, hard-wizard.

| Sistem | Model | Başarı | Medyan süre | Görev başı $ (ort.) | Medyan tur | Ajan sızıntısı |
| --- | --- | --- | --- | --- | --- | --- |
| pc-eye (erişilebilirlik + eylem programı + doğrulama) | claude-haiku-5-5 | 21/21 (100%) | 21.7 s | $0.005 | 10 | 0/21 |
| pc-eye (erişilebilirlik + eylem programı + doğrulama) | claude-sonnet-5-5 | 21/21 (100%) | 24.2 s | $0.049 | 6 | 0/21 |
| piksel tabanı (yalnız ekran görüntüsü + fare/klavye; computer-use/CUA mimarisi) | claude-sonnet-5-5 | 19/21 (90%) | 18.9 s | $0.075 | 10 | 0/21 |

## Görev bazında (başarı · medyan süre · ortalama $)

| Görev | full/haiku-5-5 | full/sonnet-5-5 | pixels/sonnet-5-5 |
| --- | --- | --- | --- |
| hard-cross-app | 3/3 · 11 s · $0.002 | 3/3 · 17 s · $0.036 | 1/3 · 11 s · $0.045 |
| hard-dolphin-move | 3/3 · 51 s · $0.013 | 3/3 · 56 s · $0.085 | 3/3 · 19 s · $0.074 |
| hard-kate-save-as | 3/3 · 23 s · $0.005 | 3/3 · 29 s · $0.049 | 3/3 · 18 s · $0.055 |
| hard-settings | 3/3 · 34 s · $0.004 | 3/3 · 32 s · $0.062 | 3/3 · 27 s · $0.130 |
| hard-web-long | 3/3 · 11 s · $0.002 | 3/3 · 12 s · $0.026 | 3/3 · 14 s · $0.058 |
| hard-web-validation | 3/3 · 17 s · $0.003 | 3/3 · 17 s · $0.040 | 3/3 · 19 s · $0.077 |
| hard-wizard | 3/3 · 24 s · $0.005 | 3/3 · 31 s · $0.043 | 3/3 · 19 s · $0.085 |

Kaynak dosyalar: 20261101-200615-g9h-agent.json, 20261101-200615-g9s-agent.json, 20261101-201745-p2-pixels-agent.json

## Tarayıcı görevleri (köprü vs erişilebilirlik ağacı)

Aynı makine (CachyOS + Hyprland), aynı görevler, rastgele değerler; başarı uygulamanın gerçek
durumundan denetlenir. Görevler ajana ayrılmış iç içe Hyprland oturumunda koşar.
Ortak görevler (6): hard-cross-app, hard-web-long, hard-web-validation, live-youtube-play, web-form-fill, web-table-read.

| Sistem | Model | Başarı | Medyan süre | Görev başı $ (ort.) | Medyan tur | Ajan sızıntısı |
| --- | --- | --- | --- | --- | --- | --- |
| pc-eye (erişilebilirlik + eylem programı + doğrulama) | claude-haiku-5-5 | 21/21 (100%) | 11.9 s | $0.003 | 4 | 0/21 |
| pc-eye + tarayıcı köprüsü (Firefox DOM, pc_eye_browser) | claude-haiku-5-5 | 18/18 (100%) | 9.0 s | $0.003 | 4 | 0/18 |

### Görev bazında (başarı · medyan süre · ortalama $)

| Görev | full/haiku-5-5 | full+bridge/haiku-5-5 |
| --- | --- | --- |
| hard-cross-app | 6/6 · 10 s · $0.003 | 3/3 · 10 s · $0.003 |
| hard-web-long | 3/3 · 9 s · $0.002 | 3/3 · 6 s · $0.002 |
| hard-web-validation | 3/3 · 15 s · $0.003 | 3/3 · 15 s · $0.002 |
| live-youtube-play | 3/3 · 26 s · $0.007 | 3/3 · 41 s · $0.007 |
| web-form-fill | 3/3 · 12 s · $0.002 | 3/3 · 9 s · $0.002 |
| web-table-read | 3/3 · 6 s · $0.001 | 3/3 · 6 s · $0.001 |

Kaynak dosyalar: 20261115-004335-aga-merged-agent.json, 20261115-004335-agb-bridge-agent.json

Not: Araç seviyesinde (probe, model yok) köprü form doldurmayı 2858 → 824 ms, doğrulamalı formu
3133 → 1015 ms, 150 satırlık listede aramayı 143 → 30 ms, tablo okumayı 56 → 15 ms'ye indirdi.
Ajan süresine model düşünme süresi hâkim; köprü kazancı en çok uzun sayfada ve formda görünür.
YouTube'da köprülü koşu yavaş: Shorts sayfasında birden çok `video` öğesi var, ajan oynayanı
bulana kadar ek tur harcadı (başarı etkilenmedi).
