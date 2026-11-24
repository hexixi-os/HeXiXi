#!/usr/bin/env bash
# 验证 EXT4 区间读取、页缓存、稀疏文件和 extent 树，并可选验证官方盘跨块组读取。
set -euo pipefail

tool_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
official_image=${1:-}
if [ -n "$official_image" ]; then
    official_image=$(cd "$(dirname "$official_image")" && pwd)/$(basename "$official_image")
fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"
rustc +stable --edition 2021 "$tool_dir/tests/ext4_probe.rs" -o ext4_probe
rustc +stable --edition 2021 --cfg 'feature="page_cache_adaptive"' \
    "$tool_dir/tests/ext4_probe.rs" -o ext4_probe_adaptive

truncate -s 32M fixture.img
mkfs.ext4 -q -F -b 4096 fixture.img
python3 - <<'PY'
from pathlib import Path

Path("expected").write_bytes(b"A" * 4096 + b"\0" * 4096 + b"Z" * 100)
Path("tree.expected").write_bytes(
    b"".join(bytes([letter]) * 4096 + b"\0" * 4096 for letter in b"ABCDEF")
)
with Path("dense.expected").open("wb") as dense:
    for block in range(1, 2049):
        dense.write(block.to_bytes(4, "little") * 1024)
PY
debugfs -w -R 'write expected sparse' fixture.img
debugfs -w -R 'write expected unwritten' fixture.img
debugfs -w -R 'write tree.expected tree' fixture.img
debugfs -w -R 'write dense.expected dense' fixture.img
debugfs -w -R 'fallocate /unwritten 1 1' fixture.img
block=$(debugfs -R 'bmap /unwritten 1' fixture.img)
python3 - "$block" <<'PY'
import sys

block, state = sys.argv[1].split()
assert state == "(uninit)", sys.argv[1]
with open("fixture.img", "r+b") as image:
    image.seek(int(block) * 4096)
    image.write(b"U" * 4096)
PY

debugfs -R 'dump /dense dense.reference' fixture.img
cmp dense.expected dense.reference
./ext4_probe fixture.img /dense dense.reference
./ext4_probe_adaptive fixture.img /dense dense.reference

for path in sparse unwritten; do
    debugfs -R "dump /$path $path.reference" fixture.img
    cmp expected "$path.reference"
    ./ext4_probe fixture.img "/$path" "$path.reference"
done

# inode 只能内联 4 个 extent。这份文件有 6 个数据 extent，必须查子节点。
debugfs -R 'dump /tree tree.reference' fixture.img
cmp tree.expected tree.reference
./ext4_probe fixture.img /tree tree.reference

if [ -n "$official_image" ]; then
    debugfs -R 'dump /glibc/basic/brk brk.reference' "$official_image"
    ./ext4_probe "$official_image" /glibc/basic/brk brk.reference
fi
