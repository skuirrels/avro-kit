/// An Avro datum. Records keep their fields in schema order; enums and unions keep the index of
/// their symbol or branch, so a value means nothing without the schema it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Boolean(bool),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Bytes(Vec<u8>),
    String(String),
    Record(Vec<Value>),
    Enum(u32),
    Array(Vec<Value>),
    Map(Vec<(String, Value)>),
    Union(u32, Box<Value>),
    Fixed(Vec<u8>),
}
