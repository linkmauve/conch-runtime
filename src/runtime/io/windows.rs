//! Defines interfaces and methods for doing IO operations on Windows HANDLEs.

use kernel32;
use winapi;

use std::fmt;
use std::fs::File;
use std::io::{Error, ErrorKind, Read, Result, Write};
use std::num::Zero;
use std::ops::{Deref, DerefMut};
use std::os::raw::c_void as HANDLE;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, RawHandle};
use std::process::Stdio;
use std::ptr;
use std::ptr::Unique as StdUnique;
use super::FileDesc;

// A Debug wrapper around `std::ptr::Unique`
struct Unique<T>(StdUnique<T>);

impl<T> Deref for Unique<T> {
    type Target = StdUnique<T>;
    fn deref(&self) -> &Self::Target { &self.0 }
}

impl<T> DerefMut for Unique<T> {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.0 }
}

impl fmt::Debug for Unique<HANDLE> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "{:p}", *self.0)
    }
}

/// A wrapper around an owned Windows HANDLE. The wrapper
/// allows reading from or write to the HANDLE, and will
/// close it once it goes out of scope.
#[derive(Debug)]
pub struct RawIo {
    /// The underlying HANDLE.
    handle: Unique<HANDLE>,
    /// Indicates whether the HANDLE has been extracted and
    /// transferred ownership or whether we should close it.
    must_close: bool,
}

impl Into<Stdio> for RawIo {
    fn into(self) -> Stdio {
        unsafe { FromRawHandle::from_raw_handle(self.into_inner()) }
    }
}

impl FromRawHandle for FileDesc {
    unsafe fn from_raw_handle(handle: RawHandle) -> Self {
        Self::new(handle)
    }
}

impl AsRawHandle for FileDesc {
    fn as_raw_handle(&self) -> RawHandle { self.inner().inner() }
}

impl IntoRawHandle for FileDesc {
    fn into_raw_handle(self) -> RawHandle { unsafe { self.into_inner().into_inner() } }
}

impl From<File> for FileDesc {
    fn from(file: File) -> Self {
        unsafe { FromRawHandle::from_raw_handle(file.into_raw_handle()) }
    }
}

impl RawIo {
    /// Takes ownership of and wraps an OS file HANDLE.
    pub unsafe fn new(handle: RawHandle) -> Self {
        RawIo {
            handle: Unique(StdUnique::new(handle)),
            must_close: true,
        }
    }

    /// Unwraps the underlying HANDLE and transfers ownership to the caller.
    pub unsafe fn into_inner(mut self) -> RawHandle {
        // Make sure our desctructor doesn't actually close
        // the handle we just transfered to the caller.
        self.must_close = false;
        **self.handle
    }

    /// Returns the underlying HANDLE without transfering ownership.
    pub fn inner(&self) -> RawHandle { **self.handle }

    /// Duplicates the underlying HANDLE.
    // Adapted from rust: libstd/sys/windows/handle.rs
    pub fn duplicate(&self) -> Result<Self> {
        unsafe {
            let mut ret = winapi::INVALID_HANDLE_VALUE;
            try!(cvt({
                let cur_proc = kernel32::GetCurrentProcess();

                kernel32::DuplicateHandle(cur_proc,
                                          **self.handle,
                                          cur_proc,
                                          &mut ret,
                                          0 as winapi::DWORD,
                                          winapi::FALSE,
                                          winapi::DUPLICATE_SAME_ACCESS)
            }));
            Ok(RawIo::new(ret))
        }
    }

    /// Reads from the underlying HANDLE.
    // Taken from rust: libstd/sys/windows/handle.rs
    pub fn read_inner(&self, buf: &mut [u8]) -> Result<usize> {
        let mut read = 0;
        let res = cvt(unsafe {
            kernel32::ReadFile(**self.handle,
                               buf.as_ptr() as winapi::LPVOID,
                               buf.len() as winapi::DWORD,
                               &mut read,
                               ptr::null_mut())
        });

        match res {
            Ok(_) => Ok(read as usize),

            // The special treatment of BrokenPipe is to deal with Windows
            // pipe semantics, which yields this error when *reading* from
            // a pipe after the other end has closed; we interpret that as
            // EOF on the pipe.
            Err(ref e) if e.kind() == ErrorKind::BrokenPipe => Ok(0),

            Err(e) => Err(e)
        }
    }

    /// Writes to the underlying HANDLE.
    // Taken from rust: libstd/sys/windows/handle.rs
    pub fn write_inner(&self, buf: &[u8]) -> Result<usize> {
        let mut amt = 0;
        try!(cvt(unsafe {
            kernel32::WriteFile(**self.handle,
                                buf.as_ptr() as winapi::LPVOID,
                                buf.len() as winapi::DWORD,
                                &mut amt,
                                ptr::null_mut())
        }));
        Ok(amt as usize)
    }
}

impl Read for RawIo {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.read_inner(buf)
    }
}

impl Write for RawIo {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        self.write_inner(buf)
    }

    fn flush(&mut self) -> Result<()> { Ok(()) }
}

impl Drop for RawIo {
    // Adapted from rust: src/libstd/sys/windows/handle.rs
    fn drop(&mut self) {
        if self.must_close {
            unsafe { let _ = kernel32::CloseHandle(**self.handle); }
        }
    }
}

// Taken from rust: src/libstd/sys/windows/mod.rs
fn cvt<I: PartialEq + Zero>(i: I) -> Result<I> {
    if i == I::zero() {
        Err(Error::last_os_error())
    } else {
        Ok(i)
    }
}

/// Creates and returns a `(reader, writer)` pipe pair.
pub fn pipe() -> Result<(RawIo, RawIo)> {
    use std::ptr;
    unsafe {
        let mut reader = winapi::INVALID_HANDLE_VALUE;
        let mut writer = winapi::INVALID_HANDLE_VALUE;
        try!(cvt(kernel32::CreatePipe(&mut reader as winapi::PHANDLE,
                                      &mut writer as winapi::PHANDLE,
                                      ptr::null_mut(),
                                      0)));

        Ok((RawIo::new(reader), RawIo::new(writer)))
    }
}

/// Duplicates file HANDLES for (stdin, stdout, stderr) and returns them in that order.
pub fn dup_stdio() -> Result<(RawIo, RawIo, RawIo)> {
    fn dup_handle(handle: winapi::DWORD) -> Result<RawIo> {
        unsafe {
            let current_process = kernel32::GetCurrentProcess();
            let mut new_handle = winapi::INVALID_HANDLE_VALUE;

            try!(cvt(kernel32::DuplicateHandle(current_process,
                                               kernel32::GetStdHandle(handle),
                                               current_process,
                                               &mut new_handle,
                                               0 as winapi::DWORD,
                                               winapi::FALSE,
                                               winapi::DUPLICATE_SAME_ACCESS)));

            Ok(RawIo::new(new_handle))
        }
    }

    Ok((
        try!(dup_handle(winapi::STD_INPUT_HANDLE)),
        try!(dup_handle(winapi::STD_OUTPUT_HANDLE)),
        try!(dup_handle(winapi::STD_ERROR_HANDLE))
    ))
}