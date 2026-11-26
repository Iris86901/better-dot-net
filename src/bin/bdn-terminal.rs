//! Private, loopback-only transport to explicitly named terminal services.
use anyhow::{Context, Result, bail, ensure};
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    io::{self, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const USAGE: &str = "\
bdn-terminal serve LOOPBACK_ADDR CERT_PEM PRIVATE_KEY_PEM POLICY_JSON
bdn-terminal connect LOOPBACK_ADDR TLS_SERVER_NAME CA_PEM SERVICE PRIVATE_TOKEN_FILE";
const ALPN: &[u8] = b"bdn-terminal/1";
const MAX_SERVICES: usize = 32;
const MAX_GRANTS: usize = 16;
const MAX_NAME: usize = 48;
const CONCURRENCY: usize = 16;
const CONFIG_MAX: u64 = 64 * 1024;
const PEM_MAX: u64 = 128 * 1024;
const HANDSHAKE: Duration = Duration::from_secs(5);
const STREAM_LIMIT: Duration = Duration::from_secs(15 * 60);
const ACCEPTED: u8 = 0;
const DENIED: u8 = 1;
const UNAVAILABLE: u8 = 2;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Route {
    name: String,
    target: SocketAddr,
    grant_sha256: Vec<String>,
}
struct Service {
    target: SocketAddr,
    grants: Vec<[u8; 32]>,
}
type Services = BTreeMap<String, Service>;

fn read_file(path: &str, bound: u64, private: bool) -> Result<Vec<u8>> {
    // Check and read the same open file. Caller owns the containing directory.
    let file = File::open(path).context("cannot open terminal configuration file")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= bound,
        "invalid terminal configuration file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            !private || metadata.permissions().mode() & 0o077 == 0,
            "terminal secret/policy file must be private"
        );
    }
    #[cfg(not(unix))]
    ensure!(!private, "private terminal files require Unix permissions");
    let mut data = Vec::with_capacity(metadata.len() as usize);
    file.take(bound + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= bound,
        "terminal configuration file exceeds limit"
    );
    Ok(data)
}

fn local(addr: SocketAddr) -> Result<SocketAddr> {
    // Canonical addresses avoid platform-specific mapping and need no DNS.
    ensure!(
        matches!(addr.ip(), std::net::IpAddr::V4(ip) if ip == std::net::Ipv4Addr::LOCALHOST)
            || matches!(addr.ip(), std::net::IpAddr::V6(ip) if ip == std::net::Ipv6Addr::LOCALHOST),
        "terminal TCP addresses must use 127.0.0.1 or ::1"
    );
    ensure!(addr.port() != 0, "terminal TCP port must be nonzero");
    if let SocketAddr::V6(addr) = addr {
        ensure!(
            addr.scope_id() == 0 && addr.flowinfo() == 0,
            "terminal IPv6 address must not have a scope"
        );
    }
    Ok(addr)
}

fn name_ok(name: &[u8]) -> bool {
    (1..=MAX_NAME).contains(&name.len())
        && name
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn decode_32(text: &[u8]) -> Result<[u8; 32]> {
    ensure!(text.len() == 64, "invalid terminal credential length");
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(text, &mut bytes).context("invalid terminal credential encoding")?;
    Ok(bytes)
}

fn load_services(path: &str, listen: SocketAddr) -> Result<Services> {
    let routes: Vec<Route> = serde_json::from_slice(&read_file(path, CONFIG_MAX, true)?)
        .context("invalid terminal policy")?;
    ensure!(
        (1..=MAX_SERVICES).contains(&routes.len()),
        "expected 1–32 terminal services"
    );
    let mut services = BTreeMap::new();
    let mut distinct = BTreeSet::new();
    for route in routes {
        ensure!(
            name_ok(route.name.as_bytes()),
            "invalid terminal service name"
        );
        let target = local(route.target)?;
        ensure!(
            target != listen,
            "terminal service cannot point to the terminal listener"
        );
        ensure!(
            (1..=MAX_GRANTS).contains(&route.grant_sha256.len()),
            "expected 1–16 grants per service"
        );
        let mut grants = Vec::with_capacity(route.grant_sha256.len());
        for hash in route.grant_sha256 {
            let digest = decode_32(hash.as_bytes())?;
            ensure!(
                distinct.insert(digest),
                "duplicate terminal credential digest"
            );
            grants.push(digest);
        }
        ensure!(
            services
                .insert(route.name, Service { target, grants })
                .is_none(),
            "duplicate terminal service"
        );
    }
    Ok(services)
}

fn certificate_chain(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    let data = read_file(path, PEM_MAX, false)?;
    let chain =
        CertificateDer::pem_slice_iter(&data).collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        (1..=16).contains(&chain.len()),
        "invalid terminal certificate chain"
    );
    Ok(chain)
}

fn fits_alpn(actual: Option<&[u8]>) -> Result<()> {
    ensure!(actual == Some(ALPN), "terminal TLS protocol mismatch");
    Ok(())
}

async fn serve_one(stream: TcpStream, tls: TlsAcceptor, services: Arc<Services>) -> Result<()> {
    stream.set_nodelay(true)?;
    let (mut client, mut target) = timeout(HANDSHAKE, async {
        let mut client = tls.accept(stream).await?;
        fits_alpn(client.get_ref().1.alpn_protocol())?;
        // Request length is capped before accessing a service or comparing a digest.
        let len = client.read_u8().await? as usize;
        let mut name = [0u8; MAX_NAME];
        if !(1..=MAX_NAME).contains(&len) {
            client.write_u8(DENIED).await?;
            client.shutdown().await?;
            bail!("terminal access denied");
        }
        client.read_exact(&mut name[..len]).await?;
        let mut token = [0u8; 32];
        client.read_exact(&mut token).await?;
        let digest: [u8; 32] = Sha256::digest(token).into();
        let service = if name_ok(&name[..len]) {
            services.get(str::from_utf8(&name[..len])?)
        } else {
            None
        };
        let mut allowed = subtle::Choice::from(0);
        for i in 0..MAX_GRANTS {
            let expected = service.and_then(|s| s.grants.get(i));
            let eq = expected.unwrap_or(&[0u8; 32]).ct_eq(&digest);
            allowed |= eq & subtle::Choice::from(expected.is_some() as u8);
        }
        let Some(service) = service.filter(|_| bool::from(allowed)) else {
            client.write_u8(DENIED).await?;
            client.shutdown().await?;
            bail!("terminal access denied");
        };
        let target = match TcpStream::connect(service.target).await {
            Ok(target) => target,
            Err(_) => {
                client.write_u8(UNAVAILABLE).await?;
                client.shutdown().await?;
                bail!("configured terminal service unavailable");
            }
        };
        target.set_nodelay(true)?;
        client.write_u8(ACCEPTED).await?;
        client.flush().await?;
        Ok::<_, anyhow::Error>((client, target))
    })
    .await
    .context("terminal handshake deadline")??;
    timeout(
        STREAM_LIMIT,
        io::copy_bidirectional(&mut client, &mut target),
    )
    .await
    .context("terminal session deadline")??;
    Ok(())
}

async fn serve(address: &str, cert_path: &str, key_path: &str, policy_path: &str) -> Result<()> {
    let address = local(address.parse().context("invalid terminal listen address")?)?;
    let services = Arc::new(load_services(policy_path, address)?);
    let key_data = read_file(key_path, PEM_MAX, true)?;
    let key = PrivateKeyDer::from_pem_slice(&key_data)?;
    let mut tls = ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_no_client_auth()
        .with_single_cert(certificate_chain(cert_path)?, key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let tls = TlsAcceptor::from(Arc::new(tls));
    let listener = TcpListener::bind(address).await?;
    let permits = Arc::new(Semaphore::new(CONCURRENCY));
    eprintln!("terminal relay ready");
    loop {
        let permit = Arc::clone(&permits).acquire_owned().await?;
        let (stream, _) = listener.accept().await?;
        let tls = tls.clone();
        let services = Arc::clone(&services);
        tokio::spawn(async move {
            let _permit = permit;
            // Do not print user input, raw addresses, credentials, or payloads.
            let _ = serve_one(stream, tls, services).await;
        });
    }
}

async fn connect(
    address: &str,
    server_name: &str,
    ca_path: &str,
    service: &str,
    token_path: &str,
) -> Result<()> {
    let address = local(address.parse().context("invalid terminal relay address")?)?;
    let server_name =
        ServerName::try_from(server_name.to_owned()).context("invalid terminal TLS server name")?;
    ensure!(name_ok(service.as_bytes()), "invalid terminal service name");
    let raw = read_file(token_path, 66, true)?;
    let token = decode_32(
        str::from_utf8(&raw)?
            .trim_end_matches(['\r', '\n'])
            .as_bytes(),
    )?;
    let mut roots = RootCertStore::empty();
    for cert in certificate_chain(ca_path)? {
        roots.add(cert)?;
    }
    let mut tls = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let tls = TlsConnector::from(Arc::new(tls));
    let stream = timeout(HANDSHAKE, async {
        let stream = TcpStream::connect(address)
            .await
            .context("cannot connect to terminal relay")?;
        stream.set_nodelay(true)?;
        let mut stream = tls
            .connect(server_name, stream)
            .await
            .context("terminal TLS verification failed")?;
        fits_alpn(stream.get_ref().1.alpn_protocol())?;
        // The credential is sent only after both peer verification and ALPN succeed.
        let mut request = Vec::with_capacity(1 + service.len() + token.len());
        request.push(service.len() as u8);
        request.extend_from_slice(service.as_bytes());
        request.extend_from_slice(&token);
        stream.write_all(&request).await?;
        stream.flush().await?;
        match stream.read_u8().await? {
            ACCEPTED => Ok(stream),
            DENIED => bail!("terminal access denied"),
            UNAVAILABLE => bail!("configured terminal service unavailable"),
            _ => bail!("invalid terminal relay response"),
        }
    })
    .await
    .context("terminal handshake deadline")??;
    let (mut receive, mut transmit) = io::split(stream);
    timeout(STREAM_LIMIT, async {
        let send = async {
            io::copy(&mut io::stdin(), &mut transmit).await?;
            transmit.shutdown().await
        };
        let read = async {
            let mut output = io::stdout();
            io::copy(&mut receive, &mut output).await?;
            output.flush().await
        };
        // Local EOF still allows the target to finish writing its response.
        // Target EOF ends the CLI even if a TTY has no further local input.
        tokio::pin!(send, read);
        tokio::select! {
            outcome = &mut send => {
                outcome?;
                read.await
            }
            outcome = &mut read => outcome,
        }
    })
    .await
    .context("terminal session deadline")??;
    Ok(())
}

fn main() -> Result<()> {
    let values = std::env::args().skip(1).collect::<Vec<_>>();
    let arguments = values.iter().map(String::as_str).collect::<Vec<_>>();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        match arguments.as_slice() {
            ["serve", address, cert, key, policy] => serve(address, cert, key, policy).await,
            ["connect", address, name, authority, service, token] => {
                connect(address, name, authority, service, token).await
            }
            ["-h" | "--help"] => {
                println!("{USAGE}");
                Ok(())
            }
            _ => bail!("{USAGE}"),
        }
    });
    // An interactive stdin read may still be blocked after the peer closes.
    runtime.shutdown_background();
    result
}
