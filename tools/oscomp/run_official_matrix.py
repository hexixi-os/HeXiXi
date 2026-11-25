#!/usr/bin/env python3
"""Run the 2025 preliminary RISC-V64 or LoongArch64 image one original group per boot."""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time


REPO = Path(__file__).resolve().parents[2]
GROUPS = (
    "basic", "busybox", "lua", "libctest", "libcbench", "lmbench",
    "unixbench", "iozone", "cyclictest", "ltp", "iperf", "netperf",
)
LIBCS = ("musl", "glibc")
BASIC_JUDGE = Path("basic/user/src/oscomp/test_runner.py")
COOPERATION_JUDGES = {"busybox", "lua", "libctest", "iozone"}
SHUTDOWN = "[kernel] Idle process exit with exit_code 0 "
KERNEL_PANIC = re.compile(r"\[kernel\] PANIC|^Panicked at")
CASE_RESULT = re.compile(r"^====== (cyclictest|iperf|netperf) (.+?) end: (success|fail) ======$", re.M)
LTP_EXIT = re.compile(r"^FAIL LTP CASE (.+?)\s*:\s*(\d+)$", re.M)


def select_groups(requested):
    names = [f"{group}-{libc}" for group in GROUPS for libc in LIBCS]
    selected = []
    for entry in requested:
        if entry in GROUPS:
            selected.extend(f"{entry}-{libc}" for libc in LIBCS)
        elif entry in names:
            selected.append(entry)
        else:
            raise ValueError(f"未知测试组 {entry!r}")
    return list(dict.fromkeys(selected)) if selected else names


def matrix_failed(results, strict):
    for row in results:
        judge = row["judge"]
        if judge["status"] == "error":
            return True
        if not strict:
            continue
        if row["status"] != "complete":
            return True
        if row["group"].startswith("basic-") and not (
            judge["status"] == "scored" and judge.get("score") == judge.get("maximum") == 102
        ):
            return True
    return False


def inspect_serial(serial, name, qemu_exit, timed_out):
    lines = serial.replace(b"\r", b"").replace(b"\0", b"").decode(errors="replace").splitlines()
    starts = [i for i, line in enumerate(lines) if line == f"#### OS COMP TEST GROUP START {name} ####"]
    ends = [i for i, line in enumerate(lines) if line == f"#### OS COMP TEST GROUP END {name} ####"]
    script_result = re.compile(rf"^\[oscomp\] result {re.escape(name)} exit=(-?\d+)$")
    results = [(i, match) for i, line in enumerate(lines) if (match := script_result.fullmatch(line))]
    result_at, script_exit = (results[0][0], int(results[0][1][1])) if len(results) == 1 else (len(lines), None)
    panics = [i for i, line in enumerate(lines) if KERNEL_PANIC.search(line)]
    coverage = "none"
    body = ""
    if len(starts) == 1 and len(ends) <= 1:
        end = min(ends + panics + [result_at, len(lines)])
        if starts[0] < end:
            body = "".join(line + "\n" for line in lines[starts[0] + 1:end])
            coverage = "complete" if ends == [end] else "partial"
    shutdown = any(line.startswith(SHUTDOWN) for line in lines[result_at + 1:])
    diagnostics = [
        line for line in lines if KERNEL_PANIC.search(line)
        or line.startswith(("[oscomp] cannot exec", "[oscomp] missing", "[oscomp] unknown group"))
    ]

    if panics:
        status = "panic"
    elif timed_out:
        status = "timeout"
    elif qemu_exit != 0:
        status = "qemu_error"
    elif not starts and script_exit not in (0, None):
        status = "launcher_error"
    elif script_exit not in (0, None):
        status = "script_error"
    elif coverage == "complete" and script_exit == 0 and shutdown:
        status = "complete"
    else:
        status = "incomplete"

    cases = [{"name": case, "result": result} for _, case, result in CASE_RESULT.findall(body)]
    cases.extend({"name": case, "exit": int(code)} for case, code in LTP_EXIT.findall(body))
    return {
        "status": status,
        "coverage": coverage,
        "script_exit": script_exit,
        "diagnostics": diagnostics,
        "case_results": cases,
    }, body


def write_json(path, data):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    temporary.replace(path)


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for data in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(data)
    return digest.hexdigest()


def checkout(path):
    def git(*args):
        return subprocess.check_output(["git", "-C", str(path), *args], text=True).strip()

    return {"commit": git("rev-parse", "HEAD"), "dirty": bool(git("status", "--porcelain"))}


def judge_group(judge, basic, log, output):
    info = {"path": str(judge), "sha256": sha256(judge)}
    command = [sys.executable, "-B", judge.name]
    if basic:
        command.append(str(log))
    try:
        with log.open("rb") as stream:
            judged = subprocess.run(
                command, cwd=judge.parent, stdin=stream, capture_output=True, timeout=30,
            )
        if judged.returncode:
            raise ValueError(judged.stderr.decode(errors="replace").strip() or f"exit {judged.returncode}")
        cases = json.loads(judged.stdout.decode().strip().splitlines()[-1])
        score = sum(case["passed" if basic else "score"] for case in cases)
        maximum = sum(case["all"] for case in cases) if all("all" in case for case in cases) else None
        write_json(output, cases)
        info.update(status="scored", score=score, maximum=maximum, cases=len(cases))
    except (ValueError, IndexError, KeyError, TypeError, OSError, subprocess.TimeoutExpired) as error:
        info.update(status="error", error=str(error))
    return info


def drive(path, name):
    escaped = str(path).replace(",", ",,")
    return f"file={escaped},if=none,format=raw,id={name}"


def run_group(args, name, run, kernel):
    directory = run / name
    directory.mkdir()
    serial = directory / "serial.log"
    group_log = directory / "group.log"
    command = [
        args.qemu, "-machine", "virt", "-m", args.memory, "-smp", str(args.smp),
        "-nographic", "-kernel", str(kernel), "-no-reboot",
    ]
    with tempfile.NamedTemporaryFile(dir=directory, prefix="scratch.", suffix=".img") as scratch:
        scratch.truncate(64 * 1024 * 1024)
        if args.arch == "riscv64":
            bootargs = f"oscomp.group={name}"
            if args.legacy_image:
                bootargs += " oscomp.legacy=1"
            command.extend([
                "-bios", "default", "-append", bootargs,
                "-snapshot", "-drive", drive(args.image, "x0"),
                "-device", "virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0",
                "-device", "virtio-net-device,netdev=net", "-netdev", "user,id=net",
                "-rtc", "base=utc", "-drive", drive(scratch.name, "x1"),
                "-device", "virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1",
            ])
        else:
            if args.legacy_image:
                command.extend(["-fw_cfg", "name=opt/hexixi/oscomp.legacy,string=1"])
            command.extend([
                "-fw_cfg", f"name=opt/hexixi/oscomp.group,string={name}",
                "-drive", drive(args.image, "x0") + ",readonly=on",
                "-device", "virtio-blk-pci,drive=x0,disable-legacy=on,romfile=",
                "-nic", "none", "-monitor", "none",
            ])
        result_path = directory / "result.json"
        result = {"group": name, "qemu_command": command, "timeout_seconds": args.timeout}
        write_json(result_path, {**result, "status": "running"})
        started = time.monotonic()
        qemu_exit = None
        timed_out = False
        with serial.open("wb") as stream:
            try:
                qemu_exit = subprocess.run(
                    command, stdin=subprocess.DEVNULL, stdout=stream, stderr=subprocess.STDOUT,
                    timeout=args.timeout,
                ).returncode
            except subprocess.TimeoutExpired:
                timed_out = True

    evidence, body = inspect_serial(serial.read_bytes(), name, qemu_exit, timed_out)
    group_log.write_text(body, encoding="utf-8")
    result.update(evidence, qemu_exit=qemu_exit, timed_out=timed_out,
                  elapsed_seconds=round(time.monotonic() - started, 3))
    group, _ = name.rsplit("-", 1)
    if group == "basic":
        judge = args.suite / BASIC_JUDGE
    elif args.judges and group in COOPERATION_JUDGES:
        judge = args.judges / "judge" / f"judge_{group}.py"
    else:
        judge = None
    result["judge"] = (judge_group(judge, group == "basic", group_log, directory / "score.json")
                       if judge and judge.is_file() else {"status": "unavailable"})
    write_json(result_path, result)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image", type=Path, help="对应架构的官方 EXT4 镜像")
    parser.add_argument("output", type=Path, help="保存评测记录的父目录")
    parser.add_argument("groups", nargs="*", help="测试组名，如 basic 或 basic-musl，留空运行全部")
    parser.add_argument("--suite", type=Path, default=os.getenv("SUITE", str(Path.home() / "testsuits")))
    parser.add_argument("--judges", type=Path, default=os.getenv("OSCOMP_JUDGES"),
                        help="可选的 oskernel-testsuits-cooperation 仓库")
    parser.add_argument("--kernel", type=Path, default=os.getenv("KERNEL"), help="复用已构建的内核")
    parser.add_argument("--arch", choices=("riscv64", "loongarch64"), default="riscv64")
    parser.add_argument("--qemu", default=os.getenv("QEMU"))
    parser.add_argument("--memory", default=os.getenv("QEMU_MEM", "1G"))
    parser.add_argument("--smp", type=int, choices=range(1, 9), default=os.getenv("QEMU_SMP", "1"))
    parser.add_argument("--timeout", type=float, default=os.getenv("QEMU_TIMEOUT", "200"))
    parser.add_argument("--legacy-image", action="store_true", help="显式启用旧评测镜像兼容")
    parser.add_argument("--strict", action="store_true", help="要求各组正常完成，且 basic 为 102/102")
    args = parser.parse_intermixed_args()
    args.qemu = args.qemu or f"qemu-system-{args.arch}"
    if not 1 <= args.smp <= 8:
        parser.error("--smp 必须在 1 至 8 之间")
    if args.arch == "loongarch64" and args.smp != 1:
        parser.error("LoongArch64 当前只支持单核")
    try:
        names = select_groups(args.groups)
    except ValueError as error:
        parser.error(str(error))
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout 必须为正数")
    args.image = args.image.resolve()
    args.suite = args.suite.resolve()
    if args.judges:
        args.judges = args.judges.resolve()
    if not args.image.is_file():
        parser.error(f"找不到镜像 {args.image}")
    if not (args.suite / BASIC_JUDGE).is_file():
        parser.error(f"在 {args.suite} 找不到 basic 判分器")
    if args.kernel and not args.kernel.is_file():
        parser.error(f"找不到内核 {args.kernel}")

    args.output.mkdir(parents=True, exist_ok=True)
    run = Path(tempfile.mkdtemp(prefix="run.", dir=args.output.resolve()))
    print(f"评测记录 {run}", flush=True)
    target, output = ("all", "KERNEL_OUT") if args.arch == "riscv64" else ("kernel-la", "LA_KERNEL_OUT")
    kernel = args.kernel.resolve() if args.kernel else run / ("kernel-rv" if target == "all" else target)
    metadata = {
        "source": checkout(REPO),
        "arch": args.arch,
        "harts": args.smp,
        "legacy_image": args.legacy_image,
        "strict": args.strict,
        "suite": checkout(args.suite),
        "judge_checkout": checkout(args.judges) if args.judges else None,
        "qemu_version": subprocess.check_output([args.qemu, "--version"], text=True).splitlines()[0],
        "groups": names,
        "image": str(args.image),
        "image_sha256": sha256(args.image),
        "kernel": str(kernel),
        "kernel_origin": "provided" if args.kernel else "built_from_checkout",
    }
    write_json(run / "run.meta.json", metadata)
    if not args.kernel:
        with (run / "build.log").open("wb") as log:
            command = ["make", "-C", str(REPO), target, f"{output}={kernel}"]
            if args.smp > 1:
                command.append("SMP=1")
            build = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
        if build.returncode:
            sys.exit(f"内核构建失败，见 {run / 'build.log'}")
    metadata["kernel_sha256"] = sha256(kernel)
    write_json(run / "run.meta.json", metadata)
    results = []
    for name in names:
        result = run_group(args, name, run, kernel)
        results.append(result)
        write_json(run / "matrix.json", results)
        judge = result["judge"]
        score = judge.get("score", "")
        if judge.get("maximum") is not None:
            score = f"{score}/{judge['maximum']}"
        print(f"{name:20} {result['status']:15} {result['elapsed_seconds']:7.2f}s  {score}", flush=True)
    return int(matrix_failed(results, args.strict))


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
