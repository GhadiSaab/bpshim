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
    let mut data = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        data.extend_from_slice(&chunk[..n]);
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&data[..header_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|l| {
            let l = l.to_ascii_lowercase();
            l.strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while data.len() < header_end + content_length {
        let n = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        data.extend_from_slice(&chunk[..n]);
    }
    let body = data[header_end..header_end + content_length].to_vec();
    let mut first = head.lines().next().unwrap().split_whitespace();
    let method = first.next().unwrap().to_string();
    let target = first.next().unwrap().to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };

    let (status, reply): (u16, Vec<u8>) = {
        let mut st = state.lock().unwrap();
        match (method.as_str(), path.as_str()) {
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
    };
    let head = format!(
        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        reply.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&reply);
}
