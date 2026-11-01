#!/usr/bin/env python3
"""Ölçüm sonuçlarından kıyas tablosu (`evals/LEADERBOARD.md`) üretir.

Aynı görevler, aynı makine; sistem = araç katmanı + model. Yalnız tüm sistemlerin koştuğu ortak
görevler karşılaştırılır (ör. piksel tabanı arka plan görevini koşmaz).

    python3 evals/leaderboard.py evals/results/A.json evals/results/B.json ...
"""

import json
import statistics
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent

LABELS = {
    "full": "pc-eye (erişilebilirlik + eylem programı + doğrulama)",
    "pixels": "piksel tabanı (yalnız ekran görüntüsü + fare/klavye; computer-use/CUA mimarisi)",
}


def load(paths):
    systems = {}
    for p in paths:
        d = json.loads(Path(p).read_text(encoding="utf-8"))
        key = (d.get("toolset") or "full", d.get("model") or "?")
        systems.setdefault(key, []).extend(r for r in d["runs"] if not r.get("setup_error"))
    return systems


def row(runs):
    costs = [r.get("cost_usd") or 0 for r in runs]
    return {
        "n": len(runs),
        "ok": sum(r["ok"] for r in runs),
        "median_s": statistics.median(r["ms"] for r in runs) / 1000,
        "median_cost": statistics.median(costs),
        "mean_cost": sum(costs) / len(costs),
        "median_turns": statistics.median(r.get("turns") or 0 for r in runs),
        # Yalnız ajan sızıntısı sayılır (kullanıcının o sırada fareyi oynatması değil).
        "disturbed": sum(bool(isinstance(r.get("user_disturbed"), dict) and r["user_disturbed"].get("agent_leak"))
                         for r in runs),
    }


def main(paths):
    systems = load(paths)
    common = set.intersection(*({r["task"] for r in runs} for runs in systems.values()))
    lines = [
        "# Kıyas tablosu",
        "",
        "Aynı makine (CachyOS + Hyprland), aynı görevler, rastgele değerler; başarı uygulamanın gerçek",
        "durumundan denetlenir. Görevler ajana ayrılmış iç içe Hyprland oturumunda koşar.",
        f"Ortak görevler ({len(common)}): " + ", ".join(sorted(common)) + ".",
        "",
        "| Sistem | Model | Başarı | Medyan süre | Görev başı $ (ort.) | Medyan tur | Ajan sızıntısı |",
        "| --- | --- | --- | --- | --- | --- | --- |",
    ]
    per_task = {}
    for (toolset, model), runs in sorted(systems.items()):
        rs = [r for r in runs if r["task"] in common]
        s = row(rs)
        lines.append(
            f"| {LABELS.get(toolset, toolset)} | {model} | {s['ok']}/{s['n']} ({100 * s['ok'] / s['n']:.0f}%) "
            f"| {s['median_s']:.1f} s | ${s['mean_cost']:.3f} | {s['median_turns']:.0f} | {s['disturbed']}/{s['n']} |"
        )
        for t in sorted(common):
            per_task.setdefault(t, {})[(toolset, model)] = row([r for r in rs if r["task"] == t])
    keys = sorted(systems)
    lines += ["", "## Görev bazında (başarı · medyan süre · ortalama $)", "",
              "| Görev | " + " | ".join(f"{t}/{m.replace('claude-', '')}" for t, m in keys) + " |",
              "| --- |" + " --- |" * len(keys)]
    for t, cells in sorted(per_task.items()):
        vals = []
        for k in keys:
            c = cells.get(k)
            vals.append(f"{c['ok']}/{c['n']} · {c['median_s']:.0f} s · ${c['mean_cost']:.3f}" if c else "—")
        lines.append(f"| {t} | " + " | ".join(vals) + " |")
    lines += ["", "Kaynak dosyalar: " + ", ".join(Path(p).name for p in paths), ""]
    out = ROOT / "LEADERBOARD.md"
    out.write_text("\n".join(lines), encoding="utf-8")
    print("\n".join(lines))


if __name__ == "__main__":
    main(sys.argv[1:])
