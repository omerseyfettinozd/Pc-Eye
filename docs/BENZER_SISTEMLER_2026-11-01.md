# Benzer Sistemler ve Alınacak Fikirler (2026-11-01)

Amaç: Pc-Eye'ı "en iyi" yapmak için başkalarının çözdüğünü yeniden icat etmemek.
Rakamlar çoğunlukla ikincil kaynaklardan; her biri kendi repo/makalesinden
doğrulanmadan iddia olarak kullanılmamalı. Kod alınacaksa önce LICENSE okunur.

## Kim ne yapıyor

| Sistem | Ne | Bizim için ders | Lisans |
| --- | --- | --- | --- |
| OpenAI Codex computer use (Windows, 2026-05) | Ekran + tıklama + yazma; Windows'ta ön planda, kullanıcı aynı anda çalışamaz | Sanal ekranımız (headless) bunun önünde; korunmalı | Kapalı |
| Microsoft UFO² ("Desktop AgentOS") | HostAgent + uygulamaya özel AppAgent; UIA + görsel (OmniParser) karma algı; GUI ile uygulama API'sini birleştiren eylem katmanı; spekülatif çoklu eylem; PiP yalıtılmış masaüstü | Uygulama adaptörleri (API önce, tıklama sonra); tek LLM turunda birden çok eylem | MIT |
| Windows-MCP (CursorTouch) | Tek `State-Tool` çağrısı: açık uygulamalar + etkileşimli/metin/kaydırılabilir elemanlar + ekran görüntüsü; tarayıcıda `use_dom` | Bizde aynı bilgi `context` + `ui` + `vision` ile 3–5 çağrı: birleşik **snapshot** aracı | MIT |
| Playwright MCP | Ekran görüntüsü yerine erişilebilirlik anlık görüntüsü; resmi tabloya göre ~200–400 token'a karşı ~3–5K | Tarayıcı katmanı (DOM/erişilebilirlik ağacı); ilk ajan ölçümünde 36 base64 görüntü vardı | Apache-2.0 |
| Agent S3 (Simular) | Planlayıcı model + ayrı **grounding** modeli (UI-TARS-1.5-7B); Behavior Best-of-N ile OSWorld'de insan seviyesi iddiası | Konum bulmayı küçük yerel bir modele devretmek (RTX 5070'de denenebilir) | Açık kaynak (LICENSE doğrulanmalı) |
| UI-TARS (ByteDance) | Ekran kullanımı için eğitilmiş görsel model + masaüstü uygulaması | Yerel grounding adayı | Doğrulanmalı |
| OmniParser v2 (Microsoft) | Ekrandan ikon/kontrol çıkarımı | Gürültülü ızgara ikon dedektörümüzün yerine; **icon_detect AGPL**, icon_caption MIT → gömmek yerine isteğe bağlı harici servis | Karma |
| Agent Workflow Memory (ICML 2025) | Başarılı yürütmelerden değer yerine yer tutucu koyarak yeniden kullanılabilir iş akışı çıkarır | Ölçüm dökümlerimiz zaten kayıtlı: başarılı koşulardan uygulama tarifleri | Makale |
| trycua/cua | Sandbox VM/konteyner, ajan çerçevesi, `cua-bench` (OSWorld, ScreenSpot, Windows Arena) | Dış kıyas için OSWorld alt kümesi; kendi ölçüm setimizin yanına | MIT |
| Linux akranları: linux-desktop-mcp, wayland-computer-use-mcp, kwin-mcp, GNOME UI MCP, hypruse, computer-use-linux (Rust) | AT-SPI + Wayland girdi; çoğu ağaç-önce karma | Hiçbiri süreç/port/günlük/çökme/ses beyaz kutusunu ve sanal ekranı birlikte sunmuyor: ayrışma noktamız. computer-use-linux çok kompozitör desteğiyle örnek | Çoğu MIT |

## Pc-Eye'ın durduğu yer

Önde: sanal ekranda kullanıcıyla eşzamanlı çalışma, arka plan beyaz kutusu
(süreç, port, journald, coredump, ses), ölçüm seti ve eylem doğrulaması.

Geride: tarayıcı içi (DOM), tek çağrılık durum özeti, uygulama API adaptörleri,
öğrenilmiş tarifler, ikon/canvas için güçlü grounding, dış benchmark.

## Önerilen sıra (her biri ölçüm setiyle önce/sonra tartılır)

1. **`snapshot` aracı** (Windows-MCP): pencereler + odak pencerenin etkileşimli elemanları + isteğe bağlı küçük görüntü tek çağrıda. Hedef: medyan tur ve girdi token'ı düşsün.
2. **Tarayıcı katmanı** (Playwright MCP / UFO² API-önce): Firefox WebDriver BiDi ile erişilebilirlik/DOM anlık görüntüsü ve eleman bazlı eylem. Yeni ölçüm görevleri: form, tablo okuma, sekme.
3. **Tarif hafızası** (AWM): başarılı ajan dökümlerinden uygulama başına iş akışı; MCP `recipes` ile sunulur.
4. **İsteğe bağlı grounding** (Agent S3 / OmniParser / UI-TARS): AT-SPI ve OCR boş kaldığında yerel model; AGPL bileşen gömülmez.
5. **Dış kıyas**: cua-bench ile küçük bir OSWorld alt kümesi.

## Kaynaklar

- Codex computer use: https://www.neowin.net/news/openai-rolls-out-major-codex-for-windows-update-with-computer-use-and-mobile-access/ , https://gigazine.net/gsc_news/en/20260601-codex-windows-computer-use
- UFO²: https://arxiv.org/html/2504.14603v2 , https://www.microsoft.com/en-us/research/publication/ufo2-the-desktop-agentos/
- Windows-MCP: https://mcpservers.org/servers/CursorTouch/Windows-MCP
- Playwright MCP snapshots: https://playwright.dev/mcp/snapshots
- Agent S: https://github.com/sbc-learn/Agent-S , https://pypi.org/project/gui-agents
- OSWorld sıralamaları (çelişkili): https://leaderboard.steel.dev/leaderboards/osworld/ , https://platform.simular.ai/compare/open-source-computer-use-agents
- OmniParser v2: https://replicate.com/microsoft/omniparser-v2/readme
- Agent Workflow Memory: https://arxiv.org/pdf/2409.07429
- trycua/cua: https://github.com/trycua/cua
- Linux MCP sunucuları: https://pypi.org/project/linux-desktop-mcp/ , https://pypi.org/project/wayland-computer-use-mcp/ , https://pypi.org/project/kwin-mcp-server/ , https://docs.rs/crate/computer-use-linux/latest/source/npm/README.md , https://pypi.org/project/hypruse/
