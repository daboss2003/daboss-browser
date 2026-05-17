use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

use super::error::{Error, Result};

/// A read/write byte stream that is either a plain TCP socket or a TLS-wrapped one.
/// Hiding the difference behind one type means the HTTP layer doesn't care which
/// transport it's talking to.
pub struct Connection {
    inner: Inner,
}

enum Inner {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Connection {
    pub fn open(
        addr: SocketAddr,
        host: &str,
        use_tls: bool,
        tls: &Arc<ClientConfig>,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<Self> {
        let tcp = TcpStream::connect_timeout(&addr, connect_timeout).map_err(Error::Connect)?;
        tcp.set_read_timeout(Some(read_timeout))?;
        tcp.set_write_timeout(Some(read_timeout))?;
        tcp.set_nodelay(true)?;

        if use_tls {
            let server_name = ServerName::try_from(host)
                .map_err(|e| Error::Tls(format!("invalid server name {host}: {e}")))?
                .to_owned();
            let conn = ClientConnection::new(tls.clone(), server_name)
                .map_err(|e| Error::Tls(e.to_string()))?;
            Ok(Self {
                inner: Inner::Tls(Box::new(StreamOwned::new(conn, tcp))),
            })
        } else {
            Ok(Self {
                inner: Inner::Plain(tcp),
            })
        }
    }
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.inner {
            Inner::Plain(s) => s.read(buf),
            Inner::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match &mut self.inner {
            Inner::Plain(s) => s.write(buf),
            Inner::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.inner {
            Inner::Plain(s) => s.flush(),
            Inner::Tls(s) => s.flush(),
        }
    }
}

pub fn default_tls_config() -> ClientConfig {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    // Advertise HTTP/2 + HTTP/1.1 via ALPN so the h2 path can take
    // over when the server supports it. tokio-rustls (the async path)
    // re-uses this same config.
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    cfg
}
