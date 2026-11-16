#!/usr/bin/env python3
"""Araç setlerini aynı görevlerde kıyaslar: sonuç dosyalarını (run_evals.py agent çıktıları) araç setine
göre toplar; görev başına başarı, medyan süre ve koşu başı ortalama maliyeti yan yana basar.
kullanım: compare_toolsets.py <sonuç.json> [...]   (araç seti dosyadaki `toolset` alanından okunur)"""
import json
import statistics
import sys
from collections import defaultdict


def main():
    data = defaultdict(lambda: defaultdict(list))  # toolset -> task -> runs
    for path in sys.argv[1:]:
        doc = json.load(open(path, encoding="utf-8"))
        for r in doc.get("runs", []):
            if not r.get("setup_error"):
                data[doc.get("toolset", "full")][r["task"]].append(r)
    toolsets = sorted(data)
    common = sorted(set.intersection(*(set(data[t]) for t in toolsets)))
    cell = lambda rs: (sum(r["ok"] for r in rs), len(rs), statistics.median(r["ms"] for r in rs) / 1000,
                       statistics.mean((r.get("cost_usd") or 0) for r in rs))
    print(f"{'görev':<22}" + "".join(f"{t:>30}" for t in toolsets))
    wins = {t: {"ok": 0, "ms": 0, "usd": 0} for t in toolsets}
    for task in common:
        cells = {t: cell(data[t][task]) for t in toolsets}
        print(f"{task:<22}" + "".join(
            f"{f'{c[0]}/{c[1]}  {c[2]:5.1f} sn  ${c[3]:.4f}':>30}" for c in (cells[t] for t in toolsets)))
        best_ok = max(c[0] / c[1] for c in cells.values())
        for t, c in cells.items():
            wins[t]["ok"] += c[0] / c[1] == best_ok
            wins[t]["ms"] += c[2] == min(x[2] for x in cells.values())
            wins[t]["usd"] += c[3] == min(x[3] for x in cells.values())
    print()
    for t in toolsets:
        runs = [r for task in common for r in data[t][task]]
        print(f"{t:<16} başarı {sum(r['ok'] for r in runs)}/{len(runs)}  "
              f"görev medyanlarının toplamı {sum(statistics.median(r['ms'] for r in data[t][k]) for k in common) / 1000:6.1f} sn  "
              f"koşu başı ort. ${statistics.mean((r.get('cost_usd') or 0) for r in runs):.4f}  "
              f"en iyi olduğu görev: başarı {wins[t]['ok']}, hız {wins[t]['ms']}, maliyet {wins[t]['usd']} / {len(common)}")


if __name__ == "__main__":
    main()
