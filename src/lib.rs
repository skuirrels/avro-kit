//! Apache Avro building blocks shared by Tessera and Mapster.
//!
//! What's here doesn't depend on how an application models schemas: each app keeps its own
//! schema parser and decoder (they serve different jobs) and uses this crate for the parts that
//! must agree byte for byte and character for character.
//!
//! - [`Value`]: a datum, as a schema-guided decoder produces it or an encoder consumes it.
//! - [`binary`]: Avro's binary encoding of values, zigzag varints and length-prefixed bytes.
//! - [`codec`]: the six container codecs (null, deflate, snappy, zstandard, bzip2, xz), both ways,
//!   with a limit on what a block may decompress to.
//! - [`container`]: a streaming object container file writer.
//! - [`text`]: logical types and bytes as text (XML Schema's lexical forms) and back: dates,
//!   times, timestamps, decimals, UUIDs, durations, base64, floats.

pub mod binary;
pub mod codec;
pub mod container;
pub mod text;
mod value;

pub use codec::Codec;
pub use container::{Writer, WriterOptions};
pub use value::Value;
