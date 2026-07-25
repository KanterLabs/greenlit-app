//! A minimal sequential-response loopback HTTP server standing in for
//! GitHub's device-flow/token-exchange host and REST API host in
//! integration tests that spawn the real `litci` binary — and so cannot
//! inject a Rust trait fake the way `crate::auth::device_flow`'s and
//! `crate::vars::remote`'s own unit tests do (see those modules' doc
//! comments for why a real, if local and minimal, HTTP server is the
//! correct boundary to fake at all). `litci` is pointed at one of these via
//! `LITCI_TEST_GITHUB_OAUTH_BASE_URL`/`LITCI_TEST_GITHUB_API_BASE_URL`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;
use std::time::Duration;

/// One canned response: HTTP status, reason phrase, and body.
pub struct Canned {
    pub status: u16,
    pub reason: &'static str,
    pub body: Vec<u8>,
}

impl Canned {
    pub fn json(status: u16, reason: &'static str, body: impl Into<String>) -> Self {
        Canned {
            status,
            reason,
            body: body.into().into_bytes(),
        }
    }

    pub fn bytes(status: u16, reason: &'static str, body: Vec<u8>) -> Self {
        Canned {
            status,
            reason,
            body,
        }
    }
}

pub struct FakeGitHub {
    listener: TcpListener,
}

impl FakeGitHub {
    pub fn bind() -> Self {
        FakeGitHub {
            listener: TcpListener::bind("127.0.0.1:0").expect("bind loopback"),
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.listener.local_addr().unwrap())
    }

    /// Serves exactly `responses.len()` sequential requests in order,
    /// returning (once joined) the request line (`METHOD /path HTTP/1.1`)
    /// seen for each — lets a test assert both the response-driven outcome
    /// and, when it matters, which endpoints were actually called.
    pub fn serve(self, responses: Vec<Canned>) -> JoinHandle<Vec<String>> {
        std::thread::spawn(move || {
            let mut paths = Vec::with_capacity(responses.len());
            for canned in responses {
                let (mut stream, _) = self.listener.accept().expect("accept");
                paths.push(read_request_line(&mut stream));
                let response = format!(
                    "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    canned.status,
                    canned.reason,
                    canned.body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write fake GitHub response");
                stream
                    .write_all(&canned.body)
                    .expect("write fake GitHub body");
            }
            paths
        })
    }
}

fn read_request_line(stream: &mut TcpStream) -> String {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut buf = [0_u8; 8192];
    let mut total = Vec::new();
    loop {
        let read = stream.read(&mut buf).unwrap_or(0);
        if read == 0 {
            break;
        }
        total.extend_from_slice(&buf[..read]);
        if total.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&total)
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}
