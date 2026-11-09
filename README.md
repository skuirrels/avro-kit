# avro-kit

Apache Avro building blocks shared by Tessera and Mapster. Pure Rust, with no C toolchain needed.

Each application keeps its own schema model and decoder. This crate has the parts that must agree between them:

| Module | What it does |
| --- | --- |
| `Value` | A datum, as a schema-guided decoder produces it or an encoder consumes it |
| `binary` | Zigzag varints, length-prefixed bytes, and the binary encoding of a `Value` |
| `codec` | null, deflate, snappy, zstandard, bzip2 and xz, compressing and decompressing, with a limit on a block's decompressed size |
| `container` | A streaming object container file writer: blocks by record count or size, user metadata, fixed or random sync marker |
| `text` | Logical types and bytes as XML Schema's lexical forms and back: `xs:date`, `xs:time`, `xs:dateTime` (with offsets), exact decimals and big-decimals, UUIDs, `xs:duration`, base64, floats |

## Example

```rust
use avro_kit::{Codec, Value, Writer, WriterOptions};

let file = std::fs::File::create("out.avro")?;
let opts = WriterOptions { codec: Codec::Zstandard, ..Default::default() };
let mut w = Writer::new(std::io::BufWriter::new(file), r#"{"type":"record","name":"R","fields":[{"name":"n","type":"long"}]}"#, opts)?;
w.append(&Value::Record(vec![Value::Long(1)]))?;
w.finish()?;
```

## Tests

`cargo test` runs the unit tests and an oracle test: files written with every codec are read back by the Apache Avro library (`apache-avro`, a dev-dependency only), and blocks it writes decompress with this crate's codecs. A third test counts allocations to check that a hostile block is refused without decompressing past the limit.

## Licence

MIT. See [LICENSE](LICENSE).
