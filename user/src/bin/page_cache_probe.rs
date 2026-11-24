#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::arch::asm;
use user_lib::{close, exec, exit, fork, open, read, waitpid, OpenFlags};

const PAGE: usize = 4096;
const COLD_PAGES: usize = 1024;
const HOT_PAGES: usize = 128;
const ROUNDS: usize = 3;

#[repr(align(4096))]
struct Block([u8; PAGE]);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Policy {
    None,
    Lru,
    Adaptive,
}

#[derive(Clone, Copy)]
struct Stats {
    policy: Policy,
    capacity: usize,
    hits: usize,
    misses: usize,
    evictions: usize,
    bypasses: usize,
    refaults: usize,
    resident: usize,
    pinned: usize,
    history: usize,
    sectors: usize,
}

fn syscall(number: usize, args: [usize; 3]) -> isize {
    let result;
    unsafe {
        asm!(
            "ecall", in("a7") number, inlateout("a0") args[0] => result,
            in("a1") args[1], in("a2") args[2],
        );
    }
    result
}

fn open_read(path: &str) -> usize {
    let fd = open(path, OpenFlags::RDONLY);
    assert!(fd >= 0, "open {}", path);
    fd as usize
}

fn snapshot() -> Stats {
    read_stats(open_read("/proc/page_cache\0"))
}

fn read_stats(fd: usize) -> Stats {
    let mut buffer = [0u8; 512];
    let len = read(fd, &mut buffer);
    assert!(len > 0 && len < buffer.len() as isize);
    assert_eq!(close(fd), 0);
    let text = core::str::from_utf8(&buffer[..len as usize]).unwrap();
    let field = |key| {
        text.lines()
            .filter_map(|line| line.split_once('='))
            .find(|&(name, _)| name == key)
            .unwrap()
            .1
    };
    let number = |key| field(key).parse::<usize>().unwrap();
    Stats {
        policy: match field("policy") {
            "none" => Policy::None,
            "lru" => Policy::Lru,
            "adaptive" => Policy::Adaptive,
            _ => panic!("unknown page cache policy"),
        },
        capacity: number("capacity"),
        hits: number("hits"),
        misses: number("misses"),
        evictions: number("evictions"),
        bypasses: number("bypasses"),
        refaults: number("refaults"),
        resident: number("resident_pages"),
        pinned: number("pinned_pages"),
        history: number("history_entries"),
        sectors: number("ext4_sectors_read"),
    }
}

fn check_stats_snapshot(config: usize) {
    let path = "/proc/page_cache\0";
    assert_eq!(open(path, OpenFlags::WRONLY), -30);
    assert_eq!(
        syscall(35, [(-100isize) as usize, path.as_ptr() as usize, 0]),
        -30
    );
    let old = open_read(path);
    assert_eq!(syscall(46, [old, 0, 0]), -30);
    let mut bytes = [0; 24];
    for _ in 0..8 {
        seek_page(config, 0);
        assert!(read(config, &mut bytes) > 0);
    }
    let current = snapshot();
    let earlier = read_stats(old);
    assert!(current.hits + current.misses >= earlier.hits + earlier.misses + 8);
}

fn now_us() -> usize {
    let mut tv = [0usize; 2];
    assert_eq!(syscall(169, [tv.as_mut_ptr() as usize, 0, 0]), 0);
    tv[0] * 1_000_000 + tv[1]
}

fn seek_page(fd: usize, page: usize) {
    assert_eq!(syscall(62, [fd, page * PAGE, 0]), (page * PAGE) as isize);
}

fn read_page(fd: usize, block: &mut Block, value: usize) {
    assert_eq!(read(fd, &mut block.0), PAGE as isize);
    let expected = (value as u32).to_le_bytes();
    assert_eq!(&block.0[..4], &expected);
    assert_eq!(&block.0[PAGE - 4..], &expected);
}

fn scan(fd: usize, block: &mut Block, first: usize, count: usize) {
    seek_page(fd, first);
    for page in first..first + count {
        read_page(fd, block, 0x10000 + page);
    }
}

fn hot(fd: usize, block: &mut Block, count: usize) {
    for index in 0..count {
        let page = (index * 17) % count;
        seek_page(fd, page);
        read_page(fd, block, 0x20000 + page);
    }
}

fn launch_busybox() {
    let path = "page-cache-busybox\0";
    let argv = [b"busybox\0".as_ptr(), b"true\0".as_ptr(), core::ptr::null()];
    let pid = fork();
    assert!(pid >= 0);
    if pid == 0 {
        exec(path, &argv);
        exit(126);
    }
    let mut code = -1;
    assert_eq!(waitpid(pid as usize, &mut code), pid);
    assert_eq!(code, 0);
}

fn exercise(scenario: &str, cold: usize, warm: usize, block: &mut Block) -> (usize, usize) {
    let mut hot_hits = 0;
    let mut hot_misses = 0;
    match scenario {
        "mixed32" | "mixed128" => {
            let count = if scenario == "mixed32" { 32 } else { HOT_PAGES };
            hot(warm, block, count);
            for _ in 0..ROUNDS {
                scan(cold, block, 0, COLD_PAGES);
                let before = snapshot();
                hot(warm, block, count);
                let after = snapshot();
                hot_hits += after.hits - before.hits;
                hot_misses += after.misses - before.misses;
            }
        }
        "sequential" => scan(cold, block, 0, COLD_PAGES),
        "reuse" => {
            for _ in 0..ROUNDS {
                scan(cold, block, 0, HOT_PAGES);
            }
        }
        "revisit" => {
            scan(cold, block, 0, 256);
            scan(cold, block, 512, HOT_PAGES);
            scan(cold, block, 512, HOT_PAGES);
            let before = snapshot();
            scan(cold, block, 512, HOT_PAGES);
            let after = snapshot();
            hot_hits = after.hits - before.hits;
            hot_misses = after.misses - before.misses;
        }
        "busybox" => {
            for _ in 0..8 {
                launch_busybox();
            }
        }
        _ => panic!("unknown scenario {}", scenario),
    }
    (hot_hits, hot_misses)
}

#[no_mangle]
fn main() -> i32 {
    let config = open_read("scenario\0");
    let mut name = [0; 24];
    let len = read(config, &mut name);
    assert!(len > 0 && len <= name.len() as isize);
    let scenario = core::str::from_utf8(&name[..len as usize]).unwrap().trim();
    check_stats_snapshot(config);
    close(config);

    let cold = open_read("cold.bin\0");
    let warm = open_read("hot.bin\0");
    let mut block = Block([0; PAGE]);
    let before = snapshot();
    let start = now_us();
    let (hot_hits, hot_misses) = exercise(scenario, cold, warm, &mut block);
    let elapsed = now_us() - start;
    let after = snapshot();
    assert_eq!(after.capacity, 256);
    assert!(after.resident <= after.capacity && after.pinned <= after.resident);
    assert!(after.history <= after.capacity * 2);
    assert!(after.refaults - before.refaults <= after.misses - before.misses);

    let monitored = matches!(scenario, "mixed32" | "mixed128" | "revisit");
    match after.policy {
        Policy::None => {
            assert_eq!(after.resident, 0);
            assert_eq!(after.hits, 0);
            assert_eq!(after.evictions, 0);
            assert_eq!(after.bypasses, after.misses);
            if monitored {
                assert_eq!(hot_hits, 0);
                assert!(hot_misses != 0);
            }
        }
        Policy::Lru if scenario == "mixed32" || scenario == "mixed128" => {
            assert!(hot_misses != 0);
            assert!(after.evictions > before.evictions);
        }
        Policy::Adaptive if monitored => {
            assert_eq!(hot_misses, 0);
            assert!(hot_hits != 0);
            assert!(after.bypasses > before.bypasses);
        }
        _ => {}
    }
    if scenario == "reuse" && after.policy != Policy::None {
        assert!(after.hits - before.hits >= (ROUNDS - 1) * HOT_PAGES);
    }

    println!(
        "page-cache-result scenario={} policy={:?} hits={} misses={} evictions={} bypasses={} refaults={} resident={} pinned={} history={} sectors={} hot_hits={} hot_misses={} us={}",
        scenario, after.policy, after.hits - before.hits, after.misses - before.misses,
        after.evictions - before.evictions, after.bypasses - before.bypasses,
        after.refaults - before.refaults, after.resident, after.pinned, after.history,
        after.sectors - before.sectors, hot_hits, hot_misses, elapsed,
    );
    close(cold);
    close(warm);
    0
}
