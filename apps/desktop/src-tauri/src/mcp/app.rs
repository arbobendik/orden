//! The running app, as `nib mcp` reaches it: the endpoint file it writes at every launch,
//! and one request to that endpoint. Nothing here starts the app: a client that runs
//! `nib mcp` while nib is closed finds it once the reader opens it (see `watch` in link.rs).
//!
//! **The file is read, never trusted to be current.** The app writes its port, its pid
//! and the installation's secret into `automation.json` as it starts and leaves the file
//! behind when it ends, so a port in it may be a port nobody listens on any more, or one
//! something else has taken since. Before the secret or a token is sent to a port, the
//! port is asked once with neither, and only nib's own refusal - a 401 with a sentence in
//! JSON - counts as nib. See endpoint.rs for the other side.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

/// How long a connection to the loopback may take before the app counts as not there.
const CONNECT: Duration = Duration::from_secs(2);

/// What the endpoint file says.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Endpoint {
    /// The port of this launch.
    pub port: u16,
    /// The installation's secret: the reader's own command line, and pairing.
    pub secret: String,
    /// The process that wrote it.
    #[serde(default)]
    pub pid: u32,
}

/// The endpoint file in the settings folder, read; `None` when there is none or it
/// cannot be read.
pub fn endpoint(dir: &Path) -> Option<Endpoint> {
    let text = std::fs::read_to_string(dir.join("automation.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// One answer from the endpoint: its status and its body, read as JSON where it is.
#[derive(Debug)]
pub struct Said {
    /// The HTTP status.
    pub status: u16,
    /// The body; a string when it was not JSON.
    pub body: Value,
}

/// One verb posted to the endpoint, with `bearer` as its credential, waiting at most
/// `patience` for the answer. An error is the endpoint not being there at all.
pub fn post(
    endpoint: &Endpoint,
    bearer: Option<&str>,
    body: &Value,
    patience: Duration,
) -> std::io::Result<Said> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, endpoint.port));
    let mut stream = TcpStream::connect_timeout(&address, CONNECT)?;
    stream.set_read_timeout(Some(patience))?;
    stream.set_write_timeout(Some(CONNECT))?;

    let body = body.to_string();
    let credential = bearer
        .map(|token| format!("authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "POST / HTTP/1.1\r\nhost: 127.0.0.1:{}\r\ncontent-type: application/json\r\n{credential}content-length: {}\r\nconnection: close\r\n\r\n",
        endpoint.port,
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()?;

    // The endpoint answers once and closes, so the answer is everything until then.
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes)?;
    parse(&bytes)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "not an HTTP answer"))
}

/// An HTTP answer: the status off the first line, the body after the blank line.
fn parse(bytes: &[u8]) -> Option<Said> {
    let text = String::from_utf8_lossy(bytes);
    let (head, body) = text.split_once("\r\n\r\n")?;
    let status = head
        .lines()
        .next()?
        .split(' ')
        .nth(1)?
        .parse::<u16>()
        .ok()?;
    let body = serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.to_owned()));
    Some(Said { status, body })
}

/// Whether nib answers on the file's port: asked with no credential at all, which nib
/// refuses in its own words. Anything else listening there is not told a secret.
pub fn answers(endpoint: &Endpoint) -> bool {
    let asked = serde_json::json!({ "verb": "agent_status" });
    post(endpoint, None, &asked, CONNECT).is_ok_and(|said| {
        said.status == 401 && said.body.get("error").is_some_and(Value::is_string)
    })
}

/// The app's endpoint, when an app is answering on it.
pub fn running(dir: &Path) -> Option<Endpoint> {
    endpoint(dir).filter(answers)
}

/// This process's stdin, stdout and stderr kept to itself. They are the client's pipes,
/// handed over inheritable, and `start` passes every inheritable handle on to the program
/// it starts - measured: a pipe cmd was given stayed open until the started program
/// ended. The app would then hold the client's stdout for as long as nib runs, and a
/// client waiting for this server's output to end would wait for nib to quit. The app
/// asks the same before it starts Claude Code or Codex; see `ai_cli/family.rs`.
#[allow(
    unsafe_code,
    reason = "whether a handle is inherited is Win32's own flag, with no safe wrapper"
)]
#[cfg(windows)]
pub(crate) fn keep_the_pipes() {
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::Foundation::{
        SetHandleInformation, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT,
    };

    let handles = [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ];
    for raw in handles.into_iter().filter(|one| !one.is_null()) {
        // SAFETY: each is one of this process's own standard handles, open for as long as
        // it runs; only whether a child inherits it changes.
        let _ =
            unsafe { SetHandleInformation(HANDLE(raw), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn an_answer_is_its_status_and_its_json() {
        let said = parse(b"HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\n\r\n{\"error\":\"no\"}")
            .expect("an answer");
        assert_eq!(said.status, 401);
        assert_eq!(said.body["error"], "no");
        let said = parse(b"HTTP/1.1 200 OK\r\n\r\nnot json").expect("an answer");
        assert_eq!(said.body, Value::String("not json".into()));
        assert!(parse(b"garbage").is_none());
    }

    #[test]
    fn the_file_reads_as_the_app_writes_it() {
        let dir = tempfile::tempdir().expect("a folder");
        assert_eq!(endpoint(dir.path()), None);
        std::fs::write(
            dir.path().join("automation.json"),
            r#"{"port":4321,"secret":"ab","eval":false,"pid":77}"#,
        )
        .expect("written");
        assert_eq!(
            endpoint(dir.path()),
            Some(Endpoint {
                port: 4321,
                secret: "ab".into(),
                pid: 77
            })
        );
    }

    /// Something else on the port is not nib, and is never sent a credential: the
    /// question that decides it carries none.
    #[test]
    fn a_stranger_on_the_port_is_not_nib() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        let port = listener.local_addr().expect("its address").port();
        let heard = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("a caller");
            let mut request = vec![0_u8; 4096];
            let read = stream.read(&mut request).expect("a request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nhi")
                .expect("an answer");
            String::from_utf8_lossy(&request[..read]).to_lowercase()
        });
        let stranger = Endpoint {
            port,
            secret: "s3cret".into(),
            pid: 0,
        };
        assert!(!answers(&stranger));
        let request = heard.join().expect("the stranger");
        assert!(!request.contains("authorization"), "{request}");
        assert!(!request.contains("s3cret"));
    }

    #[test]
    fn nobody_on_the_port_is_not_nib() {
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|one| one.local_addr())
            .expect("a port")
            .port();
        // The listener is gone: the port now answers nothing.
        assert!(!answers(&Endpoint {
            port,
            secret: String::new(),
            pid: 0
        }));
    }
}
