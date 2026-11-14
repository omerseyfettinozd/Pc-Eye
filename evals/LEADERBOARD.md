# Kıyas tablosu

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
