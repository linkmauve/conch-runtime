use env::SubEnvironment;
use futures::{Async, Future, Poll, Sink, Stream};
use futures::stream::Fuse;
use futures::sync::mpsc::{channel, Receiver};
use futures_cpupool::{CpuFuture, CpuPool};
use env::{AsyncIoEnvironment, AsyncIoEnvironment2};
use io::FileDesc;
use mio::would_block;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use tokio_io::AsyncRead;
use void::Void;

/// An `AsyncIoEnvironment` implementation that uses a threadpool for doing
/// reads and writes on **synchronous/blocking** `FileDesc` handles.
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
#[derive(Debug, Clone)]
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

/// An adapter for async reads from a `FileDesc`.
///
/// Note that this type is also "futures aware" meaning that it is both
/// (a) nonblocking and (b) will panic if used off of a future's task.
#[must_use]
#[derive(Debug)]
pub struct ThreadPoolReadAsync {
    /// A reference to the CpuFuture to avoid early cancellation.
    _cpu_future: CpuFuture<(), Void>,
    /// A receiver for fetching additional buffers of data.
    rx: Fuse<Receiver<Vec<u8>>>,
    /// The current buffer we are reading from.
    buf: Option<Vec<u8>>,
}

impl AsyncRead for ThreadPoolReadAsync {}
impl Read for ThreadPoolReadAsync {
    fn read(&mut self, mut buf: &mut [u8]) -> io::Result<usize> {
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

/// An future that represents writing data into a file handle.
#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub struct ThreadPoolWriteAll(CpuFuture<(), io::Error>);

impl AsyncIoEnvironment for ThreadPoolAsyncIoEnv {
    type IoHandle = FileDesc;
    type Read = ThreadPoolReadAsync;
    type WriteAll = ThreadPoolWriteAll;

    fn read_async(&mut self, fd: FileDesc) -> Self::Read {
        let _ = try_set_blocking(&fd); // Best effort here...

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
                    // descriptors to be provided to us (NB we can't enforce
                    // this after the fact on Windows), so if we constantly
                    // loop on WouldBlock errors we would burn a lot of CPU
                    // so it's best to return an explicit error.
                    Err(ref e) if e.kind() == ErrorKind::Interrupted => 0,
                    Err(_) => break,
                };

                buf_reader.consume(num_consumed);
            }

            Ok(())
        });

        ThreadPoolReadAsync {
            _cpu_future: cpu,
            rx: rx.fuse(),
            buf: None,
        }
    }

    fn write_all(&mut self, mut fd: FileDesc, data: Vec<u8>) -> Self::WriteAll {
        let _ = try_set_blocking(&fd); // Best effort here...

        // We could use `tokio` IO adapters here, however, it would cause
        // problems if the file descriptor was set to nonblocking mode, since
        // we aren't registering it with any event loop and no one will wake
        // us up ever. By doing the operation ourselves at worst we'll end up
        // bailing out at the first WouldBlock error, which can at least be
        // noticed by a caller, instead of silently deadlocking.
        ThreadPoolWriteAll(self.pool.spawn_fn(move || {
            try!(fd.write_all(&data));
            fd.flush()
        }))
    }

    fn write_all_best_effort(&mut self, fd: FileDesc, data: Vec<u8>) {
        AsyncIoEnvironment::write_all(self, fd, data).0.forget();
    }
}

impl AsyncIoEnvironment2 for ThreadPoolAsyncIoEnv {
    type IoHandle = FileDesc;
    type Read = ThreadPoolReadAsync;
    type WriteAll = ThreadPoolWriteAll;

    fn read_async(&mut self, fd: FileDesc) -> io::Result<Self::Read> {
        Ok(AsyncIoEnvironment::read_async(self, fd))
    }

    fn write_all(&mut self, fd: FileDesc, data: Vec<u8>) -> io::Result<Self::WriteAll> {
        Ok(AsyncIoEnvironment::write_all(self, fd, data))
    }

    fn write_all_best_effort(&mut self, fd: FileDesc, data: Vec<u8>) {
        AsyncIoEnvironment::write_all_best_effort(self, fd, data)
    }
}

impl Future for ThreadPoolWriteAll {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll()
    }
}

#[cfg(unix)]
fn try_set_blocking(fd: &FileDesc) -> io::Result<()> {
    use os::unix::io::FileDescExt;

    fd.set_nonblock(false)
}

#[cfg(not(unix))]
fn try_set_blocking(_fd: &FileDesc) -> io::Result<()> {
    Ok(())
}