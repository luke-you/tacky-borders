use anyhow::Context;
use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;
use std::thread::{self, JoinHandle};
use std::time;
use std::{io, mem, ptr};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Networking::WinSock::{
    ADDRESS_FAMILY, AF_UNIX, AcceptEx, FIONBIO, INVALID_SOCKET, POLLWRNORM, SD_BOTH,
    SEND_RECV_FLAGS, SO_ERROR, SO_RCVTIMEO, SO_SNDTIMEO, SOCK_STREAM, SOCKADDR, SOCKADDR_UN,
    SOCKET, SOCKET_ERROR, SOL_SOCKET, SOMAXCONN, WSA_FLAG_OVERLAPPED, WSA_IO_PENDING, WSABUF,
    WSAEWOULDBLOCK, WSAGetLastError, WSAPOLLFD, WSAPoll, WSARecv, WSASend, WSASocketW, accept,
    bind, closesocket, connect, getsockopt, ioctlsocket, listen, recv, send, setsockopt, shutdown,
};
use windows::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatus, GetQueuedCompletionStatusEx,
    OVERLAPPED, OVERLAPPED_ENTRY, PostQueuedCompletionStatus,
};
use windows::Win32::System::Threading::INFINITE;
use windows::core::PSTR;

use crate::utils::LogIfErr;

const UNIX_ADDR_LEN: u32 = mem::size_of::<SOCKADDR_UN>() as u32;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SocketId(usize);

fn timeout_millis(timeout: time::Duration) -> io::Result<u32> {
    if timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket timeout must be greater than zero",
        ));
    }

    let has_partial_millisecond = !timeout.subsec_nanos().is_multiple_of(1_000_000);
    let millis = timeout
        .as_millis()
        .checked_add(u128::from(has_partial_millisecond))
        .and_then(|millis| u32::try_from(millis).ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket timeout exceeds the Windows millisecond limit",
            )
        })?;

    Ok(millis)
}

pub(crate) fn shutdown_socket(socket_id: SocketId) -> io::Result<()> {
    if unsafe { shutdown(SOCKET(socket_id.0), SD_BOTH) } == SOCKET_ERROR {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

// Adapted from Rust's standard library (std::sys::net::connection::socket::windows, commit 6ba0ce4)
mod winsock {
    use std::mem;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering::{AcqRel, Relaxed};

    use windows::Win32::Networking::WinSock::{WSACleanup, WSADATA, WSAStartup};

    static WSA_INITIALIZED: AtomicBool = AtomicBool::new(false);

    /// Checks whether the Windows socket interface has been started already, and
    /// if not, starts it.
    #[inline]
    pub fn init() {
        if !WSA_INITIALIZED.load(Relaxed) {
            wsa_startup();
        }
    }

    #[cold]
    fn wsa_startup() {
        unsafe {
            let mut data: WSADATA = mem::zeroed();
            let ret = WSAStartup(
                0x202, // version 2.2
                &mut data,
            );
            assert_eq!(ret, 0);
            if WSA_INITIALIZED.swap(true, AcqRel) {
                // If another thread raced with us and called WSAStartup first then call
                // WSACleanup so it's as though WSAStartup was only called once.
                WSACleanup();
            }
        }
    }

    #[allow(unused)]
    pub fn cleanup() {
        // We don't need to call WSACleanup here because exiting the process will cause
        // the OS to clean everything for us, which is faster than doing it manually.
        // See #141799.
    }
}

pub struct OverlappedContext {
    pub overlapped: Box<OVERLAPPED>,
    pub buffer: Option<Vec<u8>>,
    pub flags: Box<u32>,
}

pub struct UnixListener {
    pub socket: UnixDomainSocket,
}

unsafe impl Send for UnixListener {}

impl UnixListener {
    pub fn bind(socket_path: &Path) -> io::Result<Self> {
        winsock::init();

        let server_socket = UnixDomainSocket::new()?;
        server_socket.bind(socket_path)?;
        server_socket.listen(SOMAXCONN as i32)?;

        Ok(Self {
            socket: server_socket,
        })
    }

    /// # Safety
    ///
    /// Performs an asynchronous I/O operation using an `OVERLAPPED` struct owned by the
    /// `UnixListener` or `UnixStream`. To uphold memory safety, ensure the following:
    ///
    /// - Only one overlapped operation should be in flight at a time per struct. Running
    ///   concurrent operations on the same struct is undefined behavior.
    /// - This struct must not be dropped until the operation has completed. Dropping it early may
    ///   lead to use-after-free errors.
    ///
    /// Upon completion, the local and remote addresses are written into the `buffer` of the
    /// returned `UnixStream`'s `overlapped_context`. The layout of this buffer is determined by
    /// AcceptEx; use GetAcceptExSockaddrs to parse the addresses.
    pub unsafe fn accept_overlapped(&self) -> io::Result<UnixStream> {
        // I'm not 100% sure why we need at least this Vec len, but it's just double the len used
        // in AcceptEx (double I assume because there's both the local and remote addresses)
        let mut socket_addr = vec![0u8; ((UNIX_ADDR_LEN + 16) * 2) as usize];
        let mut overlapped = Box::new(OVERLAPPED::default());
        let client_socket = unsafe {
            self.socket
                .accept_overlapped(&mut socket_addr, &mut overlapped)
        }?;

        Ok(UnixStream {
            socket: client_socket,
            overlapped_context: OverlappedContext {
                overlapped,
                buffer: Some(socket_addr),
                flags: Box::new(0),
            },
        })
    }

    pub fn accept(&self) -> io::Result<UnixStream> {
        let client_socket = self.socket.accept(None, None)?;

        Ok(UnixStream {
            socket: client_socket,
            overlapped_context: OverlappedContext {
                overlapped: Box::new(OVERLAPPED::default()),
                buffer: None,
                flags: Box::new(0),
            },
        })
    }

    pub(crate) fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.socket.set_nonblocking(nonblocking)
    }

    pub fn token(&self) -> usize {
        self.socket.0.0
    }
}

pub struct UnixStream {
    pub socket: UnixDomainSocket,
    pub overlapped_context: OverlappedContext,
}

unsafe impl Send for UnixStream {}

impl UnixStream {
    pub(crate) fn connect_timeout(path: &Path, timeout: time::Duration) -> io::Result<Self> {
        winsock::init();
        let socket = UnixDomainSocket::new()?;
        socket.set_nonblocking(true)?;
        let deadline = time::Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "connect deadline overflow")
        })?;
        if let Err(error) = socket.connect(path) {
            if error.raw_os_error() != Some(WSAEWOULDBLOCK.0) {
                return Err(error);
            }
            let remaining = deadline.saturating_duration_since(time::Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "IPC connect timed out",
                ));
            }
            let mut poll = [WSAPOLLFD {
                fd: socket.0,
                events: POLLWRNORM,
                revents: Default::default(),
            }];
            let millis = timeout_millis(remaining)?.min(i32::MAX as u32) as i32;
            let ready = unsafe { WSAPoll(poll.as_mut_ptr(), 1, millis) };
            if ready == SOCKET_ERROR {
                return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError().0 }));
            }
            if ready == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "IPC connect timed out",
                ));
            }
            let mut error = [0u8; 4];
            let mut length = 4;
            if unsafe {
                getsockopt(
                    socket.0,
                    SOL_SOCKET,
                    SO_ERROR,
                    PSTR(error.as_mut_ptr()),
                    &mut length,
                )
            } == SOCKET_ERROR
            {
                return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError().0 }));
            }
            let code = i32::from_ne_bytes(error);
            if code != 0 {
                return Err(io::Error::from_raw_os_error(code));
            }
        }
        socket.set_nonblocking(false)?;
        Ok(Self {
            socket,
            overlapped_context: OverlappedContext {
                overlapped: Box::new(OVERLAPPED::default()),
                buffer: None,
                flags: Box::new(0),
            },
        })
    }

    pub fn connect(path: &Path) -> io::Result<Self> {
        winsock::init();

        let client_socket = UnixDomainSocket::new()?;
        client_socket.connect(path)?;

        Ok(Self {
            socket: client_socket,
            overlapped_context: OverlappedContext {
                overlapped: Box::new(OVERLAPPED::default()),
                buffer: None,
                flags: Box::new(0),
            },
        })
    }

    /// # Safety
    ///
    /// Performs an asynchronous I/O operation using an `OVERLAPPED` struct owned by the
    /// `UnixListener` or `UnixStream`. To uphold memory safety, ensure the following:
    ///
    /// - Only one overlapped operation should be in flight at a time per struct. Running
    ///   concurrent operations on the same struct is undefined behavior.
    /// - This struct must not be dropped until the operation has completed. Dropping it early may
    ///   lead to use-after-free errors.
    ///
    /// The return value indicates the immediate number of bytes transferred. A non-zero value
    /// indicates synchronous completion. Otherwise, the operation is pending and will complete
    /// asynchronously, and the number of bytes transferred must be retrieved via the completion
    /// notification mechanism.
    ///
    /// NOTE: This takes ownership of the input buffer to avoid race conditions
    pub unsafe fn read_overlapped(&mut self, outputbuffer: Vec<u8>) -> io::Result<u32> {
        let context = &mut self.overlapped_context;

        // Reset flags between I/O operations
        *context.flags = 0;

        // Here is where we take ownership of the buffer
        context.buffer = Some(outputbuffer);

        unsafe {
            self.socket.read_overlapped(
                context.buffer.as_mut().unwrap(),
                &mut context.overlapped,
                &mut context.flags,
            )
        }
    }

    /// # Safety
    ///
    /// Performs an asynchronous I/O operation using an `OVERLAPPED` struct owned by the
    /// `UnixListener` or `UnixStream`. To uphold memory safety, ensure the following:
    ///
    /// - Only one overlapped operation should be in flight at a time per struct. Running
    ///   concurrent operations on the same struct is undefined behavior.
    /// - This struct must not be dropped until the operation has completed. Dropping it early may
    ///   lead to use-after-free errors.
    ///
    /// The return value indicates the immediate number of bytes transferred. A non-zero value
    /// indicates synchronous completion. Otherwise, the operation is pending and will complete
    /// asynchronously, and the number of bytes transferred must be retrieved via the completion
    /// notification mechanism.
    pub unsafe fn write_overlapped(&mut self, inputbuffer: &[u8]) -> io::Result<u32> {
        let context = &mut self.overlapped_context;

        // Reset flags between I/O operations
        *context.flags = 0;

        unsafe {
            self.socket
                .write_overlapped(inputbuffer, &mut context.overlapped, *context.flags)
        }
    }

    pub fn token(&self) -> usize {
        self.socket.0.0
    }

    pub(crate) fn set_read_timeout(&self, timeout: time::Duration) -> io::Result<()> {
        self.socket.set_timeout(SO_RCVTIMEO, timeout)
    }

    pub(crate) fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.socket.set_nonblocking(nonblocking)
    }

    pub(crate) fn set_write_timeout(&self, timeout: time::Duration) -> io::Result<()> {
        self.socket.set_timeout(SO_SNDTIMEO, timeout)
    }

    pub(crate) fn socket_id(&self) -> SocketId {
        SocketId(self.socket.0.0)
    }

    pub fn take_overlapped_buffer(&mut self) -> Option<Vec<u8>> {
        self.overlapped_context.buffer.take()
    }
}

// These delegate to their respective methods for &UnixStream
impl io::Read for UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut &*self, buf)
    }
}

impl io::Write for UnixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut &*self, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut &*self)
    }
}

// These allow use with non-mut references (mut isn't necessary for this type)
impl io::Read for &UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.socket
            .read(buf, SEND_RECV_FLAGS::default())
            .map(|bytes| bytes as usize)
    }
}

impl io::Write for &UnixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.socket
            .write(buf, SEND_RECV_FLAGS::default())
            .map(|bytes| bytes as usize)
    }

    // This is a no-op in Rust's standard library's UnixStream too
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub struct UnixDomainSocket(SOCKET);

impl UnixDomainSocket {
    pub fn new() -> io::Result<Self> {
        let socket = unsafe {
            WSASocketW(
                AF_UNIX as i32,
                SOCK_STREAM.0,
                0,
                None,
                0,
                WSA_FLAG_OVERLAPPED,
            )
        }?;

        Ok(Self(socket))
    }

    pub fn bind(&self, path: &Path) -> io::Result<()> {
        let sockaddr_un = sockaddr_un(path)?;

        let iresult = unsafe {
            bind(
                self.0,
                ptr::addr_of!(sockaddr_un) as *const SOCKADDR,
                mem::size_of_val(&sockaddr_un) as i32,
            )
        };
        if iresult == SOCKET_ERROR {
            let last_error = io::Error::last_os_error();
            return Err(last_error);
        }

        Ok(())
    }

    pub fn connect(&self, path: &Path) -> io::Result<()> {
        let sockaddr_un = sockaddr_un(path)?;

        if unsafe {
            connect(
                self.0,
                ptr::addr_of!(sockaddr_un) as *const SOCKADDR,
                mem::size_of_val(&sockaddr_un) as i32,
            )
        } == SOCKET_ERROR
        {
            let last_error = io::Error::last_os_error();
            return Err(last_error);
        }

        Ok(())
    }

    pub fn listen(&self, backlog: i32) -> io::Result<()> {
        if unsafe { listen(self.0, backlog) } == SOCKET_ERROR {
            let last_error = io::Error::last_os_error();
            return Err(last_error);
        }

        Ok(())
    }

    fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        let mut value = u32::from(nonblocking);
        if unsafe { ioctlsocket(self.0, FIONBIO, &mut value) } == SOCKET_ERROR {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    fn set_timeout(&self, option: i32, timeout: time::Duration) -> io::Result<()> {
        let millis = timeout_millis(timeout)?;
        let value = millis.to_ne_bytes();
        if unsafe { setsockopt(self.0, SOL_SOCKET, option, Some(&value)) } == SOCKET_ERROR {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    pub fn accept(
        &self,
        addr: Option<*mut SOCKADDR>,
        addrlen: Option<&mut i32>,
    ) -> io::Result<UnixDomainSocket> {
        let socket = unsafe { accept(self.0, addr, addrlen.map(|mut_ref| mut_ref as *mut _)) }
            .map_err(|_| io::Error::from_raw_os_error(unsafe { WSAGetLastError().0 }))?;

        Ok(UnixDomainSocket(socket))
    }

    /// Performs an overlapped (asynchronous) I/O operation. Completion must be handled via
    /// external mechanisms such as an I/O completion port.
    ///
    /// # Safety
    ///
    /// The caller must ensure that all references passed to this function remain valid and are not
    /// not reused until the operation completes. Refer to MSDN for more details on overlapped I/O.
    pub unsafe fn accept_overlapped(
        &self,
        lpoutputbuffer: &mut [u8],
        lpoverlapped: &mut OVERLAPPED,
    ) -> io::Result<UnixDomainSocket> {
        // Zero out unused OVERLAPPED struct fields (as per MSDN recommendation)
        *lpoverlapped = OVERLAPPED {
            hEvent: lpoverlapped.hEvent,
            ..Default::default()
        };

        let client_socket = UnixDomainSocket::new()?;
        let mut bytes_transferred = 0;

        if !unsafe {
            AcceptEx(
                self.0,
                client_socket.0,
                lpoutputbuffer as *mut _ as *mut _,
                0,
                // We add 16 to the address length because MSDN says so
                UNIX_ADDR_LEN + 16,
                UNIX_ADDR_LEN + 16,
                &mut bytes_transferred,
                lpoverlapped,
            )
        }
        .as_bool()
        {
            let last_error = io::Error::last_os_error();

            if last_error.raw_os_error() != Some(WSA_IO_PENDING.0) {
                return Err(last_error);
            }
        };

        Ok(client_socket)
    }

    pub fn read(&self, buf: &mut [u8], flags: SEND_RECV_FLAGS) -> io::Result<u32> {
        let bytes_transferred = unsafe { recv(self.0, buf, flags) };

        if bytes_transferred == SOCKET_ERROR {
            let last_error = io::Error::last_os_error();
            return Err(last_error);
        }

        Ok(bytes_transferred as u32)
    }

    /// Performs an overlapped (asynchronous) I/O operation. Completion must be handled via
    /// external mechanisms such as an I/O completion port.
    ///
    /// # Safety
    ///
    /// The caller must ensure that all references passed to this function remain valid and are not
    /// not reused until the operation completes. Refer to MSDN for more details on overlapped I/O.
    pub unsafe fn read_overlapped(
        &self,
        lpoutputbuffer: &mut [u8],
        lpoverlapped: &mut OVERLAPPED,
        lpflags: &mut u32,
    ) -> io::Result<u32> {
        // Zero out unused OVERLAPPED struct fields (as per MSDN recommendation)
        *lpoverlapped = OVERLAPPED {
            hEvent: lpoverlapped.hEvent,
            ..Default::default()
        };

        let lpbuffers = WSABUF {
            len: lpoutputbuffer.len() as u32,
            buf: PSTR(lpoutputbuffer.as_mut_ptr()),
        };
        let mut bytes_transferred = 0;

        // Note that we set lpnumberofbytesrecvd to a non-null pointer even though MSDN recommends
        // setting it to null when lpoverlapped is non-null. We do this anyways because the field
        // is still updated if the operation completes immediately, allowing us to indicate so.
        let iresult = unsafe {
            WSARecv(
                self.0,
                &[lpbuffers],
                Some(&mut bytes_transferred),
                lpflags,
                Some(lpoverlapped),
                None,
            )
        };

        if iresult == SOCKET_ERROR {
            let last_error = io::Error::last_os_error();

            if last_error.raw_os_error() != Some(WSA_IO_PENDING.0) {
                return Err(last_error);
            }
        }

        Ok(bytes_transferred)
    }

    pub fn write(&self, buf: &[u8], flags: SEND_RECV_FLAGS) -> io::Result<u32> {
        let bytes_transferred = unsafe { send(self.0, buf, flags) };

        if bytes_transferred == SOCKET_ERROR {
            let last_error = io::Error::last_os_error();
            return Err(last_error);
        }

        Ok(bytes_transferred as u32)
    }

    /// Performs an overlapped (asynchronous) I/O operation. Completion must be handled via
    /// external mechanisms such as an I/O completion port.
    ///
    /// # Safety
    ///
    /// The caller must ensure that all references passed to this function remain valid and are not
    /// not reused until the operation completes. Refer to MSDN for more details on overlapped I/O.
    pub unsafe fn write_overlapped(
        &self,
        lpinputbuffer: &[u8],
        lpoverlapped: &mut OVERLAPPED,
        lpflags: u32,
    ) -> io::Result<u32> {
        // Zero out unused OVERLAPPED struct fields (as per MSDN recommendation)
        *lpoverlapped = OVERLAPPED {
            hEvent: lpoverlapped.hEvent,
            ..Default::default()
        };

        // WSABUF requires a mut ptr to the buffer, but WSASend shouldn't mutate anything.
        // It should be safe to cast a const ptr to a mut ptr as a workaround.
        let lpbuffers = WSABUF {
            len: lpinputbuffer.len() as u32,
            buf: PSTR(lpinputbuffer.as_ptr() as *mut _),
        };
        let mut bytes_transferred = 0;

        // Note that we set lpnumberofbytessent to a non-null pointer even though MSDN recommends
        // setting it to null when lpoverlapped is non-null. We do this anyways because the field
        // is still updated if the operation completes immediately, allowing us to indicate so.
        let iresult = unsafe {
            WSASend(
                self.0,
                &[lpbuffers],
                Some(&mut bytes_transferred),
                lpflags,
                Some(lpoverlapped),
                None,
            )
        };

        if iresult == SOCKET_ERROR {
            let last_error = io::Error::last_os_error();

            if last_error.raw_os_error() != Some(WSA_IO_PENDING.0) {
                return Err(last_error);
            }
        }

        Ok(bytes_transferred)
    }

    pub fn to_handle(&self) -> HANDLE {
        HANDLE(self.0.0 as _)
    }
}

impl Drop for UnixDomainSocket {
    fn drop(&mut self) {
        let iresult = unsafe { closesocket(self.0) };
        if iresult != 0 {
            error!(
                "could not close unix domain socket {:?}: {}",
                self.0,
                io::Error::last_os_error()
            )
        }
    }
}

fn sockaddr_un(path: &Path) -> io::Result<SOCKADDR_UN> {
    let mut sun_path = [0i8; 108];
    let path_bytes = path.as_os_str().as_encoded_bytes();

    if path_bytes.len() > sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path is too long",
        ));
    }

    for (i, byte) in path_bytes.iter().enumerate() {
        sun_path[i] = *byte as i8;
    }

    Ok(SOCKADDR_UN {
        sun_family: ADDRESS_FAMILY(AF_UNIX),
        sun_path,
    })
}

#[derive(Debug)]
pub struct CompletionPort(HANDLE);

unsafe impl Send for CompletionPort {}

impl CompletionPort {
    pub fn new(threads: u32) -> io::Result<Self> {
        let iocp_handle =
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, threads) }?;

        Ok(Self(iocp_handle))
    }

    pub fn associate_handle(&self, handle: HANDLE, token: usize) -> io::Result<()> {
        // This just returns the HANDLE of the existing iocp, so we can ignore the return value
        let _ = unsafe { CreateIoCompletionPort(handle, Some(self.0), token, 0) }?;

        Ok(())
    }

    pub fn poll_single(
        &self,
        timeout: Option<time::Duration>,
        entry: &mut OVERLAPPED_ENTRY,
    ) -> io::Result<()> {
        let mut bytes_transferred = 0u32;
        let mut completion_key = 0usize;
        let mut lpoverlapped: *mut OVERLAPPED = ptr::null_mut();

        let timeout_ms = match timeout {
            Some(duration) => duration.as_millis() as u32,
            None => INFINITE,
        };

        unsafe {
            GetQueuedCompletionStatus(
                self.0,
                &mut bytes_transferred,
                &mut completion_key,
                &mut lpoverlapped,
                timeout_ms,
            )
        }?;

        *entry = OVERLAPPED_ENTRY {
            lpCompletionKey: completion_key,
            lpOverlapped: lpoverlapped,
            Internal: 0,
            dwNumberOfBytesTransferred: bytes_transferred,
        };

        Ok(())
    }

    pub fn poll_many(
        &self,
        timeout: Option<time::Duration>,
        entries: &mut [OVERLAPPED_ENTRY],
    ) -> io::Result<u32> {
        let mut num_entries_removed = 0u32;

        let timeout_ms = match timeout {
            Some(duration) => duration.as_millis() as u32,
            None => INFINITE,
        };

        unsafe {
            GetQueuedCompletionStatusEx(
                self.0,
                entries,
                &mut num_entries_removed,
                timeout_ms,
                false,
            )
        }?;

        Ok(num_entries_removed)
    }
}

impl Drop for CompletionPort {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) }
            .with_context(|| format!("could not close i/o completion port {:?}", self.0))
            .log_if_err();
    }
}

// Like AsRawHandle, but specifically for windows-rs' HANDLE type
pub trait AsWin32Handle {
    fn as_win32_handle(&self) -> HANDLE;
}

impl AsWin32Handle for CompletionPort {
    fn as_win32_handle(&self) -> HANDLE {
        self.0
    }
}

pub trait AsWin32Socket {
    fn as_win32_socket(&self) -> SOCKET;
}

impl AsWin32Socket for UnixDomainSocket {
    fn as_win32_socket(&self) -> SOCKET {
        self.0
    }
}

pub struct UnixStreamSink {
    iocp_handle: HANDLE,
    thread_handle: Option<JoinHandle<()>>,
}

impl UnixStreamSink {
    const MAX_COMPLETION_EVENTS: usize = 8;
    const BUFFER_POOL_PRUNE_INTERVAL: time::Duration = time::Duration::from_secs(600);
    const BUFFER_SIZE: usize = 32768;

    // Currently, tokens/keys are just the values of the corresponding SOCKETs, which is why the value
    // below (INVALID_SOCKET) should work as a special key that won't interfere with others.
    const STOP_PACKET_KEY: usize = INVALID_SOCKET.0;

    pub fn new(
        socket_path: &Path,
        mut callback: impl FnMut(&[u8], u32) + Send + 'static,
    ) -> anyhow::Result<Self> {
        let listener = UnixListener::bind(socket_path).context("could not bind listener")?;
        let listener_key = listener.token();

        let port = CompletionPort::new(2).context("could not create iocp")?;
        port.associate_handle(listener.socket.to_handle(), listener_key)
            .context("could not associate listener with iocp")?;

        let iocp_handle = port.as_win32_handle();

        let thread_handle = thread::spawn(move || {
            debug!("entering unix stream sink thread");

            move || -> anyhow::Result<()> {
                let mut entries = vec![OVERLAPPED_ENTRY::default(); Self::MAX_COMPLETION_EVENTS];
                let mut buffer_pool = VecDeque::<Vec<u8>>::new();
                let mut streams_queue = VecDeque::<(usize, Box<UnixStream>)>::new();
                let mut last_buffer_pool_prune = time::Instant::now();

                // Queue up our first accept I/O operation.
                let stream = Box::new(
                    unsafe { listener.accept_overlapped() }.context("could not accept stream")?,
                );
                port.associate_handle(stream.socket.to_handle(), stream.token())
                    .context("could not associate stream with iocp")?;
                streams_queue.push_back((stream.token(), stream));

                let mut should_cleanup = false;

                loop {
                    if last_buffer_pool_prune.elapsed() > Self::BUFFER_POOL_PRUNE_INTERVAL {
                        debug!("pruning buffer pool for unix stream sink");
                        buffer_pool.truncate(1);
                        last_buffer_pool_prune = time::Instant::now();
                    }

                    // This will block until an I/O operation has completed
                    let num_removed = port
                        .poll_many(None, &mut entries)
                        .context("could not poll with iocp")?;

                    for entry in entries[..num_removed as usize].iter() {
                        if entry.lpCompletionKey == listener_key {
                            // Stream has been accepted; ready to read
                            let stream =
                                &mut streams_queue.back_mut().context("could not get stream")?.1;

                            // Attempt to retrieve a buffer from the bufferpool
                            let outputbuffer = buffer_pool.pop_front().unwrap_or_else(|| {
                                debug!("creating new buffer for unix stream sink");
                                vec![0u8; Self::BUFFER_SIZE]
                            });
                            unsafe { stream.read_overlapped(outputbuffer) }
                                .context("could not read with stream")?;

                            // Queue up a new accept I/O operation.
                            let stream = Box::new(
                                unsafe { listener.accept_overlapped() }
                                    .context("could not accept stream")?,
                            );
                            port.associate_handle(stream.socket.to_handle(), stream.token())
                                .context("could not associate stream with iocp")?;
                            streams_queue.push_back((stream.token(), stream));
                        } else if entry.lpCompletionKey != Self::STOP_PACKET_KEY {
                            // Stream has been read; ready to process
                            let position = streams_queue
                                .iter()
                                .position(|(token, _)| *token == entry.lpCompletionKey)
                                .context("could not find stream")?;
                            let mut stream = streams_queue
                                .remove(position)
                                .context("could not remove stream from queue")?
                                .1;
                            let outputbuffer = stream
                                .take_overlapped_buffer()
                                .context("unix stream's buffer is None")?;

                            callback(&outputbuffer, entry.dwNumberOfBytesTransferred);

                            // We don't need this stream anymore, so place its buffer into the pool
                            buffer_pool.push_back(outputbuffer);
                        } else {
                            // Stop packet has been sent; cleanup and exit the thread
                            should_cleanup = true;
                        }
                    }

                    if should_cleanup {
                        Self::cleanup(listener, listener_key, port, entries, streams_queue)?;
                        break;
                    }
                }

                Ok(())
            }()
            .log_if_err();

            debug!("exiting unix stream sink thread");
        });

        Ok(Self {
            iocp_handle,
            thread_handle: Some(thread_handle),
        })
    }

    fn cleanup(
        listener: UnixListener,
        listener_key: usize,
        port: CompletionPort,
        mut entries: Vec<OVERLAPPED_ENTRY>,
        mut streams_queue: VecDeque<(usize, Box<UnixStream>)>,
    ) -> anyhow::Result<()> {
        // Cancel any pending I/O operations on the listener
        let listener_handle = listener.socket.to_handle();
        unsafe { CancelIoEx(listener_handle, None) }
            .with_context(|| format!("could not cancel i/o for listener {listener_handle:?}"))
            .log_if_err();

        // Cancel any pending I/O operations on each stream
        // NOTE: A stream may not have any pending I/O operations if it is still in
        // the accept stage, and CancelIoEx will return an error in those cases.
        for (_, stream) in streams_queue.iter() {
            let stream_handle = stream.socket.to_handle();
            unsafe { CancelIoEx(stream_handle, None) }
                .with_context(|| format!("could not cancel i/o for stream {stream_handle:?}"))
                .log_if_err();
        }

        // MSDN states that we must wait for I/O operations to complete (even if canceled)
        // before dropping OVERLAPPED structs to avoid use-after-free, so we'll wait below.
        while !streams_queue.is_empty() {
            // NOTE: poll_many() should return an error after the timeout.
            let timeout = time::Duration::from_secs(1);
            let num_removed = port
                .poll_many(Some(timeout), &mut entries)
                .context("could not poll with iocp")?;

            for entry in entries[..num_removed as usize].iter() {
                if entry.lpCompletionKey == listener_key {
                    let _ = streams_queue.pop_back();
                } else {
                    let position = streams_queue
                        .iter()
                        .position(|(token, _)| *token == entry.lpCompletionKey)
                        .context("could not find completion key")?;
                    let _ = streams_queue.remove(position);
                }
            }
        }

        Ok(())
    }
}

impl Drop for UnixStreamSink {
    fn drop(&mut self) {
        let post_res =
            unsafe { PostQueuedCompletionStatus(self.iocp_handle, 0, Self::STOP_PACKET_KEY, None) };

        match post_res {
            Ok(()) => match self.thread_handle.take() {
                Some(handle) => {
                    if let Err(err) = handle.join() {
                        error!("could not join unix stream sink thread handle: {err:?}");
                    }
                }
                None => error!("could not take unix stream sink thread handle"),
            },
            Err(err) => error!(
                "could not post stop packet to iocp {:?} for unix stream sink: {err:#}",
                self.iocp_handle
            ),
        }
    }
}

pub fn write_to_unix_socket(socket_path: &Path, message: &mut [u8]) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(socket_path).context("could not connect stream")?;
    stream
        .write_all(message)
        .context("could not write all with stream")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{UnixListener, UnixStream, shutdown_socket, timeout_millis};
    use std::io::{self, Read};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static NEXT_SOCKET_PATH: AtomicU64 = AtomicU64::new(0);

    struct TestSocketPath(PathBuf);

    impl TestSocketPath {
        fn new(name: &str) -> io::Result<Self> {
            let id = NEXT_SOCKET_PATH.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "tacky-borders-{name}-{}-{id}.sock",
                std::process::id()
            ));
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            Ok(Self(path))
        }
    }

    impl Drop for TestSocketPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn timeout_millis_rejects_zero() {
        let error = timeout_millis(Duration::ZERO).expect_err("zero timeout should be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn timeout_millis_rounds_up_to_nonzero_milliseconds() -> io::Result<()> {
        assert_eq!(timeout_millis(Duration::from_nanos(1))?, 1);
        assert_eq!(timeout_millis(Duration::from_millis(1))?, 1);
        assert_eq!(timeout_millis(Duration::from_micros(1_001))?, 2);

        Ok(())
    }

    #[test]
    fn timeout_millis_accepts_maximum_windows_value() -> io::Result<()> {
        assert_eq!(
            timeout_millis(Duration::from_millis(u32::MAX.into()))?,
            u32::MAX
        );

        Ok(())
    }

    #[test]
    fn timeout_millis_rejects_overflow() {
        let error = timeout_millis(Duration::from_millis(u64::from(u32::MAX) + 1))
            .expect_err("overflowing timeout should be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn nonblocking_listener_reports_would_block() -> io::Result<()> {
        let socket_path = TestSocketPath::new("nonblocking")?;
        let listener = UnixListener::bind(&socket_path.0)?;
        listener.set_nonblocking(true)?;

        let error = match listener.accept() {
            Ok(_) => panic!("accept without a pending client should not succeed"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        Ok(())
    }

    #[test]
    fn shutdown_socket_releases_blocked_read() -> io::Result<()> {
        let socket_path = TestSocketPath::new("shutdown")?;
        let listener = UnixListener::bind(&socket_path.0)?;
        let _client = UnixStream::connect(&socket_path.0)?;
        let mut server_stream = listener.accept()?;
        server_stream.set_read_timeout(Duration::from_secs(2))?;
        let socket_id = server_stream.socket_id();
        let reader = std::thread::spawn(move || {
            let mut buffer = [0u8; 1];
            server_stream.read(&mut buffer)
        });

        shutdown_socket(socket_id)?;
        let result = reader.join().expect("blocked reader panicked");

        assert!(matches!(result, Ok(0) | Err(_)));
        Ok(())
    }
}
