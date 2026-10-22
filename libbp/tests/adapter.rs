mod mock_dtn7;

use std::time::{Duration, Instant};

use bp::{BpaAdapter, Dtn7Adapter, Eid};
use bp7::dtntime::CreationTimestamp;
use bp7::flags::BundleControlFlags;
use bp7::primary::PrimaryBlockBuilder;
use bp7::{Bundle, EndpointID};
use mock_dtn7::{dead_url, MockNode};

fn adapter(node: &MockNode) -> Dtn7Adapter {
    Dtn7Adapter::new(&node.url).with_poll_interval(Duration::from_millis(10))
}

fn bundle_cbor(src: (u64, u64), dst: (u64, u64), payload: &[u8], admin: bool) -> Vec<u8> {
    let mut flags = BundleControlFlags::BUNDLE_MUST_NOT_FRAGMENTED;
    if admin {
        flags |= BundleControlFlags::BUNDLE_ADMINISTRATIVE_RECORD_PAYLOAD;
    }
    let primary = PrimaryBlockBuilder::new()
        .bundle_control_flags(flags.bits())
        .destination(EndpointID::with_ipn(dst.0, dst.1).unwrap())
        .source(EndpointID::with_ipn(src.0, src.1).unwrap())
        .report_to(EndpointID::none())
        .creation_timestamp(CreationTimestamp::with_time_and_seq(5, 0))
        .lifetime(Duration::from_secs(60))
        .build()
        .unwrap();
    let mut b = bp7::bundle::BundleBuilder::new()
        .primary(primary)
        .payload(payload.to_vec())
        .build()
        .unwrap();
    b.set_crc(bp7::crc::CRC_NO);
    b.to_cbor()
}

#[test]
fn bound_send_inserts_a_bundle_sourced_at_the_local_endpoint() {
    let node = MockNode::start();
    let mut a = adapter(&node);
    a.open(Eid::new(10, 2)).unwrap();
    a.send(Eid::new(20, 1), b"Hello!\0").unwrap();

    let st = node.state.lock().unwrap();
    assert!(st.registered.contains("ipn:10.2"));
    assert_eq!(st.inserted.len(), 1);
    let b = Bundle::try_from(st.inserted[0].as_slice()).unwrap();
    assert_eq!(b.primary.source.to_string(), "ipn:10.2");
    assert_eq!(b.primary.destination.to_string(), "ipn:20.1");
    assert_eq!(b.payload().unwrap().as_slice(), b"Hello!\0");
}

#[test]
fn each_send_gets_its_own_creation_timestamp() {
    let node = MockNode::start();
    let mut a = adapter(&node);
    a.open(Eid::new(10, 2)).unwrap();
    a.send(Eid::new(20, 1), b"a").unwrap();
    a.send(Eid::new(20, 1), b"b").unwrap();
    let st = node.state.lock().unwrap();
    let ids: Vec<String> = st
        .inserted
        .iter()
        .map(|c| Bundle::try_from(c.as_slice()).unwrap().id())
        .collect();
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn unbound_send_falls_back_to_the_send_endpoint() {
    let node = MockNode::start();
    let mut a = adapter(&node);
    a.send(Eid::new(20, 1), b"hi").unwrap();
    let st = node.state.lock().unwrap();
    assert_eq!(st.sent.len(), 1);
    assert!(st.sent[0].0.starts_with("dst=ipn:20.1&lifetime="));
    assert_eq!(st.sent[0].1, b"hi");
}

#[test]
fn recv_polls_until_a_bundle_arrives() {
    let node = MockNode::start();
    {
        let mut st = node.state.lock().unwrap();
        st.queue.push_back(bundle_cbor((10, 2), (20, 1), b"late", false));
        st.hold_back = 3;
    }
    let mut a = adapter(&node);
    a.open(Eid::new(20, 1)).unwrap();
    let (src, payload) = a.recv(Some(Duration::from_secs(3))).unwrap();
    assert_eq!(src, Eid::new(10, 2));
    assert_eq!(payload, b"late");
    assert!(node.state.lock().unwrap().endpoint_calls >= 4);
}

#[test]
fn recv_times_out_with_eagain() {
    let node = MockNode::start();
    let mut a = adapter(&node);
    a.open(Eid::new(20, 1)).unwrap();
    let start = Instant::now();
    let err = a.recv(Some(Duration::from_millis(200))).unwrap_err();
    let took = start.elapsed();
    assert_eq!(err.errno, libc::EAGAIN);
    assert!(took >= Duration::from_millis(200), "returned early: {:?}", took);
    assert!(took < Duration::from_millis(1000), "overshot: {:?}", took);
}

#[test]
fn zero_timeout_checks_exactly_once() {
    let node = MockNode::start();
    let mut a = adapter(&node);
    a.open(Eid::new(20, 1)).unwrap();
    assert_eq!(a.recv(Some(Duration::ZERO)).unwrap_err().errno, libc::EAGAIN);
    assert_eq!(node.state.lock().unwrap().endpoint_calls, 1);
}

#[test]
fn recv_before_bind_is_rejected() {
    let node = MockNode::start();
    let mut a = adapter(&node);
    assert_eq!(
        a.recv(Some(Duration::ZERO)).unwrap_err().errno,
        libc::EDESTADDRREQ
    );
}

#[test]
fn administrative_records_are_skipped() {
    let node = MockNode::start();
    {
        let mut st = node.state.lock().unwrap();
        st.queue.push_back(bundle_cbor((20, 0), (20, 1), b"report", true));
        st.queue.push_back(bundle_cbor((10, 2), (20, 1), b"data", false));
    }
    let mut a = adapter(&node);
    a.open(Eid::new(20, 1)).unwrap();
    let (_, payload) = a.recv(Some(Duration::from_secs(1))).unwrap();
    assert_eq!(payload, b"data");
}

#[test]
fn registration_lost_by_the_node_is_restored() {
    let node = MockNode::start();
    let mut a = adapter(&node);
    a.open(Eid::new(20, 1)).unwrap();
    node.state.lock().unwrap().registered.clear();
    let _ = a.recv(Some(Duration::from_millis(50)));
    assert!(node.state.lock().unwrap().registered.contains("ipn:20.1"));
}

#[test]
fn close_unregisters_and_is_idempotent() {
    let node = MockNode::start();
    let mut a = adapter(&node);
    a.open(Eid::new(20, 1)).unwrap();
    a.close().unwrap();
    a.close().unwrap();
    assert!(node.state.lock().unwrap().registered.is_empty());
}

#[test]
fn node_down_reports_econnrefused() {
    let mut a = Dtn7Adapter::new(&dead_url());
    assert_eq!(
        a.send(Eid::new(20, 1), b"x").unwrap_err().errno,
        libc::ECONNREFUSED
    );
    assert_eq!(a.open(Eid::new(20, 1)).unwrap_err().errno, libc::ECONNREFUSED);
}

#[test]
fn node_zero_is_rejected_before_any_request() {
    let mut a = Dtn7Adapter::new(&dead_url());
    assert_eq!(a.open(Eid::new(0, 1)).unwrap_err().errno, libc::EINVAL);
    assert_eq!(a.send(Eid::new(0, 1), b"x").unwrap_err().errno, libc::EINVAL);
}

#[test]
fn polling_reuses_one_keep_alive_connection() {
    let node = MockNode::start();
    let mut a = adapter(&node);
    a.open(Eid::new(20, 1)).unwrap();
    assert_eq!(a.recv(Some(Duration::from_millis(300))).unwrap_err().errno, libc::EAGAIN);
    let st = node.state.lock().unwrap();
    assert!(st.endpoint_calls >= 10, "expected many polls, got {}", st.endpoint_calls);
    assert_eq!(st.connections, 1, "every poll opened a new connection");
}

#[test]
fn a_connection_the_server_silently_dropped_is_replaced() {
    let node = MockNode::start();
    node.state.lock().unwrap().drop_silently = true;
    let mut a = adapter(&node);
    a.open(Eid::new(20, 1)).unwrap();
    for _ in 0..3 {
        assert_eq!(a.recv(Some(Duration::ZERO)).unwrap_err().errno, libc::EAGAIN);
    }
    a.send(Eid::new(20, 2), b"still works").unwrap();
}

#[test]
fn a_forked_child_does_not_share_the_parents_connection() {
    let node = MockNode::start();
    let mut a = adapter(&node);
    a.open(Eid::new(10, 2)).unwrap(); // parent now holds a pooled connection
    let before = node.state.lock().unwrap().connections;
    // SAFETY: the child only calls the adapter and then `_exit`s.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let ok = a.send(Eid::new(20, 1), b"from child").is_ok();
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    }
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    assert!(node.state.lock().unwrap().connections > before, "child reused the parent's socket");
    // the parent's own connection is still good
    a.send(Eid::new(20, 1), b"from parent").unwrap();
}
