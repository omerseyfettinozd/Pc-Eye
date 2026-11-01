# Kıyas tablosu

Aynı makine (CachyOS + Hyprland), aynı görevler, rastgele değerler; başarı uygulamanın gerçek
durumundan denetlenir. Görevler ajana ayrılmış iç içe Hyprland oturumunda koşar.
Ortak görevler (7): hard-cross-app, hard-dolphin-move, hard-kate-save-as, hard-settings, hard-web-long, hard-web-validation, hard-wizard.

| Sistem | Model | Başarı | Medyan süre | Görev başı $ (ort.) | Medyan tur | Ajan sızıntısı |
| --- | --- | --- | --- | --- | --- | --- |
| pc-eye (erişilebilirlik + eylem programı + doğrulama) | claude-haiku-5-5 | 21/21 (100%) | 26.8 s | $0.005 | 8 | 0/21 |
| pc-eye (erişilebilirlik + eylem programı + doğrulama) | claude-sonnet-5-5 | 21/21 (100%) | 26.6 s | $0.067 | 7 | 0/21 |
| piksel tabanı (yalnız ekran görüntüsü + fare/klavye; computer-use/CUA mimarisi) | claude-sonnet-5-5 | 12/14 (86%) | 17.1 s | $0.096 | 11 | 0/14 |

## Görev bazında (başarı · medyan süre · ortalama $)

| Görev | full/haiku-5-5 | full/sonnet-5-5 | pixels/sonnet-5-5 |
| --- | --- | --- | --- |
| hard-cross-app | 3/3 · 19 s · $0.003 | 3/3 · 17 s · $0.037 | 1/2 · 13 s · $0.058 |
| hard-dolphin-move | 3/3 · 66 s · $0.011 | 3/3 · 36 s · $0.131 | 1/2 · 20 s · $0.084 |
| hard-kate-save-as | 3/3 · 35 s · $0.004 | 3/3 · 26 s · $0.085 | 2/2 · 15 s · $0.064 |
| hard-settings | 3/3 · 26 s · $0.003 | 3/3 · 54 s · $0.071 | 2/2 · 25 s · $0.137 |
| hard-web-long | 3/3 · 9 s · $0.002 | 3/3 · 16 s · $0.026 | 2/2 · 24 s · $0.162 |
| hard-web-validation | 3/3 · 25 s · $0.003 | 3/3 · 20 s · $0.039 | 2/2 · 15 s · $0.088 |
| hard-wizard | 3/3 · 45 s · $0.006 | 3/3 · 60 s · $0.079 | 2/2 · 18 s · $0.080 |

Kaynak dosyalar: 20261101-181842-g4h-agent.json, 20261101-181842-g4s-agent.json, 20261101-150617-f3p-pixels-agent.json
