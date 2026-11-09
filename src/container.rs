//! Writing object container files (`.avro`): the header (magic, metadata with the schema and
//! codec, sync marker), then blocks of records, each compressed and followed by the sync marker.
//!
//! [`Writer`] streams: records are encoded into the current block, which goes to the output once
//! it holds enough records or bytes, so a file of any size is written in little memory.

use std::io::{self, Write};

use crate::binary::{encode, write_bytes, write_long};
use crate::{Codec, Value};

pub const MAGIC: &[u8; 4] = b"Obj\x01";

/// Records per block unless [`WriterOptions::block_records`] says otherwise.
pub const DEFAULT_BLOCK_RECORDS: usize = 1000;

#[derive(Debug, Clone)]
pub struct WriterOptions {
    pub codec: Codec,
    /// Compression level, for the codecs that have them (see [`Codec::compress`]).
    pub level: Option<u32>,
    /// A block is written once it holds this many records (0: the default).
    pub block_records: usize,
    /// ... or once its encoded records reach this many bytes, if set.
    pub block_bytes: Option<usize>,
    /// User metadata for the header. Keys starting `avro.` are reserved and refused.
    pub metadata: Vec<(String, Vec<u8>)>,
    /// A fixed sync marker, for byte-identical output; random otherwise.
    pub sync: Option<[u8; 16]>,
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self { codec: Codec::Deflate, level: None, block_records: DEFAULT_BLOCK_RECORDS, block_bytes: None, metadata: Vec::new(), sync: None }
    }
}

/// Writes an object container file. Call [`Writer::finish`] at the end: dropping a writer
/// without it loses the records of the last block.
pub struct Writer<W: Write> {
    inner: W,
    codec: Codec,
    level: Option<u32>,
    block_records: usize,
    block_bytes: Option<usize>,
    sync: [u8; 16],
    /// The current block's encoded records, and how many there are.
    block: Vec<u8>,
    count: usize,
    records: u64,
    written: u64,
}

impl<W: Write> Writer<W> {
    /// Write the header for `schema` (the schema's JSON, ideally in canonical or compact form)
    /// and get ready for records.
    pub fn new(mut inner: W, schema: &str, opts: WriterOptions) -> io::Result<Self> {
        let metadata: Vec<&(String, Vec<u8>)> = opts.metadata.iter().filter(|(k, _)| !k.trim().is_empty()).collect();
        let invalid = |m: String| io::Error::new(io::ErrorKind::InvalidInput, m);
        if let Some((k, _)) = metadata.iter().find(|(k, _)| k.starts_with("avro.")) {
            return Err(invalid(format!("metadata key {k} is reserved (keys starting avro. are)")));
        }
        // The header's metadata is a map: a key given twice would leave readers keeping one.
        for (i, (k, _)) in metadata.iter().enumerate() {
            if metadata[..i].iter().any(|(earlier, _)| earlier == k) {
                return Err(invalid(format!("metadata key {k} is given twice")));
            }
        }
        let sync = opts.sync.unwrap_or_else(random_sync);
        let mut head = MAGIC.to_vec();
        write_long(&mut head, 2 + metadata.len() as i64);
        write_bytes(&mut head, b"avro.schema");
        write_bytes(&mut head, schema.as_bytes());
        write_bytes(&mut head, b"avro.codec");
        write_bytes(&mut head, opts.codec.name().as_bytes());
        for (k, v) in metadata {
            write_bytes(&mut head, k.as_bytes());
            write_bytes(&mut head, v);
        }
        head.push(0);
        head.extend_from_slice(&sync);
        inner.write_all(&head)?;
        let block_records = if opts.block_records == 0 { DEFAULT_BLOCK_RECORDS } else { opts.block_records };
        Ok(Self {
            inner,
            codec: opts.codec,
            level: opts.level,
            block_records,
            block_bytes: opts.block_bytes.filter(|n| *n > 0),
            sync,
            block: Vec::new(),
            count: 0,
            records: 0,
            written: head.len() as u64,
        })
    }

    /// Add a record.
    pub fn append(&mut self, v: &Value) -> io::Result<()> {
        encode(&mut self.block, v);
        self.added()
    }

    /// Add a record that is already in the binary encoding.
    pub fn append_encoded(&mut self, datum: &[u8]) -> io::Result<()> {
        self.block.extend_from_slice(datum);
        self.added()
    }

    /// Add a record that `encode` writes straight into the block. If it fails, whatever it wrote
    /// is taken back, so the record is left out and the block stays whole.
    pub fn append_with<E: From<io::Error>>(&mut self, encode: impl FnOnce(&mut Vec<u8>) -> Result<(), E>) -> Result<(), E> {
        let start = self.block.len();
        if let Err(e) = encode(&mut self.block) {
            self.block.truncate(start);
            return Err(e);
        }
        self.added().map_err(E::from)
    }

    fn added(&mut self) -> io::Result<()> {
        self.count += 1;
        self.records += 1;
        if self.count >= self.block_records || self.block_bytes.is_some_and(|n| self.block.len() >= n) {
            self.flush_block()?;
        }
        Ok(())
    }

    /// Write the current block now, if it has records.
    pub fn flush_block(&mut self) -> io::Result<()> {
        if self.count == 0 {
            return Ok(());
        }
        let data = self.codec.compress(&self.block, self.level)?;
        check_block_size(data.len())?;
        let mut head = Vec::with_capacity(20);
        write_long(&mut head, self.count as i64);
        write_long(&mut head, data.len() as i64);
        self.inner.write_all(&head)?;
        self.inner.write_all(&data)?;
        self.inner.write_all(&self.sync)?;
        self.written += (head.len() + data.len() + self.sync.len()) as u64;
        self.block.clear();
        self.count = 0;
        Ok(())
    }

    /// Records added so far.
    pub fn records(&self) -> u64 {
        self.records
    }

    /// Bytes handed to the output so far (the current block not yet among them).
    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    pub fn sync_marker(&self) -> [u8; 16] {
        self.sync
    }

    /// Write the last block and flush, returning the output.
    pub fn finish(mut self) -> io::Result<W> {
        self.flush_block()?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

/// The largest block the Java library reads: its reader refuses a block size over `i32::MAX`.
pub const MAX_BLOCK_SIZE: usize = i32::MAX as usize;

fn check_block_size(n: usize) -> io::Result<()> {
    if n > MAX_BLOCK_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a block of {n} bytes is larger than Avro readers accept ({MAX_BLOCK_SIZE}); write fewer records per block"),
        ));
    }
    Ok(())
}

/// A sync marker that differs from file to file: 128 bits of SipHash keyed from the operating
/// system's randomness (std's `RandomState`, whose keys also differ between calls), over the
/// time. Sync markers only need to be unlikely to occur in the data, not secret.
pub fn random_sync() -> [u8; 16] {
    use std::hash::{BuildHasher, Hasher};
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut out = [0u8; 16];
    for (k, chunk) in out.chunks_mut(8).enumerate() {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(now);
        h.write_usize(k);
        chunk.copy_from_slice(&h.finish().to_le_bytes());
    }
    out
}

/// A sync marker written as 32 hex digits (whitespace ignored).
pub fn parse_sync(hex: &str) -> Option<[u8; 16]> {
    crate::text::hex16(&hex.chars().filter(|c| !c.is_whitespace()).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(block_records: usize) -> WriterOptions {
        WriterOptions { codec: Codec::Null, block_records, sync: Some([7; 16]), ..Default::default() }
    }

    #[test]
    fn blocks_by_count_and_by_size() {
        let mut w = Writer::new(Vec::new(), r#""long""#, opts(2)).unwrap();
        for i in 0..5 {
            w.append(&Value::Long(i)).unwrap();
        }
        assert_eq!(w.records(), 5);
        let out = w.finish().unwrap();
        // three blocks (2, 2, 1), each ending with the marker
        assert_eq!(out.windows(16).filter(|s| *s == [7; 16]).count(), 4);

        let o = WriterOptions { block_bytes: Some(3), ..opts(1000) };
        let mut w = Writer::new(Vec::new(), r#""string""#, o).unwrap();
        for s in ["ab", "cd", "e"] {
            w.append(&Value::String(s.into())).unwrap();
        }
        let out = w.finish().unwrap();
        assert_eq!(out.windows(16).filter(|s| *s == [7; 16]).count(), 4, "each record fills a block");
    }

    #[test]
    fn no_records_is_just_the_header() {
        let out = Writer::new(Vec::new(), r#""null""#, opts(0)).unwrap().finish().unwrap();
        assert!(out.starts_with(MAGIC));
        assert!(out.ends_with(&[7; 16]));
    }

    #[test]
    fn a_failed_record_is_taken_back() {
        let mut w = Writer::new(Vec::new(), r#""long""#, opts(0)).unwrap();
        w.append(&Value::Long(1)).unwrap();
        let r: Result<(), io::Error> = w.append_with(|out| {
            out.extend_from_slice(&[1, 2, 3]);
            Err(io::Error::other("no"))
        });
        assert!(r.is_err());
        assert_eq!(w.records(), 1);
        assert_eq!(w.block, [2]);
    }

    #[test]
    fn reserved_metadata_is_refused() {
        let o = WriterOptions { metadata: vec![("avro.codec".into(), b"x".to_vec())], ..opts(0) };
        assert!(Writer::new(Vec::new(), r#""null""#, o).is_err());
    }

    #[test]
    fn metadata_keys_given_twice_are_refused() {
        let o = WriterOptions { metadata: vec![("app".into(), b"a".to_vec()), ("v".into(), b"1".to_vec()), ("app".into(), b"b".to_vec())], ..opts(0) };
        let e = Writer::new(Vec::new(), r#""null""#, o).err().unwrap();
        assert!(e.to_string().contains("app is given twice"), "{e}");
    }

    #[test]
    fn blocks_java_cannot_read_are_refused() {
        assert!(check_block_size(MAX_BLOCK_SIZE).is_ok());
        assert!(check_block_size(MAX_BLOCK_SIZE + 1).is_err());
    }

    #[test]
    fn sync_markers() {
        assert_eq!(parse_sync("000102030405060708090a0b0c0d0e0f"), Some([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]));
        assert_eq!(parse_sync("00 01 02 03 04 05 06 07 08 09 0A 0B 0C 0D 0E 0F"), Some([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]));
        assert_eq!(parse_sync("00 01"), None);
        assert_eq!(parse_sync("zz0102030405060708090a0b0c0d0e0f"), None);
        // from_str_radix takes a sign: "+f" is not two hex digits
        assert_eq!(parse_sync("+f0102030405060708090a0b0c0d0e0f"), None);
        let markers: std::collections::HashSet<[u8; 16]> = (0..1000).map(|_| random_sync()).collect();
        assert_eq!(markers.len(), 1000);
    }
}
