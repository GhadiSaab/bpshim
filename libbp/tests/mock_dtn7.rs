//! Fake dtn7-rs HTTP node: just enough of the client API to exercise the adapter.
#![allow(dead_code)]

use std::collections::{HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Default)]
pub struct State {
    pub registered: HashSet<String>,
    pub queue: VecDeque<Vec<u8>>,
    pub inserted: Vec<Vec<u8>>,
    pub sent: Vec<(String, Vec<u8>)>,
    pub endpoint_calls: usize,
    pub cts_calls: u64,
    /// Answer "Nothing to receive" for this many more /endpoint calls even if a bundle is queued.
    pub hold_back: usize,
    /// `/insert` pushes the bundle onto the receive queue (a one-node loopback).
    pub loopback: bool,
    /// TCP connections accepted so far.
    pub connections: usize,
    /// Answer with `Connection: close` and hang up after every response (default: keep-alive).
    pub close_each: bool,
    /// Hang up after every response *without saying so*, like a server that timed out an idle connection.
    pub drop_silently: bool,
}

pub struct MockNode {
    pub url: String,
    pub state: Arc<Mutex<State>>,
}

impl MockNode {
    pub fn start() -> MockNode {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let shared = shared.clone();
                thread::spawn(move || serve(stream, shared));
            }
        });
        MockNode { url, state }
    }
}

/// A URL nothing listens on.
pub fn dead_url() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    url
}

fn serve(mut stream: TcpStream, state: Arc<Mutex<State>>) {
    state.lock().unwrap().connections += 1;
    let mut pending = Vec::new();
    while let Some((method, target, body)) = read_request(&mut stream, &mut pending) {
        let (status, reply) = handle(&state, &method, &target, body);
        let (close_each, drop_silently) = {
            let st = state.lock().unwrap();
            (st.close_each, st.drop_silently)
        };
        let head = format!(
            "HTTP/1.1 {} X\r\nContent-Length: {}\r\n{}\r\n",
            status,
            reply.len(),
            if close_each { "Connection: close\r\n" } else { "" }
        );
        // One write: two small ones would stall on Nagle + delayed ACK over a reused connection.
        let mut response = head.into_bytes();
        response.extend_from_slice(&reply);
        if stream.write_all(&response).is_err() {
            return;
        }
        if close_each || drop_silently {
            return;
        }
    }
}

/// Reads one request; `pending` carries bytes already read that belong to the next one.
fn read_request(stream: &mut TcpStream, pending: &mut Vec<u8>) -> Option<(String, String, Vec<u8>)> {
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => pending.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&pending[..header_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while pending.len() < header_end + content_length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => pending.extend_from_slice(&chunk[..n]),
        }
    }
    let body = pending[header_end..header_end + content_length].to_vec();
    pending.drain(..header_end + content_length);
    let mut first = head.lines().next()?.split_whitespace();
    Some((first.next()?.to_string(), first.next()?.to_string(), body))
}

fn handle(state: &Arc<Mutex<State>>, method: &str, target: &str, body: Vec<u8>) -> (u16, Vec<u8>) {
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };
    let mut st = state.lock().unwrap();
    match (method, path.as_str()) {
        ("GET", "/register") => {
            st.registered.insert(query.clone());
            (200, format!("Registered URI: {}", query).into_bytes())
        }
        ("GET", "/unregister") => {
            st.registered.remove(&query);
            (200, b"Unregistered".to_vec())
        }
        ("GET", "/endpoint") => {
            st.endpoint_calls += 1;
            if !st.registered.contains(&query) {
                (404, b"No such endpoint registered!".to_vec())
            } else if st.hold_back > 0 {
                st.hold_back -= 1;
                (200, b"Nothing to receive".to_vec())
            } else if let Some(b) = st.queue.pop_front() {
                (200, b)
            } else {
                (200, b"Nothing to receive".to_vec())
            }
        }
        ("GET", "/cts") => {
            st.cts_calls += 1;
            (200, format!("[690468652541,{}]", st.cts_calls - 1).into_bytes())
        }
        ("POST", "/insert") => {
            st.inserted.push(body.clone());
            if st.loopback {
                st.queue.push_back(body);
            }
            (200, b"Sent".to_vec())
        }
        ("POST", "/send") => {
            st.sent.push((query, body));
            (200, b"Sent payload".to_vec())
        }
        _ => (404, b"unknown".to_vec()),
    }
}
