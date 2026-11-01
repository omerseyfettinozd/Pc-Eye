# Kıyas tablosu

Aynı makine (CachyOS + Hyprland), aynı görevler, rastgele değerler; başarı uygulamanın gerçek
durumundan denetlenir. Görevler ajana ayrılmış iç içe Hyprland oturumunda koşar.
Ortak görevler (7): hard-cross-app, hard-dolphin-move, hard-kate-save-as, hard-settings, hard-web-long, hard-web-validation, hard-wizard.

| Sistem | Model | Başarı | Medyan süre | Görev başı $ (ort.) | Medyan tur | Ajan sızıntısı |
| --- | --- | --- | --- | --- | --- | --- |
| pc-eye (erişilebilirlik + eylem programı + doğrulama) | claude-haiku-5-5 | 20/21 (95%) | 22.7 s | $0.006 | 7 | 0/21 |
| pc-eye (erişilebilirlik + eylem programı + doğrulama) | claude-sonnet-5-5 | 19/21 (90%) | 19.4 s | $0.062 | 8 | 0/21 |
| piksel tabanı (yalnız ekran görüntüsü + fare/klavye; computer-use/CUA mimarisi) | claude-sonnet-5-5 | 12/14 (86%) | 17.1 s | $0.096 | 11 | 0/14 |

## Görev bazında (başarı · medyan süre · ortalama $)

| Görev | full/haiku-5-5 | full/sonnet-5-5 | pixels/sonnet-5-5 |
| --- | --- | --- | --- |
| hard-cross-app | 3/3 · 13 s · $0.003 | 3/3 · 15 s · $0.037 | 1/2 · 13 s · $0.058 |
| hard-dolphin-move | 2/3 · 45 s · $0.015 | 1/3 · 30 s · $0.098 | 1/2 · 20 s · $0.084 |
| hard-kate-save-as | 3/3 · 21 s · $0.004 | 3/3 · 18 s · $0.084 | 2/2 · 15 s · $0.064 |
| hard-settings | 3/3 · 30 s · $0.004 | 3/3 · 29 s · $0.064 | 2/2 · 25 s · $0.137 |
| hard-web-long | 3/3 · 16 s · $0.005 | 3/3 · 13 s · $0.040 | 2/2 · 24 s · $0.162 |
| hard-web-validation | 3/3 · 23 s · $0.003 | 3/3 · 19 s · $0.038 | 2/2 · 15 s · $0.088 |
| hard-wizard | 3/3 · 93 s · $0.006 | 3/3 · 98 s · $0.072 | 2/2 · 18 s · $0.080 |

Kaynak dosyalar: 20261101-150617-f3h-agent.json, 20261101-150617-f3s-agent.json, 20261101-150617-f3p-pixels-agent.json
