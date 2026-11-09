//! What avro-kit writes, read back by the Apache Avro library, for every codec.

use apache_avro::types::Value as A;
use avro_kit::text::{Unit, decimal_bytes, parse_date, parse_timestamp, parse_uuid};
use avro_kit::{Codec, Value, Writer, WriterOptions};

const SCHEMA: &str = r#"{"type":"record","name":"Order","namespace":"shop","fields":[
    {"name":"id","type":"long"},
    {"name":"customer","type":["null","string"]},
    {"name":"lines","type":{"type":"array","items":{"type":"record","name":"Line","fields":[
        {"name":"sku","type":"string"},
        {"name":"qty","type":"int"},
        {"name":"price","type":{"type":"bytes","logicalType":"decimal","precision":9,"scale":2}}]}}},
    {"name":"tags","type":{"type":"map","values":"double"}},
    {"name":"status","type":{"type":"enum","name":"Status","symbols":["NEW","PAID","SENT"]}},
    {"name":"ref","type":{"type":"string","logicalType":"uuid"}},
    {"name":"day","type":{"type":"int","logicalType":"date"}},
    {"name":"at","type":{"type":"long","logicalType":"timestamp-millis"}},
    {"name":"hash","type":{"type":"fixed","name":"Hash","size":4}},
    {"name":"ok","type":"boolean"},
    {"name":"ratio","type":"float"},
    {"name":"blob","type":"bytes"}]}"#;

fn order(i: i64) -> Value {
    let line = |sku: &str, qty: i32, price: &str| {
        Value::Record(vec![Value::String(sku.into()), Value::Int(qty), Value::Bytes(decimal_bytes(price, 2, Some(9), None).unwrap())])
    };
    let uuid = parse_uuid("123e4567-e89b-12d3-a456-426614174000").unwrap();
    Value::Record(vec![
        Value::Long(i),
        if i % 2 == 0 { Value::Union(0, Box::new(Value::Null)) } else { Value::Union(1, Box::new(Value::String(format!("customer {i}")))) },
        Value::Array((0..(i % 4)).map(|k| line(&format!("SKU-{k}"), k as i32 + 1, "-12.34")).collect()),
        Value::Map(vec![("a".into(), Value::Double(1.5)), ("b".into(), Value::Double(-0.25))]),
        Value::Enum((i % 3) as u32),
        Value::String(avro_kit::text::uuid_text(&uuid)),
        Value::Int(parse_date("2026-11-09").unwrap() as i32),
        Value::Long(parse_timestamp("2026-11-09T10:00:00.250+01:00", Unit::Millis, true).unwrap()),
        Value::Fixed(vec![1, 2, 3, i as u8]),
        Value::Boolean(i % 2 == 1),
        Value::Float(0.5),
        Value::Bytes(vec![0, 255, i as u8]),
    ])
}

fn field<'a>(r: &'a A, name: &str) -> &'a A {
    let A::Record(fields) = r else { panic!("not a record: {r:?}") };
    &fields.iter().find(|(n, _)| n == name).unwrap_or_else(|| panic!("no field {name}")).1
}

#[test]
fn apache_avro_reads_every_codec() {
    for codec in Codec::ALL {
        let opts = WriterOptions { codec, block_records: 7, metadata: vec![("app".into(), b"avro-kit".to_vec())], ..Default::default() };
        let mut w = Writer::new(Vec::new(), SCHEMA, opts).unwrap();
        for i in 0..50 {
            w.append(&order(i)).unwrap();
        }
        let bytes = w.finish().unwrap();
        let reader = apache_avro::Reader::new(&bytes[..]).unwrap_or_else(|e| panic!("{}: {e}", codec.name()));
        assert_eq!(reader.user_metadata().get("app").map(Vec::as_slice), Some(&b"avro-kit"[..]));
        let records: Vec<A> = reader.map(|r| r.unwrap_or_else(|e| panic!("{}: {e}", codec.name()))).collect();
        assert_eq!(records.len(), 50, "{}", codec.name());
        for (i, r) in records.iter().enumerate() {
            let i = i as i64;
            assert_eq!(field(r, "id"), &A::Long(i));
            match field(r, "customer") {
                A::Union(0, v) => assert_eq!(**v, A::Null),
                A::Union(1, v) => assert_eq!(**v, A::String(format!("customer {i}"))),
                other => panic!("{other:?}"),
            }
            let A::Array(lines) = field(r, "lines") else { panic!() };
            assert_eq!(lines.len() as i64, i % 4);
            if let Some(l) = lines.first() {
                assert_eq!(field(l, "price"), &A::Decimal(apache_avro::Decimal::from(decimal_bytes("-12.34", 2, None, None).unwrap())));
            }
            let A::Map(tags) = field(r, "tags") else { panic!() };
            assert_eq!(tags.get("b"), Some(&A::Double(-0.25)));
            let A::Enum(k, symbol) = field(r, "status") else { panic!() };
            assert_eq!((*k as i64, symbol.as_str()), (i % 3, ["NEW", "PAID", "SENT"][(i % 3) as usize]));
            let A::Uuid(u) = field(r, "ref") else { panic!("{:?}", field(r, "ref")) };
            assert_eq!(u.to_string(), "123e4567-e89b-12d3-a456-426614174000");
            assert_eq!(field(r, "day"), &A::Date(20766));
            assert_eq!(field(r, "at"), &A::TimestampMillis(1_794_214_800_250));
            assert_eq!(field(r, "hash"), &A::Fixed(4, vec![1, 2, 3, i as u8]));
            assert_eq!(field(r, "ok"), &A::Boolean(i % 2 == 1));
            assert_eq!(field(r, "ratio"), &A::Float(0.5));
            assert_eq!(field(r, "blob"), &A::Bytes(vec![0, 255, i as u8]));
        }
    }
}

#[test]
fn apache_avro_output_decompresses_with_our_codecs() {
    // Blocks written by the Apache library (one per codec) decompress to the same records.
    let schema = apache_avro::Schema::parse_str(r#"{"type":"record","name":"R","fields":[{"name":"s","type":"string"}]}"#).unwrap();
    let codecs = [
        (apache_avro::Codec::Null, Codec::Null),
        (apache_avro::Codec::Deflate(Default::default()), Codec::Deflate),
        (apache_avro::Codec::Snappy, Codec::Snappy),
        (apache_avro::Codec::Zstandard(Default::default()), Codec::Zstandard),
        (apache_avro::Codec::Bzip2(Default::default()), Codec::Bzip2),
        (apache_avro::Codec::Xz(Default::default()), Codec::Xz),
    ];
    for (theirs, ours) in codecs {
        let mut w = apache_avro::Writer::with_codec(&schema, Vec::new(), theirs).unwrap();
        for i in 0..100 {
            w.append_value(A::Record(vec![("s".into(), A::String(format!("record {i}")))])).unwrap();
        }
        let bytes = w.into_inner().unwrap();
        // The single block follows the header: find it from the end (data, then the sync marker).
        let sync = &bytes[bytes.len() - 16..];
        let header_end = bytes.windows(16).position(|s| s == sync).unwrap() + 16;
        let mut pos = header_end;
        let count = avro_kit::binary::read_long_at(&bytes, &mut pos).unwrap();
        let size = avro_kit::binary::read_long_at(&bytes, &mut pos).unwrap() as usize;
        assert_eq!(count, 100);
        let data = ours.decompress(bytes[pos..pos + size].to_vec(), 1 << 20).unwrap_or_else(|e| panic!("{}: {e}", ours.name()));
        let mut expected = Vec::new();
        for i in 0..100 {
            avro_kit::binary::encode(&mut expected, &Value::Record(vec![Value::String(format!("record {i}"))]));
        }
        assert_eq!(data, expected, "{}", ours.name());
    }
}

#[test]
fn apache_avro_reads_files_without_records() {
    // Just the header (magic, metadata, sync marker), with no blocks: as the Java library writes
    // an empty file.
    for codec in Codec::ALL {
        let opts = WriterOptions { codec, metadata: vec![("app".into(), b"avro-kit".to_vec())], sync: Some([9; 16]), ..Default::default() };
        let bytes = Writer::new(Vec::new(), SCHEMA, opts).unwrap().finish().unwrap();
        assert!(bytes.ends_with(&[9; 16]));
        let reader = apache_avro::Reader::new(&bytes[..]).unwrap_or_else(|e| panic!("{}: {e}", codec.name()));
        assert_eq!(reader.count(), 0, "{}", codec.name());
    }
}
