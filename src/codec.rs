//! The codecs of object container files, in pure Rust: compressing a block when writing, and
//! decompressing one when reading, with a limit so a small hostile block can't exhaust memory.
//!
//! The framing is the Java library's: deflate is raw (RFC 1951, no zlib header or checksum),
//! snappy is followed by the big-endian CRC-32 of the uncompressed data, zstandard, bzip2 and xz
//! are their own formats (one or more frames or streams).

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

    /// A block's data, compressed. `level` applies to deflate (0 to 9, default 6), bzip2 (1 to
    /// 9, default 9) and xz (0 to 9, default 6), as in the Java library; the others have one
    /// level.
    pub fn compress(self, data: &[u8], level: Option<u32>) -> std::io::Result<Vec<u8>> {
        Ok(match self {
            Codec::Null => data.to_vec(),
            Codec::Deflate => {
                let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(level.unwrap_or(6).min(9)));
                e.write_all(data)?;
                e.finish()?
            }
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
                let opts = lzma_rust2::XzOptions::with_preset(level.unwrap_or(6).min(9));
                let mut e = lzma_rust2::XzWriter::new(Vec::new(), opts)?;
                e.write_all(data)?;
                e.finish()?
            }
        })
    }

    /// A block's data, decompressed, refusing more than `limit` bytes of output. Every codec
    /// streams into the limit, so a small hostile block can't make it use more memory than that
    /// (plus, for xz and zstandard, a dictionary or window of at most [`MAX_WINDOW`] bytes).
    pub fn decompress(self, data: Vec<u8>, limit: u64) -> Result<Vec<u8>, String> {
        self.decompress_limited(data, limit).map_err(|f| match f {
            Fail::TooBig => format!("a block decompresses to more than {}", size_text(limit)),
            Fail::Corrupt(e) => format!("the {} data is corrupt: {e}", self.name()),
        })
    }

    fn decompress_limited(self, data: Vec<u8>, limit: u64) -> Result<Vec<u8>, Fail> {
        let mut out = Vec::new();
        match self {
            Codec::Null => {
                if data.len() as u64 > limit {
                    return Err(Fail::TooBig);
                }
                return Ok(data);
            }
            // Raw deflate (RFC 1951), as the Java library writes it: no zlib header.
            Codec::Deflate => read_into(flate2::read::DeflateDecoder::new(&data[..]), &mut out, limit)?,
            Codec::Snappy => {
                if data.len() < 4 {
                    return Err(Fail::Corrupt("the block is too short for its checksum".into()));
                }
                let (body, crc) = data.split_at(data.len() - 4);
                let len = snap::raw::decompress_len(body).map_err(|e| Fail::Corrupt(e.to_string()))?;
                if len as u64 > limit {
                    return Err(Fail::TooBig);
                }
                out = snap::raw::Decoder::new().decompress_vec(body).map_err(|e| Fail::Corrupt(e.to_string()))?;
                let expected = u32::from_be_bytes(crc.try_into().expect("4 bytes"));
                if crc32fast::hash(&out) != expected {
                    return Err(Fail::Corrupt("the block's checksum doesn't match".into()));
                }
            }
            Codec::Zstandard => {
                use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
                let mut input: &[u8] = &data;
                // Usually one frame; several may be concatenated, and skippable frames skipped.
                while !input.is_empty() {
                    match ruzstd::decoding::StreamingDecoder::new(&mut input) {
                        Ok(frame) => read_into(frame, &mut out, limit)?,
                        Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame { length, .. })) => {
                            input = input.get(length as usize..).ok_or_else(|| Fail::Corrupt("a skippable frame runs past the block".into()))?;
                        }
                        Err(e) => return Err(Fail::Corrupt(e.to_string())),
                    }
                }
            }
            Codec::Bzip2 => read_into(bzip2::read::MultiBzDecoder::new(&data[..]), &mut out, limit)?,
            Codec::Xz => {
                let kb = u32::try_from(MAX_WINDOW >> 10).unwrap_or(u32::MAX);
                read_into(lzma_rust2::XzReader::new_mem_limit(&data[..], true, kb), &mut out, limit)?;
            }
        }
        Ok(out)
    }
}

/// The most memory an xz block's dictionary (with the decoder's overhead) may take when
/// decompressing: room for the Java library's highest level (`-9`, a 64 MiB dictionary).
pub const MAX_WINDOW: u64 = 128 << 20;

enum Fail {
    TooBig,
    Corrupt(String),
}

/// Append all of `r` to `out`, refusing to let `out` grow past `limit` bytes.
fn read_into(r: impl Read, out: &mut Vec<u8>, limit: u64) -> Result<(), Fail> {
    let room = limit.saturating_add(1).saturating_sub(out.len() as u64);
    r.take(room).read_to_end(out).map_err(|e| Fail::Corrupt(e.to_string()))?;
    if out.len() as u64 > limit {
        return Err(Fail::TooBig);
    }
    Ok(())
}

/// A byte count for a message: in MB when it is a whole number of them.
fn size_text(n: u64) -> String {
    if n >= 1 << 20 && n % (1 << 20) == 0 { format!("{} MB", n >> 20) } else { format!("{n} bytes") }
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
    fn deflate_is_byte_identical_with_flate2_at_every_level() {
        // ~1 MB of JSON-like text: miniz_oxide's one-shot compress_to_vec gave 170875 bytes at
        // level 1 where flate2 (and so Mapster's files) gave 170838.
        let mut text = Vec::new();
        for i in 0..40_000u32 {
            text.extend_from_slice(format!("{{\"id\":{i},\"name\":\"customer {}\",\"sku\":\"SKU-{}\"}}", i * 7919 % 1000, i % 37).as_bytes());
        }
        text.truncate(1 << 20);
        let sizes: Vec<usize> = (0..=9).map(|l| Codec::Deflate.compress(&text, Some(l)).unwrap().len()).collect();
        assert_eq!(sizes, [1_048_746, 170_838, 152_743, 151_431, 145_271, 145_381, 132_489, 132_488, 132_488, 132_488]);
        assert_eq!(Codec::Deflate.compress(&text, None).unwrap().len(), 132_489, "level 6 by default");
    }

    #[test]
    fn xz_compresses() {
        // lzma-rs, used before, wrote only stored chunks: its "xz" was larger than the input.
        let data: Vec<u8> = (0..50_000u32).flat_map(|i| (i % 251).to_le_bytes()).collect();
        let packed = Codec::Xz.compress(&data, None).unwrap();
        assert!(packed.len() < data.len() / 10, "{} of {}", packed.len(), data.len());
        assert_eq!(Codec::Xz.decompress(packed, 1 << 20).unwrap(), data);
    }

    #[test]
    fn null_blocks_are_limited_too() {
        assert_eq!(Codec::Null.decompress(vec![0; 10], 10).unwrap().len(), 10);
        assert_eq!(Codec::Null.decompress(vec![0; 11], 10).unwrap_err(), "a block decompresses to more than 10 bytes");
    }

    #[test]
    fn zstandard_frames_are_limited_together_and_skippable_ones_skipped() {
        let frame = |n: usize| Codec::Zstandard.compress(&vec![7u8; n], None).unwrap();
        let skippable = |payload: &[u8]| {
            let mut f = 0x184D_2A53u32.to_le_bytes().to_vec();
            f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            f.extend_from_slice(payload);
            f
        };
        let mut data = skippable(b"abc");
        data.extend(frame(1000));
        data.extend(skippable(&[]));
        data.extend(frame(500));
        assert_eq!(Codec::Zstandard.decompress(data, 1500).unwrap(), vec![7u8; 1500]);
        // two frames that fit alone but not together
        let mut data = frame(1000);
        data.extend(frame(1000));
        assert!(Codec::Zstandard.decompress(data, 1500).unwrap_err().contains("more than 1500 bytes"));
        let mut data = skippable(b"abc");
        data.truncate(9);
        assert!(Codec::Zstandard.decompress(data, 1500).unwrap_err().contains("skippable"));
    }

    #[test]
    fn corrupt_blocks_say_so() {
        for c in Codec::ALL.into_iter().filter(|c| *c != Codec::Null) {
            let mut packed = c.compress(&vec![1u8; 5000], None).unwrap();
            let n = packed.len();
            packed.truncate(n / 2);
            let e = c.decompress(packed, 1 << 20).unwrap_err();
            assert!(e.starts_with(&format!("the {} data is corrupt", c.name())), "{e}");
        }
        // snappy's CRC-32 is big-endian: little-endian is a mismatch
        let mut packed = Codec::Snappy.compress(b"hello", None).unwrap();
        let n = packed.len();
        packed[n - 4..].reverse();
        assert!(Codec::Snappy.decompress(packed, 100).unwrap_err().contains("checksum"));
        // deflate is raw: a zlib stream (header 78 9c) is refused
        let zlib = [0x78, 0x9c, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00, 0x06, 0x2c, 0x02, 0x15];
        assert!(Codec::Deflate.decompress(zlib.to_vec(), 100).is_err());
    }

    #[test]
    fn names() {
        for c in Codec::ALL {
            assert_eq!(Codec::from_name(c.name()), Some(c));
        }
        assert_eq!(Codec::from_name("lz4"), None);
    }
}
