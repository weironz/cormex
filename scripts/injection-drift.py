# -*- coding: utf-8 -*-
"""核对 injection.rs 两份实现有没有漂。

`cormex-core/src/injection.rs` 在两个仓库各有一份 —— 这边给 MCP，Cormex 那边给
agent 注入 —— 而它们**没有任何编译期联系**。漂移的症状是同一条记忆经 MCP 看到的
和经 agent 看到的长得不一样，且**不报错**。

比的是去掉注释后的 token 流。注释语言两侧已经分家（这边英文、那边中文），那是
有意的；不能分家的是**渲染出来的字节** —— 所以字符串字面量要参与比较，注释不要。

用法：

    python scripts/injection-drift.py ../cormex/crates/cormex-core/src/injection.rs

拿不到另一份时（开源用户、CI）直接跳过并返回 0 —— 这个检查对只有这一个仓库的人
没有意义，而让它红会把「你缺一个私有仓库」变成一次构建失败。
"""

import io
import os
import re
import sys

MINE = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "crates",
    "cormex-core",
    "src",
    "injection.rs",
)


def tokens(path):
    s = io.open(path, encoding="utf-8").read()
    # 测试模块不比：两侧的测试本来就可以不一样
    cut = s.find("#[cfg(test)]")
    if cut > 0:
        s = s[:cut]
    s = re.sub(r"/\*.*?\*/", "", s, flags=re.S)
    # 逐行削 `//`，**行首行尾都要**。只削行首会把一条行尾注释报成「渲染不一致」，
    # 而一个假阳性比没有检查更糟：下一次真漂的时候没人再信它。
    # 要避开字符串里的 `//`（如 `cormex://profile`），所以跟一下引号状态。
    out = []
    for line in s.split("\n"):
        in_str = False
        i = 0
        while i < len(line):
            c = line[i]
            if c == "\\" and in_str:
                i += 2
                continue
            if c == '"':
                in_str = not in_str
            elif c == "/" and not in_str and line[i : i + 2] == "//":
                line = line[:i]
                break
            i += 1
        out.append(line)
    return re.findall(r"\w+|[^\s\w]", "\n".join(out))


def main():
    if len(sys.argv) != 2:
        print(__doc__.strip().split("\n\n")[-2])
        return 2
    other = sys.argv[1]
    if not os.path.exists(other):
        print("skipped: %s does not exist" % other)
        return 0

    a, b = tokens(MINE), tokens(other)
    if a == b:
        print("OK  identical after stripping comments (%d tokens)" % len(a))
        return 0

    print("DRIFT  the two renderers no longer agree")
    n = min(len(a), len(b))
    for i in range(n):
        if a[i] != b[i]:
            print("  first divergence at token %d: here=%r there=%r" % (i, a[i], b[i]))
            print("  here : %s" % " ".join(a[max(0, i - 12) : i + 12]))
            print("  there: %s" % " ".join(b[max(0, i - 12) : i + 12]))
            return 1
    print("  first %d tokens agree, then lengths differ: %d vs %d" % (n, len(a), len(b)))
    return 1


if __name__ == "__main__":
    sys.exit(main())
