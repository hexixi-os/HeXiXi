#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::arch::asm;
use core::mem::size_of;
use user_lib::{close, exit, fork, open, read, waitpid, write, OpenFlags};

const SYS_EPOLL_CREATE1: usize = 20;
const SYS_EPOLL_CTL: usize = 21;
const SYS_EPOLL_PWAIT: usize = 22;
const SYS_FCNTL: usize = 25;
const SYS_PPOLL: usize = 73;
const SYS_SOCKET: usize = 198;
const SYS_BIND: usize = 200;
const SYS_LISTEN: usize = 201;
const SYS_ACCEPT: usize = 202;
const SYS_CONNECT: usize = 203;
const SYS_GETSOCKNAME: usize = 204;
const SYS_SENDTO: usize = 206;
const SYS_RECVFROM: usize = 207;
const SYS_SETSOCKOPT: usize = 208;
const SYS_SHUTDOWN: usize = 210;
const SYS_ACCEPT4: usize = 242;

const AF_INET: usize = 2;
const SOCK_STREAM: usize = 1;
const SOCK_DGRAM: usize = 2;
const SOCK_NONBLOCK: usize = 0o4000;
const SOCK_CLOEXEC: usize = 0o2000000;
const F_GETFD: usize = 1;
const F_GETFL: usize = 3;
const F_SETFL: usize = 4;
const FD_CLOEXEC: isize = 1;
const POLLIN: i16 = 1;
const LOOPBACK: [u8; 4] = [127, 0, 0, 1];
const VIRTIO_PORT: u16 = 42019;

const EBADF: isize = -9;
const EAGAIN: isize = -11;
const EINVAL: isize = -22;
const ENOTSOCK: isize = -88;
const EAFNOSUPPORT: isize = -97;
const EADDRINUSE: isize = -98;

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
    println!("[net-abi] PASS");

    let marker = open("/net-virtio\0", OpenFlags::RDONLY);
    if marker >= 0 {
        assert_eq!(close(marker as usize), 0);
        test_virtio_echo();
    }
    0
}
