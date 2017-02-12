use env::SubEnvironment;
use futures::{Async, Future, Sink, Stream};
use futures::stream::Fuse;
use futures::sync::mpsc::{channel, Receiver};
use futures_cpupool::{CpuFuture, CpuPool};
use io::FileDesc;
use mio::would_block;
use std::io::{BufRead, BufReader, Error as IoError, ErrorKind, Read, Result, Write};
use std::fmt;
use tokio_core::reactor::Remote;
use void::Void;

/// An environment for performing async operations on `FileDesc` handles.
pub trait AsyncIoEnvironment {
    /// An async/futures-aware `Read` adapter around a `FileDesc`.
    type Read: Read;
    /// An future that represents writing data into a `FileDesc`.
    // FIXME: Unfortunately we cannot support resolving/unwrapping futures/adapters
    // to the `FileDesc` since the Unix extension cannot (currently) support it.
    // Thus having some impls resolve to the FileDesc and others not could cause
    // weird deadlock issues (e.g. caller unaware the handle isn't getting dropped
    // automatically).
    type WriteAll: Future<Item = (), Error = IoError>;

    /// Creates a futures-aware adapter to read data from a `FileDesc` asynchronously.
    fn read_async(&mut self, fd: FileDesc) -> Self::Read;

    /// Creates a future for writing data into a `FileDesc`.
    fn write_all(&mut self, fd: FileDesc, data: Vec<u8>) -> Self::WriteAll;
}

impl<'a, T: ?Sized + AsyncIoEnvironment> AsyncIoEnvironment for &'a mut T {
    type Read = T::Read;
    type WriteAll = T::WriteAll;

    fn read_async(&mut self, fd: FileDesc) -> Self::Read {
        (**self).read_async(fd)
    }

    fn write_all(&mut self, fd: FileDesc, data: Vec<u8>) -> Self::WriteAll {
        (**self).write_all(fd, data)
    }
}

/// A platform specific adapter for async reads from a `FileDesc`.
///
/// Note that this type is also "futures aware" meaning that it is both
/// (a) nonblocking and (b) will panic if used off of a future's task.
#[cfg(unix)]
pub type PlatformSpecificRead = ::os::unix::env::ReadAsync;

/// A platform specific future that will write some data to a `FileDesc`.
///
/// Created by the `EventedAsyncIoEnv::write_all` method.
#[cfg(unix)]
pub type PlatformSpecificWriteAll = ::os::unix::env::WriteAll;

/// A platform specific adapter for async reads from a `FileDesc`.
///
/// Note that this type is also "futures aware" meaning that it is both
/// (a) nonblocking and (b) will panic if used off of a future's task.
#[cfg(not(unix))]
pub type PlatformSpecificRead = ReadAsync;

/// A platform specific future that will write some data to a `FileDesc`.
///
/// Created by the `EventedAsyncIoEnv::write_all` method.
#[cfg(not(unix))]
pub type PlatformSpecificWriteAll = CpuFuture<(), IoError>;

/// A platform specific environment efficiently using a `tokio` event loop,
/// if the current platform supports efficient async IO, or a `ThreadPoolAsyncIoEnv`
/// otherwise.
#[derive(Debug, Clone)]
pub struct PlatformSpecificAsyncIoEnv {
    #[cfg(unix)]
    inner: ::os::unix::env::EventedAsyncIoEnv,
    #[cfg(not(unix))]
    inner: ThreadPoolAsyncIoEnv,
}

impl PlatformSpecificAsyncIoEnv {
    /// Creates a new platform specific environment using a `tokio` event loop,
    /// if such an environment is supported on the current platform.
    ///
    /// Otherwise, we will fall back to to a `ThreadPoolAsyncIoEnv` with the
    /// specified number of threads. If `None` is specified, we'll use one
    /// thread per CPU.
    pub fn new(remote: Remote, fallback_num_threads: Option<usize>) -> Self {
        #[cfg(unix)]
        let get_inner = |remote: Remote, _: Option<usize>| {
            ::os::unix::env::EventedAsyncIoEnv::new(remote)
        };

        #[cfg(not(unix))]
        let get_inner = |_: Remote, num_threads| {
            num_threads.map_or_else(
                || ThreadPoolAsyncIoEnv::new_num_cpus(),
                ThreadPoolAsyncIoEnv::new
            )
        };

        PlatformSpecificAsyncIoEnv {
            inner: get_inner(remote, fallback_num_threads),
        }
    }
}

impl SubEnvironment for PlatformSpecificAsyncIoEnv {
    fn sub_env(&self) -> Self {
        self.clone()
    }
}

impl AsyncIoEnvironment for PlatformSpecificAsyncIoEnv {
    type Read = PlatformSpecificRead;
    type WriteAll = PlatformSpecificWriteAll;

    fn read_async(&mut self, fd: FileDesc) -> Self::Read {
        self.inner.read_async(fd)
    }

    fn write_all(&mut self, fd: FileDesc, data: Vec<u8>) -> Self::WriteAll {
        self.inner.write_all(fd, data)
    }
}

/// An `AsyncIoEnvironment` implementation that uses a threadpool for doing
/// reads and writes on **synchronous** `FileDesc` handles.
///
/// This is a pretty costly implementation which may be required on systems
/// that do not support asynchronous read/write operations (easily or at all).
/// If running on a system that supports more efficient async operations, it is
/// strongly encouraged to use an alternative implementation.
///
/// > **Note**: Caller should ensure that any `FileDesc` handles passed into
/// > this environment are **not** configured for asynchronous operations,
/// > otherwise operations will fail with a `WouldBlock` error. This is done
/// > to avoid burning CPU cycles while spinning on read/write operations.
#[derive(Clone)]
pub struct ThreadPoolAsyncIoEnv {
    pool: CpuPool, // CpuPool uses an internal Arc, so clones should be shallow/"cheap"
}

impl SubEnvironment for ThreadPoolAsyncIoEnv {
    fn sub_env(&self) -> Self {
        self.clone()
    }
}

impl ThreadPoolAsyncIoEnv {
    /// Creates a new thread pool with `size` worker threads associated with it.
    pub fn new(size: usize) -> Self {
        ThreadPoolAsyncIoEnv {
            pool: CpuPool::new(size),
        }
    }

    /// Creates a new thread pool with a number of workers equal to the number
    /// of CPUs on the host.
    pub fn new_num_cpus() -> Self {
        ThreadPoolAsyncIoEnv {
            pool: CpuPool::new_num_cpus(),
        }
    }
}

impl fmt::Debug for ThreadPoolAsyncIoEnv {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("ThreadPoolAsyncIoEnv")
            .field("pool", &"..")
            .finish()
    }
}

/// An adapter for async reads from a `FileDesc`.
///
/// Note that this type is also "futures aware" meaning that it is both
/// (a) nonblocking and (b) will panic if used off of a future's task.
pub struct ReadAsync {
    /// A reference to the CpuFuture to avoid early cancellation.
    _cpu_future: CpuFuture<(), Void>,
    /// A receiver for fetching additional buffers of data.
    rx: Fuse<Receiver<Vec<u8>>>,
    /// The current buffer we are reading from.
    buf: Option<Vec<u8>>,
}

impl fmt::Debug for ReadAsync {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("ReadAsync")
            .field("buf", &self.buf)
            .finish()
    }
}

impl Read for ReadAsync {
    fn read(&mut self, mut buf: &mut [u8]) -> Result<usize> {
        loop {
            match self.buf {
                None => {},
                Some(ref data) if data.is_empty() => {},

                Some(ref mut data) => {
                    // Safety check so we don't panic when draining
                    let n = ::std::cmp::min(data.len(), try!(buf.write(data)));
                    let drain = data.drain(0..n);
                    drop(drain);

                    return Ok(n);
                },
            }

            match self.rx.poll() {
                Ok(Async::Ready(maybe_buf)) => {
                    // If we got a new buffer, we should try reading from it
                    // and loop around. But if the stream is exhausted, signal
                    // that nothing more can be read.
                    self.buf = maybe_buf;
                    if self.buf.is_none() {
                        return Ok(0);
                    }
                },

                // New buffer not yet ready, we'll get unparked
                // when it becomes ready for us to consume
                Ok(Async::NotReady) => return Err(would_block()),

                // Buffer stream went away, not much we can do here
                // besides signal no more data
                Err(()) => {
                    self.buf = None;
                    return Ok(0);
                },
            };
        }
    }
}

impl AsyncIoEnvironment for ThreadPoolAsyncIoEnv {
    type Read = ReadAsync;
    type WriteAll = CpuFuture<(), IoError>;

    fn read_async(&mut self, fd: FileDesc) -> Self::Read {
        let (mut tx, rx) = channel(0); // NB: we have a guaranteed slot for all senders

        let cpu = self.pool.spawn_fn(move || {
            let mut buf_reader = BufReader::new(fd);

            loop {
                let num_consumed = match buf_reader.fill_buf() {
                    Ok(filled_buf) => {
                        if filled_buf.is_empty() {
                            break;
                        }

                        // FIXME: might be more efficient to pass around the same vec
                        // via two channels than allocating new copies each time?
                        let buf = Vec::from(filled_buf);
                        let len = buf.len();

                        match tx.send(buf).wait() {
                            Ok(t) => tx = t,
                            Err(_) => break,
                        }

                        len
                    },

                    // We explicitly do not handle WouldBlock errors here,
                    // and propagate them to the caller. We expect blocking
                    // descriptors to be provided to us, so if we constantly
                    // loop on WouldBlock errors we would burn a lot of CPU
                    // so it's best to return an explicit error.
                    Err(ref e) if e.kind() == ErrorKind::Interrupted => 0,
                    Err(_) => break,
                };

                buf_reader.consume(num_consumed);
            }

            Ok(())
        });

        ReadAsync {
            _cpu_future: cpu,
            rx: rx.fuse(),
            buf: None,
        }
    }

    fn write_all(&mut self, mut fd: FileDesc, data: Vec<u8>) -> Self::WriteAll {
        // We could use `tokio` IO adapters here, however, it would cause
        // problems if the file descriptor was set to nonblocking mode, since
        // we aren't registering it with any event loop and no one will wake
        // us up ever. By doing the operation ourselves at worst we'll end up
        // bailing out at the first WouldBlock error, which can at least be
        // noticed by a caller, instead of silently deadlocking.
        self.pool.spawn_fn(move || {
            try!(fd.write_all(&data));
            fd.flush()
        })
    }
}
