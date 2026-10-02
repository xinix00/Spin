//! AES-128-GCM with the existing pinned no_std RustCrypto backend.
//! No allocation; authentication is checked before any plaintext is exposed.
//! AES round keys and GHASH state enable their zeroize-on-drop features.
use aes_gcm::{Aes128Gcm, KeyInit, aead::AeadInPlace};
/// TLS uses a full 128-bit authentication tag.
pub(crate) const TAG_LEN: usize = 16;
pub(crate) struct Gcm(Aes128Gcm);
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TagMismatch;
impl Gcm {
    pub(crate) fn new(key: &[u8; 16]) -> Self {
        Self(Aes128Gcm::new(key.into()))
    }
    pub(crate) fn seal(
        &self,
        nonce: &[u8; 12],
        aad: &[u8],
        data: &mut [u8],
    ) -> Result<[u8; TAG_LEN], TagMismatch> {
        self.0
            .encrypt_in_place_detached(nonce.into(), aad, data)
            .map(Into::into)
            .map_err(|_| TagMismatch)
    }
    pub(crate) fn open(
        &self,
        nonce: &[u8; 12],
        aad: &[u8],
        data: &mut [u8],
        tag: &[u8],
    ) -> Result<(), TagMismatch> {
        let tag: &[u8; TAG_LEN] = tag.try_into().map_err(|_| TagMismatch)?;
        self.0
            .decrypt_in_place_detached(nonce.into(), aad, data, tag.into())
            .map_err(|_| TagMismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::testutil::unhex;

    fn key(hex: &str) -> [u8; 16] {
        let mut k = [0u8; 16];
        k.copy_from_slice(&unhex(hex));
        k
    }

    fn iv(hex: &str) -> [u8; 12] {
        let mut k = [0u8; 12];
        k.copy_from_slice(&unhex(hex));
        k
    }

    /// Sleutel, nonce, klaartekst, AAD, ciphertext, tag.
    type Case = (
        &'static str,
        &'static str,
        Vec<u8>,
        Vec<u8>,
        Vec<u8>,
        &'static str,
    );

    /// De GCM-testgevallen 1 tot en met 4 (AES-128) uit McGrew en Viega,
    /// "The Galois/Counter Mode of Operation", bijlage B.
    #[test]
    fn gcm_spec_vectors() {
        let p3 = "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72\
                  1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255";
        let c3 = "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e\
                  21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091473f5985";
        let cases: [Case; 4] = [
            (
                "00000000000000000000000000000000",
                "000000000000000000000000",
                Vec::new(),
                Vec::new(),
                Vec::new(),
                "58e2fccefa7e3061367f1d57a4e7455a",
            ),
            (
                "00000000000000000000000000000000",
                "000000000000000000000000",
                vec![0; 16],
                Vec::new(),
                unhex("0388dace60b6a392f328c2b971b2fe78"),
                "ab6e47d42cec13bdf53a67b21257bddf",
            ),
            (
                "feffe9928665731c6d6a8f9467308308",
                "cafebabefacedbaddecaf888",
                unhex(p3),
                Vec::new(),
                unhex(c3),
                "4d5c2af327cd64a62cf35abd2ba6fab4",
            ),
            (
                "feffe9928665731c6d6a8f9467308308",
                "cafebabefacedbaddecaf888",
                unhex(p3)[..60].to_vec(),
                unhex("feedfacedeadbeeffeedfacedeadbeefabaddad2"),
                unhex(c3)[..60].to_vec(),
                "5bc94fbc3221a5db94fae95ae7121a47",
            ),
        ];
        for (i, (k, n, p, a, c, t)) in cases.iter().enumerate() {
            let g = Gcm::new(&key(k));
            let mut buf = p.clone();
            let tag = g.seal(&iv(n), a, &mut buf).unwrap();
            assert_eq!(&buf, c, "testgeval {}: ciphertext", i + 1);
            assert_eq!(tag.to_vec(), unhex(t), "testgeval {}: tag", i + 1);
            assert_eq!(g.open(&iv(n), a, &mut buf, &tag), Ok(()));
            assert_eq!(&buf, p, "testgeval {}: terug naar klaartekst", i + 1);
        }
    }

    /// Een omgedraaide bit in tag, data of AAD: weigeren en niets ontsleutelen.
    #[test]
    fn open_rejects_tampering() {
        let g = Gcm::new(&[7u8; 16]);
        let n = [1u8; 12];
        let mut buf = b"leantls record".to_vec();
        let tag = g.seal(&n, b"hdr", &mut buf).unwrap();
        let sealed = buf.clone();

        let mut bad_tag = tag;
        bad_tag[15] ^= 1;
        assert_eq!(g.open(&n, b"hdr", &mut buf, &bad_tag), Err(TagMismatch));
        assert_eq!(buf, sealed, "data aangeraakt ondanks foute tag");

        buf[0] ^= 1;
        assert_eq!(g.open(&n, b"hdr", &mut buf, &tag), Err(TagMismatch));
        buf[0] ^= 1;
        assert_eq!(g.open(&n, b"hdX", &mut buf, &tag), Err(TagMismatch));
        assert_eq!(g.open(&n, b"hdr", &mut buf, &tag), Ok(()));
        assert_eq!(buf, b"leantls record");
    }
}
