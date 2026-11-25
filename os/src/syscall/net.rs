use super::fs::{file_by_fd, with_io_file};
use crate::fs::File;
use crate::mm::{
    prepare_user_write, translated_byte_buffer, try_read_user, try_write_user, MapPermission,
    UserBuffer,
};
use crate::net::{Kind, SocketFile, MSG_DONTWAIT, MSG_PEEK, MSG_TRUNC};
use crate::task::{current_process, current_user_token};
use alloc::sync::Arc;
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};

const AF_INET: u16 = 2;
const SOCK_NONBLOCK: usize = 0x800;
const SOCK_CLOEXEC: usize = 0x80000;
const SOCK_FLAGS: usize = SOCK_NONBLOCK | SOCK_CLOEXEC;
const MSG_NOSIGNAL: usize = 0x4000;
const SOCKADDR_LEN: usize = 16;

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

fn on_socket<T>(
    fd: usize,
    action: impl FnOnce(&SocketFile) -> Result<T, isize>,
) -> Result<T, isize> {
    let file = file_by_fd(fd)?;
    if file.socket().is_none() {
        return Err(-88);
    }
    with_io_file(file, |file| action(file.socket().unwrap()))
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

pub(super) fn socket([family, flags, protocol, ..]: [usize; 6]) -> isize {
    let result = (|| {
        if family != AF_INET as usize {
            return Err(-97);
        }
        let kind = match (flags & !SOCK_FLAGS, protocol) {
            (1, 0 | 6) => Kind::Tcp,
            (2, 0 | 17) => Kind::Udp,
            (1 | 2, _) => return Err(-93),
            _ => return Err(-22),
        };
        install(
            SocketFile::new(kind, flags & SOCK_NONBLOCK != 0)?,
            flags & SOCK_CLOEXEC != 0,
        )
    })();
    result.map_or_else(|error| error, |fd| fd as isize)
}

pub(super) fn bind([fd, address, len, ..]: [usize; 6]) -> isize {
    on_socket(fd, |socket| socket.bind(read_address(address, len)?))
        .err()
        .unwrap_or(0)
}

pub(super) fn listen([fd, backlog, ..]: [usize; 6]) -> isize {
    on_socket(fd, |socket| socket.listen(backlog as i32))
        .err()
        .unwrap_or(0)
}

pub(super) fn accept([fd, address, len, flags, ..]: [usize; 6]) -> isize {
    if flags & !SOCK_FLAGS != 0 {
        return -22;
    }
    let result = on_socket(fd, |socket| {
        let (accepted, peer) = socket.accept(flags & SOCK_NONBLOCK != 0, || {
            let process = current_process();
            let inner = process.inner_exclusive_access();
            inner.fd_table.iter().any(Option::is_none) || inner.fd_table.len() < inner.nofile_limit
        })?;
        if address != 0 {
            write_address(address, len, peer)?;
        }
        install(accepted, flags & SOCK_CLOEXEC != 0)
    });
    result.map_or_else(|error| error, |fd| fd as isize)
}

pub(super) fn connect([fd, address, len, ..]: [usize; 6]) -> isize {
    on_socket(fd, |socket| socket.connect(read_address(address, len)?))
        .err()
        .unwrap_or(0)
}

pub(super) fn get_name([fd, address, len, ..]: [usize; 6], peer: bool) -> isize {
    on_socket(fd, |socket| {
        if address == 0 || len == 0 {
            return Err(-14);
        }
        let endpoint = if peer { socket.peer()? } else { socket.local() };
        write_address(address, len, endpoint)
    })
    .err()
    .unwrap_or(0)
}

pub(super) fn send_to([fd, address, len, flags, peer_ptr, peer_len]: [usize; 6]) -> isize {
    if flags & !(MSG_DONTWAIT | MSG_NOSIGNAL) != 0 {
        return -95;
    }
    on_socket(fd, |socket| {
        let peer = if peer_ptr == 0 {
            None
        } else {
            Some(read_address(peer_ptr, peer_len)?)
        };
        socket.send(&user_buffer(address, len, MapPermission::R)?, flags, peer)
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
    on_socket(fd, |socket| {
        let (count, peer) = socket.receive(&user_buffer(address, len, MapPermission::W)?, flags)?;
        if peer_ptr != 0 {
            if let Some(peer) = peer {
                write_address(peer_ptr, peer_len, peer)?;
            }
        }
        Ok(count)
    })
    .map_or_else(|error| error, |count| count as isize)
}

pub(super) fn shutdown([fd, how, ..]: [usize; 6]) -> isize {
    on_socket(fd, |socket| socket.shutdown(how))
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
    on_socket(fd, |socket| {
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
            socket.set_io_timeout(receive, (duration != 0).then_some(duration));
            return Ok(());
        }

        if len < 4 {
            return Err(-22);
        }
        let value = try_read_user(token, ptr as *const i32).ok_or(-14isize)?;
        socket.set_option(level, option, value)
    })
    .err()
    .unwrap_or(0)
}

pub(super) fn get_option([fd, level, option, ptr, len, ..]: [usize; 6]) -> isize {
    on_socket(fd, |socket| {
        let token = current_user_token();
        let capacity = try_read_user(token, len as *const i32).ok_or(-14isize)?;
        if capacity < 0 {
            return Err(-22);
        }
        let length = prepare_user_write(token, len as *mut u32).ok_or(-14isize)?;
        let timeout = timeout_option(level, option);
        let size = (capacity as usize).min(if timeout.is_some() { 16 } else { 4 });
        let buffer = user_buffer(ptr, size, MapPermission::W)?;
        let mut value = [0; 16];
        if let Some(receive) = timeout {
            let duration = socket.io_timeout(receive);
            value[..8].copy_from_slice(&((duration / 1_000_000_000) as i64).to_ne_bytes());
            value[8..].copy_from_slice(&((duration / 1000 % 1_000_000) as i64).to_ne_bytes());
        } else {
            value[..4].copy_from_slice(&socket.get_option(level, option)?.to_ne_bytes());
        }
        buffer.copy_from(&value[..size]);
        length.write(size as u32);
        Ok(())
    })
    .err()
    .unwrap_or(0)
}
