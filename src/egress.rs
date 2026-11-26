use http::uri::Authority;
use std::{
    fmt,
    io::{self, Error, ErrorKind},
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, lookup_host},
    time::{sleep, timeout},
};

// Intentionally don't derive Debug: credentials must never be printed.
#[derive(Clone)]
pub enum Route {
    Direct,
    Socks {
        server: Authority,
        auth: Option<(Vec<u8>, Vec<u8>)>,
    },
}

impl fmt::Debug for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.mode())
    }
}
#[derive(Clone, Copy)]
pub struct Target<'a> {
    pub host: &'a str,
    pub port: u16,
}
impl<'a> Target<'a> {
    pub fn new(addr: &'a Authority, default: Option<u16>) -> io::Result<Self> {
        if addr.as_str().contains('@') {
            return Err(invalid("userinfo is not allowed in a destination"));
        }
        let port = if addr.port().is_some() {
            addr.port_u16()
        } else {
            default
        }
        .filter(|p| *p != 0)
        .ok_or_else(|| invalid("valid destination port required"))?;
        let host = addr.host().trim_start_matches('[').trim_end_matches(']');
        if host.is_empty() || addr.port().is_none() && addr.as_str() != addr.host() {
            return Err(invalid("invalid destination"));
        }
        Ok(Self { host, port })
    }
}

fn invalid(message: &'static str) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}
fn failed(message: &'static str) -> Error {
    Error::other(message)
}

fn decode(input: &str) -> io::Result<Vec<u8>> {
    fn hex(c: u8) -> Option<u8> {
        (c as char).to_digit(16).map(|n| n as u8)
    }
    let mut out = Vec::with_capacity(input.len());
    let mut src = input.bytes();
    while let Some(c) = src.next() {
        if c == b'%' {
            let hi = src
                .next()
                .and_then(hex)
                .ok_or_else(|| invalid("bad credential escape"))?;
            let lo = src
                .next()
                .and_then(hex)
                .ok_or_else(|| invalid("bad credential escape"))?;
            out.push(hi << 4 | lo);
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

impl Route {
    pub fn mode(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Socks { .. } => "socks5",
        }
    }

    pub fn parse(raw: &str) -> io::Result<Self> {
        let raw = raw.trim();
        if raw == "direct" {
            return Ok(Self::Direct);
        }
        if raw.len() > 4096 || raw.bytes().any(|x| x.is_ascii_whitespace()) {
            return Err(invalid("invalid upstream URI"));
        }
        let (scheme, rest) = raw
            .split_once("://")
            .ok_or_else(|| invalid("upstream URI needs a scheme"))?;
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.bytes().any(|c| matches!(c, b'/' | b'?' | b'#')) {
            return Err(invalid("upstream URI must contain only authority"));
        }
        let (raw_auth, server) = rest
            .rsplit_once('@')
            .map_or((None, rest), |(u, h)| (Some(u), h));
        let server: Authority = server
            .parse()
            .map_err(|_| invalid("invalid upstream authority"))?;
        Target::new(&server, None)?;
        let auth = raw_auth
            .map(|value| {
                let (u, p) = value
                    .split_once(':')
                    .ok_or_else(|| invalid("upstream auth requires user:password"))?;
                let user = decode(u)?;
                let password = decode(p)?;
                if user.is_empty()
                    || password.is_empty()
                    || user.len() > 255
                    || password.len() > 255
                    || user.contains(&b':')
                {
                    return Err(invalid("invalid upstream credentials"));
                }
                Ok::<_, Error>((user, password))
            })
            .transpose()?;
        match scheme {
            "socks5" | "socks5h" => Ok(Self::Socks { server, auth }),
            _ => Err(invalid("meow-rs must expose SOCKS5 to the gateway")),
        }
    }

    /// All browser destinations must already be resolved and checked by policy.
    /// Never let an upstream re-resolve the visitor-controlled destination.
    pub async fn connect_pinned(&self, destination: SocketAddr) -> io::Result<TcpStream> {
        timeout(Duration::from_secs(10), async {
            match self {
                Self::Direct => {
                    let stream = TcpStream::connect(destination).await?;
                    stream.set_nodelay(true)?;
                    Ok(stream)
                }
                Self::Socks { server, auth, .. } => {
                    let mut socket = dial(Target::new(server, None)?).await?;
                    socks(&mut socket, destination, auth.as_ref()).await?;
                    Ok(socket)
                }
            }
        })
        .await
        .map_err(|_| Error::new(ErrorKind::TimedOut, "connection deadline"))?
    }
}

async fn try_addresses(addresses: impl Iterator<Item = SocketAddr>) -> io::Result<TcpStream> {
    let mut err = failed("no reachable address");
    for addr in addresses {
        match TcpStream::connect(addr).await {
            Ok(s) => return Ok(s),
            Err(e) => err = e,
        }
    }
    Err(err)
}

async fn dial(target: Target<'_>) -> io::Result<TcpStream> {
    let stream = if let Ok(ip) = target.host.parse::<IpAddr>() {
        TcpStream::connect(SocketAddr::new(ip, target.port)).await?
    } else {
        let mut addrs = lookup_host((target.host, target.port)).await?;
        let first = addrs.next().ok_or_else(|| failed("no DNS address"))?;
        let mut primary = vec![first];
        let mut alternative = Vec::new();
        for addr in addrs.take(15) {
            if addr.is_ipv4() == first.is_ipv4() {
                primary.push(addr);
            } else {
                alternative.push(addr);
            }
        }
        if alternative.is_empty() {
            try_addresses(primary.into_iter()).await?
        } else {
            let p = try_addresses(primary.into_iter());
            tokio::pin!(p);
            tokio::select! {
                result = &mut p => match result {
                    Ok(s) => s,
                    Err(_) => try_addresses(alternative.into_iter()).await?,
                },
                _ = sleep(Duration::from_millis(250)) => {
                    let alt = try_addresses(alternative.into_iter());
                    tokio::pin!(alt);
                    tokio::select! {
                        result = &mut p => match result {
                            Ok(s) => s, Err(_) => alt.await?,
                        },
                        result = &mut alt => match result {
                            Ok(s) => s, Err(_) => p.await?,
                        },
                    }
                }
            }
        }
    };
    stream.set_nodelay(true)?;
    Ok(stream)
}

async fn socks(
    stream: &mut TcpStream,
    destination: SocketAddr,
    auth: Option<&(Vec<u8>, Vec<u8>)>,
) -> io::Result<()> {
    // Negotiate precisely the configured auth mode, no silent downgrade.
    let method = if auth.is_some() { 2 } else { 0 };
    stream.write_all(&[5, 1, method]).await?;
    let mut answer = [0; 2];
    stream.read_exact(&mut answer).await?;
    if answer != [5, method] {
        return Err(failed("SOCKS5 authentication method rejected"));
    }
    if let Some((user, password)) = auth {
        let mut packet = Vec::with_capacity(3 + user.len() + password.len());
        packet.extend([1, user.len() as u8]);
        packet.extend(user);
        packet.push(password.len() as u8);
        packet.extend(password);
        stream.write_all(&packet).await?;
        stream.read_exact(&mut answer).await?;
        if answer != [1, 0] {
            return Err(failed("SOCKS5 authentication rejected"));
        }
    }
    let mut packet = [0u8; 257];
    packet[..3].copy_from_slice(&[5, 1, 0]);
    let end = match destination.ip() {
        IpAddr::V4(addr) => {
            packet[3] = 1;
            packet[4..8].copy_from_slice(&addr.octets());
            8
        }
        IpAddr::V6(addr) => {
            packet[3] = 4;
            packet[4..20].copy_from_slice(&addr.octets());
            20
        }
    };
    packet[end..end + 2].copy_from_slice(&destination.port().to_be_bytes());
    stream.write_all(&packet[..end + 2]).await?;
    let mut head = [0; 4];
    stream.read_exact(&mut head).await?;
    if head[0] != 5 || head[2] != 0 {
        return Err(failed("malformed SOCKS5 reply"));
    }
    if head[1] != 0 {
        return Err(Error::new(
            ErrorKind::AddrNotAvailable,
            "SOCKS5 upstream refused destination",
        ));
    }
    let n = match head[3] {
        1 => 4,
        4 => 16,
        3 => stream.read_u8().await? as usize,
        _ => return Err(failed("malformed SOCKS5 bound address")),
    };
    stream.read_exact(&mut packet[..n + 2]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn socks_pins_ipv4_and_sends_configured_auth() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let route = Route::parse(&format!("socks5h://a:b@{}", l.local_addr().unwrap())).unwrap();
        let peer = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut data = [0; 10];
            s.read_exact(&mut data[..3]).await.unwrap();
            assert_eq!(&data[..3], &[5, 1, 2]);
            s.write_all(&[5, 2]).await.unwrap();
            s.read_exact(&mut data[..5]).await.unwrap();
            assert_eq!(&data[..5], &[1, 1, b'a', 1, b'b']);
            s.write_all(&[1, 0]).await.unwrap();
            s.read_exact(&mut data).await.unwrap();
            assert_eq!(data, [5, 1, 0, 1, 1, 1, 1, 1, 1, 187]);
            s.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                .await
                .unwrap();
            s.write_all(b"Y").await.unwrap();
        });
        let mut s = route
            .connect_pinned("1.1.1.1:443".parse().unwrap())
            .await
            .unwrap();
        assert_eq!(s.read_u8().await.unwrap(), b'Y');
        peer.await.unwrap();
    }
}
