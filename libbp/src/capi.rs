//! C ABI over the adapter.
//!
//! Conventions, for every function: failure returns `-1` (or `NULL` for
//! `bp_socket`) with `errno` set. Nothing here may unwind into C, so each body
//! runs under `catch_unwind`; a panic becomes `-1` with `errno = EIO`.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;

use crate::adapter::BpaAdapter;
use crate::dtn7::{Dtn7Adapter, DEFAULT_URL};
use crate::eid::Eid;
use crate::error::{BpError, Result};

/// Opaque handle for one BP socket.
pub struct BpSocket {
    adapter: Box<dyn BpaAdapter>,
    bound: bool,
}

fn set_errno(errno: c_int) {
    // SAFETY: `__errno_location` always returns a valid pointer to this thread's errno.
    unsafe { *libc::__errno_location() = errno };
}

/// Runs `f`, converting both `Err` and panics into `fail` plus an errno.
fn ffi<T>(fail: T, f: impl FnOnce() -> Result<T>) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(value)) => value,
        Ok(Err(err)) => {
            set_errno(err.errno);
            fail
        }
        Err(_) => {
            set_errno(libc::EIO);
            fail
        }
    }
}

fn socket_mut<'a>(sock: *mut BpSocket) -> Result<&'a mut BpSocket> {
    // SAFETY: the caller promises `sock` came from `bp_socket` and is not yet closed.
    unsafe { sock.as_mut() }.ok_or_else(|| BpError::new(libc::EBADF, "null socket"))
}

/// Creates a socket backed by a dtn7-rs node.
///
/// `base_url` is the node's HTTP address (e.g. `http://127.0.0.1:3000`); pass `NULL`
/// to use `$BP_DTN7_URL`, falling back to `http://127.0.0.1:3000`. `$BP_POLL_MS`
/// overrides the 100 ms receive poll interval. No connection is made yet.
#[no_mangle]
pub unsafe extern "C" fn bp_socket(base_url: *const c_char) -> *mut BpSocket {
    ffi(std::ptr::null_mut(), || {
        let url = if base_url.is_null() {
            std::env::var("BP_DTN7_URL").unwrap_or_else(|_| DEFAULT_URL.to_string())
        } else {
            CStr::from_ptr(base_url)
                .to_str()
                .map_err(|_| BpError::invalid("base url is not valid UTF-8"))?
                .to_string()
        };
        let mut adapter = Dtn7Adapter::new(&url);
        if let Some(ms) = std::env::var("BP_POLL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&ms| ms > 0)
        {
            adapter = adapter.with_poll_interval(Duration::from_millis(ms));
        }
        Ok(Box::into_raw(Box::new(BpSocket {
            adapter: Box::new(adapter),
            bound: false,
        })))
    })
}

/// Binds the socket to `ipn:<node>.<service>`: registers the endpoint with the node.
/// Fails with `EINVAL` if already bound or `node` is 0, `ECONNREFUSED` if the node is down.
#[no_mangle]
pub unsafe extern "C" fn bp_bind(sock: *mut BpSocket, node: u32, service: u32) -> c_int {
    ffi(-1, || {
        let s = socket_mut(sock)?;
        if s.bound {
            return Err(BpError::invalid("socket is already bound"));
        }
        s.adapter.open(Eid::new(node, service))?;
        s.bound = true;
        Ok(0)
    })
}

/// Sends one bundle carrying `buf[0..len]` to `ipn:<node>.<service>`.
/// Returns the number of bytes sent (always `len`). A bound socket is the bundle's source.
#[no_mangle]
pub unsafe extern "C" fn bp_sendto(
    sock: *mut BpSocket,
    buf: *const c_void,
    len: usize,
    node: u32,
    service: u32,
) -> isize {
    ffi(-1, || {
        let s = socket_mut(sock)?;
        let payload: &[u8] = if len == 0 {
            &[]
        } else if buf.is_null() {
            return Err(BpError::new(libc::EFAULT, "null buffer"));
        } else {
            std::slice::from_raw_parts(buf as *const u8, len)
        };
        s.adapter.send(Eid::new(node, service), payload)?;
        Ok(len as isize)
    })
}

/// Waits for one bundle and copies up to `len` bytes of its payload into `buf`
/// (datagram semantics: any excess is discarded).
///
/// * `timeout_ms`: `< 0` waits forever, `0` checks once; otherwise the maximum wait.
///   On timeout returns `-1` with `EAGAIN`; if a signal interrupts the wait, `EINTR`.
/// * `src_node` / `src_service` (optional) receive the sender's endpoint.
/// * `full_len` (optional) receives the real payload size, so the caller can detect truncation.
///
/// Returns the number of bytes copied. Requires a prior `bp_bind`.
#[no_mangle]
pub unsafe extern "C" fn bp_recvfrom(
    sock: *mut BpSocket,
    buf: *mut c_void,
    len: usize,
    timeout_ms: c_int,
    src_node: *mut u32,
    src_service: *mut u32,
    full_len: *mut usize,
) -> isize {
    ffi(-1, || {
        let s = socket_mut(sock)?;
        if len > 0 && buf.is_null() {
            return Err(BpError::new(libc::EFAULT, "null buffer"));
        }
        let timeout = if timeout_ms < 0 {
            None
        } else {
            Some(Duration::from_millis(timeout_ms as u64))
        };
        let (src, payload) = s.adapter.recv(timeout)?;
        let copied = payload.len().min(len);
        if copied > 0 {
            std::ptr::copy_nonoverlapping(payload.as_ptr(), buf as *mut u8, copied);
        }
        if !src_node.is_null() {
            *src_node = src.node;
        }
        if !src_service.is_null() {
            *src_service = src.service;
        }
        if !full_len.is_null() {
            *full_len = payload.len();
        }
        Ok(copied as isize)
    })
}

/// Releases the endpoint (best effort) and frees the socket. The handle is invalid
/// afterwards even if this reports an error (e.g. the node was already gone).
#[no_mangle]
pub unsafe extern "C" fn bp_close(sock: *mut BpSocket) -> c_int {
    ffi(-1, || {
        if sock.is_null() {
            return Err(BpError::new(libc::EBADF, "null socket"));
        }
        let mut s = Box::from_raw(sock);
        s.adapter.close()?;
        Ok(0)
    })
}
