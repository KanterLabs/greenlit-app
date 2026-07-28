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
use std::time::{Duration, Instant};

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
        self.serve_requests(responses, false)
    }

    /// Serves canned responses while retaining each bounded request head.
    /// Credential capability tests use this true external boundary to prove
    /// which bearer value a later compiled `litci` process actually loaded.
    pub fn serve_recorded(self, responses: Vec<Canned>) -> JoinHandle<Vec<String>> {
        self.serve_requests(responses, true)
    }

    fn serve_requests(
        self,
        responses: Vec<Canned>,
        record_full_head: bool,
    ) -> JoinHandle<Vec<String>> {
        self.listener
            .set_nonblocking(true)
            .expect("make fake GitHub listener nonblocking");
        std::thread::spawn(move || {
            let mut requests = Vec::with_capacity(responses.len());
            for canned in responses {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut stream = loop {
                    match self.listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "fake GitHub did not receive its expected request"
                            );
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("accept fake GitHub request: {error}"),
                    }
                };
                let head = read_request_head(&mut stream);
                requests.push(if record_full_head {
                    head
                } else {
                    head.lines().next().unwrap_or("").to_string()
                });
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
            requests
        })
    }
}

fn read_request_head(stream: &mut TcpStream) -> String {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut buf = [0_u8; 8192];
    let mut total = Vec::new();
    let mut expected_len = None;
    loop {
        let read = stream.read(&mut buf).unwrap_or(0);
        if read == 0 {
            break;
        }
        total.extend_from_slice(&buf[..read]);
        assert!(
            total.len() <= 64 * 1024,
            "fake GitHub request exceeded its 64 KiB test bound"
        );
        if expected_len.is_none()
            && let Some(header_end) = total.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let head_len = header_end + 4;
            let head = String::from_utf8_lossy(&total[..head_len]);
            let content_len = head.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| {
                    value
                        .trim()
                        .parse::<usize>()
                        .expect("numeric Content-Length")
                })
            });
            expected_len = Some(head_len + content_len.unwrap_or(0));
        }
        if expected_len.is_some_and(|expected| total.len() >= expected) {
            break;
        }
    }
    assert!(
        expected_len.is_some_and(|expected| total.len() >= expected),
        "fake GitHub received an incomplete request"
    );
    String::from_utf8_lossy(&total).into_owned()
}
