//! Message Stream Encryption / Protocol Encryption (MSE/PE), the obfuscated handshake
//! used by every major client: 768-bit Diffie-Hellman key exchange followed by RC4.

use crate::bail;
use crate::util::{Result, rand_bytes, rand_u64, sha1};
use std::io::{self, Read, Write};
use std::net::TcpStream;

const LIMBS: usize = 24;
type Big = [u32; LIMBS];

/// The MSE prime (768 bits), big-endian.
const PRIME: [u8; 96] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xC9, 0x0F, 0xDA, 0xA2, 0x21, 0x68, 0xC2, 0x34,
    0xC4, 0xC6, 0x62, 0x8B, 0x80, 0xDC, 0x1C, 0xD1, 0x29, 0x02, 0x4E, 0x08, 0x8A, 0x67, 0xCC, 0x74,
    0x02, 0x0B, 0xBE, 0xA6, 0x3B, 0x13, 0x9B, 0x22, 0x51, 0x4A, 0x08, 0x79, 0x8E, 0x34, 0x04, 0xDD,
    0xEF, 0x95, 0x19, 0xB3, 0xCD, 0x3A, 0x43, 0x1B, 0x30, 0x2B, 0x0A, 0x6D, 0xF2, 0x5F, 0x14, 0x37,
    0x4F, 0xE1, 0x35, 0x6D, 0x6D, 0x51, 0xC2, 0x45, 0xE4, 0x85, 0xB5, 0x76, 0x62, 0x5E, 0x7E, 0xC6,
    0xF4, 0x4C, 0x42, 0xE9, 0xA6, 0x3A, 0x36, 0x21, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0x05, 0x63,
];

const VC: [u8; 8] = [0; 8];

/// Encryption policy (stored in `Ctx::encryption`).
pub const ENC_PREFER: u8 = 0;
pub const ENC_REQUIRE: u8 = 1;
pub const ENC_DISABLE: u8 = 2;
pub const CRYPTO_PLAIN: u32 = 1;
pub const CRYPTO_RC4: u32 = 2;
const MAX_PAD: usize = 512;

// ---- 768-bit modular exponentiation (Montgomery) ----

fn from_be(b: &[u8]) -> Big {
    let mut out = [0u32; LIMBS];
    for (i, &byte) in b.iter().rev().enumerate().take(LIMBS * 4) {
        out[i / 4] |= (byte as u32) << (8 * (i % 4));
    }
    out
}

fn to_be(a: &Big) -> [u8; 96] {
    let mut out = [0u8; 96];
    for i in 0..96 {
        out[95 - i] = (a[i / 4] >> (8 * (i % 4))) as u8;
    }
    out
}

fn geq(a: &Big, b: &Big) -> bool {
    for i in (0..LIMBS).rev() {
        if a[i] != b[i] {
            return a[i] > b[i];
        }
    }
    true
}

fn sub_in_place(a: &mut Big, b: &Big) {
    let mut borrow = 0i64;
    for i in 0..LIMBS {
        let d = a[i] as i64 - b[i] as i64 - borrow;
        a[i] = d as u32;
        borrow = (d < 0) as i64;
    }
}

struct Mont {
    m: Big,
    minv: u32,
    r2: Big,
}

impl Mont {
    fn new(m: Big) -> Mont {
        // -m^-1 mod 2^32 by Newton iteration.
        let mut inv: u32 = 1;
        for _ in 0..5 {
            inv = inv.wrapping_mul(2u32.wrapping_sub(m[0].wrapping_mul(inv)));
        }
        // R^2 mod m with R = 2^768, by doubling.
        let mut r2 = [0u32; LIMBS];
        r2[0] = 1;
        for _ in 0..2 * 32 * LIMBS {
            let mut carry = 0u32;
            for limb in r2.iter_mut() {
                let v = *limb;
                *limb = v << 1 | carry;
                carry = v >> 31;
            }
            if carry != 0 || geq(&r2, &m) {
                sub_in_place(&mut r2, &m);
            }
        }
        Mont {
            m,
            minv: inv.wrapping_neg(),
            r2,
        }
    }

    fn mul(&self, a: &Big, b: &Big) -> Big {
        let mut t = [0u32; LIMBS + 2];
        for &bi in b.iter() {
            let mut c = 0u64;
            for j in 0..LIMBS {
                let x = t[j] as u64 + a[j] as u64 * bi as u64 + c;
                t[j] = x as u32;
                c = x >> 32;
            }
            let x = t[LIMBS] as u64 + c;
            t[LIMBS] = x as u32;
            t[LIMBS + 1] = (x >> 32) as u32;
            let q = t[0].wrapping_mul(self.minv);
            let x = t[0] as u64 + q as u64 * self.m[0] as u64;
            let mut c = x >> 32;
            for j in 1..LIMBS {
                let x = t[j] as u64 + q as u64 * self.m[j] as u64 + c;
                t[j - 1] = x as u32;
                c = x >> 32;
            }
            let x = t[LIMBS] as u64 + c;
            t[LIMBS - 1] = x as u32;
            t[LIMBS] = t[LIMBS + 1] + (x >> 32) as u32;
            t[LIMBS + 1] = 0;
        }
        let mut out: Big = t[..LIMBS].try_into().unwrap();
        if t[LIMBS] != 0 || geq(&out, &self.m) {
            sub_in_place(&mut out, &self.m);
        }
        out
    }

    fn pow(&self, base: &Big, exp: &[u8]) -> Big {
        let mut base = *base;
        while geq(&base, &self.m) {
            sub_in_place(&mut base, &self.m);
        }
        let mut one = [0u32; LIMBS];
        one[0] = 1;
        let b = self.mul(&base, &self.r2);
        let mut r = self.mul(&one, &self.r2);
        for &byte in exp {
            for bit in (0..8).rev() {
                r = self.mul(&r, &r);
                if byte >> bit & 1 == 1 {
                    r = self.mul(&r, &b);
                }
            }
        }
        self.mul(&r, &one)
    }
}

fn modpow(base: &[u8], exp: &[u8]) -> [u8; 96] {
    let m = Mont::new(from_be(&PRIME));
    to_be(&m.pow(&from_be(base), exp))
}

struct Keys {
    private: [u8; 20],
    public: [u8; 96],
}

fn keypair() -> Keys {
    let private = rand_bytes::<20>();
    Keys {
        private,
        public: modpow(&[2], &private),
    }
}

// ---- RC4 ----

#[derive(Clone)]
pub struct Rc4 {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    fn new(key: &[u8]) -> Rc4 {
        let mut s = [0u8; 256];
        for (i, v) in s.iter_mut().enumerate() {
            *v = i as u8;
        }
        let mut j = 0u8;
        for i in 0..256 {
            j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
            s.swap(i, j as usize);
        }
        let mut r = Rc4 { s, i: 0, j: 0 };
        r.apply(&mut [0u8; 1024]); // MSE discards the first 1 KiB of keystream
        r
    }

    pub fn apply(&mut self, data: &mut [u8]) {
        for b in data {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.s[self.i as usize]);
            self.s.swap(self.i as usize, self.j as usize);
            let k =
                self.s[(self.s[self.i as usize].wrapping_add(self.s[self.j as usize])) as usize];
            *b ^= k;
        }
    }
}

fn hash(parts: &[&[u8]]) -> [u8; 20] {
    sha1(&parts.concat())
}

// ---- stream wrappers ----

/// Reading half of a peer connection; decrypts if RC4 was negotiated.
pub struct PeerReader {
    s: TcpStream,
    rc4: Option<Rc4>,
    prefix: Vec<u8>,
    pos: usize,
}

impl Read for PeerReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos < self.prefix.len() {
            let n = (self.prefix.len() - self.pos).min(buf.len());
            buf[..n].copy_from_slice(&self.prefix[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        let n = self.s.read(buf)?;
        if let Some(r) = &mut self.rc4 {
            r.apply(&mut buf[..n]);
        }
        Ok(n)
    }
}

/// Writing half of a peer connection; encrypts if RC4 was negotiated.
pub struct PeerWriter {
    s: TcpStream,
    rc4: Option<Rc4>,
    scratch: Vec<u8>,
    /// The MSE handshake was used (even if the stream itself is plaintext).
    obfuscated: bool,
}

impl PeerWriter {
    pub fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        match &mut self.rc4 {
            None => self.s.write_all(data),
            Some(r) => {
                self.scratch.clear();
                self.scratch.extend_from_slice(data);
                r.apply(&mut self.scratch);
                self.s.write_all(&self.scratch)
            }
        }
    }

    /// 0 = plaintext, 1 = encrypted handshake only, 2 = fully encrypted.
    pub fn level(&self) -> u8 {
        if self.rc4.is_some() {
            2
        } else {
            self.obfuscated as u8
        }
    }
}

pub fn plain(s: &TcpStream) -> io::Result<(PeerReader, PeerWriter)> {
    Ok((
        PeerReader {
            s: s.try_clone()?,
            rc4: None,
            prefix: Vec::new(),
            pos: 0,
        },
        PeerWriter {
            s: s.try_clone()?,
            rc4: None,
            scratch: Vec::new(),
            obfuscated: false,
        },
    ))
}

fn random_pad() -> Vec<u8> {
    let n = (rand_u64() % 64) as usize;
    let mut p = vec![0u8; n];
    for c in p.chunks_mut(8) {
        let r = rand_u64().to_le_bytes();
        c.copy_from_slice(&r[..c.len()]);
    }
    p
}

/// Read until the last `pattern.len()` bytes equal `pattern`, giving up after `max` extra bytes.
fn sync_to(s: &mut TcpStream, pattern: &[u8], max: usize) -> Result<()> {
    let mut win = vec![0u8; pattern.len()];
    s.read_exact(&mut win)?;
    let mut extra = 0;
    while win != pattern {
        if extra >= max {
            bail!("encryption handshake: sync failed");
        }
        let mut b = [0u8; 1];
        s.read_exact(&mut b)?;
        win.remove(0);
        win.push(b[0]);
        extra += 1;
    }
    Ok(())
}

/// Error from `initiate`: `fallback` is true when the peer probably just doesn't speak MSE
/// (it never sent a valid key), so a plaintext retry makes sense.
pub struct InitError {
    pub err: crate::util::Error,
    pub fallback: bool,
}

/// Outgoing (A side) handshake. `ia` (our BitTorrent handshake) is sent inside it.
pub fn initiate(
    s: &TcpStream,
    skey: &[u8; 20],
    ia: &[u8],
    provide: u32,
) -> std::result::Result<(PeerReader, PeerWriter), InitError> {
    let mut s = s.try_clone().map_err(|e| InitError {
        err: e.into(),
        fallback: false,
    })?;
    let keys = keypair();
    let early = |e: io::Error| InitError {
        err: e.into(),
        fallback: true,
    };
    let mut out = keys.public.to_vec();
    out.extend_from_slice(&random_pad());
    s.write_all(&out).map_err(early)?;
    let mut yb = [0u8; 96];
    s.read_exact(&mut yb).map_err(early)?;
    let late = |e: crate::util::Error| InitError {
        err: e,
        fallback: false,
    };
    (|| -> Result<(PeerReader, PeerWriter)> {
        let secret = modpow(&yb, &keys.private);
        let mut enc = Rc4::new(&hash(&[b"keyA", &secret, skey]));
        let mut dec = Rc4::new(&hash(&[b"keyB", &secret, skey]));
        let req2 = hash(&[b"req2", skey]);
        let req3 = hash(&[b"req3", &secret]);
        let mut msg = hash(&[b"req1", &secret]).to_vec();
        msg.extend(req2.iter().zip(req3).map(|(a, b)| a ^ b));
        let mut body = VC.to_vec();
        body.extend_from_slice(&provide.to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes()); // len(PadC)
        body.extend_from_slice(&(ia.len() as u16).to_be_bytes());
        body.extend_from_slice(ia);
        enc.apply(&mut body);
        msg.extend_from_slice(&body);
        s.write_all(&msg)?;
        // B's reply starts with ENCRYPT(VC) somewhere after its padding.
        let mut vc = VC;
        dec.clone().apply(&mut vc);
        sync_to(&mut s, &vc, MAX_PAD)?;
        dec.apply(&mut [0u8; 8]);
        let mut hdr = [0u8; 6];
        s.read_exact(&mut hdr)?;
        dec.apply(&mut hdr);
        let select = u32::from_be_bytes(hdr[..4].try_into().unwrap());
        let pad_len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
        if pad_len > MAX_PAD {
            bail!("encryption handshake: bad padding");
        }
        let mut pad = vec![0u8; pad_len];
        s.read_exact(&mut pad)?;
        dec.apply(&mut pad);
        if select & provide == 0 || select.count_ones() != 1 {
            bail!("encryption handshake: bad crypto selection");
        }
        let rc4 = select == CRYPTO_RC4;
        Ok((
            PeerReader {
                s: s.try_clone()?,
                rc4: rc4.then_some(dec),
                prefix: Vec::new(),
                pos: 0,
            },
            PeerWriter {
                s,
                rc4: rc4.then_some(enc),
                scratch: Vec::new(),
                obfuscated: true,
            },
        ))
    })()
    .map_err(late)
}

/// Incoming (B side) handshake. `first` holds bytes already read from the socket (part of
/// Ya). `skeys` are the info hashes we serve. Returns the streams and the matched info hash.
pub fn respond(
    s: &TcpStream,
    first: &[u8],
    skeys: &[[u8; 20]],
    allow: u32,
) -> Result<(PeerReader, PeerWriter, [u8; 20])> {
    let mut s = s.try_clone()?;
    if first.len() > 96 {
        bail!("bad encryption handshake");
    }
    let mut ya = [0u8; 96];
    ya[..first.len()].copy_from_slice(first);
    s.read_exact(&mut ya[first.len()..])?;
    let keys = keypair();
    let mut out = keys.public.to_vec();
    out.extend_from_slice(&random_pad());
    s.write_all(&out)?;
    let secret = modpow(&ya, &keys.private);
    sync_to(&mut s, &hash(&[b"req1", &secret]), MAX_PAD)?;
    let mut x = [0u8; 20];
    s.read_exact(&mut x)?;
    let req3 = hash(&[b"req3", &secret]);
    for (a, b) in x.iter_mut().zip(req3) {
        *a ^= b;
    }
    let Some(skey) = skeys.iter().find(|k| hash(&[b"req2", *k]) == x).copied() else {
        bail!("encryption handshake: unknown torrent");
    };
    let mut dec = Rc4::new(&hash(&[b"keyA", &secret, &skey]));
    let mut enc = Rc4::new(&hash(&[b"keyB", &secret, &skey]));
    let mut hdr = [0u8; 14];
    s.read_exact(&mut hdr)?;
    dec.apply(&mut hdr);
    if hdr[..8] != VC {
        bail!("encryption handshake: bad verification");
    }
    let provide = u32::from_be_bytes(hdr[8..12].try_into().unwrap());
    let pad_len = u16::from_be_bytes([hdr[12], hdr[13]]) as usize;
    if pad_len > MAX_PAD {
        bail!("encryption handshake: bad padding");
    }
    let mut pad = vec![0u8; pad_len + 2];
    s.read_exact(&mut pad)?;
    dec.apply(&mut pad);
    let ia_len = u16::from_be_bytes([pad[pad_len], pad[pad_len + 1]]) as usize;
    if ia_len > 4096 {
        bail!("encryption handshake: initial payload too long");
    }
    let mut ia = vec![0u8; ia_len];
    s.read_exact(&mut ia)?;
    dec.apply(&mut ia);
    let select = if provide & allow & CRYPTO_RC4 != 0 {
        CRYPTO_RC4
    } else if provide & allow & CRYPTO_PLAIN != 0 {
        CRYPTO_PLAIN
    } else {
        bail!("encryption handshake: no common method");
    };
    let mut reply = VC.to_vec();
    reply.extend_from_slice(&select.to_be_bytes());
    reply.extend_from_slice(&0u16.to_be_bytes());
    enc.apply(&mut reply);
    s.write_all(&reply)?;
    let rc4 = select == CRYPTO_RC4;
    Ok((
        PeerReader {
            s: s.try_clone()?,
            rc4: rc4.then_some(dec),
            prefix: ia,
            pos: 0,
        },
        PeerWriter {
            s,
            rc4: rc4.then_some(enc),
            scratch: Vec::new(),
            obfuscated: true,
        },
        skey,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::hex;
    use std::net::TcpListener;

    #[test]
    fn modpow_matches_reference() {
        // 2^0x10001 mod P and a DH agreement check.
        let r = modpow(&[2], &[0x01, 0x00, 0x01]);
        assert_eq!(
            hex(&r[..12]),
            "90bebd74b81b239d8cd10f49",
            "full: {}",
            hex(&r)
        );
        let a = keypair();
        let b = keypair();
        assert_eq!(modpow(&b.public, &a.private), modpow(&a.public, &b.private));
    }

    #[test]
    fn rc4_vector() {
        // RFC 6229, key 0x0102030405: keystream bytes 1024..1028 (MSE skips the first 1024).
        let mut r = Rc4::new(&[1, 2, 3, 4, 5]);
        let mut b = [0u8; 4];
        r.apply(&mut b);
        assert_eq!(hex(&b), "30abbcc7");
    }

    fn roundtrip(provide: u32, allow: u32, expect_rc4: bool) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let skey = [7u8; 20];
        let server = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut first = [0u8; 20];
            s.read_exact(&mut first).unwrap();
            let (mut r, mut w, k) = respond(&s, &first, &[[1u8; 20], skey], allow).unwrap();
            assert_eq!(k, skey);
            let mut ia = [0u8; 5];
            r.read_exact(&mut ia).unwrap();
            assert_eq!(&ia, b"hello");
            let mut more = [0u8; 4];
            r.read_exact(&mut more).unwrap();
            assert_eq!(&more, b"more");
            w.write_all(b"reply").unwrap();
            w.level() == 2
        });
        let s = TcpStream::connect(addr).unwrap();
        let (mut r, mut w) = initiate(&s, &skey, b"hello", provide).ok().unwrap();
        w.write_all(b"more").unwrap();
        let mut buf = [0u8; 5];
        r.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"reply");
        assert_eq!(w.level() == 2, expect_rc4);
        assert_eq!(server.join().unwrap(), expect_rc4);
    }

    #[test]
    fn handshake_both_sides() {
        roundtrip(CRYPTO_RC4 | CRYPTO_PLAIN, CRYPTO_RC4 | CRYPTO_PLAIN, true);
        roundtrip(CRYPTO_RC4 | CRYPTO_PLAIN, CRYPTO_PLAIN, false);
        roundtrip(CRYPTO_RC4, CRYPTO_RC4, true);
    }
}
