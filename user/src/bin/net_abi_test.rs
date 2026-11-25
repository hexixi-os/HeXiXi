#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::arch::asm;
use core::mem::size_of;
use core::sync::atomic::{AtomicUsize, Ordering};
use user_lib::{close, exit, fork, open, read, waitpid, write, OpenFlags};

const SYS_EPOLL_CREATE1: usize = 20;
const SYS_EPOLL_CTL: usize = 21;
const SYS_EPOLL_PWAIT: usize = 22;
const SYS_FCNTL: usize = 25;
const SYS_UNLINKAT: usize = 35;
const SYS_PSELECT6: usize = 72;
const SYS_PPOLL: usize = 73;
const SYS_KILL: usize = 129;
const SYS_RT_SIGACTION: usize = 134;
const SYS_RT_SIGPROCMASK: usize = 135;
const SYS_GETPID: usize = 172;
const SYS_SOCKET: usize = 198;
const SYS_SOCKETPAIR: usize = 199;
const SYS_BIND: usize = 200;
const SYS_LISTEN: usize = 201;
const SYS_ACCEPT: usize = 202;
const SYS_CONNECT: usize = 203;
const SYS_GETSOCKNAME: usize = 204;
const SYS_SENDTO: usize = 206;
const SYS_RECVFROM: usize = 207;
const SYS_SETSOCKOPT: usize = 208;
const SYS_GETSOCKOPT: usize = 209;
const SYS_SHUTDOWN: usize = 210;
const SYS_ACCEPT4: usize = 242;

const AT_FDCWD: usize = (-100isize) as usize;
const AF_UNIX: usize = 1;
const AF_INET: usize = 2;
const SOCK_STREAM: usize = 1;
const SOCK_DGRAM: usize = 2;
const SOCK_SEQPACKET: usize = 5;
const SOCK_NONBLOCK: usize = 0o4000;
const SOCK_CLOEXEC: usize = 0o2000000;
const MSG_PEEK: usize = 0x2;
const MSG_TRUNC: usize = 0x20;
const MSG_NOSIGNAL: usize = 0x4000;
const SOL_SOCKET: usize = 1;
const SO_PEERCRED: usize = 17;
const IPPROTO_TCP: usize = 6;
const TCP_INFO: usize = 11;
const F_GETFD: usize = 1;
const F_GETFL: usize = 3;
const F_SETFL: usize = 4;
const FD_CLOEXEC: isize = 1;
const POLLIN: i16 = 1;
const POLLRDHUP: i16 = 0x2000;
const EPOLLET: u32 = 1 << 31;
const SIGUSR1: usize = 10;
const SIG_BLOCK: usize = 0;
const SIG_SETMASK: usize = 2;
const LOOPBACK: [u8; 4] = [127, 0, 0, 1];
const VIRTIO_PORT: u16 = 42019;

const ENOENT: isize = -2;
const EINTR: isize = -4;
const EBADF: isize = -9;
const EAGAIN: isize = -11;
const EINVAL: isize = -22;
const EPIPE: isize = -32;
const ENOTSOCK: isize = -88;
const ENOPROTOOPT: isize = -92;
const EAFNOSUPPORT: isize = -97;
const EADDRINUSE: isize = -98;
const ECONNREFUSED: isize = -111;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SockAddrIn {
    family: u16,
    port: [u8; 2],
    addr: [u8; 4],
    padding: [u8; 8],
}

impl SockAddrIn {
    fn new(addr: [u8; 4], port: u16) -> Self {
        Self {
            family: AF_INET as u16,
            port: port.to_be_bytes(),
            addr,
            padding: [0; 8],
        }
    }

    fn as_ptr(&self) -> usize {
        self as *const Self as usize
    }

    fn as_mut_ptr(&mut self) -> usize {
        self as *mut Self as usize
    }
}

const SOCKADDR_LEN: usize = size_of::<SockAddrIn>();

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

fn syscall6(id: usize, [a0, a1, a2, a3, a4, a5]: [usize; 6]) -> isize {
    let result;
    unsafe {
        #[cfg(target_arch = "riscv64")]
        asm!(
            "ecall",
            in("a7") id,
            inlateout("a0") a0 => result,
            in("a1") a1,
            in("a2") a2,
            in("a3") a3,
            in("a4") a4,
            in("a5") a5,
        );
        #[cfg(target_arch = "loongarch64")]
        asm!(
            "syscall 0",
            in("$a7") id,
            inlateout("$a0") a0 => result,
            in("$a1") a1,
            in("$a2") a2,
            in("$a3") a3,
            in("$a4") a4,
            in("$a5") a5,
        );
    }
    result
}

fn syscall(id: usize, [a, b, c]: [usize; 3]) -> isize {
    syscall6(id, [a, b, c, 0, 0, 0])
}

fn expect_fd(result: isize, operation: &str) -> usize {
    assert!(result >= 0, "{} returned {}", operation, result);
    result as usize
}

fn socket(kind: usize) -> usize {
    expect_fd(syscall(SYS_SOCKET, [AF_INET, kind, 0]), "socket(AF_INET)")
}

fn bind(fd: usize, addr: &SockAddrIn) -> isize {
    syscall(SYS_BIND, [fd, addr.as_ptr(), SOCKADDR_LEN])
}

fn listen(fd: usize) -> isize {
    syscall(SYS_LISTEN, [fd, 2, 0])
}

fn bind_loopback(fd: usize) -> SockAddrIn {
    assert_eq!(bind(fd, &SockAddrIn::new(LOOPBACK, 0)), 0, "bind");
    let mut addr = SockAddrIn::default();
    let mut len = SOCKADDR_LEN as u32;
    assert_eq!(
        syscall(
            SYS_GETSOCKNAME,
            [fd, addr.as_mut_ptr(), &mut len as *mut u32 as usize]
        ),
        0,
        "getsockname"
    );
    assert_eq!(len, SOCKADDR_LEN as u32);
    assert_eq!(addr.family, AF_INET as u16);
    assert_eq!(addr.addr, LOOPBACK);
    assert_ne!(addr.port, [0; 2]);
    addr
}

fn read_exact(fd: usize, mut bytes: &mut [u8]) {
    while !bytes.is_empty() {
        let count = read(fd, bytes);
        assert!(
            count > 0 && count as usize <= bytes.len(),
            "read returned {}",
            count
        );
        bytes = &mut bytes[count as usize..];
    }
}

fn write_all(fd: usize, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let count = write(fd, bytes);
        assert!(
            count > 0 && count as usize <= bytes.len(),
            "write returned {}",
            count
        );
        bytes = &bytes[count as usize..];
    }
}

fn test_tcp_loopback() {
    const MESSAGE: &[u8; 12] = b"pr19-tcp-abi";
    let listener = socket(SOCK_STREAM);
    let addr = bind_loopback(listener);
    assert_eq!(listen(listener), 0, "listen on TCP port");

    let mut events = PollFd {
        fd: listener as i32,
        events: POLLIN,
        revents: -1,
    };
    let timeout = [0i64; 2];
    assert_eq!(
        syscall(
            SYS_PPOLL,
            [
                &mut events as *mut PollFd as usize,
                1,
                timeout.as_ptr() as usize
            ]
        ),
        0,
        "empty TCP listener is not readable"
    );
    assert_eq!(events.revents, 0);

    let pid = fork();
    assert!(pid >= 0, "fork returned {}", pid);
    let mut bytes = [0; MESSAGE.len()];
    if pid == 0 {
        assert_eq!(close(listener), 0);
        let client = socket(SOCK_STREAM);
        assert_eq!(
            syscall(SYS_CONNECT, [client, addr.as_ptr(), SOCKADDR_LEN]),
            0,
            "TCP connect"
        );
        write_all(client, MESSAGE);
        assert_eq!(syscall(SYS_SHUTDOWN, [client, 1, 0]), 0);
        read_exact(client, &mut bytes);
        assert_eq!(&bytes, MESSAGE);
        assert_eq!(close(client), 0);
        exit(0);
    }

    let mut peer = SockAddrIn::default();
    let mut len = SOCKADDR_LEN as u32;
    let stream = expect_fd(
        syscall6(
            SYS_ACCEPT4,
            [
                listener,
                peer.as_mut_ptr(),
                &mut len as *mut u32 as usize,
                SOCK_CLOEXEC,
                0,
                0,
            ],
        ),
        "accept4",
    );
    assert_eq!(len, SOCKADDR_LEN as u32);
    assert_eq!(peer.family, AF_INET as u16);
    assert_eq!(peer.addr, LOOPBACK);
    assert_ne!(peer.port, [0; 2]);
    assert_eq!(
        syscall(SYS_FCNTL, [stream, F_GETFD, 0]),
        FD_CLOEXEC,
        "accept4 close-on-exec"
    );
    read_exact(stream, &mut bytes);
    assert_eq!(&bytes, MESSAGE);
    assert_eq!(
        read(stream, &mut bytes),
        0,
        "peer write shutdown yields EOF"
    );
    write_all(stream, &bytes);

    let mut status = -1;
    assert_eq!(waitpid(pid as usize, &mut status), pid);
    assert_eq!(status, 0);
    assert_eq!(close(stream), 0);
    assert_eq!(close(listener), 0);
}

fn test_udp_loopback() {
    const MESSAGE: &[u8] = b"pr19-udp-abi";
    let receiver = socket(SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC);
    let addr = bind_loopback(receiver);
    let mut bytes = [0; 64];
    assert_eq!(
        syscall(SYS_FCNTL, [receiver, F_GETFD, 0]),
        FD_CLOEXEC,
        "socket close-on-exec"
    );
    let flags = syscall(SYS_FCNTL, [receiver, F_GETFL, 0]);
    assert!(
        flags >= 0 && flags as usize & SOCK_NONBLOCK != 0,
        "F_GETFL returned {}",
        flags
    );
    assert_eq!(read(receiver, &mut bytes), EAGAIN, "empty UDP socket read");
    assert_eq!(
        syscall(SYS_FCNTL, [receiver, F_SETFL, 0]),
        0,
        "clear O_NONBLOCK"
    );
    let flags = syscall(SYS_FCNTL, [receiver, F_GETFL, 0]);
    assert!(
        flags >= 0 && flags as usize & SOCK_NONBLOCK == 0,
        "F_GETFL after F_SETFL returned {}",
        flags
    );

    let sender = socket(SOCK_DGRAM);
    let source = bind_loopback(sender);
    assert_eq!(
        syscall6(
            SYS_SENDTO,
            [
                sender,
                MESSAGE.as_ptr() as usize,
                MESSAGE.len(),
                0,
                addr.as_ptr(),
                SOCKADDR_LEN
            ]
        ),
        MESSAGE.len() as isize,
        "UDP sendto"
    );
    let mut peer = SockAddrIn::default();
    let mut len = SOCKADDR_LEN as u32;
    assert_eq!(
        syscall6(
            SYS_RECVFROM,
            [
                receiver,
                bytes.as_mut_ptr() as usize,
                bytes.len(),
                0,
                peer.as_mut_ptr(),
                &mut len as *mut u32 as usize
            ]
        ),
        MESSAGE.len() as isize,
        "UDP recvfrom"
    );
    assert_eq!(&bytes[..MESSAGE.len()], MESSAGE);
    assert_eq!(len, SOCKADDR_LEN as u32);
    assert_eq!(peer, source);
    assert_eq!(close(sender), 0);
    assert_eq!(close(receiver), 0);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EpollEvent {
    events: u32,
    padding: u32,
    data: u64,
}

fn send_datagram(fd: usize, addr: &SockAddrIn, data: &[u8]) {
    assert_eq!(
        syscall6(
            SYS_SENDTO,
            [
                fd,
                data.as_ptr() as usize,
                data.len(),
                0,
                addr.as_ptr(),
                SOCKADDR_LEN
            ]
        ),
        data.len() as isize
    );
}

fn test_udp_shared_port_and_epoll() {
    let peer = socket(SOCK_DGRAM);
    let peer_addr = bind_loopback(peer);
    let other = socket(SOCK_DGRAM);
    let wildcard_addr = SockAddrIn::new([0; 4], 42020);
    let public_addr = SockAddrIn::new(LOOPBACK, 42020);
    let connected = socket(SOCK_DGRAM | SOCK_NONBLOCK);
    let unconnected = socket(SOCK_DGRAM | SOCK_NONBLOCK);
    let reuse = 1i32;
    for fd in [connected, unconnected] {
        assert_eq!(
            syscall6(
                SYS_SETSOCKOPT,
                [fd, 1, 2, &reuse as *const i32 as usize, 4, 0]
            ),
            0
        );
        assert_eq!(bind(fd, &wildcard_addr), 0);
    }
    assert_eq!(
        syscall(SYS_CONNECT, [connected, peer_addr.as_ptr(), SOCKADDR_LEN]),
        0
    );
    let epoll = expect_fd(
        syscall(SYS_EPOLL_CREATE1, [SOCK_CLOEXEC, 0, 0]),
        "epoll_create1",
    );
    assert_eq!(syscall(SYS_FCNTL, [epoll, F_GETFD, 0]), FD_CLOEXEC);
    let mut event = EpollEvent {
        events: 0,
        padding: 0,
        data: 0,
    };
    let mut wait = || {
        syscall6(
            SYS_EPOLL_PWAIT,
            [epoll, &mut event as *mut EpollEvent as usize, 1, 0, 0, 0],
        )
    };
    assert_eq!(wait(), 0);
    let interest = EpollEvent {
        events: 0x40 | (1 << 30), // EPOLLRDNORM | EPOLLONESHOT
        padding: 0,
        data: 0x1933_4488_5566_7788,
    };
    let pointer = &interest as *const EpollEvent as usize;
    assert_eq!(
        syscall6(SYS_EPOLL_CTL, [epoll, 1, connected, pointer, 0, 0]),
        0
    );
    send_datagram(other, &public_addr, b"unrelated");
    assert_eq!(wait(), 0, "unrelated peer does not wake connected UDP");
    let mut bytes = [0; 32];
    assert_eq!(read(connected, &mut bytes), EAGAIN);
    assert_eq!(read(unconnected, &mut bytes), 9);
    assert_eq!(&bytes[..9], b"unrelated");
    send_datagram(peer, &public_addr, b"for peer");
    assert_eq!(wait(), 1);
    assert_eq!(wait(), 0, "one-shot watch is disabled until MOD");
    assert_eq!(
        syscall6(SYS_EPOLL_CTL, [epoll, 3, connected, pointer, 0, 0]),
        0
    );
    assert_eq!(wait(), 1, "one-shot watch is rearmed by MOD");
    assert_eq!(event.events, 0x40);
    assert_eq!(event.data, interest.data);
    assert_eq!(read(connected, &mut bytes), 8);
    assert_eq!(&bytes[..8], b"for peer");
    send_datagram(connected, &peer_addr, b"reply");
    let mut source = SockAddrIn::default();
    let mut len = SOCKADDR_LEN as u32;
    assert_eq!(
        syscall6(
            SYS_RECVFROM,
            [
                peer,
                bytes.as_mut_ptr() as usize,
                bytes.len(),
                0,
                source.as_mut_ptr(),
                &mut len as *mut u32 as usize
            ]
        ),
        5
    );
    assert_eq!(source, public_addr, "wildcard UDP uses the loopback source");
    assert_eq!(
        close(connected),
        0,
        "the first socket may close while the shared bind stays live"
    );
    send_datagram(peer, &public_addr, b"still bound");
    assert_eq!(read(unconnected, &mut bytes), 11);
    for fd in [peer, other, unconnected, epoll] {
        assert_eq!(close(fd), 0);
    }
}

fn test_socket_errors() {
    assert_eq!(syscall(SYS_SOCKET, [1234, SOCK_STREAM, 0]), EAFNOSUPPORT);
    assert_eq!(listen(usize::MAX), EBADF);
    assert_eq!(listen(0), ENOTSOCK);

    let first = socket(SOCK_STREAM | SOCK_NONBLOCK);
    let addr = bind_loopback(first);
    assert_eq!(listen(first), 0);
    assert_eq!(
        syscall(SYS_ACCEPT, [first, 0, 0]),
        EAGAIN,
        "empty nonblocking accept"
    );
    let second = socket(SOCK_STREAM);
    assert_eq!(
        syscall(SYS_BIND, [second, addr.as_ptr(), SOCKADDR_LEN - 1]),
        EINVAL
    );
    assert_eq!(bind(second, &addr), EADDRINUSE);
    assert_eq!(close(second), 0);
    assert_eq!(close(first), 0);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddrUn {
    family: u16,
    path: [u8; 108],
}

impl SockAddrUn {
    fn empty() -> Self {
        Self {
            family: 0,
            path: [0; 108],
        }
    }

    fn from_path(path: &[u8]) -> (Self, usize) {
        let mut addr = Self {
            family: AF_UNIX as u16,
            path: [0; 108],
        };
        addr.path[..path.len()].copy_from_slice(path);
        (addr, 2 + path.len() + 1)
    }

    fn from_abstract(name: &[u8]) -> (Self, usize) {
        let mut addr = Self {
            family: AF_UNIX as u16,
            path: [0; 108],
        };
        addr.path[0] = 0;
        addr.path[1..1 + name.len()].copy_from_slice(name);
        (addr, 2 + 1 + name.len())
    }

    fn as_ptr(&self) -> usize {
        self as *const Self as usize
    }

    fn as_mut_ptr(&mut self) -> usize {
        self as *mut Self as usize
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct UCred {
    pid: i32,
    uid: u32,
    gid: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KernelSigAction {
    handler: usize,
    flags: usize,
    mask: u64,
}

#[repr(C)]
struct PselectSigmask {
    ss: *const u64,
    ss_len: usize,
}

static SIGUSR1_COUNT: AtomicUsize = AtomicUsize::new(0);

extern "C" fn sigusr1_handler(_sig: i32) {
    SIGUSR1_COUNT.fetch_add(1, Ordering::SeqCst);
}

fn test_unix_socketpair() {
    let mut sv = [-1i32; 2];
    assert_eq!(
        syscall6(
            SYS_SOCKETPAIR,
            [
                AF_UNIX,
                SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC,
                0,
                sv.as_mut_ptr() as usize,
                0,
                0,
            ],
        ),
        0,
        "socketpair(AF_UNIX, SOCK_STREAM)"
    );
    let a = sv[0] as usize;
    let b = sv[1] as usize;
    assert_eq!(syscall(SYS_FCNTL, [a, F_GETFD, 0]), FD_CLOEXEC);
    assert_eq!(syscall(SYS_FCNTL, [b, F_GETFD, 0]), FD_CLOEXEC);
    assert_ne!(
        syscall(SYS_FCNTL, [a, F_GETFL, 0]) as usize & SOCK_NONBLOCK,
        0
    );

    let mut buf = [0u8; 32];
    assert_eq!(read(a, &mut buf), EAGAIN, "empty nonblock unix stream");

    let mut cred = UCred::default();
    let mut cred_len = size_of::<UCred>() as u32;
    assert_eq!(
        syscall6(
            SYS_GETSOCKOPT,
            [
                a,
                SOL_SOCKET,
                SO_PEERCRED,
                &mut cred as *mut UCred as usize,
                &mut cred_len as *mut u32 as usize,
                0,
            ],
        ),
        0,
        "SO_PEERCRED on socketpair"
    );
    assert_eq!(cred_len, size_of::<UCred>() as u32);
    assert_eq!(cred.pid as isize, syscall(SYS_GETPID, [0, 0, 0]));
    assert_eq!(cred.uid, 0);
    assert_eq!(cred.gid, 0);

    write_all(a, b"hello-unix");
    read_exact(b, &mut buf[..10]);
    assert_eq!(&buf[..10], b"hello-unix");

    write_all(b, b"reply");
    read_exact(a, &mut buf[..5]);
    assert_eq!(&buf[..5], b"reply");

    assert_eq!(syscall(SYS_SHUTDOWN, [a, 1, 0]), 0, "unix shutdown(SHUT_WR)");
    let mut pfd = PollFd {
        fd: b as i32,
        events: POLLIN | POLLRDHUP,
        revents: 0,
    };
    let timeout = [0i64; 2];
    assert_eq!(
        syscall(
            SYS_PPOLL,
            [
                &mut pfd as *mut PollFd as usize,
                1,
                timeout.as_ptr() as usize,
            ],
        ),
        1
    );
    assert_ne!(pfd.revents & POLLRDHUP, 0, "peer write shutdown sets POLLRDHUP");
    assert_eq!(read(b, &mut buf), 0, "peer write shutdown yields EOF");

    assert_eq!(close(b), 0);
    assert_eq!(
        syscall6(
            SYS_SENDTO,
            [a, b"x".as_ptr() as usize, 1, MSG_NOSIGNAL, 0, 0],
        ),
        EPIPE,
        "send on closed unix peer yields EPIPE"
    );
    assert_eq!(close(a), 0);
}

fn test_unix_dgram_and_seqpacket() {
    let mut sv = [-1i32; 2];
    assert_eq!(
        syscall6(
            SYS_SOCKETPAIR,
            [
                AF_UNIX,
                SOCK_DGRAM | SOCK_NONBLOCK,
                0,
                sv.as_mut_ptr() as usize,
                0,
                0,
            ],
        ),
        0,
        "socketpair(AF_UNIX, SOCK_DGRAM)"
    );
    let a = sv[0] as usize;
    let b = sv[1] as usize;
    assert_eq!(write(a, b"first"), 5);
    assert_eq!(write(a, b"second"), 6);

    let mut buf = [0u8; 16];
    assert_eq!(
        syscall6(
            SYS_RECVFROM,
            [b, buf.as_mut_ptr() as usize, buf.len(), MSG_PEEK, 0, 0],
        ),
        5,
        "unix dgram MSG_PEEK"
    );
    assert_eq!(&buf[..5], b"first");

    let mut small = [0u8; 3];
    assert_eq!(
        syscall6(
            SYS_RECVFROM,
            [b, small.as_mut_ptr() as usize, small.len(), MSG_TRUNC, 0, 0],
        ),
        5,
        "unix dgram MSG_TRUNC returns full length"
    );
    assert_eq!(&small, b"fir");

    assert_eq!(read(b, &mut buf), 6, "second datagram intact");
    assert_eq!(&buf[..6], b"second");
    assert_eq!(read(b, &mut buf), EAGAIN);
    assert_eq!(close(a), 0);
    assert_eq!(close(b), 0);

    assert_eq!(
        syscall6(
            SYS_SOCKETPAIR,
            [
                AF_UNIX,
                SOCK_SEQPACKET | SOCK_NONBLOCK,
                0,
                sv.as_mut_ptr() as usize,
                0,
                0,
            ],
        ),
        0,
        "socketpair(AF_UNIX, SOCK_SEQPACKET)"
    );
    let a = sv[0] as usize;
    let b = sv[1] as usize;
    assert_eq!(write(a, b"alpha"), 5);
    assert_eq!(write(a, b"beta"), 4);
    assert_eq!(read(b, &mut buf), 5);
    assert_eq!(&buf[..5], b"alpha");
    assert_eq!(read(b, &mut buf), 4);
    assert_eq!(&buf[..4], b"beta");
    assert_eq!(close(a), 0);
    assert_eq!(read(b, &mut buf), 0, "seqpacket yields EOF on peer close");
    assert_eq!(close(b), 0);
}

fn test_unix_path_and_abstract() {
    let path = b"/pr24_unix.sock";
    let (addr, addr_len) = SockAddrUn::from_path(path);
    let listener = expect_fd(
        syscall(SYS_SOCKET, [AF_UNIX, SOCK_STREAM, 0]),
        "socket(AF_UNIX)",
    );
    assert_eq!(
        syscall(SYS_BIND, [listener, addr.as_ptr(), addr_len]),
        0,
        "unix bind path"
    );
    let dup = expect_fd(
        syscall(SYS_SOCKET, [AF_UNIX, SOCK_STREAM, 0]),
        "socket(AF_UNIX) dup",
    );
    assert_eq!(
        syscall(SYS_BIND, [dup, addr.as_ptr(), addr_len]),
        EADDRINUSE,
        "duplicate unix path bind"
    );
    assert_eq!(close(dup), 0);
    assert_eq!(listen(listener), 0, "unix listen");

    let mut bound = SockAddrUn::empty();
    let mut bound_len = size_of::<SockAddrUn>() as u32;
    assert_eq!(
        syscall(
            SYS_GETSOCKNAME,
            [
                listener,
                bound.as_mut_ptr(),
                &mut bound_len as *mut u32 as usize,
            ],
        ),
        0
    );
    assert_eq!(bound.family, AF_UNIX as u16);
    assert_eq!(&bound.path[..path.len()], path);

    let pid = fork();
    assert!(pid >= 0);
    if pid == 0 {
        assert_eq!(close(listener), 0);
        let client = expect_fd(
            syscall(SYS_SOCKET, [AF_UNIX, SOCK_STREAM, 0]),
            "unix client",
        );
        assert_eq!(
            syscall(SYS_CONNECT, [client, addr.as_ptr(), addr_len]),
            0,
            "unix connect"
        );
        write_all(client, b"ping-path");
        let mut reply = [0u8; 9];
        read_exact(client, &mut reply);
        assert_eq!(&reply, b"ping-path");
        assert_eq!(close(client), 0);
        exit(0);
    }

    let accepted = expect_fd(
        syscall6(SYS_ACCEPT4, [listener, 0, 0, SOCK_CLOEXEC, 0, 0]),
        "unix accept4",
    );
    assert_eq!(syscall(SYS_FCNTL, [accepted, F_GETFD, 0]), FD_CLOEXEC);
    let mut cred = UCred::default();
    let mut cred_len = size_of::<UCred>() as u32;
    assert_eq!(
        syscall6(
            SYS_GETSOCKOPT,
            [
                accepted,
                SOL_SOCKET,
                SO_PEERCRED,
                &mut cred as *mut UCred as usize,
                &mut cred_len as *mut u32 as usize,
                0,
            ],
        ),
        0
    );
    assert_eq!(cred.pid as isize, pid);

    let mut msg = [0u8; 9];
    read_exact(accepted, &mut msg);
    assert_eq!(&msg, b"ping-path");
    write_all(accepted, &msg);

    let mut status = -1;
    assert_eq!(waitpid(pid as usize, &mut status), pid);
    assert_eq!(status, 0);
    assert_eq!(close(accepted), 0);
    assert_eq!(close(listener), 0);

    // Path still exists on filesystem, but listener is gone -> ECONNREFUSED.
    let probe = expect_fd(
        syscall(SYS_SOCKET, [AF_UNIX, SOCK_STREAM, 0]),
        "unix probe",
    );
    assert_eq!(
        syscall(SYS_CONNECT, [probe, addr.as_ptr(), addr_len]),
        ECONNREFUSED,
        "connect after listener closed"
    );
    assert_eq!(
        syscall(SYS_UNLINKAT, [AT_FDCWD, b"/pr24_unix.sock\0".as_ptr() as usize, 0]),
        0,
        "unlink unix path"
    );
    assert_eq!(
        syscall(SYS_CONNECT, [probe, addr.as_ptr(), addr_len]),
        ENOENT,
        "connect after unlink"
    );
    assert_eq!(
        syscall(SYS_BIND, [probe, addr.as_ptr(), addr_len]),
        0,
        "rebind after unlink"
    );
    assert_eq!(close(probe), 0);
    assert_eq!(
        syscall(SYS_UNLINKAT, [AT_FDCWD, b"/pr24_unix.sock\0".as_ptr() as usize, 0]),
        0
    );

    // Abstract namespace socket: auto-released on close.
    let (abs_addr, abs_len) = SockAddrUn::from_abstract(b"pr24-abs");
    let abs_srv = expect_fd(
        syscall(SYS_SOCKET, [AF_UNIX, SOCK_STREAM, 0]),
        "unix abstract server",
    );
    assert_eq!(
        syscall(SYS_BIND, [abs_srv, abs_addr.as_ptr(), abs_len]),
        0,
        "bind abstract"
    );
    let abs_dup = expect_fd(
        syscall(SYS_SOCKET, [AF_UNIX, SOCK_STREAM, 0]),
        "unix abstract dup",
    );
    assert_eq!(
        syscall(SYS_BIND, [abs_dup, abs_addr.as_ptr(), abs_len]),
        EADDRINUSE
    );
    assert_eq!(close(abs_dup), 0);
    assert_eq!(listen(abs_srv), 0);

    let abs_cli = expect_fd(
        syscall(SYS_SOCKET, [AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0]),
        "unix abstract client",
    );
    assert_eq!(
        syscall(SYS_CONNECT, [abs_cli, abs_addr.as_ptr(), abs_len]),
        0,
        "connect abstract"
    );
    let abs_acc = expect_fd(syscall(SYS_ACCEPT, [abs_srv, 0, 0]), "accept abstract");
    write_all(abs_cli, b"abs-ok");
    let mut abs_buf = [0u8; 6];
    read_exact(abs_acc, &mut abs_buf);
    assert_eq!(&abs_buf, b"abs-ok");
    assert_eq!(close(abs_cli), 0);
    assert_eq!(close(abs_acc), 0);
    assert_eq!(close(abs_srv), 0);

    // Abstract address must be free immediately after listener close.
    let abs_rebind = expect_fd(
        syscall(SYS_SOCKET, [AF_UNIX, SOCK_STREAM, 0]),
        "unix abstract rebind",
    );
    assert_eq!(
        syscall(SYS_BIND, [abs_rebind, abs_addr.as_ptr(), abs_len]),
        0,
        "rebind abstract after close"
    );
    assert_eq!(close(abs_rebind), 0);
}

fn test_tcp_info() {
    let listener = socket(SOCK_STREAM);
    let addr = bind_loopback(listener);
    assert_eq!(listen(listener), 0);

    let mut info = [0u8; 256];
    let mut len = info.len() as u32;
    assert_eq!(
        syscall6(
            SYS_GETSOCKOPT,
            [
                listener,
                IPPROTO_TCP,
                TCP_INFO,
                info.as_mut_ptr() as usize,
                &mut len as *mut u32 as usize,
                0,
            ],
        ),
        0,
        "TCP_INFO on listener"
    );
    assert_eq!(len, 232, "TCP_INFO struct size");
    assert_eq!(info[0], 10, "TCP_LISTEN state");

    let pid = fork();
    assert!(pid >= 0);
    if pid == 0 {
        assert_eq!(close(listener), 0);
        let client = socket(SOCK_STREAM);
        assert_eq!(
            syscall(SYS_CONNECT, [client, addr.as_ptr(), SOCKADDR_LEN]),
            0
        );
        let mut cinfo = [0u8; 232];
        let mut clen = cinfo.len() as u32;
        assert_eq!(
            syscall6(
                SYS_GETSOCKOPT,
                [
                    client,
                    IPPROTO_TCP,
                    TCP_INFO,
                    cinfo.as_mut_ptr() as usize,
                    &mut clen as *mut u32 as usize,
                    0,
                ],
            ),
            0,
            "TCP_INFO on connected client"
        );
        assert_eq!(clen, 232);
        assert_eq!(cinfo[0], 1, "TCP_ESTABLISHED state");

        let mut short_len = 16u32;
        assert_eq!(
            syscall6(
                SYS_GETSOCKOPT,
                [
                    client,
                    IPPROTO_TCP,
                    TCP_INFO,
                    cinfo.as_mut_ptr() as usize,
                    &mut short_len as *mut u32 as usize,
                    0,
                ],
            ),
            0,
            "truncated TCP_INFO"
        );
        assert_eq!(short_len, 16);
        assert_eq!(cinfo[0], 1);
        write_all(client, b"k");
        let mut done = [0u8; 1];
        let _ = read(client, &mut done);
        assert_eq!(close(client), 0);
        exit(0);
    }

    let accepted = expect_fd(syscall(SYS_ACCEPT, [listener, 0, 0]), "accept tcp_info");
    let mut ack = [0u8; 1];
    read_exact(accepted, &mut ack);
    assert_eq!(&ack, b"k");
    assert_eq!(close(accepted), 0);
    let mut status = -1;
    assert_eq!(waitpid(pid as usize, &mut status), pid);
    assert_eq!(status, 0);
    assert_eq!(close(listener), 0);

    // UDP and AF_UNIX must reject TCP_INFO with ENOPROTOOPT.
    let udp = socket(SOCK_DGRAM);
    let mut ulen = 232u32;
    assert_eq!(
        syscall6(
            SYS_GETSOCKOPT,
            [
                udp,
                IPPROTO_TCP,
                TCP_INFO,
                info.as_mut_ptr() as usize,
                &mut ulen as *mut u32 as usize,
                0,
            ],
        ),
        ENOPROTOOPT
    );
    assert_eq!(close(udp), 0);

    let unix = expect_fd(syscall(SYS_SOCKET, [AF_UNIX, SOCK_STREAM, 0]), "unix");
    let mut xlen = 232u32;
    assert_eq!(
        syscall6(
            SYS_GETSOCKOPT,
            [
                unix,
                IPPROTO_TCP,
                TCP_INFO,
                info.as_mut_ptr() as usize,
                &mut xlen as *mut u32 as usize,
                0,
            ],
        ),
        ENOPROTOOPT
    );
    assert_eq!(close(unix), 0);
}

fn test_epoll_et() {
    let mut sv = [-1i32; 2];
    assert_eq!(
        syscall6(
            SYS_SOCKETPAIR,
            [
                AF_UNIX,
                SOCK_STREAM | SOCK_NONBLOCK,
                0,
                sv.as_mut_ptr() as usize,
                0,
                0,
            ],
        ),
        0
    );
    let tx = sv[0] as usize;
    let rx = sv[1] as usize;
    let epfd = expect_fd(syscall(SYS_EPOLL_CREATE1, [0, 0, 0]), "epoll_create1");
    let interest = EpollEvent {
        events: (POLLIN as u32) | EPOLLET,
        padding: 0,
        data: 0x24_24_24_24,
    };
    assert_eq!(
        syscall6(
            SYS_EPOLL_CTL,
            [epfd, 1, rx, &interest as *const EpollEvent as usize, 0, 0],
        ),
        0,
        "epoll_ctl ADD EPOLLET"
    );

    let mut ev = EpollEvent {
        events: 0,
        padding: 0,
        data: 0,
    };
    let poll_once = |out: &mut EpollEvent| {
        syscall6(
            SYS_EPOLL_PWAIT,
            [epfd, out as *mut EpollEvent as usize, 1, 0, 0, 0],
        )
    };

    assert_eq!(poll_once(&mut ev), 0, "EPOLLET idle");
    write_all(tx, b"abcdef");
    assert_eq!(poll_once(&mut ev), 1, "EPOLLET fires on first arrival");
    assert_eq!(ev.data, interest.data);
    assert_ne!(ev.events & (POLLIN as u32), 0);

    // Partial read: data still buffered, must NOT re-fire until new arrival or MOD.
    let mut partial = [0u8; 2];
    assert_eq!(read(rx, &mut partial), 2);
    assert_eq!(&partial, b"ab");
    assert_eq!(poll_once(&mut ev), 0, "EPOLLET does not re-fire after partial read");

    // New write advances inbound generation -> must re-fire once.
    write_all(tx, b"gh");
    assert_eq!(poll_once(&mut ev), 1, "EPOLLET re-fires on new write");
    assert_eq!(poll_once(&mut ev), 0, "EPOLLET quiet again until next edge");

    // EPOLL_CTL_MOD re-arms edge state while data is still buffered.
    assert_eq!(
        syscall6(
            SYS_EPOLL_CTL,
            [epfd, 3, rx, &interest as *const EpollEvent as usize, 0, 0],
        ),
        0
    );
    assert_eq!(poll_once(&mut ev), 1, "EPOLLET re-armed by EPOLL_CTL_MOD");

    let mut rest = [0u8; 6];
    read_exact(rx, &mut rest);
    assert_eq!(&rest, b"cdefgh");
    assert_eq!(poll_once(&mut ev), 0, "EPOLLET quiet after drain");

    assert_eq!(close(epfd), 0);
    assert_eq!(close(tx), 0);
    assert_eq!(close(rx), 0);
}

fn test_pwait_sigmask() {
    SIGUSR1_COUNT.store(0, Ordering::SeqCst);
    let act = KernelSigAction {
        handler: sigusr1_handler as *const () as usize,
        flags: 0,
        mask: 0,
    };
    assert_eq!(
        syscall6(
            SYS_RT_SIGACTION,
            [SIGUSR1, &act as *const KernelSigAction as usize, 0, 8, 0, 0],
        ),
        0,
        "install SIGUSR1 handler"
    );

    let usr1_bit: u64 = 1u64 << (SIGUSR1 - 1);
    let mut saved_mask: u64 = 0;
    assert_eq!(
        syscall6(
            SYS_RT_SIGPROCMASK,
            [
                SIG_BLOCK,
                &usr1_bit as *const u64 as usize,
                &mut saved_mask as *mut u64 as usize,
                8,
                0,
                0,
            ],
        ),
        0,
        "block SIGUSR1"
    );

    let pid = syscall(SYS_GETPID, [0, 0, 0]) as usize;
    let empty_mask: u64 = 0;

    // 1. epoll_pwait with temporary sigmask
    let epfd = expect_fd(syscall(SYS_EPOLL_CREATE1, [0, 0, 0]), "epoll_create1");
    let mut ev = EpollEvent {
        events: 0,
        padding: 0,
        data: 0,
    };
    assert_eq!(
        syscall6(
            SYS_EPOLL_PWAIT,
            [
                epfd,
                &mut ev as *mut EpollEvent as usize,
                1,
                0,
                &empty_mask as *const u64 as usize,
                4, // invalid sigsetsize
            ],
        ),
        EINVAL,
        "epoll_pwait invalid sigsetsize"
    );
    assert_eq!(syscall(SYS_KILL, [pid, SIGUSR1, 0]), 0);
    assert_eq!(SIGUSR1_COUNT.load(Ordering::SeqCst), 0, "SIGUSR1 blocked");
    assert_eq!(
        syscall6(
            SYS_EPOLL_PWAIT,
            [
                epfd,
                &mut ev as *mut EpollEvent as usize,
                1,
                50,
                &empty_mask as *const u64 as usize,
                8,
            ],
        ),
        EINTR,
        "epoll_pwait unblocks SIGUSR1 and returns EINTR"
    );
    assert_eq!(SIGUSR1_COUNT.load(Ordering::SeqCst), 1);
    let mut cur_mask: u64 = 0;
    assert_eq!(
        syscall6(
            SYS_RT_SIGPROCMASK,
            [SIG_BLOCK, 0, &mut cur_mask as *mut u64 as usize, 8, 0, 0],
        ),
        0
    );
    assert_ne!(
        cur_mask & usr1_bit,
        0,
        "original SIGUSR1 mask restored after epoll_pwait"
    );
    assert_eq!(close(epfd), 0);

    // 2. ppoll with temporary sigmask
    let timeout = [0i64, 50_000_000i64];
    assert_eq!(syscall(SYS_KILL, [pid, SIGUSR1, 0]), 0);
    assert_eq!(
        syscall6(
            SYS_PPOLL,
            [
                0,
                0,
                timeout.as_ptr() as usize,
                &empty_mask as *const u64 as usize,
                8,
                0,
            ],
        ),
        EINTR,
        "ppoll unblocks SIGUSR1 and returns EINTR"
    );
    assert_eq!(SIGUSR1_COUNT.load(Ordering::SeqCst), 2);
    cur_mask = 0;
    assert_eq!(
        syscall6(
            SYS_RT_SIGPROCMASK,
            [SIG_BLOCK, 0, &mut cur_mask as *mut u64 as usize, 8, 0, 0],
        ),
        0
    );
    assert_ne!(
        cur_mask & usr1_bit,
        0,
        "original SIGUSR1 mask restored after ppoll"
    );

    // 3. pselect6 with temporary sigmask
    let sigarg = PselectSigmask {
        ss: &empty_mask as *const u64,
        ss_len: 8,
    };
    assert_eq!(syscall(SYS_KILL, [pid, SIGUSR1, 0]), 0);
    assert_eq!(
        syscall6(
            SYS_PSELECT6,
            [
                0,
                0,
                0,
                0,
                timeout.as_ptr() as usize,
                &sigarg as *const PselectSigmask as usize,
            ],
        ),
        EINTR,
        "pselect6 unblocks SIGUSR1 and returns EINTR"
    );
    assert_eq!(SIGUSR1_COUNT.load(Ordering::SeqCst), 3);
    cur_mask = 0;
    assert_eq!(
        syscall6(
            SYS_RT_SIGPROCMASK,
            [SIG_BLOCK, 0, &mut cur_mask as *mut u64 as usize, 8, 0, 0],
        ),
        0
    );
    assert_ne!(
        cur_mask & usr1_bit,
        0,
        "original SIGUSR1 mask restored after pselect6"
    );

    // Restore original mask.
    assert_eq!(
        syscall6(
            SYS_RT_SIGPROCMASK,
            [SIG_SETMASK, &saved_mask as *const u64 as usize, 0, 8, 0, 0],
        ),
        0
    );
}

fn test_virtio_echo() {
    const MESSAGE: &[u8; 16] = b"pr19-virtio-echo";
    let listener = socket(SOCK_STREAM);
    assert_eq!(
        bind(listener, &SockAddrIn::new([0; 4], VIRTIO_PORT)),
        0,
        "bind external listener"
    );
    assert_eq!(listen(listener), 0, "listen on external port");
    println!("[net-abi] VIRTIO READY");
    let stream = expect_fd(syscall(SYS_ACCEPT, [listener, 0, 0]), "external accept");
    let mut bytes = [0; MESSAGE.len()];
    read_exact(stream, &mut bytes);
    assert_eq!(&bytes, MESSAGE);
    write_all(stream, &bytes);
    // The host closes its send half after it verifies the echo.
    assert_eq!(read(stream, &mut bytes), 0, "external TCP EOF");
    assert_eq!(close(stream), 0);
    assert_eq!(close(listener), 0);
    println!("[net-abi] VIRTIO PASS");
}

#[no_mangle]
pub fn main() -> i32 {
    test_tcp_loopback();
    test_udp_loopback();
    test_udp_shared_port_and_epoll();
    test_socket_errors();
    test_unix_socketpair();
    test_unix_dgram_and_seqpacket();
    test_unix_path_and_abstract();
    test_tcp_info();
    test_epoll_et();
    test_pwait_sigmask();
    println!("[net-abi] PASS");

    let marker = open("/net-virtio\0", OpenFlags::RDONLY);
    if marker >= 0 {
        assert_eq!(close(marker as usize), 0);
        test_virtio_echo();
    }
    0
}
