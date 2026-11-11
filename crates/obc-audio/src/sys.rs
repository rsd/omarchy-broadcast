//! Thin wrappers over the Linux system calls the transport needs:
//! `SOCK_SEQPACKET` Unix sockets with `SCM_RIGHTS`, eventfds and `poll`.
//!
//! Everything here is non-real-time except [`wake_signal`] (one `send(2)`
//! with `MSG_DONTWAIT`, what the plugin's audio thread calls to wake the
//! worker) and [`eventfd_signal`] (one write to a plugin-local eventfd).

use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use std::time::Duration;

/// The most file descriptors one message may carry. The hello carries two;
/// a receiver closes anything beyond what it expects.
pub const MAX_FDS: usize = 4;

/// Turns a `-1` return into the thread's `errno`.
fn cvt(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

/// Wraps a freshly returned descriptor.
fn owned(fd: libc::c_int) -> io::Result<OwnedFd> {
    let fd = cvt(fd)?;
    // SAFETY: `fd` was just returned by the kernel and is owned by no one else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn sockaddr(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    // SAFETY: sockaddr_un is plain old data; all-zero is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path is empty, too long for sockaddr_un, or contains NUL",
        ));
    }
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    let len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    Ok((addr, len as libc::socklen_t))
}

fn seqpacket_socket() -> io::Result<OwnedFd> {
    // SAFETY: plain syscall, no pointers.
    owned(unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) })
}

/// One connected `SOCK_SEQPACKET` Unix socket: message boundaries are kept,
/// so every `send` arrives as exactly one `recv`.
#[derive(Debug)]
pub struct SeqPacket(OwnedFd);

impl SeqPacket {
    /// Connects to a listening socket at `path`.
    pub fn connect(path: &Path) -> io::Result<Self> {
        let fd = seqpacket_socket()?;
        let (addr, len) = sockaddr(path)?;
        // SAFETY: `addr` is a valid sockaddr_un of `len` bytes.
        cvt(unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&raw const addr).cast::<libc::sockaddr>(),
                len,
            )
        })?;
        Ok(Self(fd))
    }

    /// Starts a non-blocking connect to `path`. `Ok((socket, false))` when
    /// connected at once, `Ok((socket, true))` when the connect is in
    /// progress (wait for `POLLOUT`, then [`SeqPacket::take_error`]). A
    /// listener whose backlog is full gives `WouldBlock` at once, never a
    /// wait. The socket stays non-blocking.
    pub fn connect_nonblocking(path: &Path) -> io::Result<(Self, bool)> {
        // SAFETY: plain syscall, no pointers.
        let fd = owned(unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        })?;
        let (addr, len) = sockaddr(path)?;
        // SAFETY: `addr` is a valid sockaddr_un of `len` bytes.
        let ret = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&raw const addr).cast::<libc::sockaddr>(),
                len,
            )
        };
        if ret == 0 {
            return Ok((Self(fd), false));
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINPROGRESS) {
            return Ok((Self(fd), true));
        }
        Err(err)
    }

    /// The socket's pending error (`SO_ERROR`), cleared by reading it.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        let mut err: libc::c_int = 0;
        let mut len = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `err` and `len` are valid out-pointers of the right size.
        cvt(unsafe {
            libc::getsockopt(
                self.0.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&raw mut err).cast(),
                &raw mut len,
            )
        })?;
        Ok((err != 0).then(|| io::Error::from_raw_os_error(err)))
    }

    /// A connected pair, for tests and in-process use.
    pub fn pair() -> io::Result<(Self, Self)> {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `fds` has room for the two descriptors socketpair writes.
        cvt(unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        })?;
        // SAFETY: both descriptors were just created and are owned by us.
        unsafe {
            Ok((
                Self(OwnedFd::from_raw_fd(fds[0])),
                Self(OwnedFd::from_raw_fd(fds[1])),
            ))
        }
    }

    /// Wraps an already connected `SOCK_SEQPACKET` descriptor.
    pub fn from_fd(fd: OwnedFd) -> Self {
        Self(fd)
    }

    /// Sets the receive and send timeouts (`None` blocks forever).
    pub fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        let tv = match timeout {
            // A zero timeval means "forever" to the kernel, so a zero
            // duration becomes one microsecond.
            Some(d) if d.is_zero() => libc::timeval {
                tv_sec: 0,
                tv_usec: 1,
            },
            Some(d) => libc::timeval {
                tv_sec: d.as_secs() as libc::time_t,
                tv_usec: d.subsec_micros() as libc::suseconds_t,
            },
            None => libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
        };
        for opt in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
            // SAFETY: `tv` is a valid timeval for the option's length.
            cvt(unsafe {
                libc::setsockopt(
                    self.0.as_raw_fd(),
                    libc::SOL_SOCKET,
                    opt,
                    (&raw const tv).cast(),
                    size_of::<libc::timeval>() as libc::socklen_t,
                )
            })?;
        }
        Ok(())
    }

    /// Sends one message with `fds` attached as `SCM_RIGHTS`.
    pub fn send(&self, bytes: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<()> {
        assert!(fds.len() <= MAX_FDS, "too many descriptors for one message");
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let mut cmsg = CmsgBuf([0; CMSG_BUF_LEN]);
        // SAFETY: msghdr is plain old data; all-zero is valid.
        let mut msg: libc::msghdr = unsafe { zeroed() };
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        if !fds.is_empty() {
            let data_len = (fds.len() * size_of::<RawFd>()) as libc::c_uint;
            msg.msg_control = cmsg.0.as_mut_ptr().cast();
            // SAFETY: CMSG_SPACE is pure arithmetic.
            msg.msg_controllen = unsafe { libc::CMSG_SPACE(data_len) } as usize;
            // SAFETY: msg_control points at a buffer of msg_controllen bytes
            // (CMSG_BUF_LEN >= CMSG_SPACE(MAX_FDS * 4)), aligned for cmsghdr,
            // so the first header and its data are in bounds.
            unsafe {
                let hdr = libc::CMSG_FIRSTHDR(&raw const msg);
                (*hdr).cmsg_level = libc::SOL_SOCKET;
                (*hdr).cmsg_type = libc::SCM_RIGHTS;
                (*hdr).cmsg_len = libc::CMSG_LEN(data_len) as usize;
                let data = libc::CMSG_DATA(hdr).cast::<RawFd>();
                for (i, fd) in fds.iter().enumerate() {
                    data.add(i).write_unaligned(fd.as_raw_fd());
                }
            }
        }
        // SAFETY: `msg` and everything it points at outlive the call.
        let sent = unsafe { libc::sendmsg(self.0.as_raw_fd(), &raw const msg, libc::MSG_NOSIGNAL) };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        if sent as usize != bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short seqpacket send",
            ));
        }
        Ok(())
    }

    /// Receives one message into `buf`, returning its length and the
    /// descriptors attached to it (received close-on-exec).
    ///
    /// A message longer than `buf`, or with more than [`MAX_FDS`]
    /// descriptors, is an `InvalidData` error (the descriptors that did
    /// arrive are closed). A length of 0 means the peer hung up.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        let mut cmsg = CmsgBuf([0; CMSG_BUF_LEN]);
        // SAFETY: msghdr is plain old data; all-zero is valid.
        let mut msg: libc::msghdr = unsafe { zeroed() };
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg.0.as_mut_ptr().cast();
        msg.msg_controllen = CMSG_BUF_LEN;
        // SAFETY: `msg` and its buffers outlive the call.
        let n = unsafe { libc::recvmsg(self.0.as_raw_fd(), &raw mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut fds = Vec::new();
        // SAFETY: the kernel filled msg_control with well-formed headers
        // within msg_controllen; CMSG_FIRSTHDR/NXTHDR stay inside it.
        unsafe {
            let mut hdr = libc::CMSG_FIRSTHDR(&raw const msg);
            while !hdr.is_null() {
                if (*hdr).cmsg_level == libc::SOL_SOCKET && (*hdr).cmsg_type == libc::SCM_RIGHTS {
                    let data = libc::CMSG_DATA(hdr).cast::<RawFd>();
                    let header_len = libc::CMSG_LEN(0) as usize;
                    let count = ((*hdr).cmsg_len - header_len) / size_of::<RawFd>();
                    for i in 0..count {
                        fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                    }
                }
                hdr = libc::CMSG_NXTHDR(&raw const msg, hdr);
            }
        }
        if msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "message or its descriptors were truncated",
            ));
        }
        if fds.len() > MAX_FDS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many descriptors",
            ));
        }
        Ok((n as usize, fds))
    }
}

impl AsFd for SeqPacket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

const CMSG_BUF_LEN: usize = 64;

/// A control-message buffer aligned for `cmsghdr`.
#[repr(C, align(8))]
struct CmsgBuf([u8; CMSG_BUF_LEN]);

/// A listening `SOCK_SEQPACKET` Unix socket.
#[derive(Debug)]
pub struct SeqPacketListener(OwnedFd);

impl SeqPacketListener {
    /// Binds and listens at `path`. The file must not exist.
    pub fn bind(path: &Path) -> io::Result<Self> {
        let fd = seqpacket_socket()?;
        let (addr, len) = sockaddr(path)?;
        // SAFETY: `addr` is a valid sockaddr_un of `len` bytes.
        cvt(unsafe { libc::bind(fd.as_raw_fd(), (&raw const addr).cast(), len) })?;
        // SAFETY: plain syscall.
        cvt(unsafe { libc::listen(fd.as_raw_fd(), 8) })?;
        Ok(Self(fd))
    }

    /// Wraps an already listening socket, e.g. one passed by systemd
    /// socket activation (`LISTEN_FDS`).
    pub fn from_fd(fd: OwnedFd) -> Self {
        Self(fd)
    }

    /// Accepts one connection, blocking.
    pub fn accept(&self) -> io::Result<SeqPacket> {
        // SAFETY: null address pointers are allowed by accept4.
        let fd = owned(unsafe {
            libc::accept4(
                self.0.as_raw_fd(),
                ptr::null_mut(),
                ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        })?;
        Ok(SeqPacket(fd))
    }

    /// Accepts one connection if one arrives within `timeout`.
    pub fn accept_timeout(&self, timeout: Duration) -> io::Result<Option<SeqPacket>> {
        let mut pfd = [pollfd(self.0.as_fd(), libc::POLLIN)];
        if poll(&mut pfd, Some(timeout))? == 0 {
            return Ok(None);
        }
        self.accept().map(Some)
    }
}

impl AsFd for SeqPacketListener {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// A `pollfd` for `fd` waiting for `events`.
pub fn pollfd(fd: BorrowedFd<'_>, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd: fd.as_raw_fd(),
        events,
        revents: 0,
    }
}

/// `poll(2)`, retried on `EINTR`; `None` waits forever. Returns the number
/// of ready descriptors.
pub fn poll(fds: &mut [libc::pollfd], timeout: Option<Duration>) -> io::Result<usize> {
    let ms = timeout.map_or(-1, |d| d.as_millis().min(i32::MAX as u128) as libc::c_int);
    loop {
        // SAFETY: `fds` is a valid array of `fds.len()` pollfd structures.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// A new non-blocking, close-on-exec eventfd.
pub fn eventfd_new() -> io::Result<OwnedFd> {
    // SAFETY: plain syscall.
    owned(unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) })
}

/// Adds 1 to an eventfd. Real-time safe: one non-blocking `write(2)`; a
/// full counter (`EAGAIN`) or any other error is ignored, since the reader
/// only needs to know that something is pending.
pub fn eventfd_signal(fd: BorrowedFd<'_>) {
    let one: u64 = 1;
    // SAFETY: writes 8 bytes from a valid u64.
    let _ = unsafe { libc::write(fd.as_raw_fd(), (&raw const one).cast(), size_of::<u64>()) };
}

/// Resets an eventfd's counter (reads it if it is non-zero). The descriptor
/// must be non-blocking.
pub fn eventfd_drain(fd: BorrowedFd<'_>) {
    let mut value: u64 = 0;
    // SAFETY: reads at most 8 bytes into a valid u64.
    let _ = unsafe { libc::read(fd.as_raw_fd(), (&raw mut value).cast(), size_of::<u64>()) };
}

/// A new wake channel: a connected `SOCK_SEQPACKET` pair, both ends
/// close-on-exec and non-blocking. The plugin keeps the first end and sends
/// the second to the worker. The two ends are separate open files, and
/// [`wake_signal`] passes `MSG_DONTWAIT` on every call, so nothing the
/// worker does with its end (clearing `O_NONBLOCK`, not reading, closing
/// it) can make the plugin's send block or raise `SIGPIPE`.
pub fn wake_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` has room for the two descriptors socketpair writes.
    cvt(unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
            fds.as_mut_ptr(),
        )
    })?;
    // SAFETY: both descriptors were just created and are owned by us.
    unsafe { Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))) }
}

/// Sends one 1-byte wake message. Real-time safe: a single `send(2)` with
/// `MSG_DONTWAIT | MSG_NOSIGNAL`, which never waits whatever the state of
/// the descriptor or of the peer. A full queue (`EAGAIN`, the worker has
/// not read the earlier wakes yet), a gone peer or any other error is
/// ignored: the reader only needs to know that something is pending.
pub fn wake_signal(fd: BorrowedFd<'_>) {
    let one = 1u8;
    // SAFETY: sends one byte from a valid u8.
    let _ = unsafe {
        libc::send(
            fd.as_raw_fd(),
            (&raw const one).cast(),
            1,
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
        )
    };
}

/// Discards the pending wake messages (at most 4096 per call, so a peer
/// flooding the channel cannot keep the caller here). Never waits.
pub fn wake_drain(fd: BorrowedFd<'_>) {
    let mut buf = [0u8; 16];
    for _ in 0..4096 {
        // SAFETY: receives at most `buf.len()` bytes into a valid buffer.
        let n = unsafe {
            libc::recv(
                fd.as_raw_fd(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if n <= 0 {
            break;
        }
    }
}

/// The type (`SO_TYPE`) of a socket; an error for a descriptor that is not
/// a socket.
pub fn socket_type(fd: BorrowedFd<'_>) -> io::Result<libc::c_int> {
    let mut ty: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `ty` and `len` are valid out-pointers of the right size.
    cvt(unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&raw mut ty).cast(),
            &raw mut len,
        )
    })?;
    Ok(ty)
}

/// Sets `O_NONBLOCK` on a descriptor received from a peer.
pub fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: plain fcntl calls on a valid descriptor.
    unsafe {
        let flags = cvt(libc::fcntl(fd.as_raw_fd(), libc::F_GETFL))?;
        cvt(libc::fcntl(
            fd.as_raw_fd(),
            libc::F_SETFL,
            flags | libc::O_NONBLOCK,
        ))?;
    }
    Ok(())
}

/// Creates a sealable memfd of `len` bytes, sealed against shrinking,
/// growing and further sealing, so neither side can make the other's
/// mapping fault (`SIGBUS`).
pub fn sealed_memfd(name: &std::ffi::CStr, len: usize) -> io::Result<OwnedFd> {
    // SAFETY: `name` is NUL-terminated.
    let fd = owned(unsafe {
        libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
    })?;
    // SAFETY: plain syscalls on our own descriptor.
    unsafe {
        cvt(libc::ftruncate(fd.as_raw_fd(), len as libc::off_t))?;
        cvt(libc::fcntl(
            fd.as_raw_fd(),
            libc::F_ADD_SEALS,
            libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL,
        ))?;
    }
    Ok(fd)
}

/// The size of a file in bytes.
pub fn file_size(fd: BorrowedFd<'_>) -> io::Result<u64> {
    // SAFETY: stat is plain old data; fstat fills it.
    let mut st: libc::stat = unsafe { zeroed() };
    // SAFETY: `st` is a valid out-pointer.
    cvt(unsafe { libc::fstat(fd.as_raw_fd(), &raw mut st) })?;
    Ok(st.st_size as u64)
}

/// The seals of a memfd (`F_GET_SEALS`); an error for a non-memfd.
pub fn seals(fd: BorrowedFd<'_>) -> io::Result<libc::c_int> {
    // SAFETY: plain fcntl.
    cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) })
}
