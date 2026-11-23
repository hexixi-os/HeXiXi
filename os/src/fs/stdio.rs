use super::File;
use crate::mm::UserBuffer;
use crate::sbi::console_getchar;
use crate::task::suspend_current_and_run_next;

pub struct Stdin;

pub struct Stdout;

impl File for Stdin {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, user_buf: UserBuffer) -> usize {
        // 支持任意长度的读：阻塞等到第一个字符，之后尽量读满
        let mut n = 0usize;
        for slot in user_buf {
            let c = loop {
                let c = console_getchar();
                if c != 0 && c != usize::MAX {
                    break c;
                }
                if n > 0 {
                    return n;
                }
                suspend_current_and_run_next();
            };
            unsafe { slot.write(c as u8); }
            n += 1;
        }
        n
    }
    fn write(&self, _user_buf: UserBuffer) -> isize {
        panic!("Cannot write to stdin!");
    }
}

impl File for Stdout {
    fn readable(&self) -> bool {
        false
    }
    fn writable(&self) -> bool {
        true
    }
    fn read(&self, _user_buf: UserBuffer) -> usize {
        panic!("Cannot read from stdout!");
    }
    fn write(&self, user_buf: UserBuffer) -> isize {
        for buffer in user_buf.buffers.iter() {
            let slice = unsafe { core::slice::from_raw_parts(buffer.as_ptr(), buffer.len()) };
            print!("{}", core::str::from_utf8(slice).unwrap());
        }
        user_buf.len() as isize
    }
}
