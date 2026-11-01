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


def pceye(*args, timeout=30):
    start = time.perf_counter()
    try:
        p = subprocess.run([PCEYE, *args], capture_output=True, text=True, timeout=timeout)
        rc, out, err = p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired as e:
        out = e.stdout.decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
        rc, err = 124, f"zaman aşımı ({timeout} sn)"
    return CallResult(rc, (time.perf_counter() - start) * 1000, out, err)


def hyprctl_json(*args):
    out = subprocess.run(["hyprctl", "-j", *args], capture_output=True, text=True).stdout
    return json.loads(out or "null")


class Sandbox:
    """Kullanıcının ekranına bitişik olmayan headless çıkış ve orada açılan süreçler."""

    def __init__(self):
        self.workspace = None
        self.pids = []
        self.cleanups = []
        self.user_cursor = None
        self.user_window = None

    def __enter__(self):
        if any(m["name"] == DISPLAY_NAME for m in hyprctl_json("monitors", "all")):
            pceye("display", "remove", "--name", DISPLAY_NAME)
        r = pceye("display", "create", "--name", DISPLAY_NAME, "--mode", DISPLAY_MODE)
        receipt = r.json()
        if r.rc != 0 or not receipt:
            raise RuntimeError(f"sanal ekran açılamadı: {r.stderr or r.stdout}")
        self.workspace = str(json.loads(receipt["details"])["workspace_id"])
        return self

    def __exit__(self, *exc):
        self.kill_all()
        pceye("display", "remove", "--name", DISPLAY_NAME)

    def remember_user_focus(self):
        self.user_cursor = hyprctl_json("cursorpos")
        active = hyprctl_json("activewindow") or {}
        self.user_window = active.get("address")

    def restore_user_focus(self):
        # Görev, imleci ve odağı sanal ekrana taşımış olabilir; kullanıcıya geri ver.
        if self.user_window:
            subprocess.run(["hyprctl", "dispatch", "focuswindow", f"address:{self.user_window}"],
                           capture_output=True)
        if self.user_cursor:
            subprocess.run(["hyprctl", "dispatch", "movecursor",
                            str(self.user_cursor["x"]), str(self.user_cursor["y"])],
                           capture_output=True)

    def launch_window(self, cmd):
        r = pceye("launch", "--workspace", self.workspace, "--timeout-ms", "5000", "--", *cmd)
        receipt = r.json() or {}
        m = re.search(r"address=(0x[0-9a-f]+) pid=(\d+)", receipt.get("target", ""))
        if not m:
            raise RuntimeError(f"pencere açılmadı: {receipt.get('target') or r.stderr}")
        address, pid = m.group(1), int(m.group(2))
        self.pids.append(pid)
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
    win = next(c for c in hyprctl_json("clients") if c["address"] == window)
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


def open_in_firefox(sb, work, url, ready_text):
    profile = work / "ffprofile"
    profile.mkdir(exist_ok=True)
    (profile / "user.js").write_text(FIREFOX_PREFS)
    window, pid = sb.launch_window(["firefox", "--new-instance", "--profile", str(profile), url])
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


SCENES = {
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
        return {k: fill(v, vars) for k, v in value.items()}
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


def check_task(check, scene, final_text=None):
    expect = fill(check.get("expect"), scene.vars)
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
            steps[-1]["stdout"] = r.stdout[:30000]
        ok = ok and step_ok
        if step.get("settle_ms"):
            time.sleep(step["settle_ms"] / 1000)
    detail = "; ".join(f"{s['args'][0]}:{s['note']}" for s in steps if not s["ok"]) or "ok"
    if task["check"]["type"] == "out_json":
        time.sleep(0.3)
        check_ok, check_detail = check_task(task["check"], scene)
        ok = ok and check_ok
        if not check_ok:
            detail = check_detail if detail == "ok" else f"{detail}; {check_detail}"
    return {
        "ok": ok,
        "detail": detail,
        "ms": round(sum(s["ms"] for s in steps), 1),
        "steps": steps,
    }


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
    mcp.write_text(json.dumps({"mcpServers": {"pc-eye": {"command": PCEYE, "args": ["mcp"]}}}))
    cmd = [
        "claude", "-p", fill(task["prompt"], scene.vars),
        "--output-format", "stream-json", "--verbose",
        "--mcp-config", str(mcp), "--strict-mcp-config",
        "--tools", "",
        "--allowedTools", "mcp__pc-eye",
        "--setting-sources", "project,local",
        "--no-session-persistence",
        "--max-budget-usd", str(args.budget),
        "--append-system-prompt", AGENT_SYSTEM.format(display=DISPLAY_NAME, workspace=sb.workspace),
    ]
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
    stamp = time.strftime("%Y%m%d-%H%M%S")
    work_root = Path(os.environ.get("XDG_RUNTIME_DIR", "/tmp")) / "pc-eye-eval" / stamp
    transcripts = RESULTS_DIR / "transcripts" / stamp
    if args.tier == "agent":
        transcripts.mkdir(parents=True, exist_ok=True)

    runs = []
    api_down = False
    with Sandbox() as sb:
        print(f"sanal ekran {DISPLAY_NAME} → çalışma alanı {sb.workspace}; seed={seed}")
        for task in tasks:
            for i in range(args.repeat):
                work = work_root / f"{task['id']}-{i}"
                work.mkdir(parents=True, exist_ok=True)
                sb.remember_user_focus()
                record = {"task": task["id"], "attempt": i}
                try:
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
        "runs": runs,
    }
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
