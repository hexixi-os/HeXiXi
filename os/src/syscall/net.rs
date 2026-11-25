use super::fs::{file_by_fd, with_io_file};
use crate::fs::File;
use crate::mm::{
    prepare_user_write, translated_byte_buffer, try_read_user, try_write_user, MapPermission,
    UserBuffer,
};
use crate::net::{
    Kind, SocketFile, UnixAddress, UnixKind, UnixSocket, MSG_DONTWAIT, MSG_PEEK, MSG_TRUNC,
};
use crate::task::{current_process, current_user_token};
use alloc::{sync::Arc, vec::Vec};
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};

const AF_UNIX: u16 = 1;
const AF_INET: u16 = 2;
const SOCK_NONBLOCK: usize = 0x800;
const SOCK_CLOEXEC: usize = 0x80000;
const SOCK_FLAGS: usize = SOCK_NONBLOCK | SOCK_CLOEXEC;
const MSG_NOSIGNAL: usize = 0x4000;
const SOCKADDR_LEN: usize = 16;
const SOCKADDR_UN_MAX: usize = 110;

fn user_buffer(address: usize, len: usize, permission: MapPermission) -> Result<UserBuffer, isize> {
    translated_byte_buffer(current_user_token(), address as *const u8, len, permission)
        .map(UserBuffer::new)
        .ok_or(-14)
}

fn read_address(address: usize, len: usize) -> Result<IpEndpoint, isize> {
    if len < SOCKADDR_LEN {
        return Err(-22);
    }
    let mut bytes = user_buffer(address, SOCKADDR_LEN, MapPermission::R)?
        .into_iter()
        .map(|byte| byte.read());
    let family = u16::from_ne_bytes(core::array::from_fn(|_| bytes.next().unwrap()));
    if family != AF_INET {
        return Err(-97);
    }
    let port = u16::from_be_bytes(core::array::from_fn(|_| bytes.next().unwrap()));
    let ip = Ipv4Address(core::array::from_fn(|_| bytes.next().unwrap()));
    Ok(IpEndpoint::new(ip.into(), port))
}

fn write_address(address: usize, len_ptr: usize, endpoint: IpEndpoint) -> Result<(), isize> {
    let token = current_user_token();
    let capacity = try_read_user(token, len_ptr as *const i32).ok_or(-14isize)?;
    if capacity < 0 {
        return Err(-22);
    }
    let size = (capacity as usize).min(SOCKADDR_LEN);
    let buffer = user_buffer(address, size, MapPermission::W)?;
    let mut bytes = [0; SOCKADDR_LEN];
    bytes[..2].copy_from_slice(&AF_INET.to_ne_bytes());
    bytes[2..4].copy_from_slice(&endpoint.port.to_be_bytes());
    if let IpAddress::Ipv4(ip) = endpoint.addr {
        bytes[4..8].copy_from_slice(&ip.0);
    }
    try_write_user(token, len_ptr as *mut u32, SOCKADDR_LEN as u32).ok_or(-14isize)?;
    buffer.copy_from(&bytes[..size]);
    Ok(())
}

fn read_unix_address(address: usize, len: usize) -> Result<UnixAddress, isize> {
    if len < 2 || len > SOCKADDR_UN_MAX {
        return Err(-22);
    }
    let bytes: Vec<u8> = user_buffer(address, len, MapPermission::R)?
        .into_iter()
        .map(|byte| byte.read())
        .collect();
    let family = u16::from_ne_bytes([bytes[0], bytes[1]]);
    if family != AF_UNIX {
        return Err(-97);
    }
    if len == 2 {
        return Ok(UnixAddress::Unnamed);
    }
    let path = &bytes[2..];
    if path[0] == 0 {
        Ok(UnixAddress::Abstract(path[1..].to_vec()))
    } else {
        let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
        if end == 0 {
            Ok(UnixAddress::Unnamed)
        } else {
            Ok(UnixAddress::Path(path[..end].to_vec()))
        }
    }
}

fn write_unix_address(address: usize, len_ptr: usize, addr: &UnixAddress) -> Result<(), isize> {
    let token = current_user_token();
    let capacity = try_read_user(token, len_ptr as *const i32).ok_or(-14isize)?;
    if capacity < 0 {
        return Err(-22);
    }
    let mut bytes = Vec::with_capacity(SOCKADDR_UN_MAX);
    bytes.extend_from_slice(&AF_UNIX.to_ne_bytes());
    match addr {
        UnixAddress::Unnamed => {}
        UnixAddress::Path(path) => {
            let take = path.len().min(SOCKADDR_UN_MAX - 3);
            bytes.extend_from_slice(&path[..take]);
            bytes.push(0);
        }
        UnixAddress::Abstract(name) => {
            bytes.push(0);
            let take = name.len().min(SOCKADDR_UN_MAX - 3);
            bytes.extend_from_slice(&name[..take]);
        }
    }
    let size = (capacity as usize).min(bytes.len());
    if size > 0 {
        let buffer = user_buffer(address, size, MapPermission::W)?;
        buffer.copy_from(&bytes[..size]);
    }
    try_write_user(token, len_ptr as *mut u32, bytes.len() as u32).ok_or(-14isize)?;
    Ok(())
}

enum AnySocket<'a> {
    Inet(&'a SocketFile),
    Unix(&'a UnixSocket),
}

fn on_any_socket<T>(
    fd: usize,
    action: impl FnOnce(AnySocket<'_>) -> Result<T, isize>,
) -> Result<T, isize> {
    let file = file_by_fd(fd)?;
    if file.socket().is_none() && file.unix_socket().is_none() {
        return Err(-88); // ENOTSOCK
    }
    with_io_file(file, |file| {
        if let Some(inet) = file.socket() {
            action(AnySocket::Inet(inet))
        } else {
            action(AnySocket::Unix(file.unix_socket().unwrap()))
        }
    })
}

pub(super) fn install(
    file: Arc<dyn File + Send + Sync>,
    close_on_exec: bool,
) -> Result<usize, isize> {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let fd = inner.alloc_fd().ok_or(-24isize)?;
    inner.fd_table[fd] = Some(file);
    inner.set_close_on_exec(fd, close_on_exec);
    Ok(fd)
}

fn parse_unix_kind(flags: usize, protocol: usize) -> Result<UnixKind, isize> {
    if protocol != 0 {
        return Err(-93); // EPROTONOSUPPORT
    }
    match flags & !SOCK_FLAGS {
        1 => Ok(UnixKind::Stream),
        2 => Ok(UnixKind::Dgram),
        5 => Ok(UnixKind::SeqPacket),
        _ => Err(-22),
    }
}

pub(super) fn socket([family, flags, protocol, ..]: [usize; 6]) -> isize {
    let result = (|| {
        let nonblock = flags & SOCK_NONBLOCK != 0;
        let cloexec = flags & SOCK_CLOEXEC != 0;
        match family as u16 {
            AF_INET => {
                let kind = match (flags & !SOCK_FLAGS, protocol) {
                    (1, 0 | 6) => Kind::Tcp,
                    (2, 0 | 17) => Kind::Udp,
                    (1 | 2, _) => return Err(-93),
                    _ => return Err(-22),
                };
                install(SocketFile::new(kind, nonblock)?, cloexec)
            }
            AF_UNIX => {
                let kind = parse_unix_kind(flags, protocol)?;
                install(UnixSocket::new(kind, nonblock), cloexec)
            }
            _ => Err(-97), // EAFNOSUPPORT
        }
    })();
    result.map_or_else(|error| error, |fd| fd as isize)
}

pub(super) fn socketpair([family, flags, protocol, sv_ptr, ..]: [usize; 6]) -> isize {
    let result = (|| -> Result<(), isize> {
        if family as u16 != AF_UNIX {
            return Err(-95); // EOPNOTSUPP / EAFNOSUPPORT
        }
        let kind = parse_unix_kind(flags, protocol)?;
        let token = current_user_token();
        if sv_ptr == 0 || sv_ptr.checked_add(8).is_none() {
            return Err(-14);
        }
        let sv0 = prepare_user_write(token, sv_ptr as *mut i32).ok_or(-14isize)?;
        let sv1 = prepare_user_write(token, (sv_ptr + 4) as *mut i32).ok_or(-14isize)?;
        let nonblock = flags & SOCK_NONBLOCK != 0;
        let cloexec = flags & SOCK_CLOEXEC != 0;
        let (a, b) = UnixSocket::pair(kind, nonblock);

        let process = current_process();
        let mut inner = process.inner_exclusive_access();
        let fd0 = inner.alloc_fd().ok_or(-24isize)?;
        inner.fd_table[fd0] = Some(a);
        let Some(fd1) = inner.alloc_fd() else {
            inner.fd_table[fd0] = None;
            return Err(-24);
        };
        inner.fd_table[fd1] = Some(b);
        inner.set_close_on_exec(fd0, cloexec);
        inner.set_close_on_exec(fd1, cloexec);
        drop(inner);

        sv0.write(fd0 as i32);
        sv1.write(fd1 as i32);
        Ok(())
    })();
    result.err().unwrap_or(0)
}

pub(super) fn bind([fd, address, len, ..]: [usize; 6]) -> isize {
    on_any_socket(fd, |socket| match socket {
        AnySocket::Inet(s) => s.bind(read_address(address, len)?),
        AnySocket::Unix(s) => s.bind(read_unix_address(address, len)?),
    })
    .err()
    .unwrap_or(0)
}

pub(super) fn listen([fd, backlog, ..]: [usize; 6]) -> isize {
    on_any_socket(fd, |socket| match socket {
        AnySocket::Inet(s) => s.listen(backlog as i32),
        AnySocket::Unix(s) => s.listen(backlog as i32),
    })
    .err()
    .unwrap_or(0)
}

pub(super) fn accept([fd, address, len, flags, ..]: [usize; 6]) -> isize {
    if flags & !SOCK_FLAGS != 0 {
        return -22;
    }
    let has_slot = || {
        let process = current_process();
        let inner = process.inner_exclusive_access();
        inner.fd_table.iter().any(Option::is_none) || inner.fd_table.len() < inner.nofile_limit
    };
    let result = on_any_socket(fd, |socket| match socket {
        AnySocket::Inet(s) => {
            let (accepted, peer) = s.accept(flags & SOCK_NONBLOCK != 0, has_slot)?;
            if address != 0 {
                write_address(address, len, peer)?;
            }
            install(accepted, flags & SOCK_CLOEXEC != 0)
        }
        AnySocket::Unix(s) => {
            let (accepted, peer) = s.accept(flags & SOCK_NONBLOCK != 0, has_slot)?;
            if address != 0 {
                write_unix_address(address, len, &peer)?;
            }
            install(accepted, flags & SOCK_CLOEXEC != 0)
        }
    });
    result.map_or_else(|error| error, |fd| fd as isize)
}

pub(super) fn connect([fd, address, len, ..]: [usize; 6]) -> isize {
    on_any_socket(fd, |socket| match socket {
        AnySocket::Inet(s) => s.connect(read_address(address, len)?),
        AnySocket::Unix(s) => s.connect(read_unix_address(address, len)?),
    })
    .err()
    .unwrap_or(0)
}

pub(super) fn get_name([fd, address, len, ..]: [usize; 6], peer: bool) -> isize {
    on_any_socket(fd, |socket| {
        if address == 0 || len == 0 {
            return Err(-14);
        }
        match socket {
            AnySocket::Inet(s) => {
                let endpoint = if peer { s.peer()? } else { s.local() };
                write_address(address, len, endpoint)
            }
            AnySocket::Unix(s) => {
                let addr = if peer { s.peer_addr()? } else { s.local_addr() };
                write_unix_address(address, len, &addr)
            }
        }
    })
    .err()
    .unwrap_or(0)
}

pub(super) fn send_to([fd, address, len, flags, peer_ptr, peer_len]: [usize; 6]) -> isize {
    if flags & !(MSG_DONTWAIT | MSG_NOSIGNAL) != 0 {
        return -95;
    }
    on_any_socket(fd, |socket| {
        let buf = user_buffer(address, len, MapPermission::R)?;
        match socket {
            AnySocket::Inet(s) => {
                let peer = if peer_ptr == 0 {
                    None
                } else {
                    Some(read_address(peer_ptr, peer_len)?)
                };
                s.send(&buf, flags, peer)
            }
            AnySocket::Unix(s) => {
                let peer = if peer_ptr == 0 {
                    None
                } else {
                    Some(read_unix_address(peer_ptr, peer_len)?)
                };
                s.send(&buf, flags, peer)
            }
        }
    })
    .map_or_else(
        |error| {
            if error == -32 && flags & MSG_NOSIGNAL == 0 {
                super::signal::queue_current(13);
            }
            error
        },
        |count| count as isize,
    )
}

pub(super) fn receive_from([fd, address, len, flags, peer_ptr, peer_len]: [usize; 6]) -> isize {
    if flags & !(MSG_DONTWAIT | MSG_PEEK | MSG_TRUNC) != 0 {
        return -95;
    }
    on_any_socket(fd, |socket| {
        let buf = user_buffer(address, len, MapPermission::W)?;
        match socket {
            AnySocket::Inet(s) => {
                let (count, peer) = s.receive(&buf, flags)?;
                if peer_ptr != 0 {
                    if let Some(peer) = peer {
                        write_address(peer_ptr, peer_len, peer)?;
                    }
                }
                Ok(count)
            }
            AnySocket::Unix(s) => {
                let (count, peer) = s.receive(&buf, flags)?;
                if peer_ptr != 0 {
                    if let Some(peer) = peer {
                        write_unix_address(peer_ptr, peer_len, &peer)?;
                    } else if peer_len != 0 {
                        let token = current_user_token();
                        let cap = try_read_user(token, peer_len as *const i32).ok_or(-14isize)?;
                        if cap < 0 {
                            return Err(-22);
                        }
                        try_write_user(token, peer_len as *mut u32, 0u32).ok_or(-14isize)?;
                    }
                }
                Ok(count)
            }
        }
    })
    .map_or_else(|error| error, |count| count as isize)
}

pub(super) fn shutdown([fd, how, ..]: [usize; 6]) -> isize {
    on_any_socket(fd, |socket| match socket {
        AnySocket::Inet(s) => s.shutdown(how),
        AnySocket::Unix(s) => s.shutdown(how),
    })
    .err()
    .unwrap_or(0)
}

fn timeout_option(level: usize, option: usize) -> Option<bool> {
    match (level, option) {
        (1, 20 | 66) => Some(true),  // SO_RCVTIMEO
        (1, 21 | 67) => Some(false), // SO_SNDTIMEO
        _ => None,
    }
}

pub(super) fn set_option([fd, level, option, ptr, len, ..]: [usize; 6]) -> isize {
    on_any_socket(fd, |socket| {
        let token = current_user_token();
        if let Some(receive) = timeout_option(level, option) {
            if len < 16 {
                return Err(-22);
            }
            let sec = try_read_user(token, ptr as *const i64).ok_or(-14isize)?;
            let usec_ptr = ptr.checked_add(8).ok_or(-14isize)?;
            let usec = try_read_user(token, usec_ptr as *const i64).ok_or(-14isize)?;
            if sec < 0 || !(0..1_000_000).contains(&usec) {
                return Err(-22);
            }
            let duration = (sec as usize)
                .saturating_mul(1_000_000_000)
                .saturating_add(usec as usize * 1000);
            let value = (duration != 0).then_some(duration);
            match socket {
                AnySocket::Inet(s) => s.set_io_timeout(receive, value),
                AnySocket::Unix(s) => s.set_io_timeout(receive, value),
            }
            return Ok(());
        }

        if len < 4 {
            return Err(-22);
        }
        let value = try_read_user(token, ptr as *const i32).ok_or(-14isize)?;
        match socket {
            AnySocket::Inet(s) => s.set_option(level, option, value),
            AnySocket::Unix(s) => s.set_option(level, option, value),
        }
    })
    .err()
    .unwrap_or(0)
}

pub(super) fn get_option([fd, level, option, ptr, len, ..]: [usize; 6]) -> isize {
    on_any_socket(fd, |socket| {
        let token = current_user_token();
        let capacity = try_read_user(token, len as *const i32).ok_or(-14isize)?;
        if capacity < 0 {
            return Err(-22);
        }
        let length = prepare_user_write(token, len as *mut u32).ok_or(-14isize)?;
        if (level, option) == (6, 11) {
            // IPPROTO_TCP, TCP_INFO
            let AnySocket::Inet(s) = socket else {
                return Err(-92);
            };
            let info = s.tcp_info()?;
            let size = (capacity as usize).min(info.len());
            let buffer = user_buffer(ptr, size, MapPermission::W)?;
            buffer.copy_from(&info[..size]);
            length.write(size as u32);
            return Ok(());
        }
        if (level, option) == (1, 17) {
            // SOL_SOCKET, SO_PEERCRED
            let AnySocket::Unix(s) = socket else {
                return Err(-92);
            };
            let cred = s.peer_cred();
            let mut raw = [0u8; 12];
            raw[0..4].copy_from_slice(&cred.pid.to_ne_bytes());
            raw[4..8].copy_from_slice(&cred.uid.to_ne_bytes());
            raw[8..12].copy_from_slice(&cred.gid.to_ne_bytes());
            let size = (capacity as usize).min(raw.len());
            let buffer = user_buffer(ptr, size, MapPermission::W)?;
            buffer.copy_from(&raw[..size]);
            length.write(size as u32);
            return Ok(());
        }
        let timeout = timeout_option(level, option);
        let size = (capacity as usize).min(if timeout.is_some() { 16 } else { 4 });
        let buffer = user_buffer(ptr, size, MapPermission::W)?;
        let mut value = [0; 16];
        if let Some(receive) = timeout {
            let duration = match socket {
                AnySocket::Inet(s) => s.io_timeout(receive),
                AnySocket::Unix(s) => s.io_timeout(receive),
            };
            value[..8].copy_from_slice(&((duration / 1_000_000_000) as i64).to_ne_bytes());
            value[8..].copy_from_slice(&((duration / 1000 % 1_000_000) as i64).to_ne_bytes());
        } else {
            let opt = match socket {
                AnySocket::Inet(s) => s.get_option(level, option)?,
                AnySocket::Unix(s) => s.get_option(level, option)?,
            };
            value[..4].copy_from_slice(&opt.to_ne_bytes());
        }
        buffer.copy_from(&value[..size]);
        length.write(size as u32);
        Ok(())
    })
    .err()
    .unwrap_or(0)
}
