//! A fake ingest server for integration tests: a plain TCP listener that
//! speaks just enough HTTP/1.1 (keep-alive, Content-Length, gzip bodies) to
//! record what the client sends and answer with scripted replies.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub const API_KEY: &str = "gs_test_0123456789abcdef";

/// The client is process-wide, so tests that install one run one at a time.
pub fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
    pub text: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Reply {
    pub fn status(status: u16) -> Reply {
        Reply { status, headers: Vec::new(), body: "{}".into() }
    }

    pub fn header(mut self, name: &str, value: &str) -> Reply {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn body(mut self, body: Value) -> Reply {
        self.body = body.to_string();
        self
    }

    /// What the real server answers for each endpoint.
    pub fn default_for(request: &Recorded) -> Reply {
        match request.path.as_str() {
            "/api/v1/verify" => Reply::status(200).body(json!({ "ok": true, "environment": "staging" })),
            "/api/v1/events/batch" => {
                let count = request.body["events"].as_array().map_or(0, Vec::len);
                let accepted: Vec<Value> = (0..count).map(|index| json!({ "index": index, "id": "x" })).collect();
                Reply::status(202).body(json!({ "accepted": accepted, "rejected": [] }))
            }
            _ => Reply::status(202).body(json!({ "id": "x" })),
        }
    }
}

type Responder = Box<dyn FnMut(&Recorded) -> Reply + Send>;

pub struct FakeServer {
    pub url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    responder: Arc<Mutex<Responder>>,
}

impl FakeServer {
    pub fn start() -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let responder: Arc<Mutex<Responder>> = Arc::new(Mutex::new(Box::new(Reply::default_for)));
        let (thread_requests, thread_responder) = (requests.clone(), responder.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (requests, responder) = (thread_requests.clone(), thread_responder.clone());
                std::thread::spawn(move || serve(stream, requests, responder));
            }
        });
        FakeServer { url, requests, responder }
    }

    pub fn respond_with(&self, responder: impl FnMut(&Recorded) -> Reply + Send + 'static) {
        *self.responder.lock().unwrap() = Box::new(responder);
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    pub fn requests_to(&self, path: &str) -> Vec<Recorded> {
        self.requests().into_iter().filter(|request| request.path == path).collect()
    }

    /// Waits until at least `count` requests arrived (or the timeout).
    pub fn wait_for(&self, count: usize, timeout: Duration) -> Vec<Recorded> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.requests.lock().unwrap().len() >= count {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.requests()
    }

    /// Every error event received, from batch and single requests.
    pub fn events(&self) -> Vec<Value> {
        let mut events = Vec::new();
        for request in self.requests() {
            match request.path.as_str() {
                "/api/v1/events/batch" => events.extend(request.body["events"].as_array().cloned().unwrap_or_default()),
                "/api/v1/events" => events.push(request.body.clone()),
                _ => {}
            }
        }
        events
    }
}

fn serve(stream: TcpStream, requests: Arc<Mutex<Vec<Recorded>>>, responder: Arc<Mutex<Responder>>) {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let mut headers = Vec::new();
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                return;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                headers.push((name.trim().to_string(), value.trim().to_string()));
            }
        }
        let find =
            |name: &str| headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.clone());
        let length: usize = find("content-length").and_then(|value| value.parse().ok()).unwrap_or(0);
        let mut raw = vec![0u8; length];
        if reader.read_exact(&mut raw).is_err() {
            return;
        }
        let bytes = if find("content-encoding").as_deref() == Some("gzip") {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut out).expect("gzip body");
            out
        } else {
            raw
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let body = serde_json::from_str(&text).unwrap_or(Value::Null);
        let recorded = Recorded { method, path, headers, body, text };
        requests.lock().unwrap().push(recorded.clone());
        let reply = (responder.lock().unwrap())(&recorded);
        let mut response = format!(
            "HTTP/1.1 {} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
            reply.status,
            reply.body.len()
        );
        for (name, value) in &reply.headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        response.push_str("\r\n");
        response.push_str(&reply.body);
        if writer.write_all(response.as_bytes()).is_err() {
            return;
        }
    }
}

/// A client configuration pointed at `server`, flushing quickly.
pub fn config(server: &FakeServer) -> goodsorcery::Config {
    goodsorcery::Config::new()
        .api_key(API_KEY)
        .endpoint(&server.url)
        .flush_interval(Duration::from_millis(50))
        .shutdown_timeout(Duration::from_secs(10))
}
