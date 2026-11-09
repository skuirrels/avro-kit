//! Avro's binary encoding: zigzag varints, length-prefixed bytes, and whole values.
//!
//! Decoding a value needs its schema, so it stays with each application's schema model; this
//! module has what decoding and encoding share.

use crate::Value;

/// Append `v` as a zigzag variable-length long (also how `int` is written).
pub fn write_long(out: &mut Vec<u8>, v: i64) {
    let mut z = ((v << 1) ^ (v >> 63)) as u64;
    while z >= 0x80 {
        out.push((z as u8 & 0x7f) | 0x80);
        z >>= 7;
    }
    out.push(z as u8);
}

/// Append bytes (or a string's UTF-8), their length first.
pub fn write_bytes(out: &mut Vec<u8>, b: &[u8]) {
    write_long(out, b.len() as i64);
    out.extend_from_slice(b);
}

/// Read a zigzag variable-length long, its bytes from `next`. Refuses one longer than 64 bits.
pub fn read_long<E: From<&'static str>>(mut next: impl FnMut() -> Result<u8, E>) -> Result<i64, E> {
    let mut value: u64 = 0;
    for shift in (0..70).step_by(7) {
        let b = next()?;
        if shift == 63 && b > 1 {
            return Err("a number is longer than 64 bits".into());
        }
        value |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(((value >> 1) as i64) ^ -((value & 1) as i64));
        }
    }
    Err("a number is longer than 64 bits".into())
}

/// Read a zigzag long from `buf` at `*pos`, moving `pos` past it.
pub fn read_long_at(buf: &[u8], pos: &mut usize) -> Result<i64, String> {
    read_long(|| {
        let b = *buf.get(*pos).ok_or("the data ends inside a number")?;
        *pos += 1;
        Ok::<u8, String>(b)
    })
}

/// Append the binary encoding of `v`. Arrays and maps are written as one block (when not empty)
/// and the closing zero count, as the Java and Rust libraries write them.
///
/// The value is trusted to fit its schema: it's the caller's schema walk that builds it.
pub fn encode(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => {}
        Value::Boolean(b) => out.push(u8::from(*b)),
        Value::Int(i) => write_long(out, i64::from(*i)),
        Value::Long(i) => write_long(out, *i),
        Value::Float(f) => out.extend_from_slice(&f.to_le_bytes()),
        Value::Double(f) => out.extend_from_slice(&f.to_le_bytes()),
        Value::Bytes(b) => write_bytes(out, b),
        Value::String(s) => write_bytes(out, s.as_bytes()),
        Value::Record(fields) => fields.iter().for_each(|f| encode(out, f)),
        Value::Enum(i) => write_long(out, i64::from(*i)),
        Value::Array(items) => {
            if !items.is_empty() {
                write_long(out, items.len() as i64);
                items.iter().for_each(|i| encode(out, i));
            }
            out.push(0);
        }
        Value::Map(entries) => {
            if !entries.is_empty() {
                write_long(out, entries.len() as i64);
                for (k, v) in entries {
                    write_bytes(out, k.as_bytes());
                    encode(out, v);
                }
            }
            out.push(0);
        }
        Value::Union(i, v) => {
            write_long(out, i64::from(*i));
            encode(out, v);
        }
        Value::Fixed(b) => out.extend_from_slice(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long(v: i64) -> Vec<u8> {
        let mut o = Vec::new();
        write_long(&mut o, v);
        o
    }

    #[test]
    fn zigzag_varints_match_the_spec() {
        for (v, b) in [(0, vec![0x00]), (-1, vec![0x01]), (1, vec![0x02]), (-2, vec![0x03]), (2, vec![0x04]), (-64, vec![0x7f]), (64, vec![0x80, 0x01])] {
            assert_eq!(long(v), b, "{v}");
            assert_eq!(read_long_at(&b, &mut 0).unwrap(), v);
        }
        for v in [i64::MIN, i64::MAX, -1_234_567_890_123, 300] {
            assert_eq!(read_long_at(&long(v), &mut 0).unwrap(), v);
        }
        assert!(read_long_at(&[0xff; 10], &mut 0).is_err());
        assert!(read_long_at(&[0x80], &mut 0).is_err());
        // The ends of i64 take all ten bytes; the tenth holds the one bit left.
        let mut max = vec![0xfe];
        max.extend([0xff; 8]);
        max.push(0x01);
        assert_eq!(long(i64::MAX), max);
        assert_eq!(long(i64::MIN), [[0xff; 9].as_slice(), &[0x01]].concat());
        let mut pos = 0;
        assert_eq!(read_long_at(&max, &mut pos).unwrap(), i64::MAX);
        assert_eq!(pos, 10);
        // A tenth byte with more than that bit, or an eleventh byte, is past 64 bits.
        assert!(read_long_at(&[[0xff; 9].as_slice(), &[0x02]].concat(), &mut 0).is_err());
        assert!(read_long_at(&[[0xff; 9].as_slice(), &[0x81, 0x00]].concat(), &mut 0).is_err());
        // Overlong but in range (as Java accepts): zero in two bytes.
        assert_eq!(read_long_at(&[0x80, 0x00], &mut 0).unwrap(), 0);
    }

    #[test]
    fn values_match_the_spec() {
        let mut o = Vec::new();
        write_bytes(&mut o, b"foo");
        assert_eq!(o, [0x06, 0x66, 0x6f, 0x6f]);
        // record {a: long = 27, b: string = "foo"}
        let mut o = Vec::new();
        encode(&mut o, &Value::Record(vec![Value::Long(27), Value::String("foo".into())]));
        assert_eq!(o, [0x36, 0x06, 0x66, 0x6f, 0x6f]);
        let mut o = Vec::new();
        encode(&mut o, &Value::Array(vec![Value::Long(3), Value::Long(27)]));
        assert_eq!(o, [0x04, 0x06, 0x36, 0x00]);
        let mut o = Vec::new();
        encode(&mut o, &Value::Union(1, Box::new(Value::String("a".into()))));
        assert_eq!(o, [0x02, 0x02, 0x61]);
        let mut o = Vec::new();
        encode(&mut o, &Value::Fixed(vec![1, 2]));
        encode(&mut o, &Value::Bytes(vec![1, 2]));
        assert_eq!(o, [1, 2, 4, 1, 2]);
    }
}
