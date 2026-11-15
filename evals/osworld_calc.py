#!/usr/bin/env python3
"""OSWorld LibreOffice Calc alt kümesi (F10): bağımsız görev setinde aynı kıyas.

Görevler OSWorld'ün kendi tanımlarıdır (evaluation_examples/examples/libreoffice_calc), puanlama
OSWorld'ün kendi `compare_table` kodudur (desktop_env/evaluators/metrics/table.py). Yalnız güvenli
ve taşınabilir olanlar seçilir: hazırlığı "dosya indir + aç", değerlendirmesi dosya karşılaştırması
olan görevler. Sistem ayarı değiştiren ya da paket kuran OSWorld görevleri (os, multi_apps) yoktur.

OSWorld'ün sanal makinesi yerine görev, ajanın yalıtılmış oturumunda koşar: dosya geçici çalışma
dizinine kopyalanır, LibreOffice (AppImage, ayrı ve temiz profil) onu açar, ajan talimatı alır.
OSWorld'deki gibi kaydetmeyi değerlendirici yapar (Ctrl+S), sonra sonuç altın dosyayla karşılaştırılır.

    python3 evals/osworld_calc.py --toolset full --session osw --limit 20
    PC_EYE_BACKEND=hyprland python3 evals/osworld_calc.py --toolset wayland-mcp --session oswr --limit 20
"""

import argparse
import hashlib
import json
import os
import random
import shutil
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import run_evals as R  # noqa: E402

BASE = Path.home() / ".local/share/pc-eye/osworld"
OSW = Path(os.environ.get("OSWORLD_DIR", BASE / "OSWorld"))
LO_DIR = BASE / "libreoffice"
VENV_PY = BASE / "venv/bin/python"
CACHE = BASE / "cache"
SUFFIX = ("\n\n(The file is already open in LibreOffice Calc. You do not need to save; the evaluator "
          "saves the document. When done, end your reply with `CEVAP: tamam`.)")

# İlk açılış ipucu penceresi ve "biçimi koru?" sorusu kapalı; ayrı profil kullanıcının ayarlarına dokunmaz.
REGISTRY = """<?xml version="1.0" encoding="UTF-8"?>
<oor:items xmlns:oor="http://openoffice.org/2001/registry" xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
<item oor:path="/org.openoffice.Office.Common/Misc"><prop oor:name="ShowTipOfTheDay" oor:op="fuse"><value>false</value></prop></item>
<item oor:path="/org.openoffice.Office.Common/Misc"><prop oor:name="FirstRun" oor:op="fuse"><value>false</value></prop></item>
<item oor:path="/org.openoffice.Office.Common/Save/Document"><prop oor:name="WarnAlienFormat" oor:op="fuse"><value>false</value></prop></item>
<item oor:path="/org.openoffice.Office.Common/Save/Document"><prop oor:name="AutoSave" oor:op="fuse"><value>false</value></prop></item>
<item oor:path="/org.openoffice.Office.Recovery/AutoSave"><prop oor:name="Enabled" oor:op="fuse"><value>false</value></prop></item>
<item oor:path="/org.openoffice.Setup/Product"><prop oor:name="ooSetupLastVersion" oor:op="fuse"><value>99.0</value></prop></item>
</oor:items>
"""

EVALUATOR = r'''
import importlib.util, json, sys, types
osw, result, expected, options = sys.argv[1], sys.argv[2], sys.argv[3], json.loads(sys.argv[4])
for name in ["desktop_env", "desktop_env.evaluators", "desktop_env.evaluators.metrics"]:
    sys.modules[name] = types.ModuleType(name)
def load(mod, path):
    spec = importlib.util.spec_from_file_location(mod, path)
    m = importlib.util.module_from_spec(spec); sys.modules[mod] = m; spec.loader.exec_module(m); return m
base = osw + "/desktop_env/evaluators/metrics/"
load("desktop_env.evaluators.metrics.utils", base + "utils.py")
table = load("desktop_env.evaluators.metrics.table", base + "table.py")
print(json.dumps({"score": float(table.compare_table(result, expected, **options))}))
'''


def select_tasks():
    out = []
    for f in sorted((OSW / "evaluation_examples/examples/libreoffice_calc").glob("*.json")):
        t = json.loads(f.read_text())
        ev = t.get("evaluator", {})
        steps = {s.get("type") for s in t.get("config", [])}
        if ev.get("func") != "compare_table" or not steps <= {"download", "open"}:
            continue
        if not isinstance(ev.get("result"), dict) or ev["result"].get("type") != "vm_file":
            continue
        if not isinstance(ev.get("expected"), dict) or ev["expected"].get("type") != "cloud_file":
            continue
        out.append(t)
    return out


def fetch(url):
    CACHE.mkdir(parents=True, exist_ok=True)
    dst = CACHE / (hashlib.sha1(url.encode()).hexdigest()[:16] + "-" + url.rsplit("/", 1)[-1])
    if not dst.exists():
        tmp = dst.with_suffix(dst.suffix + ".part")
        with urllib.request.urlopen(url, timeout=120) as r, open(tmp, "wb") as f:
            shutil.copyfileobj(r, f)
        tmp.rename(dst)
    return dst


def local(path, work):
    return work / Path(path).name


def lo_binary():
    app = LO_DIR / "AppRun"
    if not app.exists():
        raise SystemExit(f"LibreOffice yok: {app} (AppImage'ı --appimage-extract ile {LO_DIR} altına açın)")
    return app


def prepare(task, work):
    work.mkdir(parents=True, exist_ok=True)
    opened = None
    for step in task["config"]:
        if step["type"] == "download":
            for f in step["parameters"]["files"]:
                shutil.copy(fetch(f["url"]), local(f["path"], work))
        elif step["type"] == "open":
            opened = local(step["parameters"]["path"], work)
    profile = work / "lo-profile/user"
    profile.mkdir(parents=True, exist_ok=True)
    (profile / "registrymodifications.xcu").write_text(REGISTRY)
    return opened, work / "lo-profile"


def save_document(win):
    # OSWorld'ün postconfig'i: pencereyi öne al, Ctrl+S.
    R.pceye("window", "focus", "--window", win)
    time.sleep(0.5)
    R.pceye("input", "key", "--keys", "ctrl+s", "--window", win)
    time.sleep(2.0)


def evaluate(task, result_path):
    ev = task["evaluator"]
    gold = fetch(ev["expected"]["path"])
    r = subprocess.run([str(VENV_PY), "-I", "-c", EVALUATOR, str(OSW), str(result_path), str(gold),
                        json.dumps(ev.get("options", {}))], capture_output=True, text=True, timeout=180)
    try:
        return json.loads(r.stdout.strip().splitlines()[-1])["score"], ""
    except Exception:
        return 0.0, (r.stderr or r.stdout)[-300:]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--toolset", choices=["full", "pixels", "wayland-mcp"], default="full")
    ap.add_argument("--session", required=True)
    ap.add_argument("--model", default="claude-haiku-5-5")
    ap.add_argument("--limit", type=int, default=20)
    ap.add_argument("--seed", type=int, default=20261115)
    ap.add_argument("--timeout", type=int, default=300)
    ap.add_argument("--budget", type=float, default=0.5)
    ap.add_argument("--only", action="append", default=[], help="yalnız bu görev kimlikleri")
    args = ap.parse_args()

    tasks = select_tasks()
    rng = random.Random(args.seed)
    rng.shuffle(tasks)
    if args.only:
        tasks = [t for t in tasks if t["id"] in args.only]
    tasks = tasks[: args.limit]
    stamp = time.strftime("%Y%m%d-%H%M%S")
    root = Path(os.environ.get("XDG_RUNTIME_DIR", "/tmp")) / "pc-eye-eval" / f"{stamp}-osw-{args.session}"
    tdir = R.RESULTS_DIR / "transcripts" / f"{stamp}-osw-{args.toolset}"
    tdir.mkdir(parents=True, exist_ok=True)
    print(f"{len(select_tasks())} uygun görevden {len(tasks)} seçildi; araç seti {args.toolset}")
    runs = []
    with R.Sandbox(args.session) as sb:
        for t in tasks:
            work = root / t["id"]
            opened, profile = prepare(t, work)
            # LibreOffice başlığı dosya adını taşır; pencere bulunamazsa görev kurulamamıştır.
            try:
                win, _ = sb.launch_window([str(lo_binary()), "--calc", "--norestore", "--nologo",
                                           f"-env:UserInstallation=file://{profile}", str(opened)],
                                          env={"SAL_USE_VCLPLUGIN": "gtk3"}, timeout_ms=30000)
                time.sleep(2.0)
            except Exception as e:
                runs.append({"task": t["id"], "ok": False, "setup_error": str(e)[:200]})
                print(f"  ! {t['id'][:8]} kurulum: {e}")
                sb.kill_all()
                continue
            scene = R.Scene({"window": win})
            task = {"prompt": t["instruction"] + SUFFIX, "check": {"type": "none"}}
            try:
                m = R.run_agent(task, scene, sb, work, args, tdir / f"{t['id']}.jsonl")
            except R.ApiUnavailable as e:
                print(f"  ! API: {e}")
                sb.kill_all()
                break
            save_document(win)
            score, err = evaluate(t, local(t["evaluator"]["result"]["path"], work))
            m.update({"task": t["id"], "ok": score >= 1.0, "score": score, "eval_error": err,
                      "instruction": t["instruction"][:160]})
            runs.append(m)
            print(f"  {'✓' if m['ok'] else '✗'} {t['id'][:8]} {m['ms'] / 1000:6.1f} s  ${m.get('cost_usd') or 0:.4f}  "
                  f"{t['instruction'][:70]}{('  [' + err[:80] + ']') if err else ''}")
            sb.kill_all()
    done = [r for r in runs if "setup_error" not in r]
    summary = {
        "tasks": len(runs), "ran": len(done), "passed": sum(r["ok"] for r in done),
        "total_s": round(sum(r["ms"] for r in done) / 1000, 1),
        "median_s": round(sorted(r["ms"] for r in done)[len(done) // 2] / 1000, 1) if done else 0,
        "cost_usd": round(sum(r.get("cost_usd") or 0 for r in done), 4),
        "input_tokens_avg": round(sum(r.get("input_tokens") or 0 for r in done) / max(len(done), 1)),
    }
    out = R.RESULTS_DIR / f"{stamp}-osw-calc-{args.toolset}.json"
    out.write_text(json.dumps({"toolset": args.toolset, "model": args.model, "seed": args.seed,
                               "summary": summary, "runs": runs}, ensure_ascii=False, indent=1))
    print(json.dumps(summary, ensure_ascii=False))
    print(f"kaydedildi: {out}")


if __name__ == "__main__":
    main()
