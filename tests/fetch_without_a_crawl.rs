//! One fetch, as the first thing this process asks of a network.
//!
//! It is a file of its own, holding one test, and that is the whole point: an integration test
//! file is a process, so anything crawling beside it would leave the engine in the state this
//! covers and the test would pass without exercising anything. A client built the way a single
//! fetch builds one could not send at all until the fix this pins, and the failure was
//! invisible for as long as every fetch in the program happened to follow a crawl, which is
//! what acquiring the subresources of a page a crawl delivered guarantees.
//!
//! The socket is on loopback and the server is in this file, so nothing here reaches the web,
//! needs setup, or answers differently tomorrow.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use archeion::crawl::{CrawlEngine, PageEvent, Seed, SpiderEngine};

const PAGE: &[u8] =
    b"<html><head><title>A page</title></head><body>Bread is mostly patience.</body></html>";

#[test]
fn a_fetch_with_no_crawl_before_it_reaches_the_server() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = answer(stream);
        }
    });

    let url = format!("http://127.0.0.1:{port}/index.html");
    let mut seed = Seed::new(url.clone());
    // The server is on loopback, which is the range a run is refused for unless it says it
    // meant it, and the only way this path is exercised at all.
    seed.allow_private_addresses = true;
    seed.max_retries = 0;
    // So a fetch that cannot send fails this test rather than hanging the suite.
    seed.request_timeout = Duration::from_secs(10);

    let event = SpiderEngine::default().fetch(&url, &seed);

    match event {
        PageEvent::Response(response) => {
            assert_eq!(response.status, 200);
            assert_eq!(response.body, PAGE.to_vec());
        }
        PageEvent::NoResponse(failure) => {
            panic!("the server answered nothing: {}", failure.reason)
        }
    }
}

fn answer(mut stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // The whole request has to be consumed before the answer, or the client sees the close as
    // a reset rather than as a response.
    let mut header = String::new();
    while reader.read_line(&mut header)? > 2 {
        header.clear();
    }
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        PAGE.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(PAGE)?;
    stream.flush()
}
