#!/usr/bin/env python3
"""PC-Eye ölçüm seti koşucusu.

İki katman:
  probe  Araç seviyesi: görev sahnesini kurar, `pc-eye` CLI adımlarını bilinen
         hedeflerle çalıştırır; algı/eylem doğruluğunu ve gecikmeyi ölçer. Ücretsiz.
  agent  Uçtan uca: aynı sahnede `claude -p` ajanına yalnız pc_eye_* araçlarını
         verip görevi doğal dille ister; başarıyı gerçek durumdan (uygulama çıktısı
         veya CEVAP satırı) denetler, tur/araç/token/maliyet kaydeder. Ücretlidir.

Tüm sahneler kullanıcının ekranından yalıtılmış `PCEYE-EVAL` sanal ekranında açılır.

    python3 evals/run_evals.py list
    python3 evals/run_evals.py probe
    python3 evals/run_evals.py agent --task ui-click-button --repeat 3 --model claude-sonnet-5-5
"""

import argparse
import html
import http.server
import json
import os
import random
import re
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent
TASKS_DIR = ROOT / "tasks"
RESULTS_DIR = ROOT / "results"
FIXTURE = ROOT / "fixtures" / "eval_app.py"
# Kurulu binary yerine yeni bir derlemeyi ölçmek için: PCEYE_BIN=target/release/pc-eye
PCEYE = os.environ.get("PCEYE_BIN") or shutil.which("pc-eye") or str(Path.home() / ".local/bin/pc-eye")
PCEYE = str(Path(PCEYE).resolve())
DISPLAY_NAME = "PCEYE-EVAL"
DISPLAY_MODE = "1600x900@30"

# Türkçe karakterli, birbirine benzemeyen düğme etiketleri.
WORD_POOL = [
    "Kırmızı", "Yeşil", "Şeftali", "Mavi", "Gökyüzü", "Çınar", "Üzüm", "Ördek",
    "Ilgaz", "İncir", "Dağ", "Bulut", "Kestane", "Sarı", "Lacivert", "Fıstık",
]
FIRST_NAMES = ["Şükrü", "Gülşen", "İlkay", "Çağrı", "Öykü", "Ümit", "Ilgın", "Doğan"]
LAST_NAMES = ["Öztürk", "Çelik", "Şahin", "Güneş", "Yıldız", "Doğru", "Işık", "Ağaoğlu"]
CITIES = ["İstanbul", "Ankara", "İzmir", "Muğla", "Çanakkale", "Şanlıurfa"]
# OCR'da karışan karakterler (O/0, I/1, S/5, B/8) dışarıda.
TOKEN_CHARS = "ACDEFGHJKLMNPRTUVWXY2346789"
CRASH_SIGNALS = ["SEGV", "ABRT", "BUS", "FPE", "ILL"]

# Rakip kıyası: Anthropic computer use / OpenAI CUA mimarisi — model yalnız ekran görüntüsü görür,
# fare ve klavyeyle koordinata göre çalışır. Aynı model, aynı görevler; yalnız araç katmanı farklı.
PIXEL_TOOLS = ["mcp__pc-eye__pc_eye_vision", "mcp__pc-eye__pc_eye_input"]
ALL_TOOLS = ["audio", "click_ui", "context", "director", "display", "input", "launch", "logs", "processes",
             "run", "snapshot", "ui", "vision", "wait", "window", "xray"]
PIXEL_SYSTEM = (
    " Yalnız ekran görüntüsü (pc_eye_vision, pencere vermeden: tüm ekran) ve fare/klavye (pc_eye_input) "
    "kullanabilirsin; koordinatlar ekran görüntüsündeki piksel koordinatlarıdır."
)

AGENT_SYSTEM = (
    "Bir PC-Eye ölçüm görevindesin. Bilgisayarı yalnız pc_eye_* araçlarıyla kullan. "
    "Görev uygulaması kullanıcının ekranından yalıtılmış `{display}` sanal ekranında, "
    "{workspace} numaralı çalışma alanında açık; bu çalışma alanı dışındaki pencerelere "
    "tıklama ya da yazma. Görevi bitirince yanıtının son satırına `CEVAP: <değer>` yaz; "
    "sorulan bir değer yoksa `CEVAP: tamam` yaz."
)


# ---------------------------------------------------------------------------
# pc-eye ve sanal ekran
# ---------------------------------------------------------------------------

@dataclass
class CallResult:
    rc: int
    ms: float
    stdout: str
    stderr: str

    def json(self):
        try:
            return json.loads(self.stdout)
        except ValueError:
            return None


def pceye(*args, timeout=30, env=None):
    start = time.perf_counter()
    try:
        p = subprocess.run([PCEYE, *args], capture_output=True, text=True, timeout=timeout,
                           env={**os.environ, **env} if env else None)
        rc, out, err = p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired as e:
        out = e.stdout.decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
        rc, err = 124, f"zaman aşımı ({timeout} sn)"
    return CallResult(rc, (time.perf_counter() - start) * 1000, out, err)


# `--session` modunda sahneler ajan oturumunun Hyprland'ındadır; kullanıcı denetimleri dış örnekte kalır.
SCENE_HYPR_SIG = None


def hyprctl_json(*args, scene=False):
    inst = ["--instance", SCENE_HYPR_SIG] if scene and SCENE_HYPR_SIG else []
    out = subprocess.run(["hyprctl", *inst, "-j", *args], capture_output=True, text=True).stdout
    return json.loads(out or "null")


class Sandbox:
    """Kullanıcının ekranına bitişik olmayan headless çıkış ve orada açılan süreçler."""

    def __init__(self, session=None):
        self.session = session
        self.label = DISPLAY_NAME
        self.workspace = None
        self.pids = []
        self.cleanups = []
        self.user_cursor = None
        self.user_window = None

    def __enter__(self):
        if self.session:
            return self._enter_session()
        if any(m["name"] == DISPLAY_NAME for m in hyprctl_json("monitors", "all")):
            pceye("display", "remove", "--name", DISPLAY_NAME)
        r = pceye("display", "create", "--name", DISPLAY_NAME, "--mode", DISPLAY_MODE)
        receipt = r.json()
        if r.rc != 0 or not receipt:
            raise RuntimeError(f"sanal ekran açılamadı: {r.stderr or r.stdout}")
        details = json.loads(receipt["details"])
        # Adlı çalışma alanı (numara kullanıcının Super+N kısayollarıyla çakışıyordu).
        ws_name = details.get("workspace_name") or ""
        self.workspace = f"name:{ws_name}" if details.get("workspace_id", 0) < 0 and ws_name else \
            str(details["workspace_id"])
        return self

    def _enter_session(self):
        global SCENE_HYPR_SIG
        os.environ.pop("PC_EYE_SESSION", None)
        r = pceye("session", "start", self.session)
        info = r.json()
        if r.rc != 0 or not info:
            raise RuntimeError(f"ajan oturumu açılamadı: {r.stderr or r.stdout}")
        SCENE_HYPR_SIG = info["signature"]
        # Bundan sonra her pc-eye çağrısı (CLI, ajanın MCP sunucusu) oturumun içindedir.
        os.environ["PC_EYE_SESSION"] = self.session
        self.workspace = "1"
        self.label = f"ajan oturumu {self.session}"
        return self

    def __exit__(self, *exc):
        global SCENE_HYPR_SIG
        self.kill_all()
        if self.session:
            os.environ.pop("PC_EYE_SESSION", None)
            SCENE_HYPR_SIG = None
            pceye("session", "stop", self.session)
            return
        pceye("display", "remove", "--name", DISPLAY_NAME)

    def remember_user_focus(self):
        self.user_cursor = hyprctl_json("cursorpos")
        active = hyprctl_json("activewindow") or {}
        self.user_window = active.get("address")

    def user_disturbed(self):
        """Görev kullanıcının imlecini ya da odağını değiştirdi mi (yalnız ajan oturumunda anlamlı).

        Değişim varsa ayrıntı döner: kullanıcı o sırada bilgisayarı kullanıyorsa imleç kendi
        ekranında kalır; ajan sızıntısı olsaydı imleç oturumun headless çıkışına giderdi.
        """
        cursor = hyprctl_json("cursorpos")
        active = hyprctl_json("activewindow") or {}
        # Pencere yerleşimi sızıntısı: kullanıcının penceresi görünmez çıkışta ya da oturum penceresi
        # (aquamarine) kullanıcının ekranında (numaralı çalışma alanı karışıklığında ikisi de oldu).
        monitors = {m["id"]: m["name"] for m in hyprctl_json("monitors") or []}
        misplaced = []
        for c in hyprctl_json("clients") or []:
            on_agent = monitors.get(c.get("monitor"), "").startswith("PCEYE-")
            if c.get("class") == "aquamarine" and not on_agent:
                misplaced.append(f"oturum penceresi kullanıcı ekranında (ws {c['workspace']['name']})")
            elif on_agent and c.get("class") != "aquamarine" and c.get("pid") not in self.pids:
                misplaced.append(f"kullanıcı penceresi görünmez çıkışta: {c.get('class')}")
        if misplaced:
            return {"misplaced": misplaced, "agent_leak": True}
        if cursor == self.user_cursor and active.get("address") == self.user_window:
            return False
        # Sızıntı: imleç kullanıcının (headless olmayan) ekranlarının dışına, ya da odak oturumun
        # penceresine geçti. Aksi halde değişim kullanıcının kendi kullanımıdır.
        real = [m for m in hyprctl_json("monitors") or [] if not m["name"].startswith(("PCEYE-", "HEADLESS"))]
        on_user_screen = any(m["x"] <= cursor["x"] < m["x"] + m["width"] and m["y"] <= cursor["y"] < m["y"] + m["height"]
                             for m in real)
        leak = not on_user_screen or active.get("class") == "aquamarine" or \
            str(active.get("title", "")).startswith("Eval ")
        return {"cursor": [self.user_cursor, cursor], "active_class": active.get("class"),
                "active_changed": active.get("address") != self.user_window, "agent_leak": leak}

    def restore_user_focus(self):
        # Ajan oturumunda geri verilecek bir şey olmamalı; ölçüm dürüst kalsın diye düzeltilmez.
        if self.session:
            return
        # Görev, imleci ve odağı sanal ekrana taşımış olabilir; kullanıcıya geri ver.
        if self.user_window:
            subprocess.run(["hyprctl", "dispatch", "focuswindow", f"address:{self.user_window}"],
                           capture_output=True)
        if self.user_cursor:
            subprocess.run(["hyprctl", "dispatch", "movecursor",
                            str(self.user_cursor["x"]), str(self.user_cursor["y"])],
                           capture_output=True)

    def launch_window(self, cmd, env=None, fullscreen=True, timeout_ms=5000):
        r = pceye("launch", "--workspace", self.workspace, "--timeout-ms", str(timeout_ms), "--", *cmd,
                  timeout=timeout_ms / 1000 + 20, env=env)
        receipt = r.json() or {}
        m = re.search(r"address=(0x[0-9a-f]+) pid=(\d+)", receipt.get("target", ""))
        if not m:
            raise RuntimeError(f"pencere açılmadı: {receipt.get('target') or r.stderr}")
        address, pid = m.group(1), int(m.group(2))
        self.pids.append(pid)
        # Çok pencereli sahnelerde tam ekran yok: pencereler yan yana döşenir, ajan ikisini de görür.
        if fullscreen:
            pceye("window", "fullscreen", "--window", address, "--arg", "on")
        pceye("wait", "visual_settle", "--timeout-ms", "2000")
        return address, pid

    def spawn(self, cmd, **kw):
        p = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                             stderr=subprocess.DEVNULL, start_new_session=True, **kw)
        self.pids.append(p.pid)
        return p

    def kill_all(self):
        def send(pid, sig):
            try:
                os.killpg(pid, sig)
            except (ProcessLookupError, PermissionError):
                try:
                    os.kill(pid, sig)
                except ProcessLookupError:
                    pass

        for pid in self.pids:
            send(pid, signal.SIGTERM)
        # Bir sonraki sahne eski süreci (ve AT-SPI kaydını) görmesin diye ölmesini bekle.
        deadline = time.time() + 2
        while time.time() < deadline and any(Path(f"/proc/{p}").exists() for p in self.pids):
            time.sleep(0.05)
        for pid in self.pids:
            if Path(f"/proc/{pid}").exists():
                send(pid, signal.SIGKILL)
        self.pids.clear()
        for fn in self.cleanups:
            fn()
        self.cleanups.clear()


# ---------------------------------------------------------------------------
# Sahneler: her biri rastgele değişkenler üretir ve görev ortamını kurar
# ---------------------------------------------------------------------------

@dataclass
class Scene:
    vars: dict
    out: Path | None = None
    extra: dict = field(default_factory=dict)


def token(rng, n=6):
    return "".join(rng.choice(TOKEN_CHARS) for _ in range(n))


def fixture_cmd(scene, out, title, **opts):
    cmd = [sys.executable, str(FIXTURE), "--scene", scene, "--out", str(out), "--title", title]
    for k, v in opts.items():
        cmd += [f"--{k}", v]
    return cmd


def scene_buttons(sb, rng, work):
    labels = rng.sample(WORD_POOL, 6)
    target = rng.choice(labels)
    out = work / "out.json"
    title = f"Eval Düğmeler {token(rng, 4)}"
    window, pid = sb.launch_window(fixture_cmd("buttons", out, title, labels="|".join(labels)))
    return Scene({"target": target, "window": window, "pid": pid, "title": title}, out)


def scene_form(sb, rng, work):
    name = f"{rng.choice(FIRST_NAMES)} {rng.choice(LAST_NAMES)}"
    cities = rng.sample(CITIES, 4)
    city = rng.choice(cities[1:])  # ilk seçenek varsayılan; değiştirmek zorunlu olsun
    out = work / "out.json"
    title = f"Eval Form {token(rng, 4)}"
    window, pid = sb.launch_window(fixture_cmd("form", out, title, cities="|".join(cities)))
    # GTK4 açılır listesi AT-SPI'da etiketiyle değil seçili değeriyle adlanır.
    return Scene({"name": name, "city": city, "default_city": cities[0],
                  "window": window, "pid": pid, "title": title}, out)


def scene_canvas(sb, rng, work):
    text = f"{token(rng, 3)}-{token(rng, 4)}"
    title = f"Eval Tuval {token(rng, 4)}"
    window, pid = sb.launch_window(fixture_cmd("canvas", work / "out.json", title, text=text))
    win = next(c for c in hyprctl_json("clients", scene=True) if c["address"] == window)
    return Scene({"token": text, "window": window, "pid": pid, "title": title, "win_x": win["at"][0]})


def scene_terminal(sb, rng, work):
    secret = token(rng, 8)
    title = f"Eval Terminal {token(rng, 4)}"
    script = f'printf "Dağıtım anahtarı: %s\\n" {secret}; exec sleep 3600'
    window, pid = sb.launch_window(["kitty", "--title", title, "sh", "-c", script])
    return Scene({"token": secret, "window": window, "pid": pid, "title": title})


def free_port(rng):
    while True:
        port = rng.randint(20000, 60000)
        with socket.socket() as s:
            if s.connect_ex(("127.0.0.1", port)) != 0:
                return port


def scene_port(sb, rng, work):
    marker = f"eval-svc-{token(rng, 4).lower()}"
    port = free_port(rng)
    code = ("import socket,time;s=socket.socket();"
            f"s.bind(('127.0.0.1',{port}));s.listen();time.sleep(3600)")
    p = sb.spawn([sys.executable, "-c", code, marker])
    deadline = time.time() + 3
    while time.time() < deadline:
        with socket.socket() as s:
            if s.connect_ex(("127.0.0.1", port)) == 0:
                break
        time.sleep(0.05)
    return Scene({"marker": marker, "port": str(port), "pid": p.pid})


def scene_journal(sb, rng, work):
    tag = f"eval-{token(rng, 4).lower()}"
    code = token(rng, 8)
    subprocess.run(["logger", "-p", "user.err", "-t", tag, f"rapor kodu {code}"], check=True)
    time.sleep(0.5)
    return Scene({"tag": tag, "token": code})


def scene_crash(sb, rng, work):
    sig = rng.choice(CRASH_SIGNALS)
    # comm 15 karakterle sınırlı: "eval-crash-" + 4 = 15.
    name = f"eval-crash-{token(rng, 4).lower()}"
    script = work / name
    script.write_text(f"#!/bin/sh\nkill -{sig} $$\n")
    script.chmod(0o755)
    p = subprocess.Popen([str(script)])
    p.wait()
    # systemd-coredump kaydı birkaç saniye sürebilir; ölçülen şey pc-eye olsun, kayıt gecikmesi değil.
    deadline = time.time() + 8
    while time.time() < deadline:
        # `-r` şart: `-n` tek başına journal'ın en eski kayıtlarını verir (pc-eye'da düzeltilen hata).
        out = subprocess.run(["coredumpctl", "list", "--json=short", "-r", "-n", "10", "--no-pager"],
                             capture_output=True, text=True).stdout
        if f'"pid":{p.pid},' in out.replace(" ", ""):
            break
        time.sleep(0.25)
    return Scene({"marker": name, "signal": f"SIG{sig}", "signal_short": sig, "crash_pid": p.pid,
                  "signal_num": signal.Signals[f"SIG{sig}"].value})


WEB_DIR = ROOT / "fixtures" / "web"
# Geçici profilde ilk açılış sekmeleri ve uyarılar kapalı: sahne yalnız test sayfasını göstersin.
FIREFOX_PREFS = """
user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("datareporting.policy.firstRunURL", "");
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
user_pref("browser.tabs.warnOnClose", false);
user_pref("browser.sessionstore.resume_from_crash", false);
user_pref("browser.translations.automaticallyPopup", false);
user_pref("browser.translations.panelShown", true);
"""


def serve_page(sb, page_html, out_path):
    """Sayfayı 127.0.0.1'de sunar; `POST /submit` gövdesini `out_path`e yazar (gerçek durum denetimi)."""
    body = page_html.encode("utf-8")

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            data = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            out_path.write_bytes(data)
            self.send_response(204)
            self.end_headers()

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    sb.cleanups.append(server.shutdown)
    return f"http://127.0.0.1:{server.server_address[1]}/"


def open_in_firefox(sb, work, url, ready_text, fullscreen=True):
    profile = work / "ffprofile"
    profile.mkdir(exist_ok=True)
    (profile / "user.js").write_text(FIREFOX_PREFS)
    window, pid = sb.launch_window(["firefox", "--new-instance", "--profile", str(profile), url],
                                   fullscreen=fullscreen)
    pceye("wait", "ui_match", "--window", window, "--match", ready_text, "--timeout-ms", "8000")
    return window, pid


def scene_web_form(sb, rng, work):
    name = f"{rng.choice(FIRST_NAMES)} {rng.choice(LAST_NAMES)}"
    cities = rng.sample(CITIES, 4)
    city = rng.choice(cities[1:])
    title = f"Eval Web Form {token(rng, 4)}"
    options = "".join(f"<option>{html.escape(c)}</option>" for c in cities)
    page = (WEB_DIR / "form.html").read_text(encoding="utf-8")
    page = page.replace("{title}", title).replace("{options}", options)
    out = work / "out.json"
    window, pid = open_in_firefox(sb, work, serve_page(sb, page, out), "Sipariş formu")
    return Scene({"name": name, "city": city, "default_city": cities[0], "title": title,
                  "window": window, "pid": pid}, out)


def scene_web_table(sb, rng, work):
    items = rng.sample(["Kalem", "Defter", "Silgi", "Cetvel", "Pergel", "Makas", "Boya", "Zımba"], 6)
    prices = {it: f"{rng.randint(3, 400)},{rng.randint(0, 99):02d} TL" for it in items}
    target = rng.choice(items)
    title = f"Eval Fiyatlar {token(rng, 4)}"
    rows = "".join(
        f"<tr><td>{html.escape(it)}</td><td>{rng.randint(0, 50)}</td><td>{prices[it]}</td></tr>"
        for it in items
    )
    page = (WEB_DIR / "table.html").read_text(encoding="utf-8")
    page = page.replace("{title}", title).replace("{rows}", rows)
    window, pid = open_in_firefox(sb, work, serve_page(sb, page, work / "out.json"), "Fiyat listesi")
    return Scene({"item": target, "price": prices[target], "title": title,
                  "window": window, "pid": pid})


# KDE uygulamaları kullanıcının ayarlarını, son oturumlarını ve açık sekmelerini yüklemesin;
# Qt erişilebilirliği her zaman açık.
def isolated_env(work):
    base = work / "xdg"
    env = {"QT_LINUX_ACCESSIBILITY_ALWAYS_ON": "1", "QT_ACCESSIBILITY": "1"}
    for var, sub in (("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"),
                     ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state")):
        (base / sub).mkdir(parents=True, exist_ok=True)
        env[var] = str(base / sub)
    return env


CUSTOMERS = ["Aslı Demir", "Burak Yıldız", "Cemre Aksoy", "Deniz Kara", "Emre Şahin", "Fulya Koç",
             "Gökhan Er", "Hande Çelik"]


def scene_cross_app(sb, rng, work):
    customers = rng.sample(CUSTOMERS, 6)
    rows = [(f"S-{rng.randint(10000, 99999)}", c, f"{rng.randint(100, 9999)},{rng.randint(0, 99):02d} TL")
            for c in customers]
    order, customer, amount = rng.choice(rows)
    src = f"Eval Siparişler {token(rng, 4)}"
    sb.launch_window(fixture_cmd("orders", work / "orders.json", src, rows="|".join(";".join(r) for r in rows)),
                     fullscreen=False)
    dst = f"Eval İade {token(rng, 4)}"
    page = (WEB_DIR / "transfer.html").read_text(encoding="utf-8").replace("{title}", dst)
    out = work / "out.json"
    window, pid = open_in_firefox(sb, work, serve_page(sb, page, out), "İade talebi", fullscreen=False)
    return Scene({"src": src, "dst": dst, "customer": customer, "order": order, "amount": amount,
                  "window": window, "pid": pid, "title": dst}, out)


def scene_process_form(sb, rng, work):
    port_scene = scene_port(sb, rng, work)
    title = f"Eval PID {token(rng, 4)}"
    out = work / "out.json"
    window, pid = sb.launch_window(fixture_cmd("pidform", out, title))
    return Scene({**port_scene.vars, "title": title, "window": window, "target_pid": str(port_scene.vars["pid"])},
                 out)


def scene_settings(sb, rng, work):
    themes = ["Açık", "Koyu", "Sistem", "Yüksek karşıtlık"]
    username = f"kullanici{rng.randint(10, 99)}"
    font = rng.choice([s for s in range(9, 25) if s != 11])
    theme = rng.choice(themes[1:])
    title = f"Eval Ayarlar {token(rng, 4)}"
    out = work / "out.json"
    window, pid = sb.launch_window(fixture_cmd("settings", out, title, cities="|".join(themes), text=username))
    return Scene({"title": title, "window": window, "pid": pid, "username": username,
                  "font": font, "theme": theme}, out)


def scene_web_validate(sb, rng, work):
    digits = f"5{rng.randint(10, 59)}{rng.randint(1000000, 9999999)}"
    pretty = f"0{digits[:3]} {digits[3:6]} {digits[6:8]} {digits[8:]}"
    email = f"{rng.choice(['ali', 'ece', 'can', 'naz'])}{rng.randint(10, 99)}@ornek.com"
    title = f"Eval İletişim {token(rng, 4)}"
    page = (WEB_DIR / "validate.html").read_text(encoding="utf-8").replace("{title}", title)
    out = work / "out.json"
    window, pid = open_in_firefox(sb, work, serve_page(sb, page, out), "İletişim bilgileri")
    return Scene({"title": title, "window": window, "pid": pid, "phone_pretty": pretty,
                  "phone": digits, "email": email}, out)


def scene_web_long(sb, rng, work):
    codes = set()
    while len(codes) < 150:
        codes.add(f"{token(rng, 2)}-{rng.randint(1000, 9999)}")
    codes = list(codes)
    stock = {c: str(rng.randint(0, 900)) for c in codes}
    target = codes[rng.randint(125, 145)]
    title = f"Eval Stok {token(rng, 4)}"
    rows = "".join(f"<tr><td>{c}</td><td>R{rng.randint(1, 40)}</td><td>{stock[c]}</td></tr>" for c in codes)
    page = (WEB_DIR / "long.html").read_text(encoding="utf-8").replace("{title}", title).replace("{rows}", rows)
    window, pid = open_in_firefox(sb, work, serve_page(sb, page, work / "out.json"), "Depo stok listesi")
    return Scene({"title": title, "window": window, "pid": pid, "code": target, "stock": stock[target]})


def scene_wizard(sb, rng, work):
    packages = rng.sample(["metin-editoru", "goruntu-araci", "yedekleme", "ag-izleyici", "ses-kaydedici",
                           "pdf-okuyucu", "takvim"], 5)
    target = rng.choice(packages)
    folder = f"kurulum_{token(rng, 4).lower()}"
    title = f"Eval Sihirbaz {token(rng, 4)}"
    out = work / "out.json"
    window, pid = sb.launch_window([sys.executable, str(FIXTURE.parent / "wizard.py"), "--out", str(out),
                                    "--title", title, "--packages", "|".join(packages)], fullscreen=False,
                                   timeout_ms=15000)
    return Scene({"title": title, "window": window, "pid": pid, "package": target, "folder": folder}, out)


def scene_dolphin(sb, rng, work):
    root = work / f"Eval Dosyalar {token(rng, 4)}"
    tok = token(rng, 4)
    target_dir = root / "Belgeler" / rng.choice(["2025", "2026"]) / rng.choice(["Mart", "Ekim"])
    for d in ["Belgeler/2025/Mart", "Belgeler/2025/Ekim", "Belgeler/2026/Mart", "Belgeler/2026/Ekim",
              "Resimler", "İndirilenler", "Arşiv"]:
        (root / d).mkdir(parents=True, exist_ok=True)
    for d in ["Belgeler/2025/Mart", "Belgeler/2026/Ekim", "İndirilenler"]:
        for i in range(3):
            (root / d / f"not-{token(rng, 4)}.txt").write_text("x", encoding="utf-8")
    src = target_dir / f"rapor-{tok}.txt"
    content = f"rapor içeriği {token(rng, 8)}"
    src.write_text(content, encoding="utf-8")
    window, pid = sb.launch_window(["dolphin", "--new-window", str(root)], env=isolated_env(work), timeout_ms=15000)
    return Scene({"title": root.name, "window": window, "pid": pid, "file": src.name,
                  "src": str(src), "dest": str(root / "Arşiv" / src.name), "content": content})


def scene_kate(sb, rng, work):
    folder = work / f"eval-notlar-{token(rng, 4).lower()}"
    folder.mkdir()
    name = f"notlar-{token(rng, 4).lower()}.txt"
    original = f"Toplantı notları {token(rng, 6)}\nKatılımcılar: {', '.join(rng.sample(CUSTOMERS, 3))}\n"
    (folder / name).write_text(original, encoding="utf-8")
    line = f"Karar: {rng.choice(['bütçe onaylandı', 'tarih ertelendi', 'ekip büyüyecek'])} ({token(rng, 4)})"
    new_name = f"ozet-{token(rng, 4).lower()}.txt"
    window, pid = sb.launch_window(["kate", "-n", str(folder / name)], env=isolated_env(work), timeout_ms=15000)
    return Scene({"title": name, "window": window, "pid": pid, "file": name, "line": line,
                  "new_name": new_name, "new_path": str(folder / new_name), "folder": str(folder),
                  "first_line": original.splitlines()[0]})


def scene_youtube(sb, rng, work):
    """Gerçek site: YouTube (ağ gerekir; değerler ve sonuçlar her seferinde farklıdır)."""
    query = rng.choice(["piyano müziği", "doğa belgeseli", "kedi videoları", "yağmur sesi", "caz müzik"])
    window, pid = open_in_firefox(sb, work, "https://www.youtube.com/", "YouTube")
    return Scene({"query": query, "window": window, "pid": pid, "title": "YouTube"})


SCENES = {
    "youtube": scene_youtube,
    "cross_app": scene_cross_app,
    "process_form": scene_process_form,
    "settings": scene_settings,
    "web_validate": scene_web_validate,
    "web_long": scene_web_long,
    "wizard": scene_wizard,
    "dolphin": scene_dolphin,
    "kate": scene_kate,
    "web_form": scene_web_form,
    "web_table": scene_web_table,
    "buttons": scene_buttons,
    "form": scene_form,
    "canvas": scene_canvas,
    "terminal": scene_terminal,
    "port": scene_port,
    "journal": scene_journal,
    "crash": scene_crash,
}


# ---------------------------------------------------------------------------
# Görevler ve denetim
# ---------------------------------------------------------------------------

def load_tasks():
    tasks = []
    for path in sorted(TASKS_DIR.glob("*.json")):
        task = json.loads(path.read_text(encoding="utf-8"))
        if task["scene"] not in SCENES:
            raise ValueError(f"{path.name}: bilinmeyen sahne {task['scene']}")
        tasks.append(task)
    return tasks


def fill(value, vars):
    """Görev şablonlarındaki `{ad}` alanlarını sahne değişkenleriyle doldurur."""
    if isinstance(value, str):
        return re.sub(r"\{(\w+)\}", lambda m: str(vars.get(m.group(1), m.group(0))), value)
    if isinstance(value, list):
        return [fill(v, vars) for v in value]
    if isinstance(value, dict):
        return {fill(k, vars): fill(v, vars) for k, v in value.items()}
    return value


def normalize(text):
    return re.sub(r"[\s`'\"*.]+", "", str(text)).casefold()


def extract_answer(final_text):
    found = re.findall(r"CEVAP:\s*(.+)", final_text or "")
    return found[-1].strip() if found else None


def check_out_json(out, expect):
    try:
        data = json.loads(Path(out).read_text(encoding="utf-8"))
    except (OSError, ValueError, TypeError):
        return False, "uygulama çıktısı yok"
    wrong = {k: data.get(k) for k, v in expect.items() if data.get(k) != v}
    return (not wrong), (f"beklenmeyen alanlar: {wrong}" if wrong else "ok")


def check_answer(final_text, expect):
    answer = extract_answer(final_text)
    if answer is None:
        return False, "CEVAP satırı yok"
    alternatives = expect if isinstance(expect, list) else [expect]
    ok = any(normalize(a) == normalize(answer) for a in alternatives)
    return ok, f"cevap={answer!r}"


def check_files(spec):
    """Dosya sistemi durumu: `exists`, `absent`, `contains` {yol: metin}."""
    missing = [p for p in spec.get("exists", []) if not Path(p).exists()]
    present = [p for p in spec.get("absent", []) if Path(p).exists()]
    wrong = []
    for path, text in (spec.get("contains") or {}).items():
        try:
            if text not in Path(path).read_text(encoding="utf-8"):
                wrong.append(path)
        except OSError:
            wrong.append(path)
    if missing or present or wrong:
        names = lambda ps: [Path(p).name for p in ps]
        return False, f"yok: {names(missing)} olmamalı: {names(present)} içerik: {names(wrong)}"
    return True, "ok"


def check_mpris(spec):
    """Tarayıcının MPRIS durumu: gerçekten oynuyor mu, hangi adreste (`url_contains`)."""
    # Oynatıcı adı sürüme göre değişir (firefox.instance_…); D-Bus adını süreç kimliğinden bul.
    names = subprocess.run(["busctl", "--user", "list"], capture_output=True, text=True).stdout.splitlines()
    player = next((l.split()[0].removeprefix("org.mpris.MediaPlayer2.") for l in names
                   if l.startswith("org.mpris.MediaPlayer2.") and len(l.split()) > 1 and l.split()[1] == str(spec["pid"])), None)
    if not player:
        return False, "oynatıcı yok (medya hiç başlamadı)"
    run = lambda *a: subprocess.run(["playerctl", "-p", player, *a], capture_output=True, text=True).stdout.strip()
    status = run("status")
    url = run("metadata", "xesam:url")
    title = run("metadata", "xesam:title")
    want = spec.get("url_contains", "")
    alternatives = want if isinstance(want, list) else [want]
    ok = status == "Playing" and any(w in url for w in alternatives)
    return ok, f"durum={status or 'yok'} başlık={title[:60]!r} url={url[:80]}"


def check_task(check, scene, final_text=None):
    expect = fill(check.get("expect"), scene.vars)
    if check["type"] == "mpris":
        return check_mpris(expect)
    if check["type"] == "files":
        return check_files(expect)
    if check["type"] == "out_json":
        return check_out_json(scene.out, expect)
    if check["type"] == "answer":
        return check_answer(final_text, expect)
    if check["type"] == "none":  # yalnız probe adımlarının beklentileri belirler
        return True, "ok"
    raise ValueError(f"bilinmeyen denetim türü: {check['type']}")


# ---------------------------------------------------------------------------
# Katmanlar
# ---------------------------------------------------------------------------

def redact_user_titles(text, scene):
    """Sonuç dosyaları depoya girer (depo herkese açık): kullanıcının kendi pencere başlıkları
    (tarayıcı sekmeleri vb.) başarısız adımın çıktısıyla birlikte yazılmasın."""
    own = {str(v) for v in scene.vars.values()}

    def mask(m):
        title = m.group(2)
        keep = title.startswith(("Eval ", "PCEYE")) or any(o and o in title for o in own if len(o) > 3)
        return m.group(0) if keep else f'{m.group(1)}"<kullanıcı penceresi>"'

    return re.sub(r'("title":\s*)"((?:[^"\\]|\\.)*)"', mask, text)


def run_probe(task, scene):
    steps = []
    ok = True
    for step in task.get("probe", []):
        args = fill(step["args"], scene.vars)
        r = pceye(*args, timeout=step.get("timeout", 30))
        step_ok = r.rc == 0
        missing = [e for e in fill(step.get("expect_contains", []), scene.vars)
                   if e.casefold() not in r.stdout.casefold()]
        present = [e for e in fill(step.get("expect_absent", []), scene.vars)
                   if e.casefold() in r.stdout.casefold()]
        # Süre bütçesi: gecikme gerilemesi de başarısızlıktır.
        too_slow = step.get("max_ms") is not None and r.ms > step["max_ms"]
        step_ok = step_ok and not missing and not present and not too_slow
        # `save`: çıktının üst düzey JSON alanını sonraki adımlar için değişkene al (ör. snap kimliği).
        out = r.json() if step.get("save") else None
        for var, key in (step.get("save") or {}).items():
            if isinstance(out, dict) and key in out:
                scene.vars[var] = out[key]
        if r.rc:
            note = r.stderr.strip()[:200]
        elif missing or present:
            note = f"eksik: {missing}" if missing else f"olmamalıydı: {present}"
        elif too_slow:
            note = f"yavaş: {r.ms:.0f} ms > {step['max_ms']} ms"
        else:
            note = "ok"
        steps.append({"args": args, "rc": r.rc, "ms": round(r.ms, 1), "ok": step_ok, "note": note})
        if not step_ok:
            steps[-1]["stdout"] = redact_user_titles(r.stdout[:30000], scene)
        ok = ok and step_ok
        if step.get("settle_ms"):
            time.sleep(step["settle_ms"] / 1000)
    detail = "; ".join(f"{s['args'][0]}:{s['note']}" for s in steps if not s["ok"]) or "ok"
    # Bazı görevlerin probe adımları yalnız sahneyi ve algıyı doğrular (ör. dosya yöneticisinde
    # taşıma CLI ile yapılamaz): `probe_check` probe katmanında asıl denetimin yerine geçer.
    check = task.get("probe_check", task["check"])
    if check["type"] in ("out_json", "files"):
        time.sleep(0.3)
        check_ok, check_detail = check_task(check, scene)
        ok = ok and check_ok
        if not check_ok:
            detail = check_detail if detail == "ok" else f"{detail}; {check_detail}"
    return {
        "ok": ok,
        "detail": detail,
        "ms": round(sum(s["ms"] for s in steps), 1),
        "steps": steps,
    }


def prime(task, sb, rng, work):
    """`prime` adımlarını aynı türden ayrı bir sahnede (başka değerlerle) çalıştırır.

    Ölçülen görev bir "ikinci sefer" olur: tarif hafızası önceki başarılı akışı bilir.
    """
    pwork = work / "prime"
    pwork.mkdir(parents=True, exist_ok=True)
    scene = SCENES[task["scene"]](sb, rng, pwork)
    try:
        r = run_probe({"probe": task["prime"], "check": {"type": "none"}}, scene)
    finally:
        sb.kill_all()
    if not r["ok"]:
        raise RuntimeError(f"hazırlık başarısız: {r['detail']}")


def parse_stream(lines):
    """`claude -p --output-format stream-json` çıktısından ölçümleri çıkarır."""
    tools = {}
    result = {}
    for line in lines:
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        if ev.get("type") == "assistant":
            for block in ev.get("message", {}).get("content", []):
                if block.get("type") == "tool_use":
                    name = block.get("name", "?").replace("mcp__pc-eye__", "")
                    tools[name] = tools.get(name, 0) + 1
        elif ev.get("type") == "result":
            result = ev
    usage = result.get("usage") or {}
    return {
        "final_text": result.get("result", ""),
        "is_error": bool(result.get("is_error")) or result.get("subtype") != "success",
        "api_error": result.get("api_error"),
        "turns": result.get("num_turns"),
        "cost_usd": result.get("total_cost_usd"),
        "input_tokens": (usage.get("input_tokens") or 0)
        + (usage.get("cache_read_input_tokens") or 0)
        + (usage.get("cache_creation_input_tokens") or 0),
        "output_tokens": usage.get("output_tokens") or 0,
        "tool_calls": sum(tools.values()),
        "tools": tools,
    }


class ApiUnavailable(RuntimeError):
    pass


def run_agent(task, scene, sb, work, args, transcript):
    mcp = work / "mcp.json"
    env = {k: os.environ[k] for k in ("PC_EYE_RECIPES", "PC_EYE_SESSION") if k in os.environ}
    if args.toolset == "pixels":
        env["PC_EYE_VISION_INLINE"] = "1"
    mcp.write_text(json.dumps({"mcpServers": {"pc-eye": {"command": PCEYE, "args": ["mcp"], "env": env}}}))
    system = AGENT_SYSTEM.format(display=sb.label, workspace=sb.workspace)
    allowed = ["mcp__pc-eye"]
    disallowed = []
    if args.toolset == "pixels":
        system += PIXEL_SYSTEM
        allowed = PIXEL_TOOLS
        # İzin verilmeyen araçlar modelin listesinden de çıkar (yoksa deneyip reddedilir, tur harcar).
        disallowed = [f"mcp__pc-eye__pc_eye_{t}" for t in ALL_TOOLS if f"mcp__pc-eye__pc_eye_{t}" not in PIXEL_TOOLS]
    cmd = [
        "claude", "-p", fill(task["prompt"], scene.vars),
        "--output-format", "stream-json", "--verbose",
        "--mcp-config", str(mcp), "--strict-mcp-config",
        "--tools", "",
        "--allowedTools", ",".join(allowed),
        "--setting-sources", "project,local",
        "--no-session-persistence",
        "--max-budget-usd", str(args.budget),
        "--append-system-prompt", system,
    ]
    if disallowed:
        cmd += ["--disallowedTools", ",".join(disallowed)]
    if args.model:
        cmd += ["--model", args.model]
    start = time.perf_counter()
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=args.timeout, cwd=work)
        out, timed_out = p.stdout, False
    except subprocess.TimeoutExpired as e:
        out = e.stdout.decode() if isinstance(e.stdout, bytes) else (e.stdout or "")
        timed_out = True
    ms = (time.perf_counter() - start) * 1000
    transcript.write_text(out, encoding="utf-8")
    m = parse_stream(out.splitlines())
    # API hatası (kullanım sınırı, aşırı yük) ajanın başarısızlığı değildir: ölçüme sayılmaz.
    if m.get("api_error"):
        raise ApiUnavailable(f"API hatası: {m['api_error']}: {m['final_text'][:120]}")
    m.pop("api_error", None)
    if timed_out:
        ok, detail = False, f"zaman aşımı ({args.timeout} sn)"
    else:
        ok, detail = check_task(task["check"], scene, m["final_text"])
        if m["is_error"] and not ok:
            detail = f"ajan hatası; {detail}"
    m.pop("final_text")
    return {"ok": ok, "detail": detail, "ms": round(ms, 1), **m}


# ---------------------------------------------------------------------------
# Rapor
# ---------------------------------------------------------------------------

def summarize(runs):
    by_task = {}
    for r in runs:
        by_task.setdefault(r["task"], []).append(r)
    rows = []
    for task_id, rs in by_task.items():
        scored = [r for r in rs if not r.get("setup_error")]
        row = {
            "task": task_id,
            "runs": len(rs),
            "passed": sum(r["ok"] for r in scored),
            "setup_errors": len(rs) - len(scored),
            "median_ms": round(statistics.median(r["ms"] for r in scored), 1) if scored else None,
        }
        for key in ("turns", "tool_calls", "input_tokens", "output_tokens"):
            vals = [r[key] for r in scored if r.get(key) is not None]
            if vals:
                row[f"median_{key}"] = statistics.median(vals)
        costs = [r["cost_usd"] for r in scored if r.get("cost_usd") is not None]
        if costs:
            row["cost_usd"] = round(sum(costs), 4)
        rows.append(row)
    scored = [r for r in runs if not r.get("setup_error")]
    return {
        "tasks": rows,
        "pass_rate": round(sum(r["ok"] for r in scored) / len(scored), 3) if scored else None,
        "total_cost_usd": round(sum(r.get("cost_usd") or 0 for r in scored), 4),
    }


def previous_result(tier, exclude):
    files = sorted(p for p in RESULTS_DIR.glob(f"*-{tier}.json") if p != exclude)
    return json.loads(files[-1].read_text(encoding="utf-8")) if files else None


def print_report(report, prev):
    prev_rows = {r["task"]: r for r in (prev or {}).get("summary", {}).get("tasks", [])}
    agent = report["tier"] == "agent"
    head = f"{'görev':<22} {'geçti':>7} {'medyan ms':>10}"
    if agent:
        head += f" {'tur':>5} {'araç':>5} {'girdi tok':>10} {'$':>7}"
    print("\n" + head + "  önceki")
    for row in report["summary"]["tasks"]:
        line = f"{row['task']:<22} {row['passed']:>3}/{row['runs'] - row['setup_errors']:<3} "
        line += f"{row['median_ms'] if row['median_ms'] is not None else '—':>10}"
        if agent:
            line += (f" {row.get('median_turns', '—'):>5} {row.get('median_tool_calls', '—'):>5}"
                     f" {row.get('median_input_tokens', '—'):>10} {row.get('cost_usd', '—'):>7}")
        p = prev_rows.get(row["task"])
        if p:
            line += f"  {p['passed']}/{p['runs'] - p['setup_errors']}, {p['median_ms']} ms"
        if row["setup_errors"]:
            line += f"  (kurulum hatası: {row['setup_errors']})"
        print(line)
    s = report["summary"]
    rate = f"{s['pass_rate'] * 100:.0f}%" if s["pass_rate"] is not None else "—"
    print(f"\nbaşarı: {rate}", end="")
    if prev and prev["summary"].get("pass_rate") is not None:
        print(f" (önceki: {prev['summary']['pass_rate'] * 100:.0f}%)", end="")
    if agent:
        print(f"   toplam maliyet: ${s['total_cost_usd']}", end="")
    print()


# ---------------------------------------------------------------------------
# Giriş
# ---------------------------------------------------------------------------

def select_tasks(tasks, args):
    if args.task:
        unknown = set(args.task) - {t["id"] for t in tasks}
        if unknown:
            sys.exit(f"bilinmeyen görev: {', '.join(sorted(unknown))}")
        tasks = [t for t in tasks if t["id"] in args.task]
    if args.tag:
        tasks = [t for t in tasks if set(args.tag) & set(t.get("tags", []))]
    if args.tier == "probe":
        tasks = [t for t in tasks if t.get("probe")]
    elif args.tier == "agent":
        tasks = [t for t in tasks if t.get("prompt")]
    return tasks


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("tier", choices=["list", "probe", "agent"])
    ap.add_argument("--task", action="append", help="yalnız bu görev (tekrarlanabilir)")
    ap.add_argument("--tag", action="append", help="yalnız bu etiketli görevler")
    ap.add_argument("--repeat", type=int, default=1)
    ap.add_argument("--seed", type=int, default=None, help="rastgele değerleri sabitle")
    ap.add_argument("--model", help="agent katmanı modeli (ör. claude-sonnet-5-5)")
    ap.add_argument("--timeout", type=int, default=300, help="agent görev süresi sınırı (sn)")
    ap.add_argument("--budget", type=float, default=1.0, help="agent görev başı $ sınırı")
    ap.add_argument("--no-save", action="store_true")
    ap.add_argument("--toolset", choices=["full", "pixels"], default="full",
                    help="pixels: yalnız ekran görüntüsü + fare/klavye (rakip mimarisi kıyası)")
    ap.add_argument("--session", help="görevleri bu adla açılan ayrı ajan oturumunda (iç içe Hyprland) koş")
    args = ap.parse_args()

    tasks = select_tasks(load_tasks(), args)
    if args.tier == "list":
        for t in tasks:
            print(f"{t['id']:<22} {t['scene']:<9} {','.join(t.get('tags', []))}")
        return
    if not tasks:
        sys.exit("seçili görev yok")

    seed = args.seed if args.seed is not None else random.randrange(1 << 30)
    rng = random.Random(seed)
    # Paralel koşucular (farklı oturumlar) aynı saniyede başlayabilir: dosyalar çakışmasın.
    stamp = (time.strftime("%Y%m%d-%H%M%S") + (f"-{args.session}" if args.session else "")
             + ("-pixels" if args.toolset == "pixels" else ""))
    work_root = Path(os.environ.get("XDG_RUNTIME_DIR", "/tmp")) / "pc-eye-eval" / stamp
    transcripts = RESULTS_DIR / "transcripts" / stamp
    if args.tier == "agent":
        transcripts.mkdir(parents=True, exist_ok=True)

    runs = []
    api_down = False
    with Sandbox(args.session) as sb:
        print(f"{sb.label} → çalışma alanı {sb.workspace}; seed={seed}")
        for task in tasks:
            for i in range(args.repeat):
                work = work_root / f"{task['id']}-{i}"
                work.mkdir(parents=True, exist_ok=True)
                sb.remember_user_focus()
                record = {"task": task["id"], "attempt": i}
                # Her görev boş tarif hafızasıyla başlar (pc-eye CLI ve MCP sunucusu ortamı devralır).
                os.environ["PC_EYE_RECIPES"] = str(work / "recipes.json")
                try:
                    if task.get("prime"):
                        prime(task, sb, rng, work)
                    scene = SCENES[task["scene"]](sb, rng, work)
                    record["vars"] = {k: v for k, v in scene.vars.items() if k != "window"}
                    if args.tier == "probe":
                        record.update(run_probe(task, scene))
                    else:
                        record.update(run_agent(task, scene, sb, work, args,
                                                transcripts / f"{task['id']}-{i}.jsonl"))
                except ApiUnavailable as e:
                    record.update({"ok": False, "setup_error": True, "detail": str(e), "ms": 0})
                    api_down = "usage_limit" in str(e)
                except Exception as e:  # sahne kurulamadıysa ölçüm sayılmaz
                    record.update({"ok": False, "setup_error": True, "detail": str(e), "ms": 0})
                finally:
                    sb.kill_all()
                    if sb.session:
                        record["user_disturbed"] = sb.user_disturbed()
                    sb.restore_user_focus()
                    # $XDG_RUNTIME_DIR küçük bir RAM diskidir (masaüstü soketleri de orada);
                    # Firefox profilleri birikince 3 GB'ı doldurup oturumu tehlikeye attı.
                    shutil.rmtree(work, ignore_errors=True)
                mark = "✓" if record["ok"] else ("!" if record.get("setup_error") else "✗")
                print(f"  {mark} {task['id']} #{i}  {record['ms']:.0f} ms  {record['detail']}")
                runs.append(record)
                if api_down:
                    break
            if api_down:
                print("kullanım sınırı: koşu durduruldu (sonuçlar ölçüme sayılmaz)")
                break
    shutil.rmtree(work_root, ignore_errors=True)

    report = {
        "tier": args.tier,
        "started": stamp,
        "seed": seed,
        "model": args.model if args.tier == "agent" else None,
        "pceye_version": (pceye("doctor").json() or {}).get("version"),
        "git_commit": subprocess.run(["git", "-C", str(ROOT), "rev-parse", "--short", "HEAD"],
                                     capture_output=True, text=True).stdout.strip(),
        "summary": summarize(runs),
        "session": args.session,
        "toolset": args.toolset,
        "runs": runs,
    }
    if args.session:
        changed = [r["user_disturbed"] for r in runs if r.get("user_disturbed")]
        leaks = sum(bool(c.get("agent_leak")) for c in changed)
        report["user_disturbed_runs"] = len(changed)
        report["agent_leak_runs"] = leaks
        print(f"ajan sızıntısı (imleç/odak kullanıcının ekranına geçti): {leaks}/{len(runs)}; "
              f"kullanıcının kendi etkinliği: {len(changed) - leaks}")
    out = RESULTS_DIR / f"{stamp}-{args.tier}.json"
    prev = previous_result(args.tier, out)
    print_report(report, prev)
    if report["summary"]["pass_rate"] is None:
        print("puanlanan koşu yok: sonuç kaydedilmedi")
    elif not args.no_save:
        RESULTS_DIR.mkdir(exist_ok=True)
        out.write_text(json.dumps(report, ensure_ascii=False, indent=1), encoding="utf-8")
        print(f"kaydedildi: {out.relative_to(ROOT.parent)}")


if __name__ == "__main__":
    main()
