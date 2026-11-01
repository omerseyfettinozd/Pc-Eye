#!/usr/bin/env python3
"""Art arda açılan üç zenity diyaloğu; cevaplar --out dosyasına yazılır.

Ajan her adımda yeni bir pencereyle karşılaşır: soru → liste seçimi → metin girişi.
"""
import argparse
import json
import os
import subprocess

p = argparse.ArgumentParser()
p.add_argument("--out", required=True)
p.add_argument("--title", required=True)
p.add_argument("--packages", required=True)
a = p.parse_args()


def zenity(*args):
    r = subprocess.run(["zenity", f"--title={a.title}", *args], capture_output=True, text=True)
    return r.returncode, r.stdout.strip()


def done(data):
    tmp = a.out + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(data, f, ensure_ascii=False)
    os.replace(tmp, a.out)


rc, _ = zenity("--question", "--text=Kuruluma devam edilsin mi?", "--ok-label=Devam", "--cancel-label=Vazgeç",
               "--width=500")
if rc != 0:
    done({"step": 1, "continued": False})
    raise SystemExit
rc, pkg = zenity("--list", "--text=Kurulacak paketi seçin", "--column=Paket", *a.packages.split("|"),
                 "--width=500", "--height=400")
if rc != 0:
    done({"step": 2, "continued": True, "package": None})
    raise SystemExit
rc, folder = zenity("--entry", "--text=Kurulum klasörünün adı:", "--width=500")
done({"step": 3, "continued": True, "package": pkg, "folder": folder if rc == 0 else None})
