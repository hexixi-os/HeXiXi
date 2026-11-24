"""Make static RISC-V ELF images with precise, linker-independent PT_LOAD layouts."""
import struct
import sys
from pathlib import Path

PAGE = 4096
HEADER = struct.Struct("<16sHHIQQQIHHHHHH")
PROGRAM_HEADER = struct.Struct("<IIQQQQQQ")
TEXT = 0x10000


def load(flags, offset, address, filesz, memsz):
    return (1, flags, offset, address, address, filesz, memsz, PAGE)


def executable(programs, payloads):
    end = max(offset + len(data) for offset, data in payloads)
    image = bytearray(end)
    ident = b"\x7fELF\x02\x01\x01" + bytes(9)
    HEADER.pack_into(image, 0, ident, 2, 243, 1, TEXT, HEADER.size, 0, 0,
                     HEADER.size, PROGRAM_HEADER.size, len(programs), 0, 0, 0)
    for index, program in enumerate(programs):
        PROGRAM_HEADER.pack_into(image, HEADER.size + index * PROGRAM_HEADER.size, *program)
    for offset, data in payloads:
        image[offset:offset + len(data)] = data
    return image


def addi(dest, source, value):
    return (value & 0xfff) << 20 | source << 15 | dest << 7 | 0x13


def lui(dest, address):
    return (address & ~0xfff) | dest << 7 | 0x37


def exit_code(status):
    return [addi(10, 0, status), addi(17, 0, 93), 0x73]


def check_program():
    # t0 holds a page-aligned base, t1 is the loaded word, t2 is the expected word.
    instructions = []
    branches = []

    def address(value):
        instructions.append(lui(5, value))  # lui t0
        return value & 0xfff

    def expect_equal(left, right):
        branches.append((len(instructions), left, right))
        instructions.append(0)

    def check(value, expected):
        offset = address(value)
        instructions.append(offset << 20 | 5 << 15 | 2 << 12 | 6 << 7 | 0x03)  # lw t1
        instructions.append(addi(7, 0, expected))
        expect_equal(6, 7)

    check(0x20010, 31)  # earlier PT_LOAD in the same page
    check(0x20018, 47)  # non-aligned writable PT_LOAD
    check(0x21000, 61)  # whole writable file page, eligible for COW
    # mprotect(0x22000, PAGE, PROT_READ) splits an executable VMA before this page faults.
    instructions.extend([lui(10, 0x22000), lui(11, PAGE), addi(12, 0, 1), addi(17, 0, 226), 0x73])
    expect_equal(10, 0)
    check(0x22008, 73)  # end of the file-backed bytes
    check(0x2200C, 0)   # BSS in that same page
    # pipe copies its result into an untouched BSS page.
    instructions.extend([lui(10, 0x23000), addi(10, 10, 8), addi(17, 0, 59), 0x73])
    expect_equal(10, 0)
    check(0x23000, 0)
    address(0x21000)
    instructions.append(addi(6, 0, 99))
    instructions.append(6 << 20 | 5 << 15 | 2 << 12 | 0x23)  # sw t1, 0(t0)
    check(0x21000, 99)
    instructions.extend(exit_code(0))
    failure = len(instructions)
    instructions.extend(exit_code(1))
    for branch, left, right in branches:
        delta = (failure - branch) * 4
        instructions[branch] = (((delta >> 12) & 1) << 31
                                | ((delta >> 5) & 0x3f) << 25
                                | right << 20 | left << 15 | 1 << 12  # bne
                                | ((delta >> 1) & 0xf) << 8
                                | ((delta >> 11) & 1) << 7 | 0x63)
    return struct.pack(f"<{len(instructions)}I", *instructions)


def write_fixtures(root):
    boundary_code = check_program().ljust(PAGE, b"\0")
    boundary = executable([
        load(5, 0x1000, TEXT, PAGE, PAGE),
        load(4, 0x2010, 0x20010, 4, 4),
        load(6, 0x3018, 0x20018, 0x1ff4, 0x3fe8),
    ], [
        (0x1000, boundary_code),
        (0x2010, struct.pack("<I", 31)),
        (0x3018, struct.pack("<I", 47)),
        (0x4000, struct.pack("<I", 61)),
        (0x5008, struct.pack("<II", 73, 0x7f7f7f7f)),
    ])
    root.joinpath("elf-boundary").write_bytes(boundary)

    warmup = struct.pack("<3I", *exit_code(0)).ljust(PAGE, b"\0")
    cold = b"X" * (8 * 1024 * 1024)
    startup = executable([
        load(5, 0x1000, TEXT, PAGE, PAGE),
        load(4, 0x2000, 0x4000000, len(cold), 32 * 1024 * 1024),
    ], [(0x1000, warmup), (0x2000, cold)])
    root.joinpath("elf-startup").write_bytes(startup)
    root.joinpath("elf-short-header").write_bytes(b"\x7fELF")
    root.joinpath("elf-short-segment").write_bytes(startup[:0x2001])
    root.joinpath("elf-long-interpreter").write_bytes(b"#!/" + b"x" * 300 + b"\n")


if __name__ == "__main__":
    write_fixtures(Path(sys.argv[1]))
