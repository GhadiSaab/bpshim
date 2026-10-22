mod mock_dtn7;

use std::ffi::{c_void, CString};

use bp::capi::*;
use mock_dtn7::{dead_url, MockNode};

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap()
}

fn open(url: &str) -> *mut BpSocket {
    let url = CString::new(url).unwrap();
    let s = unsafe { bp_socket(url.as_ptr()) };
    assert!(!s.is_null());
    s
}

#[test]
fn loopback_round_trip_through_the_c_api() {
    let node = MockNode::start();
    node.state.lock().unwrap().loopback = true;
    let s = open(&node.url);
    unsafe {
        assert_eq!(bp_bind(s, 10, 2), 0);
        let msg = b"Hello!\0";
        assert_eq!(
            bp_sendto(s, msg.as_ptr() as *const c_void, msg.len(), 10, 2),
            msg.len() as isize
        );
        let mut buf = [0u8; 64];
        let (mut n, mut svc, mut full) = (0u32, 0u32, 0usize);
        let got = bp_recvfrom(
            s,
            buf.as_mut_ptr() as *mut c_void,
            buf.len(),
            1000,
            &mut n,
            &mut svc,
            &mut full,
        );
        assert_eq!(got, msg.len() as isize);
        assert_eq!(&buf[..msg.len()], msg);
        assert_eq!((n, svc, full), (10, 2, msg.len()));
        assert_eq!(bp_close(s), 0);
    }
    assert!(node.state.lock().unwrap().registered.is_empty());
}

#[test]
fn small_buffer_truncates_and_reports_the_full_length() {
    let node = MockNode::start();
    node.state.lock().unwrap().loopback = true;
    let s = open(&node.url);
    unsafe {
        assert_eq!(bp_bind(s, 10, 2), 0);
        let msg = b"0123456789";
        bp_sendto(s, msg.as_ptr() as *const c_void, msg.len(), 10, 2);
        let mut buf = [0u8; 4];
        let mut full = 0usize;
        let got = bp_recvfrom(
            s,
            buf.as_mut_ptr() as *mut c_void,
            buf.len(),
            1000,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut full,
        );
        assert_eq!(got, 4);
        assert_eq!(&buf, b"0123");
        assert_eq!(full, 10);
        bp_close(s);
    }
}

#[test]
fn errors_set_errno_and_return_minus_one() {
    let node = MockNode::start();
    let s = open(&node.url);
    unsafe {
        let mut buf = [0u8; 8];
        // receive before bind
        assert_eq!(
            bp_recvfrom(
                s,
                buf.as_mut_ptr() as *mut c_void,
                8,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            -1
        );
        assert_eq!(errno(), libc::EDESTADDRREQ);
        // bind twice
        assert_eq!(bp_bind(s, 10, 2), 0);
        assert_eq!(bp_bind(s, 10, 3), -1);
        assert_eq!(errno(), libc::EINVAL);
        // nothing waiting
        assert_eq!(
            bp_recvfrom(
                s,
                buf.as_mut_ptr() as *mut c_void,
                8,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            -1
        );
        assert_eq!(errno(), libc::EAGAIN);
        // null handle and null buffer
        assert_eq!(bp_bind(std::ptr::null_mut(), 1, 1), -1);
        assert_eq!(errno(), libc::EBADF);
        assert_eq!(bp_sendto(s, std::ptr::null(), 4, 20, 1), -1);
        assert_eq!(errno(), libc::EFAULT);
        bp_close(s);
    }
}

#[test]
fn node_down_is_econnrefused() {
    let s = open(&dead_url());
    unsafe {
        let msg = b"x";
        assert_eq!(bp_sendto(s, msg.as_ptr() as *const c_void, 1, 20, 1), -1);
        assert_eq!(errno(), libc::ECONNREFUSED);
        assert_eq!(bp_bind(s, 10, 2), -1);
        assert_eq!(errno(), libc::ECONNREFUSED);
        bp_close(s);
    }
}
