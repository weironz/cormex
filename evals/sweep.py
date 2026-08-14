#!/usr/bin/env python3
"""A/B 扫描的跑腿脚本 —— 一次跑一组配置，把关键数字拉平成一张表。

评测本体不做这件事是刻意的：`cortex-evals` 的产物是**一次**运行的报告，
把「跑 N 次再对比」塞进它会让 CI 的那条路径也背上一堆用不到的旋钮。
扫描是人在调参时的临时工具，放在这里，别进 CI。

    python evals/sweep.py abstain      # 弃权阈值曲线
    python evals/sweep.py recency      # 近因形态 A/B
    python evals/sweep.py episodes     # episodes 一路 A/B
    python evals/sweep.py peak         # 单路强命中补偿
    python evals/sweep.py replay       # 时间回放排序 A/B
"""

import io
import json
import subprocess
import sys
import tempfile
from pathlib import Path

# Windows 控制台默认 GBK，中文题型名会直接乱码/抛异常
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8", errors="replace")

ROOT = Path(__file__).resolve().parent.parent
OUT = Path(tempfile.gettempdir()) / "cortex-sweep"
OUT.mkdir(exist_ok=True)

KINDS = ["中文语义", "专名精确", "时间回放", "关系推理", "跨域检索", "应召不到"]
CHANNELS = ["bm25", "vector", "graph", "recency", "episode"]


def run(name, args):
    path = OUT / f"{name}.json"
    cmd = ["cargo", "run", "-q", "-p", "cortex-evals", "--", "run", "--json", str(path)] + args
    r = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True,
                       encoding="utf-8", errors="replace")
    if r.returncode != 0 or not path.exists():
        print(f"!! {name} 失败\n{r.stderr[-2000:]}")
        return None
    return json.loads(path.read_text(encoding="utf-8"))


def row(name, rep):
    o = rep["overall"]
    kinds = {a["label"]: a for a in rep["by_kind"]}
    return dict(
        name=name,
        r1=o["recall"]["1"], r5=o["recall"]["5"], r10=o["recall"]["10"],
        rinf=o["recall_any"], mrr=o["mrr"], rank=o["avg_gold_rank"],
        bad5=o["forbidden_rate"]["5"], inj=o["avg_returned"],
        **{k: kinds[k]["recall"]["5"] for k in KINDS if k in kinds},
        unans=kinds["应召不到"]["forbidden_rate"]["5"] if "应召不到" in kinds else 0.0,
        unans_inj=kinds["应召不到"]["avg_returned"] if "应召不到" in kinds else 0.0,
    )


def table(rows):
    head = ["name", "r1", "r5", "r10", "rinf", "mrr", "rank", "bad5", "inj"] + KINDS + ["unans", "unans_inj"]
    print("| " + " | ".join(head) + " |")
    print("|" + "---|" * len(head))
    for r in rows:
        cells = [r["name"]] + [
            f"{r[k]:.3f}" if isinstance(r[k], float) else str(r[k])
            for k in head[1:]
        ]
        print("| " + " | ".join(cells) + " |")


def channels(rows_named):
    print()
    print("| 配置 | " + " | ".join(f"{c} 覆盖/独占/R@5/干扰" for c in CHANNELS) + " |")
    print("|" + "---|" * (len(CHANNELS) + 1))
    for name, rep in rows_named:
        cs = rep["overall"]["channels"]
        cells = []
        for c in CHANNELS:
            v = cs.get(c)
            cells.append("—" if not v else
                         f"{v['covered']}/{v['exclusive']}/{v['recall_at_k']:.3f}/{v['forbidden_contributions']}")
        print(f"| {name} | " + " | ".join(cells) + " |")


SUITES = {
    # 弃权阈值扫描。0.0164 是「单路排名第一」的 RRF 贡献，这条线的两侧行为完全不同
    "abstain": [(f"t={t}", ["--abstain-below", str(t)])
                for t in [0.0, 0.008, 0.012, 0.0165, 0.017, 0.020, 0.025, 0.030, 0.035, 0.045]],
    "recency": [
        ("channel", ["--recency", "channel"]),
        ("off", ["--recency", "off"]),
        ("decay .3/3d", ["--recency", "decay", "--recency-strength", "0.3", "--recency-half-life", "3"]),
        ("decay .6/7d", ["--recency", "decay", "--recency-strength", "0.6", "--recency-half-life", "7"]),
        ("decay .15/1d", ["--recency", "decay", "--recency-strength", "0.15", "--recency-half-life", "1"]),
    ],
    "episodes": [("on", ["--episodes"]), ("off", ["--no-episodes"])],
    # episodes 与强命中补偿是有交互的：episodes 一路返回的是**整段对话里
    # 抽出来的全部事实**，补偿项会连带把 gold 的兄弟事实一起抬上来。
    # 单独 A/B 任何一个都会得出错的结论
    "episodes_x_peak": [
        ("ep+peak", ["--episodes"]),
        ("ep only", ["--episodes", "--peak-bonus", "0"]),
        ("peak only", ["--no-episodes"]),
        ("neither", ["--no-episodes", "--peak-bonus", "0"]),
    ],
    "peak": [(f"w={w}", ["--peak-bonus", str(w)])
             for w in [0.0, 0.005, 0.010, 0.020, 0.040]],
    "replay": [("ranked", []), ("chronological", ["--replay-chronological"])],
}


def main():
    which = sys.argv[1] if len(sys.argv) > 1 else "abstain"
    base = sys.argv[2:]
    # `abstain:0.05,0.08` / `floor:0.4,0.5` —— 临时扫一段，省得改常量表
    if ":" in which:
        head, vals = which.split(":", 1)
        flag = {"abstain": "--abstain-below", "floor": "--semantic-floor",
                "peak": "--peak-bonus"}[head]
        SUITES[which] = [(f"{head}={v}", [flag, v]) for v in vals.split(",")]
    rows, named = [], []
    for name, args in SUITES[which]:
        rep = run(f"{which}-{name}".replace(" ", "_").replace("/", "_"), args + base)
        if rep:
            rows.append(row(name, rep))
            named.append((name, rep))
    table(rows)
    channels(named)


if __name__ == "__main__":
    main()
