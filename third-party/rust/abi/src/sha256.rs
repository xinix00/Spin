//! SHA-256 (FIPS 180-4): die van Lean (`leancrypto::sha256`).
//!
//! Eén keer voor de hele boom: de DRBG van `cpu`, de controlesom van de
//! vastgelegde hopfs-boom en de som van een flip-bundel (`kern`). Uit
//! leancrypto, de primitieven die Lean deelt; een kern die één hash nodig
//! heeft, linkt zo geen TLS-stapel.

pub use leancrypto::sha256::Sha256;

/// De digest van `data` in één keer.
pub fn digest(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// RFC 6234 §8.5 TEST1, TEST2_1, TEST3 en TEST4 voor SHA-256, plus
    /// de lege invoer en FIPS 180-4's 896-bit-bericht.
    #[test]
    fn rfc6234_vectors() {
        let cases: [(&[u8], usize, &str); 5] = [
            (
                b"",
                1,
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc",
                1,
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                1,
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
            (
                b"a",
                1_000_000,
                "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0",
            ),
            (
                b"01234567012345670123456701234567",
                20,
                "594847328451bdfa85056225462cc1d867d877fb388df0ce35f25ab5562bfbb5",
            ),
        ];
        for (msg, repeat, want) in cases {
            let mut s = Sha256::new();
            for _ in 0..repeat {
                s.update(msg);
            }
            assert_eq!(hex(&s.finish()), want, "{repeat} x {msg:?}");
        }
        assert_eq!(
            hex(&digest(
                b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"
            )),
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
        );
    }

    #[test]
    fn split_updates_match_one_shot() {
        let data: Vec<u8> = (0..300u32).map(|i| (i * 7) as u8).collect();
        let want = digest(&data);
        for cut in [0, 1, 55, 56, 63, 64, 65, 128, 299] {
            let mut s = Sha256::new();
            s.update(&data[..cut]);
            s.update(&data[cut..]);
            assert_eq!(s.finish(), want, "cut at {cut}");
        }
    }
}
