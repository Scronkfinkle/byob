//! Connections: TCP with deadlines, rustls (ring, webpki roots: the same stack ureq links),
//! and a byte budget per connection so a hostile server can't make us buffer without end.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

/// Connecting to one address.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// One read or write.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Most bytes read from one connection (a session is a handful of commands).
pub const MAX_READ: u64 = 48 * 1024 * 1024;

/// How a connection is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// TLS from the first byte (IMAP 993, SMTP 465).
    Tls,
    /// Plain first, then `STARTTLS` (required: no fallback to plain).
    StartTls,
    /// No encryption: only for local test servers.
    Plain,
}

impl Security {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "tls" | "ssl" | "ssl/tls" | "implicit" => Self::Tls,
            "starttls" | "start_tls" | "start-tls" => Self::StartTls,
            "none" | "plain" | "off" | "insecure" => Self::Plain,
            _ => return None,
        })
    }
}

/// A connected stream: plain TCP or rustls over it, with a read budget.
pub struct Stream {
    inner: Inner,
    budget: u64,
}

enum Inner {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Stream {
    pub fn plain(tcp: TcpStream) -> Self {
        Self { inner: Inner::Plain(tcp), budget: MAX_READ }
    }

    /// Wrap `tcp` in TLS for `host` (the handshake runs on first use, under the deadlines).
    pub fn tls(tcp: TcpStream, host: &str) -> io::Result<Self> {
        let name = ServerName::try_from(host.to_owned()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{host:?} isn't a host name")))?;
        let conn = ClientConnection::new(tls_config(), name).map_err(io::Error::other)?;
        let mut s = StreamOwned::new(conn, tcp);
        // Handshake now, so a bad certificate is reported as such.
        while s.conn.is_handshaking() {
            s.conn.complete_io(&mut s.sock)?;
        }
        Ok(Self { inner: Inner::Tls(Box::new(s)), budget: MAX_READ })
    }

    pub fn is_tls(&self) -> bool {
        matches!(self.inner, Inner::Tls(_))
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.budget == 0 {
            return Err(io::Error::other(format!("the server sent more than {} MB on one connection", MAX_READ / (1024 * 1024))));
        }
        let want = buf.len().min(usize::try_from(self.budget).unwrap_or(usize::MAX));
        let buf = buf.get_mut(..want).unwrap_or_default();
        let n = match &mut self.inner {
            Inner::Plain(s) => s.read(buf)?,
            Inner::Tls(s) => s.read(buf)?,
        };
        self.budget = self.budget.saturating_sub(n as u64);
        Ok(n)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.inner {
            Inner::Plain(s) => s.write(buf),
            Inner::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.inner {
            Inner::Plain(s) => s.flush(),
            Inner::Tls(s) => s.flush(),
        }
    }
}

fn tls_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    Arc::clone(CONFIG.get_or_init(|| {
        let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = match ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions() {
            Ok(b) => b.with_root_certificates(roots).with_no_client_auth(),
            // ring supports the default versions; this only keeps us from panicking.
            Err(_) => ClientConfig::builder().with_root_certificates(RootCertStore::empty()).with_no_client_auth(),
        };
        Arc::new(config)
    }))
}

/// TCP to `host:port`, trying each address with [`CONNECT_TIMEOUT`]; reads and writes get
/// [`IO_TIMEOUT`].
pub fn connect(host: &str, port: u16) -> io::Result<TcpStream> {
    let addrs: Vec<_> = (host, port).to_socket_addrs()?.take(8).collect();
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host} has no address"));
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => {
                s.set_read_timeout(Some(IO_TIMEOUT))?;
                s.set_write_timeout(Some(IO_TIMEOUT))?;
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// One CRLF-terminated line from `r`, byte by byte (before TLS: nothing may be read ahead),
/// at most `max` bytes.
pub fn read_line(r: &mut impl Read, max: usize) -> io::Result<String> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    loop {
        if r.read(&mut b)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the server closed the connection"));
        }
        if b[0] == b'\n' {
            break;
        }
        if line.len() >= max {
            return Err(io::Error::other("the server sent an over-long line"));
        }
        line.push(b[0]);
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_names() {
        assert_eq!(Security::parse(" TLS "), Some(Security::Tls));
        assert_eq!(Security::parse("starttls"), Some(Security::StartTls));
        assert_eq!(Security::parse("none"), Some(Security::Plain));
        assert_eq!(Security::parse("maybe"), None);
    }

    #[test]
    fn the_read_budget_stops_a_flood() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let chunk = vec![b'x'; 64 * 1024];
                while s.write_all(&chunk).is_ok() {}
            }
        });
        let mut s = Stream::plain(connect("127.0.0.1", addr.port()).unwrap());
        s.budget = 100_000;
        let mut sink = Vec::new();
        let err = s.read_to_end(&mut sink).unwrap_err();
        assert!(err.to_string().contains("more than"), "{err}");
        assert_eq!(sink.len(), 100_000);
    }

    #[test]
    fn lines_are_capped() {
        let mut r: &[u8] = b"* OK hi\r\nrest";
        assert_eq!(read_line(&mut r, 100).unwrap(), "* OK hi");
        let mut r: &[u8] = b"aaaaaaaaaa\r\n";
        assert!(read_line(&mut r, 4).is_err());
    }
}
