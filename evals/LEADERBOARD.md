# Kıyas tablosu

Aynı makine (CachyOS + Hyprland), aynı görevler, rastgele değerler; başarı uygulamanın gerçek
durumundan denetlenir. Görevler ajana ayrılmış iç içe Hyprland oturumunda koşar.
Ortak görevler (7): hard-cross-app, hard-dolphin-move, hard-kate-save-as, hard-settings, hard-web-long, hard-web-validation, hard-wizard.

| Sistem | Model | Başarı | Medyan süre | Görev başı $ (ort.) | Medyan tur | Ajan sızıntısı |
| --- | --- | --- | --- | --- | --- | --- |
| pc-eye (erişilebilirlik + eylem programı + doğrulama) | claude-haiku-5-5 | 21/21 (100%) | 22.5 s | $0.005 | 8 | 0/21 |
| pc-eye (erişilebilirlik + eylem programı + doğrulama) | claude-sonnet-5-5 | 21/21 (100%) | 21.9 s | $0.077 | 8 | 0/21 |
| piksel tabanı (yalnız ekran görüntüsü + fare/klavye; computer-use/CUA mimarisi) | claude-sonnet-5-5 | 12/14 (86%) | 17.1 s | $0.096 | 11 | 0/14 |

## Görev bazında (başarı · medyan süre · ortalama $)

| Görev | full/haiku-5-5 | full/sonnet-5-5 | pixels/sonnet-5-5 |
| --- | --- | --- | --- |
| hard-cross-app | 3/3 · 16 s · $0.002 | 3/3 · 22 s · $0.042 | 1/2 · 13 s · $0.058 |
| hard-dolphin-move | 3/3 · 64 s · $0.017 | 3/3 · 68 s · $0.229 | 1/2 · 20 s · $0.084 |
| hard-kate-save-as | 3/3 · 23 s · $0.005 | 3/3 · 33 s · $0.102 | 2/2 · 15 s · $0.064 |
| hard-settings | 3/3 · 26 s · $0.003 | 3/3 · 41 s · $0.050 | 2/2 · 25 s · $0.137 |
| hard-web-long | 3/3 · 12 s · $0.003 | 3/3 · 10 s · $0.026 | 2/2 · 24 s · $0.162 |
| hard-web-validation | 3/3 · 21 s · $0.003 | 3/3 · 17 s · $0.044 | 2/2 · 15 s · $0.088 |
| hard-wizard | 3/3 · 29 s · $0.004 | 3/3 · 23 s · $0.045 | 2/2 · 18 s · $0.080 |

Kaynak dosyalar: 20261101-194220-g8h-agent.json, 20261101-191943-g7s-agent.json, 20261101-150617-f3p-pixels-agent.json
