//! The client's RSA key and self-signed certificate. A host remembers the
//! certificate when it is paired and expects it on every HTTPS request.

use crate::Result;
use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::sha2::Sha256;
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use std::{str::FromStr, time::Duration};
use x509_cert::builder::{Builder, CertificateBuilder, profile::BuilderProfile};
use x509_cert::der::{DecodePem, Encode, EncodePem, referenced::OwnedToRef};
use x509_cert::ext::{ToExtension, pkix::BasicConstraints};
use x509_cert::{Certificate, name::Name, serial_number::SerialNumber, time::Validity};

const TWENTY_YEARS: Duration = Duration::from_secs(60 * 60 * 24 * 365 * 20);

/// A certificate signed by its own key. The one extension keeps it at X.509
/// version 3, which TLS libraries require.
struct SelfSigned(Name);

impl BuilderProfile for SelfSigned {
    fn get_issuer(&self, subject: &Name) -> Name { subject.clone() }

    fn get_subject(&self) -> Name { self.0.clone() }

    fn build_extensions(
        &self,
        _key: x509_cert::spki::SubjectPublicKeyInfoRef<'_>,
        _issuer_key: x509_cert::spki::SubjectPublicKeyInfoRef<'_>,
        certificate: &x509_cert::TbsCertificate,
    ) -> x509_cert::builder::Result<Vec<x509_cert::ext::Extension>> {
        let not_an_authority = BasicConstraints { ca: false, path_len_constraint: None };
        Ok(vec![not_an_authority.to_extension(certificate.subject(), &[])?])
    }
}

pub struct Identity {
    key: rsa::PrivateKey,
    certificate: Certificate,
}

fn failed(what: &str, error: impl std::fmt::Display) -> String {
    format!("{what}: {error}")
}

impl Identity {
    pub fn generate() -> Result<Self> {
        let key = rsa::PrivateKey::new(&mut rand::rng(), 2048).map_err(|e| failed("RSA key", e))?;
        let subject = Name::from_str("CN=NVIDIA GameStream Client").map_err(|e| failed("certificate name", e))?;
        let public = x509_cert::spki::SubjectPublicKeyInfo::from_key(&key.to_public_key())
            .map_err(|e| failed("public key", e))?;
        let validity = Validity::from_now(TWENTY_YEARS).map_err(|e| failed("certificate validity", e))?;
        let builder = CertificateBuilder::new(SelfSigned(subject), SerialNumber::from(0u32), validity, public)
            .map_err(|e| failed("certificate", e))?;
        let certificate = builder.build::<_, Signature>(&SigningKey::<Sha256>::new(key.clone()))
            .map_err(|e| failed("certificate signature", e))?;
        Ok(Self { key, certificate })
    }

    pub fn from_pem(key: &str, certificate: &str) -> Result<Self> {
        Ok(Self {
            key: rsa::PrivateKey::from_pkcs8_pem(key).map_err(|e| failed("client key", e))?,
            certificate: Certificate::from_pem(certificate).map_err(|e| failed("client certificate", e))?,
        })
    }

    pub fn key_pem(&self) -> Result<String> {
        self.key.to_pkcs8_pem(LineEnding::LF).map(|pem| pem.to_string()).map_err(|e| failed("client key", e))
    }

    pub fn certificate_pem(&self) -> Result<String> {
        self.certificate.to_pem(LineEnding::LF).map_err(|e| failed("client certificate", e))
    }

    pub(crate) fn key_der(&self) -> Result<Vec<u8>> {
        self.key.to_pkcs8_der().map(|der| der.as_bytes().to_vec()).map_err(|e| failed("client key", e))
    }

    pub(crate) fn certificate_der(&self) -> Result<Vec<u8>> {
        self.certificate.to_der().map_err(|e| failed("client certificate", e))
    }

    /// RSA PKCS#1 v1.5 signature over the SHA-256 of `message`.
    pub(crate) fn sign(&self, message: &[u8]) -> Vec<u8> {
        SigningKey::<Sha256>::new(self.key.clone()).sign(message).to_vec()
    }

    pub(crate) fn certificate_signature(&self) -> Vec<u8> {
        self.certificate.signature().raw_bytes().to_vec()
    }
}

/// A host certificate as received in pairing.
pub(crate) struct HostCertificate(Certificate);

impl HostCertificate {
    pub(crate) fn from_pem(pem: &str) -> Result<Self> {
        Certificate::from_pem(pem).map(Self).map_err(|e| failed("host certificate", e))
    }

    pub(crate) fn der(&self) -> Result<Vec<u8>> {
        self.0.to_der().map_err(|e| failed("host certificate", e))
    }

    pub(crate) fn signature(&self) -> Vec<u8> {
        self.0.signature().raw_bytes().to_vec()
    }

    /// Whether `signature` is this certificate's key signing the SHA-256 of `message`.
    pub(crate) fn signed(&self, message: &[u8], signature: &[u8]) -> bool {
        let Ok(key) = rsa::PublicKey::try_from(self.0.tbs_certificate().subject_public_key_info().owned_to_ref()) else {
            return false;
        };
        let Ok(signature) = Signature::try_from(signature) else { return false };
        VerifyingKey::<Sha256>::new(key).verify(message, &signature).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_survives_its_pem_form_and_its_signatures_verify() {
        let identity = Identity::generate().unwrap();
        let restored = Identity::from_pem(&identity.key_pem().unwrap(), &identity.certificate_pem().unwrap()).unwrap();
        assert_eq!(restored.certificate_der().unwrap(), identity.certificate_der().unwrap());
        assert_eq!(identity.certificate_signature().len(), 256);
        assert_eq!(identity.certificate.tbs_certificate().version(), x509_cert::Version::V3);

        let certificate = HostCertificate::from_pem(&identity.certificate_pem().unwrap()).unwrap();
        let signature = restored.sign(b"secret");
        assert!(certificate.signed(b"secret", &signature));
        assert!(!certificate.signed(b"other", &signature));
        assert_eq!(certificate.signature(), identity.certificate_signature());
    }
}
