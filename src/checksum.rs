//! Flexible checksums (botocore `httpchecksum.py`): the algorithms, request
//! checksum resolution (header or aws-chunked trailer) and response
//! validation.
//!
//! botocore takes the XXHASH family from the CRT; awsr has its own XXH64 and
//! uses the `xxhash-rust` crate for XXH3 (64 and 128 bit).

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use sha1::Digest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    Crc64Nvme,
    Crc32c,
    Crc32,
    Sha1,
    Sha256,
    Sha512,
    Xxhash64,
    Xxhash3,
    Xxhash128,
}

/// `DEFAULT_CHECKSUM_ALGORITHM`.
pub const DEFAULT: Algorithm = Algorithm::Crc64Nvme;

/// `_ALGORITHMS_PRIORITY_LIST`.
pub const PRIORITY: &[Algorithm] = &[
    Algorithm::Xxhash128,
    Algorithm::Xxhash3,
    Algorithm::Crc64Nvme,
    Algorithm::Xxhash64,
    Algorithm::Crc32c,
    Algorithm::Crc32,
    Algorithm::Sha1,
    Algorithm::Sha256,
    Algorithm::Sha512,
];

impl Algorithm {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "crc64nvme" => Algorithm::Crc64Nvme,
            "crc32c" => Algorithm::Crc32c,
            "crc32" => Algorithm::Crc32,
            "sha1" => Algorithm::Sha1,
            "sha256" => Algorithm::Sha256,
            "sha512" => Algorithm::Sha512,
            "xxhash64" => Algorithm::Xxhash64,
            "xxhash3" => Algorithm::Xxhash3,
            "xxhash128" => Algorithm::Xxhash128,
            _ => return None,
        })
    }

    /// Lower-case name as used in `x-amz-checksum-<name>`.
    pub fn name(self) -> &'static str {
        match self {
            Algorithm::Crc64Nvme => "crc64nvme",
            Algorithm::Crc32c => "crc32c",
            Algorithm::Crc32 => "crc32",
            Algorithm::Sha1 => "sha1",
            Algorithm::Sha256 => "sha256",
            Algorithm::Sha512 => "sha512",
            Algorithm::Xxhash64 => "xxhash64",
            Algorithm::Xxhash3 => "xxhash3",
            Algorithm::Xxhash128 => "xxhash128",
        }
    }

    pub fn header(self) -> String {
        format!("x-amz-checksum-{}", self.name())
    }

    pub fn hasher(self) -> Hasher {
        Hasher(match self {
            Algorithm::Crc64Nvme => State::Crc64(!0),
            Algorithm::Crc32c => State::Crc32c(!0),
            Algorithm::Crc32 => State::Crc32(crc32fast::Hasher::new()),
            Algorithm::Sha1 => State::Sha1(sha1::Sha1::new()),
            Algorithm::Sha256 => State::Sha256(sha2::Sha256::new()),
            Algorithm::Sha512 => State::Sha512(sha2::Sha512::new()),
            Algorithm::Xxhash64 => State::Xxh64(Xxh64::new()),
            Algorithm::Xxhash3 => State::Xxh3_64(Box::default()),
            Algorithm::Xxhash128 => State::Xxh3_128(Box::default()),
        })
    }

    /// Base64 digest of `data`.
    pub fn b64digest(self, data: &[u8]) -> String {
        let mut h = self.hasher();
        h.update(data);
        h.b64digest()
    }
}

/// Incremental checksum.
#[derive(Clone)]
pub struct Hasher(State);

#[derive(Clone)]
enum State {
    Crc64(u64),
    Crc32c(u32),
    Crc32(crc32fast::Hasher),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha512(sha2::Sha512),
    Xxh64(Xxh64),
    Xxh3_64(Box<xxhash_rust::xxh3::Xxh3Default>),
    Xxh3_128(Box<xxhash_rust::xxh3::Xxh3Default>),
}

impl Hasher {
    pub fn update(&mut self, data: &[u8]) {
        match &mut self.0 {
            State::Crc64(c) => *c = crc_update(&CRC64_NVME, *c, data),
            State::Crc32c(c) => *c = crc_update(&CRC32C, *c as u64, data) as u32,
            State::Crc32(h) => h.update(data),
            State::Sha1(h) => h.update(data),
            State::Sha256(h) => h.update(data),
            State::Sha512(h) => h.update(data),
            State::Xxh64(h) => h.update(data),
            State::Xxh3_64(h) | State::Xxh3_128(h) => h.update(data),
        }
    }

    pub fn digest(self) -> Vec<u8> {
        match self.0 {
            State::Crc64(c) => (!c).to_be_bytes().to_vec(),
            State::Crc32c(c) => (!c).to_be_bytes().to_vec(),
            State::Crc32(h) => h.finalize().to_be_bytes().to_vec(),
            State::Sha1(h) => h.finalize().to_vec(),
            State::Sha256(h) => h.finalize().to_vec(),
            State::Sha512(h) => h.finalize().to_vec(),
            State::Xxh64(h) => h.finish().to_be_bytes().to_vec(),
            State::Xxh3_64(h) => h.digest().to_be_bytes().to_vec(),
            State::Xxh3_128(h) => h.digest128().to_be_bytes().to_vec(),
        }
    }

    pub fn b64digest(self) -> String {
        B64.encode(self.digest())
    }
}

// --------------------------------------------------------------------- CRC

/// Lookup table for a reflected CRC with polynomial `poly`.
const fn crc_table(poly: u64) -> [u64; 256] {
    let mut table = [0u64; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u64;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ poly } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC64_NVME: [u64; 256] = crc_table(0x9a6c_9329_ac4b_c9b5);
static CRC32C: [u64; 256] = crc_table(0x82f6_3b78);

fn crc_update(table: &[u64; 256], mut crc: u64, data: &[u8]) -> u64 {
    for &b in data {
        crc = table[((crc ^ b as u64) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

// ---------------------------------------------------------------- XXH64

const P1: u64 = 0x9e37_79b1_85eb_ca87;
const P2: u64 = 0xc2b2_ae3d_27d4_eb4f;
const P3: u64 = 0x1656_67b1_9e37_79f9;
const P4: u64 = 0x85eb_ca77_c2b2_ae63;
const P5: u64 = 0x27d4_eb2f_1656_67c5;

/// XXH64 with seed 0.
#[derive(Clone)]
struct Xxh64 {
    acc: [u64; 4],
    buf: Vec<u8>,
    total: u64,
}

fn round(acc: u64, lane: u64) -> u64 {
    acc.wrapping_add(lane.wrapping_mul(P2))
        .rotate_left(31)
        .wrapping_mul(P1)
}

fn merge(acc: u64, v: u64) -> u64 {
    (acc ^ round(0, v)).wrapping_mul(P1).wrapping_add(P4)
}

fn lane(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

impl Xxh64 {
    fn new() -> Self {
        Xxh64 {
            acc: [P1.wrapping_add(P2), P2, 0, 0u64.wrapping_sub(P1)],
            buf: Vec::with_capacity(32),
            total: 0,
        }
    }

    fn stripe(&mut self, s: &[u8]) {
        for (i, a) in self.acc.iter_mut().enumerate() {
            *a = round(*a, lane(&s[i * 8..]));
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        if !self.buf.is_empty() {
            let need = 32 - self.buf.len();
            let take = need.min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() < 32 {
                return;
            }
            let buf = std::mem::take(&mut self.buf);
            self.stripe(&buf);
        }
        let mut chunks = data.chunks_exact(32);
        for s in &mut chunks {
            self.stripe(s);
        }
        self.buf.extend_from_slice(chunks.remainder());
    }

    fn finish(&self) -> u64 {
        let [v1, v2, v3, v4] = self.acc;
        let mut h = if self.total >= 32 {
            let h = v1
                .rotate_left(1)
                .wrapping_add(v2.rotate_left(7))
                .wrapping_add(v3.rotate_left(12))
                .wrapping_add(v4.rotate_left(18));
            [v1, v2, v3, v4].into_iter().fold(h, merge)
        } else {
            P5
        };
        h = h.wrapping_add(self.total);
        let mut rest = self.buf.as_slice();
        while rest.len() >= 8 {
            h = (h ^ round(0, lane(rest)))
                .rotate_left(27)
                .wrapping_mul(P1)
                .wrapping_add(P4);
            rest = &rest[8..];
        }
        if rest.len() >= 4 {
            let v = u32::from_le_bytes(rest[..4].try_into().unwrap()) as u64;
            h = (h ^ v.wrapping_mul(P1))
                .rotate_left(23)
                .wrapping_mul(P2)
                .wrapping_add(P3);
            rest = &rest[4..];
        }
        for &b in rest {
            h = (h ^ (b as u64).wrapping_mul(P5))
                .rotate_left(11)
                .wrapping_mul(P1);
        }
        h ^= h >> 33;
        h = h.wrapping_mul(P2);
        h ^= h >> 29;
        h = h.wrapping_mul(P3);
        h ^ (h >> 32)
    }
}

// ------------------------------------------------------------ aws-chunked

const CHUNK_SIZE: usize = 1024 * 1024;

/// Encode `body` as `aws-chunked` with the checksum as a trailer
/// (`AwsChunkedWrapper`).
pub fn aws_chunked(body: &[u8], algorithm: Algorithm) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 128);
    let mut hasher = algorithm.hasher();
    for chunk in body.chunks(CHUNK_SIZE) {
        hasher.update(chunk);
        out.extend(format!("{:x}\r\n", chunk.len()).as_bytes());
        out.extend(chunk);
        out.extend(b"\r\n");
    }
    out.extend(format!("0\r\n{}:{}\r\n\r\n", algorithm.header(), hasher.b64digest()).as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(a: Algorithm, data: &[u8]) -> String {
        let mut h = a.hasher();
        h.update(data);
        hex::encode(h.digest())
    }

    #[test]
    fn check_values() {
        let d = b"123456789";
        assert_eq!(hex(Algorithm::Crc32, d), "cbf43926");
        assert_eq!(hex(Algorithm::Crc32c, d), "e3069283");
        assert_eq!(hex(Algorithm::Crc64Nvme, d), "ae8b14860a799888");
        assert_eq!(hex(Algorithm::Xxhash64, b""), "ef46db3751d8e999");
        assert_eq!(hex(Algorithm::Xxhash64, b"abc"), "44bc2cf5ad770999");
        // botocore `_CHECKSUM_DIGEST_CASES` (tests/unit/botocore/test_httpchecksum.py).
        for (a, want) in [
            (Algorithm::Crc32, "DUoRhQ=="),
            (Algorithm::Sha1, "Kq5sNclPz7QV2+lfQIuc6R7oRu0="),
            (
                Algorithm::Sha256,
                "uU0nuZNNPgilLlLX2n2r+sSE7+N6U4DukIj3rOLvzek=",
            ),
            (
                Algorithm::Sha512,
                "MJ7MSJwS1utMxA9QyQLytNDtd+5RGnx6m808qG1M2G+YndNbxf9JlnDaNCVbRbDP2DDoH2Bdz33FVC6TrpzXbw==",
            ),
            (Algorithm::Crc32c, "yZRlqg=="),
            (Algorithm::Crc64Nvme, "jSnVw/bqjr4="),
            (Algorithm::Xxhash64, "RatnNLIeaWg="),
            (Algorithm::Xxhash3, "1Eex6kDmmIs="),
            (Algorithm::Xxhash128, "340J6T+HSQCpm4d1zBW2xw=="),
        ] {
            assert_eq!(a.b64digest(b"hello world"), want, "{a:?}");
        }
    }

    #[test]
    fn xxh64_incremental_matches_one_shot() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 % 251) as u8).collect();
        let one = hex(Algorithm::Xxhash64, &data);
        let mut h = Algorithm::Xxhash64.hasher();
        for c in data.chunks(13) {
            h.update(c);
        }
        assert_eq!(hex::encode(h.digest()), one);
    }

    #[test]
    fn aws_chunked_layout() {
        assert_eq!(
            aws_chunked(b"abc", Algorithm::Crc32),
            b"3\r\nabc\r\n0\r\nx-amz-checksum-crc32:NSRBwg==\r\n\r\n".to_vec()
        );
        assert_eq!(
            aws_chunked(b"", Algorithm::Crc32),
            b"0\r\nx-amz-checksum-crc32:AAAAAA==\r\n\r\n".to_vec()
        );
    }
}
