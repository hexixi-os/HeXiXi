#!/usr/bin/env bash
# score_basic.sh — 用官方判分器给 basic 组的串口日志打分
#
# 用法：tools/oscomp/score_basic.sh <串口日志>
#   例：tools/oscomp/score_basic.sh ~/oscomp-work/official/basic.log
#
# 判分器是官方 $SUITE/basic/user/src/oscomp/test_runner.py：它在日志里找
# "========== START test_xxx ==========" 与 "END" 之间的输出，逐条断言（*_test.py）。
# 每个用例的满分 = 该 *_test.py 里的断言条数，总分 = 全部断言之和。
# 没有出现 START 的用例得 0 分；只有 START 没有 END（如 execve、中途崩溃）仍会被判分。
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"
need_suite

LOG=${1:-}
[ -f "$LOG" ] || die "用法: $0 <串口日志>"

# 串口日志里夹着 CR 和 OpenSBI 横幅，先清洗成纯 LF 文本
CLEAN=$(mktemp); trap 'rm -f "$CLEAN"' EXIT
tr -d '\r\0' < "$LOG" > "$CLEAN"

python3 -I - "$SUITE/basic/user/src/oscomp" "$CLEAN" <<'EOF'
import json, subprocess, sys
d, log = sys.argv[1], sys.argv[2]
out = subprocess.run([sys.executable, "test_runner.py", log], cwd=d,
                     capture_output=True, text=True)
if out.returncode != 0:
    sys.exit("test_runner.py 失败:\n" + out.stderr)
res = json.loads(out.stdout.strip().splitlines()[-1])
tot = sum(r["all"] for r in res)
got = sum(r["passed"] for r in res)
bad = [r for r in res if r["passed"] < r["all"]]
print(f"basic 得分: {got}/{tot}   用例满分 {len(res) - len(bad)}/{len(res)}")
for r in sorted(bad, key=lambda r: r["name"]):
    fails = [x for x in r["results"] if not x["res"]]
    why = f"  首个失败断言: {fails[0]['arg']} ({fails[0]['rep']})" if fails else "  （日志里没有这个用例）"
    print(f"  ✗ {r['name']}: {r['passed']}/{r['all']}{why}")
EOF
