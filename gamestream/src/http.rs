//! The host's HTTP API. Pairing and status use plain HTTP; everything a
//! paired client may do uses HTTPS with the client certificate, and the host
//! must show the certificate it showed while pairing.

use crate::{Identity, Result, identity::HostCertificate};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, ring::default_provider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::{sync::Arc, time::Duration};

pub const HTTP_PORT: u16 = 47989;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Starting an app can take the host a long time.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, PartialEq)]
pub struct Host {
    pub address: String,
    pub http_port: u16,
    pub https_port: u16,
    /// PEM of the certificate the host showed while pairing.
    pub certificate: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ServerInfo {
    pub name: String,
    pub unique_id: String,
    pub mac: String,
    /// The first number is the protocol generation.
    pub app_version: String,
    pub https_port: u16,
    pub paired: bool,
    /// The ID of the running app; 0 when the host is free.
    pub current_app: u32,
    pub codec_support: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct App {
    pub id: u32,
    pub title: String,
    pub hdr: bool,
}

/// What the client asks the host to stream.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Launch {
    pub app: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub hdr: bool,
    /// Keep the sound on the PC's own speakers too.
    pub host_audio: bool,
    /// The other app that may be ended if the PC still runs it. Any other
    /// running app refuses the launch: ending one can cost the user their game.
    pub end_other: Option<u32>,
}

/// How to start an app on a host.
#[derive(Debug, PartialEq)]
enum Start { Launch, Resume, EndOtherThenLaunch }

/// How to start `app` on a host that runs `running`, or nothing when 0.
fn start(running: u32, app: u32, end_other: Option<u32>) -> Result<Start> {
    match running {
        0 => Ok(Start::Launch),
        running if running == app => Ok(Start::Resume),
        running if end_other == Some(running) => Ok(Start::EndOtherThenLaunch),
        _ => Err("The PC is running another app".into()),
    }
}

impl ServerInfo {
    /// Whether the host can encode HDR, which takes 10-bit HEVC.
    pub fn encodes_hdr(&self) -> bool {
        const HEVC_MAIN10: u32 = 0x200;
        self.codec_support & HEVC_MAIN10 != 0
    }
}

/// A started app, ready for the stream handshake.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub rtsp_url: String,
    /// The AES key of this session, chosen by the client.
    pub key: [u8; 16],
    /// The first four bytes of the session's initialisation vectors.
    pub key_id: u32,
}

fn launch_query(request: &Launch, key: &[u8; 16], key_id: u32) -> String {
    // Stereo: channel mask 3 in the high word, two channels in the low word.
    const STEREO: u32 = 3 << 16 | 2;
    let hdr = if request.hdr {
        "&hdrMode=1&clientHdrCapVersion=0&clientHdrCapSupportedFlagsInUint32=0\
         &clientHdrCapMetaDataId=NV_STATIC_METADATA_TYPE_1&clientHdrCapDisplayData=0x0x0x0x0x0x0x0x0x0x0"
    } else { "" };
    // `sops=1` lets the host set its display to the requested resolution.
    format!("appid={}&mode={}x{}x{}&additionalStates=1&sops=1&rikey={}&rikeyid={key_id}{hdr}\
        &localAudioPlayMode={}&surroundAudioInfo={STEREO}&remoteControllersBitmap=1&gcmap=1&gcpersist=0",
        request.app, request.width, request.height, request.fps, hex::encode(key), u8::from(request.host_audio))
}

impl Host {
    /// Starts the app, or rejoins it when it is the one already running.
    /// Another running app is ended first only when the request names it.
    pub fn launch(&self, identity: &Identity, info: &ServerInfo, request: &Launch) -> Result<Session> {
        let verb = match start(info.current_app, request.app, request.end_other)? {
            Start::Launch => "launch",
            Start::Resume => "resume",
            Start::EndOtherThenLaunch => { self.cancel(identity)?; "launch" }
        };
        let mut key = [0u8; 16];
        rand::fill(&mut key);
        // The host reads the ID as a signed number.
        let key_id = rand::random::<u32>() >> 1;
        let xml = self.get(identity, true, verb, &launch_query(request, &key, key_id), Some(LAUNCH_TIMEOUT))?;
        check_status(&xml)?;
        let rtsp_url = value(&xml, "sessionUrl0").filter(|url| !url.is_empty())
            .ok_or("The PC did not start the stream")?;
        Ok(Session { rtsp_url, key, key_id })
    }

    /// Ends the running app, which also restores the PC's display.
    pub fn cancel(&self, identity: &Identity) -> Result<()> {
        check_status(&self.get(identity, true, "cancel", "", Some(LAUNCH_TIMEOUT))?)
    }

    pub fn new(address: &str) -> Self {
        Self { address: address.to_owned(), http_port: HTTP_PORT, https_port: 0, certificate: None }
    }

    /// Asks over HTTPS when paired, because only that answer says so.
    pub fn info(&mut self, identity: &Identity) -> Result<ServerInfo> {
        if self.certificate.is_none() || self.https_port == 0 {
            let info = self.ask(identity, false)?;
            self.https_port = info.https_port;
            if self.certificate.is_none() { return Ok(info); }
        }
        // A host that no longer accepts this pairing still answers in plain.
        let info = self.ask(identity, true).or_else(|_| self.ask(identity, false))?;
        self.https_port = info.https_port;
        Ok(info)
    }

    fn ask(&self, identity: &Identity, secure: bool) -> Result<ServerInfo> {
        let xml = self.get(identity, secure, "serverinfo", "", Some(REQUEST_TIMEOUT))?;
        check_status(&xml)?;
        let text = |tag| value(&xml, tag).unwrap_or_default();
        Ok(ServerInfo {
            name: text("hostname"),
            unique_id: text("uniqueid"),
            mac: text("mac"),
            app_version: text("appversion"),
            https_port: text("HttpsPort").parse().unwrap_or(47984),
            paired: text("PairStatus") == "1",
            current_app: text("currentgame").parse().unwrap_or(0),
            codec_support: text("ServerCodecModeSupport").parse().unwrap_or(0),
        })
    }

    pub fn apps(&self, identity: &Identity) -> Result<Vec<App>> {
        let xml = self.get(identity, true, "applist", "", Some(REQUEST_TIMEOUT))?;
        check_status(&xml)?;
        Ok(xml.split("<App>").skip(1).filter_map(|app| {
            Some(App {
                id: value(app, "ID")?.parse().ok()?,
                title: value(app, "AppTitle")?,
                hdr: value(app, "IsHdrSupported").as_deref() == Some("1"),
            })
        }).collect())
    }

    /// One GET; returns the XML body. Without a timeout it waits for as long
    /// as the host takes, which pairing needs while the PIN is being entered.
    pub(crate) fn get(&self, identity: &Identity, secure: bool, path: &str, query: &str,
        timeout: Option<Duration>) -> Result<String>
    {
        let port = if secure { self.https_port } else { self.http_port };
        let address = (self.address.as_str(), port).to_socket_addrs()
            .map_err(|e| format!("Cannot resolve {}: {e}", self.address))?
            .next().ok_or_else(|| format!("Cannot resolve {}", self.address))?;
        let mut socket = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
            .map_err(|e| format!("Cannot reach {}: {e}", self.address))?;
        socket.set_read_timeout(timeout).and_then(|()| socket.set_write_timeout(Some(REQUEST_TIMEOUT)))
            .map_err(|e| e.to_string())?;
        let mut request_id = [0u8; 16];
        rand::fill(&mut request_id);
        let request = format!(
            "GET /{path}?uniqueid=0123456789ABCDEF&uuid={}{}{query} HTTP/1.1\r\nHost: {}:{port}\r\nConnection: close\r\n\r\n",
            hex::encode(request_id), if query.is_empty() { "" } else { "&" }, self.address);
        let response = if secure {
            let mut tls = self.tls(identity)?;
            let mut stream = rustls::Stream::new(&mut tls, &mut socket);
            exchange(&mut stream, &request)
        } else {
            exchange(&mut socket, &request)
        }.map_err(|e| format!("Request to {} failed: {e}", self.address))?;
        body(&response)
    }

    fn tls(&self, identity: &Identity) -> Result<rustls::ClientConnection> {
        let pinned = self.certificate.as_deref().ok_or("This PC is not paired")?;
        let provider = Arc::new(default_provider());
        let verifier = Pinned {
            certificate: HostCertificate::from_pem(pinned)?.der()?,
            algorithms: provider.signature_verification_algorithms,
        };
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions().map_err(|e| e.to_string())?
            .dangerous().with_custom_certificate_verifier(Arc::new(verifier))
            .with_client_auth_cert(vec![CertificateDer::from(identity.certificate_der()?)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der()?)))
            .map_err(|e| e.to_string())?;
        let name = ServerName::try_from(self.address.clone()).map_err(|e| e.to_string())?;
        rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| e.to_string())
    }
}

/// Accepts exactly the certificate the host showed while pairing. Hosts use
/// self-signed certificates, so there is no chain or name to check.
#[derive(Debug)]
struct Pinned {
    certificate: Vec<u8>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
        _name: &ServerName<'_>, _ocsp: &[u8], _now: UnixTime)
        -> std::result::Result<ServerCertVerified, rustls::Error>
    {
        if end_entity.as_ref() == self.certificate {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure))
        }
    }

    fn verify_tls12_signature(&self, message: &[u8], certificate: &CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct)
        -> std::result::Result<HandshakeSignatureValid, rustls::Error>
    {
        rustls::crypto::verify_tls12_signature(message, certificate, signature, &self.algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], certificate: &CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct)
        -> std::result::Result<HandshakeSignatureValid, rustls::Error>
    {
        rustls::crypto::verify_tls13_signature(message, certificate, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

fn exchange(stream: &mut (impl Read + Write), request: &str) -> std::io::Result<Vec<u8>> {
    stream.write_all(request.as_bytes())?;
    let mut response = Vec::new();
    match stream.read_to_end(&mut response) {
        // Hosts close the connection without a TLS goodbye.
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof && !response.is_empty() => {}
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    Ok(response)
}

fn body(response: &[u8]) -> Result<String> {
    let text = String::from_utf8_lossy(response);
    let (head, body) = text.split_once("\r\n\r\n").ok_or("The PC sent an incomplete answer")?;
    let status = head.split_whitespace().nth(1).unwrap_or_default();
    if status != "200" { return Err(format!("The PC answered HTTP {status}")); }
    Ok(body.to_owned())
}

/// The text of the first `<tag>…</tag>`.
pub(crate) fn value(xml: &str, tag: &str) -> Option<String> {
    let rest = xml.split_once(&format!("<{tag}>"))?.1;
    let text = rest.split_once(&format!("</{tag}>"))?.0;
    Some(text.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">")
        .replace("&quot;", "\"").replace("&apos;", "'"))
}

/// The host reports failures in the root element, with HTTP status 200.
pub(crate) fn check_status(xml: &str) -> Result<()> {
    let attribute = |name: &str| {
        let rest = xml.split_once(&format!("{name}=\""))?.1;
        Some(rest.split_once('"')?.0.to_owned())
    };
    match attribute("status_code").as_deref() {
        Some("200") => Ok(()),
        Some(code) => Err(attribute("status_message").filter(|message| !message.is_empty())
            .unwrap_or_else(|| format!("The PC refused the request ({code})"))),
        None => Err("The PC sent an unexpected answer".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_values_and_the_root_status() {
        let xml = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\">\
            <hostname>gaming-pc</hostname><AppTitle>Tom &amp; Jerry</AppTitle></root>";
        assert_eq!(value(xml, "hostname").as_deref(), Some("gaming-pc"));
        assert_eq!(value(xml, "AppTitle").as_deref(), Some("Tom & Jerry"));
        assert_eq!(value(xml, "missing"), None);
        assert_eq!(check_status(xml), Ok(()));
        let refused = "<root status_code=\"401\" status_message=\"The client is not authorized\"/>";
        assert_eq!(check_status(refused), Err("The client is not authorized".into()));
        assert!(check_status("<html/>").is_err());
    }

    #[test]
    fn a_host_encodes_hdr_when_it_has_ten_bit_hevc() {
        let host = |codec_support| ServerInfo { name: String::new(), unique_id: String::new(), mac: String::new(),
            app_version: String::new(), https_port: 0, paired: true, current_app: 0, codec_support };
        // What the gaming PC reports: H.264, HEVC, HEVC Main10, AV1 and more.
        assert!(host(0x1F0301).encodes_hdr());
        // H.264 and 8-bit HEVC only.
        assert!(!host(0x101).encodes_hdr());
    }

    #[test]
    fn only_the_named_running_app_is_ended() {
        assert_eq!(start(0, 7, None), Ok(Start::Launch));
        assert_eq!(start(7, 7, None), Ok(Start::Resume));
        assert_eq!(start(7, 7, Some(9)), Ok(Start::Resume));
        assert!(start(9, 7, None).is_err());
        assert_eq!(start(9, 7, Some(9)), Ok(Start::EndOtherThenLaunch));
        // The user agreed to end app 9; app 8 took its place since.
        assert!(start(8, 7, Some(9)).is_err());
    }

    #[test]
    fn asks_for_the_mode_and_adds_hdr_only_on_request() {
        let request = Launch { app: 881448767, width: 1920, height: 1080, fps: 120, hdr: false, host_audio: false,
            end_other: None };
        let query = launch_query(&request, &[0xab; 16], 7);
        assert!(query.starts_with("appid=881448767&mode=1920x1080x120&additionalStates=1&sops=1&rikey=abababab"));
        assert!(query.ends_with("&rikeyid=7&localAudioPlayMode=0&surroundAudioInfo=196610&remoteControllersBitmap=1&gcmap=1&gcpersist=0"));
        let query = launch_query(&Launch { hdr: true, host_audio: true, ..request }, &[0; 16], 7);
        assert!(query.contains("&rikeyid=7&hdrMode=1&clientHdrCapVersion=0&"));
        assert!(query.contains("0x0x0x0&localAudioPlayMode=1&"));
        assert!(!query.contains(' '));
    }

    #[test]
    fn takes_the_body_of_a_successful_answer_only() {
        assert_eq!(body(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n<root/>").as_deref(), Ok("<root/>"));
        assert!(body(b"HTTP/1.1 404 Not Found\r\n\r\n").is_err());
        assert!(body(b"garbage").is_err());
    }
}
