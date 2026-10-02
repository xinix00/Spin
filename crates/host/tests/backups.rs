//! Onafhankelijke ZIP-lezer controleert framing, ZIP64 en CRCs van de stream.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use std::{
    io::Write,
    process::{Command, Stdio},
};
struct Bytes(Vec<u8>);
impl spin_core::backup::Source for Bytes {
    fn size(&self) -> u64 {
        self.0.len() as u64
    }
    fn read(&mut self, offset: u64, length: usize) -> spin_domain::Fallible<Vec<u8>> {
        assert!(length <= 1 << 20);
        Ok(self.0[offset as usize..offset as usize + length].to_vec())
    }
}
fn extract(
    source: &mut Bytes,
    entry: spin_core::backup::Entry,
) -> Result<Vec<u8>, spin_domain::Error> {
    let mut decoder = spin_core::backup::Decoder::new(entry);
    let mut bytes = Vec::new();
    while let Some(chunk) = decoder.next(source)? {
        assert!(chunk.len() <= spin_core::backup::CHUNK);
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
#[test]
fn reads_independent_zip_variants_and_rejects_corruption() {
    for mode in ["stored", "deflate", "zip64", "descriptor"] {
        let output=Command::new("python3").args(["-c",r#"
import io,sys,zipfile
class Stream(io.BytesIO):
    def seekable(self): return False
    def seek(self,*args): raise io.UnsupportedOperation()
b=Stream() if sys.argv[1]=='descriptor' else io.BytesIO()
with zipfile.ZipFile(b,'w',compression=zipfile.ZIP_STORED if sys.argv[1]=='stored' else zipfile.ZIP_DEFLATED) as z:
    with z.open('spin.db','w',force_zip64=sys.argv[1]=='zip64') as f:
        f.write(bytes(n%251 for n in range(500003)))
    z.writestr('master-key.txt','portable-key')
sys.stdout.buffer.write(b.getvalue())
"#,mode]).output().unwrap();
        assert!(output.status.success());
        let mut source = Bytes(output.stdout);
        let archive = spin_core::backup::Archive::open(&mut source).unwrap();
        assert_eq!(
            extract(&mut source, archive.database).unwrap(),
            (0..500003).map(|n| (n % 251) as u8).collect::<Vec<_>>()
        );
        assert_eq!(extract(&mut source, archive.key).unwrap(), b"portable-key");
        let directory = source
            .0
            .windows(4)
            .position(|b| b == b"PK\x01\x02")
            .unwrap();
        source.0[directory + 16] ^= 1;
        let corrupt = spin_core::backup::Archive::open(&mut source).unwrap();
        assert!(extract(&mut source, corrupt.database).is_err());
        source.0[directory + 46] = b'/';
        assert!(spin_core::backup::Archive::open(&mut source).is_err());
    }
}
#[test]
fn backup_stream_is_readable_by_python_zipfile() {
    let source: Vec<u8> = (0..131_071).map(|n| (n % 251) as u8).collect();
    let key = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
    let mut zip = spin_core::backup::Zip::new(source.len() as u64, key).unwrap();
    let total = zip.size();
    let mut archive = Vec::new();
    loop {
        let bytes = zip
            .read_range()
            .map(|(offset, len)| source[offset as usize..offset as usize + len].to_vec());
        let Some(chunk) = zip.next(bytes).unwrap() else {
            break;
        };
        assert!(chunk.len() <= spin_core::backup::CHUNK);
        archive.extend_from_slice(&chunk);
    }
    assert_eq!(archive.len() as u64, total);
    let mut source_zip = Bytes(archive.clone());
    let parsed = spin_core::backup::Archive::open(&mut source_zip).unwrap();
    assert_eq!(extract(&mut source_zip, parsed.database).unwrap(), source);
    assert_eq!(
        extract(&mut source_zip, parsed.key).unwrap(),
        key.as_bytes()
    );
    let mut python = Command::new("python3")
        .args([
            "-c",
            r#"
import io, sys, zipfile
with zipfile.ZipFile(io.BytesIO(sys.stdin.buffer.read())) as z:
    assert z.namelist() == ['spin.db', 'master-key.txt']
    assert z.testzip() is None
    assert z.read('spin.db') == bytes(n % 251 for n in range(131071))
    assert z.read('master-key.txt') == b'AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8='
"#,
        ])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    python.stdin.take().unwrap().write_all(&archive).unwrap();
    assert!(python.wait().unwrap().success());
    let mut large = spin_core::backup::Zip::new((1 << 32) + 65536, key).unwrap();
    assert!(large.size() > (1 << 32));
    let header = large.next(None).unwrap().unwrap();
    assert_eq!(
        u64::from_le_bytes(header[41..49].try_into().unwrap()),
        (1 << 32) + 65536
    );
    assert!(large.next(Some(vec![0; 3])).is_err());
    assert_eq!(large.read_range(), Some((0, 65536)));
}
