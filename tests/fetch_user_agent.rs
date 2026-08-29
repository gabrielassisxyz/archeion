//! A single fetch, outside any crawl, sends the identity its seed carries.
//!
//! `fetch_without_a_crawl.rs` already pins that this path can send at all, in a file of its
//! own so nothing crawling beside it can leave the engine in a state that test's single
//! assertion would not have exercised. This file asks a narrower question of the same path,
//! that a seed's own `user_agent` reaches the request `fetch` sends and that a seed with none
//! sends the compiled default byte for byte, so it stays a file of its own for the same
//! reason rather than being folded into that one.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use archeion::crawl::{CrawlEngine, DEFAULT_USER_AGENT, PageEvent, Seed, SpiderEngine};

const PAGE: &[u8] = b"<html><head><title>A page</title></head><body>fetched once</body></html>";

#[test]
fn a_fetch_sends_the_seeds_own_user_agent() {
    let (url, header) = fetch_and_capture_user_agent(Some("archive-bot/9.0".to_owned()));
    assert_eq!(
        header.as_deref(),
        Some("archive-bot/9.0"),
        "fetching {url} did not send the seed's own identity"
    );
}

#[test]
fn a_fetch_with_no_override_sends_the_compiled_default() {
    let (url, header) = fetch_and_capture_user_agent(None);
    assert_eq!(
        header.as_deref(),
        Some(DEFAULT_USER_AGENT),
        "fetching {url} with no override did not send the compiled default byte for byte"
    );
}

/// Fetches one page off a server this call starts on loopback, under a seed carrying
/// `user_agent`, and answers with the `User-Agent` header the server actually received.
fn fetch_and_capture_user_agent(user_agent: Option<String>) -> (String, Option<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = answer(stream, sender.clone());
        }
    });

    let url = format!("http://127.0.0.1:{port}/index.html");
    let mut seed = Seed::new(url.clone());
    // The server is on loopback, which is the range a run is refused for unless it says it
    // meant it, and the only way this path is exercised at all.
    seed.allow_private_addresses = true;
    seed.max_retries = 0;
    seed.request_timeout = Duration::from_secs(10);
    seed.user_agent = user_agent;

    let event = SpiderEngine::default().fetch(&url, &seed);
    match event {
        PageEvent::Response(response) => assert_eq!(response.status, 200),
        PageEvent::NoResponse(failure) => panic!("the server answered nothing: {}", failure.reason),
    }

    let header = receiver.recv_timeout(Duration::from_secs(10)).ok();
    (url, header)
}

/// Answers the request with a fixed page and reports the `User-Agent` header it read, or
/// nothing when the request carried none.
fn answer(mut stream: TcpStream, sender: mpsc::Sender<String>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut user_agent = None;
    let mut header = String::new();
    while reader.read_line(&mut header)? > 2 {
        if let Some(value) = header
            .strip_prefix("User-Agent:")
            .or_else(|| header.strip_prefix("user-agent:"))
        {
            user_agent = Some(value.trim().to_owned());
        }
        header.clear();
    }
    if let Some(value) = user_agent {
        let _ = sender.send(value);
    }
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        PAGE.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(PAGE)?;
    stream.flush()
}
