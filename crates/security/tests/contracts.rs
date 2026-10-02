//! Go bewijst de bestaande AES-GCM- en PBKDF2-bytes onafhankelijk van Rust.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use spin_domain::json::{self, Value};
use spin_security::*;

struct Nonce;
impl Entropy for Nonce {
    fn fill(&mut self, out: &mut [u8]) -> Result {
        for (i, b) in out.iter_mut().enumerate() {
            *b = i as u8;
        }
        Ok(())
    }
}
fn string<'a>(object: &'a Value, key: &str) -> &'a str {
    object
        .as_object()
        .unwrap()
        .get(key)
        .unwrap()
        .as_str()
        .unwrap()
}

#[test]
fn aes256_matches_go_bytes_and_rejects_other_purpose() {
    let fixtures = json::parse(include_bytes!("fixtures/encrypted.json")).unwrap();
    for fixture in fixtures.as_array().unwrap() {
        let cipher = Cipher::from_encoded(string(fixture, "key")).unwrap();
        let value = string(fixture, "value");
        let purpose = string(fixture, "purpose");
        let sealed = string(fixture, "sealed");
        assert_eq!(cipher.encrypt(value, purpose, &mut Nonce).unwrap(), sealed);
        assert_eq!(cipher.decrypt(sealed, purpose).unwrap(), value);
        assert!(cipher.decrypt(sealed, "another field").is_err());
        assert!(Cipher::new([0; 32]).decrypt(sealed, purpose).is_err());
        let mut changed = sealed.as_bytes().to_vec();
        changed[15] = if changed[15] == b'A' { b'B' } else { b'A' };
        assert!(
            cipher
                .decrypt(core::str::from_utf8(&changed).unwrap(), purpose)
                .is_err()
        );
    }
}
#[test]
fn pbkdf2_matches_go_and_can_yield_between_rounds() {
    let fixtures = json::parse(include_bytes!("fixtures/passwords.json")).unwrap();
    for fixture in fixtures.as_array().unwrap() {
        let password = string(fixture, "password");
        let salt = string(fixture, "salt");
        let rounds = fixture
            .as_object()
            .unwrap()
            .get("rounds")
            .unwrap()
            .as_u64()
            .unwrap() as u32;
        let key = derive_password(password.as_bytes(), salt.as_bytes(), rounds).unwrap();
        assert_eq!(
            encode_base64(&key, false).unwrap(),
            string(fixture, "base64")
        );
        let mut task = PasswordDeriver::new(password.as_bytes(), salt.as_bytes(), rounds).unwrap();
        while !task.step(17) {}
        assert_eq!(task.result().unwrap(), key);
        let encoded = format!(
            "pbkdf2-sha256${rounds}${}${}",
            encode_base64(salt.as_bytes(), false).unwrap(),
            string(fixture, "base64")
        );
        assert!(verify_password(password, &encoded).unwrap());
        assert!(!verify_password("wrong", &encoded).unwrap());
    }
}
#[test]
fn missing_entropy_and_invalid_envelopes_fail_closed() {
    struct Broken;
    impl Entropy for Broken {
        fn fill(&mut self, _: &mut [u8]) -> Result {
            Err(Error::Entropy(5))
        }
    }
    let cipher = Cipher::new([0; 32]);
    assert_eq!(
        cipher.encrypt("secret", "purpose", &mut Broken),
        Err(Error::Entropy(5))
    );
    assert_eq!(cipher.encrypt("", "purpose", &mut Broken).unwrap(), "");
    for value in ["plain", "enc:v2:AA", "enc:v1:AA", "enc:v1:===="] {
        assert!(cipher.decrypt(value, "purpose").is_err());
    }
    assert!(Cipher::from_encoded("AA").is_err());
    assert!(PasswordDeriver::new(b"x", b"y", 0).is_err());
    assert!(!verify_password("x", "pbkdf2-sha256$2000001$c2FsdA$abcd").unwrap());
}
