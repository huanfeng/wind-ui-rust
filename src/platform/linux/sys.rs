//! 事件循环的两件系统原语：`poll(2)` 与跨线程唤醒管道；外加 Wayland 共享内存缓冲用的
//! `memfd_create(2)`。
//!
//! 只声明用到的那几个 libc 函数，不为此引入 `libc` crate。

use std::ffi::{c_int, c_short, c_ulong};
use std::io::{Read, Write};
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;
use std::sync::OnceLock;
use std::time::Duration;

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

extern "C" {
    fn poll(fds: *mut PollFd, nfds: c_ulong, timeout: c_int) -> c_int;
    #[cfg(feature = "wayland")]
    fn memfd_create(name: *const std::ffi::c_char, flags: std::ffi::c_uint) -> c_int;
}

/// 匿名内存文件（`MFD_CLOEXEC`），长度 `len` 字节。Wayland 的 `wl_shm` 缓冲以它为底：
/// 合成器 mmap 同一个 fd，我们用 `pwrite` 写像素，不在本进程映射。
#[cfg(feature = "wayland")]
pub(super) fn memfd(name: &std::ffi::CStr, len: u64) -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    const MFD_CLOEXEC: std::ffi::c_uint = 1;
    // SAFETY：`name` 是以 NUL 结尾的 CStr，调用期间有效；flags 只含 MFD_CLOEXEC。
    let fd = unsafe { memfd_create(name.as_ptr(), MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY：fd 刚由 memfd_create 返回、无人持有，所有权交给 File。
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.set_len(len)?;
    Ok(file)
}

const POLLIN: c_short = 1;
const POLLOUT: c_short = 4;

/// 阻塞到任一 fd 可读或超时（`None` = 无限等待）。被信号打断视同超时返回。
pub(super) fn wait_readable(fds: &[RawFd], timeout: Option<Duration>) {
    wait_io(fds, None, timeout);
}

/// 同 [`wait_readable`]，另外在 `writable` 可写时也返回（发送缓冲积压、等对端腾出空间）。
pub(super) fn wait_io(fds: &[RawFd], writable: Option<RawFd>, timeout: Option<Duration>) {
    let mut pfds: Vec<PollFd> = fds
        .iter()
        .map(|&fd| PollFd {
            fd,
            events: if Some(fd) == writable {
                POLLIN | POLLOUT
            } else {
                POLLIN
            },
            revents: 0,
        })
        .collect();
    let ms = match timeout {
        None => -1,
        // 向上取整到毫秒：向下取整会让「还差 0.4ms 到截止」变成 0 超时 → 空转一轮。
        Some(d) => d.as_micros().div_ceil(1000).min(i32::MAX as u128) as c_int,
    };
    unsafe { poll(pfds.as_mut_ptr(), pfds.len() as c_ulong, ms) };
}

/// 进程级唤醒管道。写端给后台线程（`Waker`、单实例 accept 线程），读端给事件循环。
pub(super) struct WakePipe {
    pub read: UnixStream,
    write: UnixStream,
}

static PIPE: OnceLock<Option<WakePipe>> = OnceLock::new();

pub(super) fn pipe() -> Option<&'static WakePipe> {
    PIPE.get_or_init(|| {
        let (r, w) = UnixStream::pair().ok()?;
        r.set_nonblocking(true).ok()?;
        w.set_nonblocking(true).ok()?;
        Some(WakePipe { read: r, write: w })
    })
    .as_ref()
}

/// 唤醒事件循环。管道写满（循环长时间没排空）时写失败也无妨：已有未读字节，循环必醒。
pub(crate) fn wake() {
    if let Some(p) = pipe() {
        let _ = (&p.write).write(&[1]);
    }
}

impl WakePipe {
    /// 排空积压的唤醒字节。返回是否有过唤醒。
    pub fn drain(&self) -> bool {
        let mut buf = [0u8; 64];
        let mut any = false;
        while let Ok(n) = (&self.read).read(&mut buf) {
            if n == 0 {
                break;
            }
            any = true;
        }
        any
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn wake_is_observed_and_drained() {
        let p = pipe().expect("socketpair");
        p.drain();
        wake();
        wait_readable(&[p.read.as_raw_fd()], Some(Duration::from_millis(500)));
        assert!(p.drain());
        assert!(!p.drain(), "排空后不应再报唤醒");
    }
}
