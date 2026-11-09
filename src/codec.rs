//! The codecs of object container files, in pure Rust: compressing a block when writing, and
//! decompressing one when reading, with a limit so a small hostile block can't exhaust memory.

use std::io::{Read, Write};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Codec {
    Null,
    #[default]
    Deflate,
    Snappy,
    Zstandard,
    Bzip2,
    Xz,
}

impl Codec {
    pub const ALL: [Codec; 6] = [Codec::Null, Codec::Deflate, Codec::Snappy, Codec::Zstandard, Codec::Bzip2, Codec::Xz];

    /// The codec named in a file's `avro.codec` metadata.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }

    pub fn name(self) -> &'static str {
        match self {
            Codec::Null => "null",
            Codec::Deflate => "deflate",
            Codec::Snappy => "snappy",
            Codec::Zstandard => "zstandard",
            Codec::Bzip2 => "bzip2",
            Codec::Xz => "xz",
        }
    }

    /// A block's data, compressed. `level` applies to deflate (0 to 9, default 6) and bzip2 (1 to
    /// 9, default 9); the others have one level.
    pub fn compress(self, data: &[u8], level: Option<u32>) -> std::io::Result<Vec<u8>> {
        Ok(match self {
            Codec::Null => data.to_vec(),
            Codec::Deflate => miniz_oxide::deflate::compress_to_vec(data, level.unwrap_or(6).min(9) as u8),
            Codec::Snappy => {
                // Snappy data, then the CRC-32 of the uncompressed data (big-endian).
                let mut out = snap::raw::Encoder::new().compress_vec(data).map_err(std::io::Error::other)?;
                out.extend_from_slice(&crc32fast::hash(data).to_be_bytes());
                out
            }
            Codec::Zstandard => ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest),
            Codec::Bzip2 => {
                let level = bzip2::Compression::new(level.unwrap_or(9).clamp(1, 9));
                let mut e = bzip2::write::BzEncoder::new(Vec::new(), level);
                e.write_all(data)?;
                e.finish()?
            }
            Codec::Xz => {
                let mut out = Vec::new();
                lzma_rs::xz_compress(&mut &data[..], &mut out)?;
                out
            }
        })
    }

    /// A block's data, decompressed, refusing more than `limit` bytes of output.
    pub fn decompress(self, data: Vec<u8>, limit: u64) -> Result<Vec<u8>, String> {
        let too_big = || format!("a block decompresses to more than {} MB", limit >> 20);
        let read_limited = |mut r: Box<dyn Read + '_>| -> Result<Vec<u8>, String> {
            let mut out = Vec::new();
            (&mut r).take(limit + 1).read_to_end(&mut out).map_err(|e| e.to_string())?;
            if out.len() as u64 > limit {
                return Err(too_big());
            }
            Ok(out)
        };
        match self {
            Codec::Null => Ok(data),
            Codec::Deflate => miniz_oxide::inflate::decompress_to_vec_with_limit(&data, usize::try_from(limit).unwrap_or(usize::MAX)).map_err(|e| match e.status {
                miniz_oxide::inflate::TINFLStatus::HasMoreOutput => too_big(),
                _ => "the deflate data is corrupt".into(),
            }),
            Codec::Snappy => {
                if data.len() < 4 {
                    return Err("the snappy block is too short".into());
                }
                let (body, crc) = data.split_at(data.len() - 4);
                let len = snap::raw::decompress_len(body).map_err(|e| format!("the snappy data is corrupt: {e}"))?;
                if len as u64 > limit {
                    return Err(too_big());
                }
                let out = snap::raw::Decoder::new().decompress_vec(body).map_err(|e| format!("the snappy data is corrupt: {e}"))?;
                let expected = u32::from_be_bytes(crc.try_into().expect("4 bytes"));
                if crc32fast::hash(&out) != expected {
                    return Err("the snappy block's checksum doesn't match: the data is corrupt".into());
                }
                Ok(out)
            }
            Codec::Zstandard => {
                let mut input: &[u8] = &data;
                let mut out = Vec::new();
                // Usually one frame; several are concatenated.
                while !input.is_empty() {
                    let decoder = ruzstd::decoding::StreamingDecoder::new(&mut input).map_err(|e| format!("the zstandard data is corrupt: {e}"))?;
                    let frame = read_limited(Box::new(decoder)).map_err(|e| if e.contains("MB") { e } else { format!("the zstandard data is corrupt: {e}") })?;
                    out.extend(frame);
                    if out.len() as u64 > limit {
                        return Err(too_big());
                    }
                }
                Ok(out)
            }
            Codec::Bzip2 => read_limited(Box::new(bzip2::read::MultiBzDecoder::new(&data[..]))).map_err(|e| if e.contains("MB") { e } else { format!("the bzip2 data is corrupt: {e}") }),
            Codec::Xz => {
                let mut out = LimitedWriter { out: Vec::new(), limit: usize::try_from(limit).unwrap_or(usize::MAX), over: false };
                lzma_rs::xz_decompress(&mut &data[..], &mut out).map_err(|e| if out.over { too_big() } else { format!("the xz data is corrupt: {e}") })?;
                Ok(out.out)
            }
        }
    }
}

struct LimitedWriter {
    out: Vec<u8>,
    limit: usize,
    /// Whether a write was refused for going past the limit.
    over: bool,
}

impl Write for LimitedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.out.len() + buf.len() > self.limit {
            self.over = true;
            return Err(std::io::Error::other("too large"));
        }
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_codec_round_trips() {
        let data: Vec<u8> = (0..50_000u32).flat_map(|i| (i % 251).to_le_bytes()).collect();
        for c in Codec::ALL {
            let packed = c.compress(&data, None).unwrap();
            assert_eq!(c.decompress(packed, 1 << 30).unwrap(), data, "{}", c.name());
            assert_eq!(c.compress(&[], None).map(|p| c.decompress(p, 10).unwrap()).unwrap(), Vec::<u8>::new(), "{} empty", c.name());
        }
    }

    #[test]
    fn blocks_that_decompress_too_far_are_refused() {
        let data = vec![0u8; 3 << 20];
        for c in Codec::ALL.into_iter().filter(|c| *c != Codec::Null) {
            let packed = c.compress(&data, None).unwrap();
            let e = c.decompress(packed, 1 << 20).unwrap_err();
            assert!(e.contains("MB"), "{}: {e}", c.name());
        }
        // a snappy block claiming 300 MB of output
        let mut body = Vec::new();
        let mut n: u64 = 300 << 20;
        while n >= 0x80 {
            body.push((n as u8 & 0x7f) | 0x80);
            n >>= 7;
        }
        body.push(n as u8);
        body.extend_from_slice(&[0, 0, 0, 0]);
        assert!(Codec::Snappy.decompress(body, 256 << 20).unwrap_err().contains("MB"));
    }

    #[test]
    fn names() {
        for c in Codec::ALL {
            assert_eq!(Codec::from_name(c.name()), Some(c));
        }
        assert_eq!(Codec::from_name("lz4"), None);
    }
}
