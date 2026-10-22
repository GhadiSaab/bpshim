//! dtn7-rs adapter, speaking its localhost HTTP client API.
//!
//! * `GET  /register?<eid>`     claim a local endpoint
//! * `GET  /unregister?<eid>`   release it
//! * `GET  /endpoint?<eid>`     pop one queued bundle (answers at once; "Nothing to receive" if empty)
//! * `GET  /cts`                a fresh, unique creation timestamp
//! * `POST /insert`             send a complete CBOR bundle (lets us choose the source endpoint)
//! * `POST /send?dst=&lifetime=` send a bare payload; dtn7-rs picks the node itself as source
//!
//! `/endpoint` never blocks, so `recv` polls. Every request uses a fresh
//! connection (no pooling) and no thread is ever spawned, so nothing is shared
//! across `fork`.

use std::io::Read;
use std::time::{Duration, Instant};

use bp7::dtntime::CreationTimestamp;
use bp7::flags::BundleControlFlags;
use bp7::primary::PrimaryBlockBuilder;
use bp7::Bundle;

use crate::adapter::BpaAdapter;
use crate::eid::Eid;
use crate::error::{BpError, Result};

pub const DEFAULT_URL: &str = "http://127.0.0.1:3000";
const NOTHING_TO_RECEIVE: &[u8] = b"Nothing to receive";
const BUNDLE_LIFETIME: Duration = Duration::from_secs(60 * 60);
const MAX_BODY: u64 = 64 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Dtn7Adapter {
    base: String,
    local: Option<Eid>,
    poll_interval: Duration,
}

impl Dtn7Adapter {
    pub fn new(base_url: &str) -> Self {
        Dtn7Adapter {
            base: base_url.trim_end_matches('/').to_string(),
            local: None,
            poll_interval: Duration::from_millis(100),
        }
    }

    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Performs one HTTP request and returns `(status, body)`. Non-2xx statuses are
    /// returned, not raised, so callers can react to e.g. 404.
    fn http(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<(u16, Vec<u8>)> {
        let agent = ureq::AgentBuilder::new()
            .max_idle_connections(0)
            .timeout(REQUEST_TIMEOUT)
            .build();
        let url = format!("{}{}", self.base, path);
        let request = agent.request(method, &url);
        let outcome = match body {
            Some(b) => request.send_bytes(b),
            None => request.call(),
        };
        match outcome {
            Ok(resp) => {
                let status = resp.status();
                Ok((status, read_body(resp)?))
            }
            Err(ureq::Error::Status(status, resp)) => Ok((status, read_body(resp)?)),
            Err(ureq::Error::Transport(t)) => Err(transport_error(&t)),
        }
    }

    fn expect_ok(&self, what: &str, status: u16, body: &[u8]) -> Result<()> {
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(BpError::protocol(format!(
                "dtn7-rs {} failed: HTTP {}: {}",
                what,
                status,
                String::from_utf8_lossy(body).trim()
            )))
        }
    }

    fn register(&self, eid: Eid) -> Result<()> {
        let (status, body) = self.http("GET", &format!("/register?{}", eid), None)?;
        self.expect_ok("register", status, &body)
    }

    /// Asks the node for the next queued bundle. `Ok(None)` means nothing is waiting.
    fn fetch(&self, eid: Eid) -> Result<Option<Bundle>> {
        let (status, body) = self.http("GET", &format!("/endpoint?{}", eid), None)?;
        if status == 404 {
            // The node forgot our registration (e.g. it restarted): claim it again.
            self.register(eid)?;
            return Ok(None);
        }
        self.expect_ok("endpoint", status, &body)?;
        if body == NOTHING_TO_RECEIVE {
            return Ok(None);
        }
        Bundle::try_from(body.as_slice())
            .map(Some)
            .map_err(|e| BpError::protocol(format!("undecodable bundle from dtn7-rs: {:?}", e)))
    }

    fn creation_timestamp(&self) -> Result<CreationTimestamp> {
        let (status, body) = self.http("GET", "/cts", None)?;
        self.expect_ok("cts", status, &body)?;
        parse_cts(&body)
    }
}

impl BpaAdapter for Dtn7Adapter {
    fn open(&mut self, eid: Eid) -> Result<()> {
        eid.validate()?;
        self.register(eid)?;
        self.local = Some(eid);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if let Some(eid) = self.local.take() {
            let (status, body) = self.http("GET", &format!("/unregister?{}", eid), None)?;
            self.expect_ok("unregister", status, &body)?;
        }
        Ok(())
    }

    fn send(&mut self, dst: Eid, payload: &[u8]) -> Result<()> {
        dst.validate()?;
        let (status, body) = match self.local {
            Some(src) => {
                let cbor = encode_bundle(src, dst, payload, self.creation_timestamp()?)?;
                self.http("POST", "/insert", Some(&cbor))?
            }
            // Not bound: there is no source endpoint to claim, so let dtn7-rs use the node's own.
            None => self.http(
                "POST",
                &format!("/send?dst={}&lifetime={}s", dst, BUNDLE_LIFETIME.as_secs()),
                Some(payload),
            )?,
        };
        self.expect_ok("send", status, &body)
    }

    fn recv(&mut self, timeout: Option<Duration>) -> Result<(Eid, Vec<u8>)> {
        let eid = self
            .local
            .ok_or_else(|| BpError::new(libc::EDESTADDRREQ, "socket is not bound"))?;
        let start = Instant::now();
        loop {
            if let Some(bundle) = self.fetch(eid)? {
                // Status reports and other administrative records are not application data.
                if !bundle.is_administrative_record() {
                    let payload = bundle
                        .payload()
                        .cloned()
                        .ok_or_else(|| BpError::protocol("bundle without payload"))?;
                    return Ok((Eid::from_bp7(&bundle.primary.source), payload));
                }
                continue;
            }
            let wait = match timeout {
                None => self.poll_interval,
                Some(limit) => {
                    let elapsed = start.elapsed();
                    if elapsed >= limit {
                        return Err(BpError::timeout());
                    }
                    self.poll_interval.min(limit - elapsed)
                }
            };
            if !sleep_interruptible(wait) {
                return Err(BpError::interrupted());
            }
        }
    }
}

/// Sleeps, but returns `false` as soon as a signal handler runs (std's `sleep` would
/// resume, and an app waiting for Ctrl+C would never notice).
fn sleep_interruptible(dur: Duration) -> bool {
    let ts = libc::timespec {
        tv_sec: dur.as_secs() as libc::time_t,
        tv_nsec: dur.subsec_nanos() as libc::c_long,
    };
    // SAFETY: `ts` is a valid timespec and the remainder pointer may be null.
    unsafe { libc::nanosleep(&ts, std::ptr::null_mut()) == 0 }
}

fn read_body(resp: ureq::Response) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    resp.into_reader()
        .take(MAX_BODY)
        .read_to_end(&mut out)
        .map_err(|e| BpError::from_io(&e))?;
    Ok(out)
}

fn transport_error(t: &ureq::Transport) -> BpError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(t);
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            return BpError::from_io(io);
        }
        source = err.source();
    }
    let errno = match t.kind() {
        ureq::ErrorKind::ConnectionFailed => libc::ECONNREFUSED,
        ureq::ErrorKind::Dns => libc::EHOSTUNREACH,
        ureq::ErrorKind::InvalidUrl | ureq::ErrorKind::UnknownScheme => libc::EINVAL,
        _ => libc::EIO,
    };
    BpError::new(errno, t.to_string())
}

/// dtn7-rs answers `/cts` with a JSON pair such as `[690468652541,0]`.
fn parse_cts(body: &[u8]) -> Result<CreationTimestamp> {
    let text = String::from_utf8_lossy(body);
    let inner = text.trim().trim_start_matches('[').trim_end_matches(']');
    let mut parts = inner.split(',').map(|p| p.trim().parse::<u64>());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(time)), Some(Ok(seq)), None) => {
            Ok(CreationTimestamp::with_time_and_seq(time, seq))
        }
        _ => Err(BpError::protocol(format!("unexpected /cts reply: {}", text))),
    }
}

/// Builds the CBOR bundle `/insert` expects, mirroring what dtn7-rs does for `/send`
/// except that the source is the socket's own endpoint.
pub(crate) fn encode_bundle(
    src: Eid,
    dst: Eid,
    payload: &[u8],
    timestamp: CreationTimestamp,
) -> Result<Vec<u8>> {
    let src = src.to_bp7()?;
    let primary = PrimaryBlockBuilder::new()
        .bundle_control_flags(BundleControlFlags::BUNDLE_MUST_NOT_FRAGMENTED.bits())
        .destination(dst.to_bp7()?)
        .source(src.clone())
        .report_to(src)
        .creation_timestamp(timestamp)
        .lifetime(BUNDLE_LIFETIME)
        .build()
        .map_err(|e| BpError::invalid(format!("cannot build bundle: {:?}", e)))?;
    let mut bundle = bp7::bundle::BundleBuilder::new()
        .primary(primary)
        .payload(payload.to_vec())
        .build()
        .map_err(|e| BpError::invalid(format!("cannot build bundle: {:?}", e)))?;
    bundle.set_crc(bp7::crc::CRC_NO);
    Ok(bundle.to_cbor())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cts() {
        let ts = parse_cts(b"[690468652541,7]\n").unwrap();
        assert_eq!(ts.dtntime(), 690468652541);
        assert_eq!(ts.seqno(), 7);
    }

    #[test]
    fn rejects_garbage_cts() {
        assert_eq!(parse_cts(b"nope").unwrap_err().errno, libc::EIO);
    }

    #[test]
    fn bundle_round_trip() {
        let ts = CreationTimestamp::with_time_and_seq(1000, 3);
        let cbor = encode_bundle(Eid::new(10, 2), Eid::new(20, 1), b"Hello!\0", ts).unwrap();
        let back = Bundle::try_from(cbor.as_slice()).unwrap();
        assert_eq!(Eid::from_bp7(&back.primary.source), Eid::new(10, 2));
        assert_eq!(Eid::from_bp7(&back.primary.destination), Eid::new(20, 1));
        assert_eq!(back.payload().unwrap().as_slice(), b"Hello!\0");
        assert!(!back.is_administrative_record());
    }
}
