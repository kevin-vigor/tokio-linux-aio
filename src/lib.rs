// ===============================================================================================
// Copyright (c) 2018 Hans-Martin Will
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.
// ===============================================================================================

extern crate aio_bindings;
extern crate futures;
extern crate libc;
extern crate mio;
extern crate tokio;

use std::cell;
use std::io;
use std::mem;
use std::ops;
use std::ops::{Deref, DerefMut};
use std::ptr;
use std::rc;
use std::sync;

use std::os::unix::io::RawFd;

use libc::{c_long, c_uint, close, eventfd, read, write, EAGAIN, O_CLOEXEC};

use tokio::executor;
use tokio::reactor;
use futures::Future;

// Relevant symbols from the native bindings exposed via aio-bindings
use aio_bindings::{aio_context_t, io_event, iocb, syscall, timespec, __NR_io_destroy,
                   __NR_io_getevents, __NR_io_setup, __NR_io_submit, EFD_NONBLOCK, EFD_SEMAPHORE,
                   IOCB_CMD_PREAD, IOCB_CMD_PWRITE, IOCB_FLAG_RESFD};

// -----------------------------------------------------------------------------------------------
// Inline functions that wrap the kernel calls for the entry points corresponding to Liux
// AIO functions
// -----------------------------------------------------------------------------------------------

// Initialize an AIO context for a given submission queue size within the kernel.
//
// See [io_setup(7)](http://man7.org/linux/man-pages/man2/io_setup.2.html) for details.
#[inline(always)]
unsafe fn io_setup(nr: c_long, ctxp: *mut aio_context_t) -> c_long {
    syscall(__NR_io_setup as c_long, nr, ctxp)
}

// Destroy an AIO context.
//
// See [io_destroy(7)](http://man7.org/linux/man-pages/man2/io_destroy.2.html) for details.
#[inline(always)]
unsafe fn io_destroy(ctx: aio_context_t) -> c_long {
    syscall(__NR_io_destroy as c_long, ctx)
}

// Submit a batch of IO operations.
//
// See [io_sumit(7)](http://man7.org/linux/man-pages/man2/io_submit.2.html) for details.
#[inline(always)]
unsafe fn io_submit(ctx: aio_context_t, nr: c_long, iocbpp: *mut *mut iocb) -> c_long {
    syscall(__NR_io_submit as c_long, ctx, nr, iocbpp)
}

// Retrieve completion events for previously submitted IO requests.
//
// See [io_getevents(7)](http://man7.org/linux/man-pages/man2/io_getevents.2.html) for details.
#[inline(always)]
unsafe fn io_getevents(
    ctx: aio_context_t,
    min_nr: c_long,
    max_nr: c_long,
    events: *mut io_event,
    timeout: *mut timespec,
) -> c_long {
    syscall(
        __NR_io_getevents as c_long,
        ctx,
        min_nr,
        max_nr,
        events,
        timeout,
    )
}

// -----------------------------------------------------------------------------------------------
// EventFd Implementation
// -----------------------------------------------------------------------------------------------

struct EventFdInner {
    fd: RawFd,
}

impl Drop for EventFdInner {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe { close(self.fd) };
        }
    }
}

impl mio::Evented for EventFdInner {
    fn register(
        &self,
        poll: &mio::Poll,
        token: mio::Token,
        interest: mio::Ready,
        opts: mio::PollOpt,
    ) -> io::Result<()> {
        mio::unix::EventedFd(&self.fd).register(poll, token, interest, opts)
    }

    fn reregister(
        &self,
        poll: &mio::Poll,
        token: mio::Token,
        interest: mio::Ready,
        opts: mio::PollOpt,
    ) -> io::Result<()> {
        mio::unix::EventedFd(&self.fd).reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &mio::Poll) -> io::Result<()> {
        mio::unix::EventedFd(&self.fd).deregister(poll)
    }
}

struct EventFd {
    evented: reactor::PollEvented<EventFdInner>,
}

impl EventFd {
    fn create(init: usize, semaphore: bool) -> Result<EventFd, io::Error> {
        let flags = if semaphore {
            O_CLOEXEC | EFD_NONBLOCK as i32 | EFD_SEMAPHORE as i32
        } else {
            O_CLOEXEC | EFD_NONBLOCK as i32
        };

        let fd = unsafe { eventfd(init as c_uint, flags) };

        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            reactor::PollEvented::new(EventFdInner { fd }, &reactor::Handle::default())
                .map(|evented| EventFd { evented })
        }
    }

    fn read(&mut self) -> Result<futures::Async<u64>, io::Error> {
        match self.evented.poll_read() {
            futures::Async::NotReady => return Ok(futures::Async::NotReady),
            _ => (),
        };

        let fd = self.evented.get_ref().fd;
        let mut result: u64 = 0;

        let rc = unsafe { read(fd, mem::transmute(&mut result), mem::size_of::<u64>()) };

        if rc < 0 {
            let error = io::Error::last_os_error();

            if error.raw_os_error().unwrap() != EAGAIN {
                // this is a regular eeror
                return Err(io::Error::last_os_error());
            } else {
                if let Err(err) = self.evented.need_read() {
                    return Err(err);
                } else {
                    return Ok(futures::Async::NotReady);
                }
            }
        } else {
            if rc as usize != mem::size_of::<u64>() {
                panic!(
                    "Writing to an eventfd should consume exactly {} bytes",
                    mem::size_of::<u64>()
                )
            }

            Ok(futures::Async::Ready(result as u64))
        }
    }

    fn add(&mut self, increment: u64) -> Result<(), io::Error> {
        let fd = { self.evented.get_ref().fd };

        let result = unsafe { write(fd, mem::transmute(&increment), mem::size_of::<u64>()) };

        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            if result as usize != mem::size_of_val(&increment) {
                panic!(
                    "Writing to an eventfd should consume exactly {} bytes",
                    mem::size_of::<u64>()
                )
            }

            Ok(())
        }
    }
}

impl futures::Future for EventFd {
    type Item = u64;
    type Error = io::Error;

    fn poll(&mut self) -> Result<futures::Async<Self::Item>, Self::Error> {
        self.read()
    }
}

#[cfg(test)]
mod tests {
    use tokio::executor::current_thread;
    use futures::future::lazy;
    use super::*;

    #[test]
    fn read_eventfd_standard() {
        let efd = EventFd::create(2, false).unwrap();
        let result = efd.wait();

        assert!(result.is_ok());
        assert!(result.unwrap() == 2);
    }

    #[test]
    fn read_eventfd_semaphore() {
        let efd = EventFd::create(2, true).unwrap();
        let result = efd.wait();

        assert!(result.is_ok());
        assert!(result.unwrap() == 1);
    }

    #[test]
    fn read_add_eventfd() {
        current_thread::run(|_| {
            let efd = EventFd::create(0, false).unwrap();
            let fd = efd.evented.get_ref().fd;

            // The execution context is setup, futures may be executed.
            current_thread::spawn(efd.map(|res| {
                assert!(res == 1);
            }).map_err(|_| {
                panic!("Error!!!");
            }));

            current_thread::spawn(lazy(move || {
                let increment: u64 = 1;

                let result =
                    unsafe { write(fd, mem::transmute(&increment), mem::size_of::<u64>()) };
                assert!(result as usize == mem::size_of::<u64>());
                Ok(())
            }));
        });
    }
}

// -----------------------------------------------------------------------------------------------
// Bindings for Linux AIO start here
// -----------------------------------------------------------------------------------------------

// Common interface in order to initialize an embedded iocb control block.
trait IocbSetup {
    fn setup(&mut self);
}

// Common data structures for futures return by `AioContext`.
struct AioBaseFuture<'a> {
    // reference to the `AioContext` that controls the submission queue for asynchronous I/O
    context: &'a AioContext,

    // the iocb control block that is used for queue submissions
    request: iocb,

    // state variable tracking if the I/O request associated with this instance has been submitted
    // to the kernel.
    submitted: bool,

    // place to capture the result of the I/O operation
    result: Option<Result<(), io::Error>>,
}

impl<'a> AioBaseFuture<'a> {
    fn poll(&mut self) -> Result<futures::Async<()>, io::Error> {
        if let Some(result) = self.result.take() {
            // procesing has completed
            return result.map(|_| futures::Async::Ready(()));
        }

        if !self.submitted {
            // See if we can secure a submission slot
            match self.context.capacity.borrow_mut().read() {
                Err(err) => return Err(err),
                Ok(futures::Async::NotReady) => return Ok(futures::Async::NotReady),
                Ok(futures::Async::Ready(_)) => (),
            }

            // submit the request
            let mut request_ptr_array: [*mut iocb; 1] = [&mut self.request as *mut iocb; 1];
            let result = unsafe {
                io_submit(
                    self.context.context,
                    1,
                    &mut request_ptr_array[0] as *mut *mut iocb,
                )
            };
            self.submitted = true;

            // if we have submission error, capture it as future result
            if result == -1 {
                return Err(io::Error::last_os_error());
            }

            // otherwise, let the future be triggered by availability of results and return not ready
            Ok(futures::Async::NotReady)
        } else {
            // See if we should look up completion events
            match self.context.completed.borrow_mut().read() {
                Err(err) => return Err(err),
                Ok(futures::Async::NotReady) => return Ok(futures::Async::NotReady),
                Ok(futures::Async::Ready(_)) => (),
            }

            // get completion events
            let mut events = self.context.completion_events.borrow_mut();
            events.clear();

            unsafe {
                let result = io_getevents(
                    self.context.context,
                    0 as c_long,
                    events.capacity() as c_long,
                    events.as_mut_ptr(),
                    ptr::null_mut::<timespec>(),
                );

                // adjust the vector size to the actual number of items returned
                if result >= 0 {
                    events.set_len(result as usize);
                } else {
                    return Err(io::Error::last_os_error());
                }
            };

            for ref event in events.iter() {
                let future: &mut AioBaseFuture = unsafe { mem::transmute(event.data) };
                let result = event.res;

                future.result = if result < 0 {
                    Some(Err(io::Error::from_raw_os_error(result as i32)))
                } else {
                    Some(Ok(()))
                };
            }

            // Release the kernel queue slots we just processed
            if let Err(err) = self.context.completed.borrow_mut().add(events.len() as u64) {
                return Err(err);
            }

            if let Some(result) = self.result.take() {
                // procesing has completed
                result.map(|_| futures::Async::Ready(()))
            } else {
                // otherwise, register this future on the completion fd and return not ready
                self.context
                    .completed
                    .borrow_mut()
                    .evented
                    .need_read()
                    .map(|_| futures::Async::NotReady)
            }
        }
    }
}

/// Future returned as result of submitting a read request via `AioContext::read`.
pub struct AioReadResultFuture<'a, ReadWriteHandle>
where
    ReadWriteHandle: ops::DerefMut<Target = [u8]>,
{
    // common AIO future state
    base: AioBaseFuture<'a>,

    // memory handle where data read from the underlying block device is being written to.
    buffer: ReadWriteHandle,
}

impl<'a, ReadWriteHandle> IocbSetup for AioReadResultFuture<'a, ReadWriteHandle>
where
    ReadWriteHandle: ops::DerefMut<Target = [u8]>,
{
    fn setup(&mut self) {
        unsafe {
            if self.base.request.aio_data == 0 {
                self.base.request.aio_data = mem::transmute(&mut self.base);
                self.base.request.aio_buf = mem::transmute(self.buffer.as_ptr());
            } else if self.base.request.aio_data != mem::transmute(&mut self.base) {
                panic!("Future was moved during I/O operation");
            }
        }
    }
}

impl<'a, ReadWriteHandle> futures::Future for AioReadResultFuture<'a, ReadWriteHandle>
where
    ReadWriteHandle: ops::DerefMut<Target = [u8]>,
{
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Result<futures::Async<Self::Item>, Self::Error> {
        self.setup();
        self.base.poll()
    }
}

/// Future returned as result of submitting a write request via `AioContext::write`.
pub struct AioWriteResultFuture<'a, ReadOnlyHandle>
where
    ReadOnlyHandle: ops::Deref<Target = [u8]>,
{
    // common AIO future state
    base: AioBaseFuture<'a>,

    // memory handle where data written to the underlying block device is being read from.
    buffer: ReadOnlyHandle,
}

impl<'a, ReadOnlyHandle> IocbSetup for AioWriteResultFuture<'a, ReadOnlyHandle>
where
    ReadOnlyHandle: ops::Deref<Target = [u8]>,
{
    fn setup(&mut self) {
        unsafe {
            if self.base.request.aio_data == 0 {
                self.base.request.aio_data = mem::transmute(&mut self.base);
                self.base.request.aio_buf = mem::transmute(self.buffer.as_ptr());
            } else if self.base.request.aio_data != mem::transmute(&mut self.base) {
                panic!("Future was moved during I/O operation");
            }
        }
    }
}

impl<'a, ReadOnlyHandle> futures::Future for AioWriteResultFuture<'a, ReadOnlyHandle>
where
    ReadOnlyHandle: ops::Deref<Target = [u8]>,
{
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Result<futures::Async<Self::Item>, Self::Error> {
        self.setup();
        self.base.poll()
    }
}

/// AioContext provides a submission queue for asycnronous I/O operations to
/// block devices within the Linux kernel.
pub struct AioContext {
    // the context handle for submitting AIO requests to the kernel
    context: aio_context_t,

    // event fd to signal that we can accept more I/O requests
    capacity: cell::RefCell<EventFd>,

    // event fd indicating that I/O requests have been completed
    completed: cell::RefCell<EventFd>,

    // vector of IO completion events; retrieved via io_getevents
    completion_events: cell::RefCell<Vec<io_event>>,
}

impl AioContext {
    /// Create a new AioContext that is driven by the provided event loop.
    ///
    /// # Params
    /// - nr: Number of submission slots fro IO requests
    pub fn new(nr: usize) -> Result<AioContext, io::Error> {
        let mut context: aio_context_t = 0;

        unsafe {
            if io_setup(nr as c_long, &mut context) != 0 {
                return Err(io::Error::last_os_error());
            }
        };

        Ok(AioContext {
            context,
            capacity: cell::RefCell::new(EventFd::create(nr, true)?),
            completed: cell::RefCell::new(EventFd::create(0, false)?),
            completion_events: cell::RefCell::new(Vec::new()),
        })
    }

    pub fn read<'a, ReadWriteHandle>(
        &'a self,
        fd: RawFd,
        offset: u64,
        buffer: ReadWriteHandle,
    ) -> AioReadResultFuture<'a, ReadWriteHandle>
    where
        ReadWriteHandle: ops::DerefMut<Target = [u8]>,
    {
        let len = buffer.len() as u64;

        // nothing really happens here until someone calls poll
        AioReadResultFuture {
            base: AioBaseFuture {
                context: self,
                request: self.init_iocb(IOCB_CMD_PREAD, fd, offset, len),
                submitted: false,
                result: None,
            },
            buffer,
        }
    }

    pub fn write<'a, ReadOnlyHandle>(
        &'a self,
        fd: RawFd,
        offset: u64,
        buffer: ReadOnlyHandle,
    ) -> AioWriteResultFuture<'a, ReadOnlyHandle>
    where
        ReadOnlyHandle: ops::Deref<Target = [u8]>,
    {
        let len = buffer.len() as u64;

        // nothing really happens here until someone calls poll
        AioWriteResultFuture {
            base: AioBaseFuture {
                context: self,
                request: self.init_iocb(IOCB_CMD_PWRITE, fd, offset, len),
                submitted: false,
                result: None,
            },
            buffer,
        }
    }

    fn init_iocb(&self, opcode: u32, fd: RawFd, offset: u64, len: u64) -> iocb {
        let mut result: iocb = unsafe { mem::zeroed() };

        result.aio_fildes = fd as u32;
        result.aio_offset = offset as i64;
        result.aio_nbytes = len;
        result.aio_lio_opcode = opcode as u16;
        result.aio_flags = IOCB_FLAG_RESFD;
        result.aio_resfd = self.completed.borrow().evented.get_ref().fd as u32;

        result
    }
}

impl Drop for AioContext {
    fn drop(&mut self) {
        let result = unsafe { io_destroy(self.context) };
        assert!(result == 0);
    }
}
