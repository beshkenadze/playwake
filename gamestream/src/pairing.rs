//! Pairing: the user types a PIN shown by the client into the host, and both
//! sides prove they know it without sending it. Afterwards the host trusts
//! the client certificate and the client pins the host certificate.

use crate::http::{REQUEST_TIMEOUT, check_status, value};
use crate::identity::HostCertificate;
use crate::{Host, Identity, Result};
use aes::Aes128;
use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use sha2::{Digest, Sha256};

const STEP: &str = "devicename=roth&updateState=1&";

/// AES-128-ECB without padding; the data is whole blocks.
fn encrypt(key: &Aes128, data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    for block in out.chunks_exact_mut(16) { key.encrypt_block(block.try_into().unwrap()); }
    out
}

fn decrypt(key: &Aes128, data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    for block in out.chunks_exact_mut(16) { key.decrypt_block(block.try_into().unwrap()); }
    out
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    rand::fill(&mut bytes);
    bytes
}

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut hash = Sha256::new();
    for part in parts { hash.update(part); }
    hash.finalize().into()
}

/// A fresh four-digit PIN for the user to enter on the host.
pub fn pin() -> String {
    format!("{:04}", rand::random_range(0..10_000u32))
}

/// Pairs with a host of protocol generation 7 or later, which is every
/// Sunshine. Blocks while the user enters `pin` on the host. On success the
/// host certificate is stored in `host`.
pub fn pair(host: &mut Host, identity: &Identity, pin: &str) -> Result<()> {
    // The last step goes over HTTPS: know its port before the user enters the PIN.
    host.learn_https_port(identity)?;
    let result = exchange(host, identity, pin);
    if result.is_err() {
        // Leave the host ready for another attempt.
        let _ = host.get(identity, false, "unpair", "", Some(REQUEST_TIMEOUT));
    }
    result
}

fn exchange(host: &mut Host, identity: &Identity, pin: &str) -> Result<()> {
    let step = |host: &Host, secure: bool, query: String, wait: bool| -> Result<String> {
        let xml = host.get(identity, secure, "pair", &format!("{STEP}{query}"), (!wait).then_some(REQUEST_TIMEOUT))?;
        check_status(&xml)?;
        if value(&xml, "paired").as_deref() == Some("1") { Ok(xml) } else { Err("The PC refused to pair".into()) }
    };
    let hex_value = |xml: &str, tag: &str| -> Result<Vec<u8>> {
        hex::decode(value(xml, tag).unwrap_or_default()).map_err(|_| "The PC sent a malformed pairing answer".to_owned())
    };

    let salt: [u8; 16] = random();
    let key = Aes128::new_from_slice(&sha256(&[&salt, pin.as_bytes()])[..16]).unwrap();

    // The host answers this request only after the PIN is entered.
    let certificate = hex::encode(identity.certificate_pem()?);
    let xml = step(host, false, format!("phrase=getservercert&salt={}&clientcert={certificate}", hex::encode(salt)), true)?;
    let host_pem = String::from_utf8(hex_value(&xml, "plaincert")?).unwrap_or_default();
    if host_pem.is_empty() { return Err("The PC is already pairing with another device".into()); }
    let host_certificate = HostCertificate::from_pem(&host_pem)?;

    let challenge: [u8; 16] = random();
    let xml = step(host, false, format!("clientchallenge={}", hex::encode(encrypt(&key, &challenge))), false)?;
    let answer = decrypt(&key, &hex_value(&xml, "challengeresponse")?);
    if answer.len() < 48 { return Err("The PC sent a malformed pairing answer".into()); }
    let (host_hash, host_challenge) = (&answer[..32], &answer[32..48]);

    let secret: [u8; 16] = random();
    let proof = sha256(&[host_challenge, &identity.certificate_signature(), &secret]);
    let xml = step(host, false, format!("serverchallengeresp={}", hex::encode(encrypt(&key, &proof))), false)?;
    let pairing_secret = hex_value(&xml, "pairingsecret")?;
    if pairing_secret.len() <= 16 { return Err("The PC sent a malformed pairing answer".into()); }
    let (host_secret, host_signature) = pairing_secret.split_at(16);
    if !host_certificate.signed(host_secret, host_signature) {
        return Err("The PC's pairing answer is not signed by its certificate".into());
    }
    if sha256(&[&challenge, &host_certificate.signature(), host_secret]) != host_hash {
        return Err("Wrong PIN".into());
    }

    let mut client_secret = secret.to_vec();
    client_secret.extend(identity.sign(&secret));
    step(host, false, format!("clientpairingsecret={}", hex::encode(client_secret)), false)?;

    host.certificate = Some(host_pem);
    step(host, true, "phrase=pairchallenge".into(), false).inspect_err(|_| host.certificate = None)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_a_new_host_asks_its_https_port_first() {
        use crate::{Host, Identity};
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let mut asked = Vec::new();
            // serverinfo, then the PIN step (closed unanswered), then the unpair
            // that cleans up; a client that skips one is not waited for long.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while asked.len() < 3 && std::time::Instant::now() < deadline {
                let Ok((mut socket, _)) = listener.accept() else {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                };
                socket.set_nonblocking(false).unwrap();
                let mut request = [0u8; 8192];
                let read = socket.read(&mut request).unwrap_or(0);
                let line = String::from_utf8_lossy(&request[..read]).lines().next().unwrap_or_default().to_owned();
                if line.starts_with("GET /serverinfo") {
                    let xml = "<root status_code=\"200\"><hostname>den</hostname><HttpsPort>47990</HttpsPort></root>";
                    let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{xml}", xml.len());
                }
                asked.push(line);
            }
            asked
        });
        let mut host = Host { http_port: port, ..Host::new("127.0.0.1") };
        assert!(pair(&mut host, &Identity::generate().unwrap(), "1234").is_err());
        let asked = server.join().unwrap();
        assert!(asked[0].starts_with("GET /serverinfo"), "{asked:?}");
        assert!(asked[1].starts_with("GET /pair"), "{asked:?}");
        // The last step, over HTTPS, would now go to the port the host named, not to 0.
        assert_eq!(host.https_port, 47990);
    }

    #[test]
    fn ecb_round_trips_whole_blocks() {
        let key = Aes128::new_from_slice(&sha256(&[b"salt", b"1234"])[..16]).unwrap();
        let data = [7u8; 32];
        let sealed = encrypt(&key, &data);
        assert_ne!(sealed, data);
        assert_eq!(&sealed[..16], &sealed[16..]);
        assert_eq!(decrypt(&key, &sealed), data);
    }
}
