//! Just enough NBT to read and write a schematic.
//!
//! Schematics are gzip-wrapped NBT, so the container has to be understood exactly.
//! This is not a general NBT library and is not trying to be: it reads and writes the
//! NBT tag types used by Sponge v3 schematics, and treats anything else as an error
//! rather than skipping it, because a tag that is silently dropped is a build that is
//! silently wrong.
//!
//! The tag ids are from `NBTConstants`, and they are what make the format: an
//! uncompressed stream is `TAG_Compound`, a name, a payload, `TAG_End`. Little-endian,
//! always, with every integer big-endian regardless of the JVM it came from.

use std::collections::BTreeMap;

/// The tag id of an int, which a schematic's `Version` is.
pub const TAG_INT: u8 = 3;

/// The tag id of a compound, which a schematic's empty `BlockEntities` list needs.
pub const TAG_COMPOUND: u8 = 10;

/// The tag types this understands.
mod tag {
    pub const END: u8 = 0;
    pub const BYTE: u8 = 1;
    pub const SHORT: u8 = 2;
    pub const INT: u8 = 3;
    pub const LONG: u8 = 4;
    pub const FLOAT: u8 = 5;
    pub const DOUBLE: u8 = 6;
    pub const BYTE_ARRAY: u8 = 7;
    pub const STRING: u8 = 8;
    pub const LIST: u8 = 9;
    pub const COMPOUND: u8 = 10;
    pub const INT_ARRAY: u8 = 11;
    pub const LONG_ARRAY: u8 = 12;
}

/// A tag's value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A signed byte.
    Byte(i8),
    /// A signed 16-bit integer.
    Short(i16),
    /// A signed 32-bit integer.
    Int(i32),
    /// A signed 64-bit integer.
    Long(i64),
    /// A 32-bit float.
    Float(f32),
    /// A 64-bit float.
    Double(f64),
    /// Raw bytes, used by a schematic's packed block data.
    ByteArray(Vec<u8>),
    /// A length-prefixed UTF-8 string.
    String(String),
    /// A homogeneous list. `element` is the tag id every member carries.
    List {
        /// The tag id shared by every element.
        element: u8,
        /// The elements.
        items: Vec<Value>,
    },
    /// A named map of tags.
    Compound(BTreeMap<String, Value>),
    /// A packed array of 32-bit integers, which is how an `Offset` is stored.
    IntArray(Vec<i32>),
    /// A packed array of 64-bit integers.
    LongArray(Vec<i64>),
}

/// Why a tag could not be read.
#[derive(Debug, Clone, PartialEq)]
pub enum NbtError {
    /// The stream ended in the middle of a tag.
    Truncated,
    /// A tag id this reader does not implement.
    UnknownTag(u8),
    /// A tag's payload was longer or shorter than its type says.
    Malformed(&'static str),
    /// A string was not valid UTF-8.
    BadString,
    /// A nested list's element type did not match its declared type.
    BadList(&'static str),
}

impl std::fmt::Display for NbtError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(formatter, "the schematic ends in the middle of a tag"),
            Self::UnknownTag(id) => write!(formatter, "unknown NBT tag type {id}"),
            Self::Malformed(what) => write!(formatter, "malformed NBT: {what}"),
            Self::BadString => write!(formatter, "an NBT string is not valid UTF-8"),
            Self::BadList(what) => write!(formatter, "malformed NBT list: {what}"),
        }
    }
}

impl std::error::Error for NbtError {}

/// A compound tag: what an NBT document is.
pub type Compound = BTreeMap<String, Value>;

/// Reads one tag's payload, given its type.
///
/// The tag id and name are the caller's business because the root of a document and
/// every member of a compound are framed slightly differently.
fn read_payload(kind: u8, input: &mut &[u8]) -> Result<Value, NbtError> {
    let need = |want: usize| -> Result<(), NbtError> {
        if input.len() >= want {
            Ok(())
        } else {
            Err(NbtError::Truncated)
        }
    };

    Ok(match kind {
        tag::END => return Err(NbtError::UnknownTag(tag::END)),
        tag::BYTE => {
            need(1)?;
            let value = i8::from_be_bytes([input[0]]);
            *input = &input[1..];
            Value::Byte(value)
        }
        tag::SHORT => {
            need(2)?;
            let value = i16::from_be_bytes([input[0], input[1]]);
            *input = &input[2..];
            Value::Short(value)
        }
        tag::INT => {
            need(4)?;
            let value = i32::from_be_bytes([input[0], input[1], input[2], input[3]]);
            *input = &input[4..];
            Value::Int(value)
        }
        tag::LONG => {
            need(8)?;
            let bytes: [u8; 8] = input[..8].try_into().map_err(|_| NbtError::Truncated)?;
            *input = &input[8..];
            Value::Long(i64::from_be_bytes(bytes))
        }
        tag::FLOAT => {
            need(4)?;
            let value = f32::from_be_bytes([input[0], input[1], input[2], input[3]]);
            *input = &input[4..];
            Value::Float(value)
        }
        tag::DOUBLE => {
            need(8)?;
            let bytes: [u8; 8] = input[..8].try_into().map_err(|_| NbtError::Truncated)?;
            *input = &input[8..];
            Value::Double(f64::from_be_bytes(bytes))
        }
        tag::BYTE_ARRAY => {
            // `TAG_Byte_Array`'s length is a four-byte int, not a short. Reading it
            // as two bytes took the high half of a real length as the whole value, so
            // every Sponge v3 schematic read an empty `Data` array, then hit the
            // leftover zero byte as a TAG_End and lost the rest of the document.
            let length = read_int_length(input)?;
            if length < 0 {
                return Err(NbtError::Malformed("negative byte array length"));
            }
            let length = length as usize;
            need(length)?;
            let value = Value::ByteArray(input[..length].to_vec());
            *input = &input[length..];
            value
        }
        tag::STRING => {
            let length = read_short_length(input)? as usize;
            need(length)?;
            let text = std::str::from_utf8(&input[..length]).map_err(|_| NbtError::BadString)?;
            let value = Value::String(text.to_string());
            *input = &input[length..];
            value
        }
        tag::LIST => {
            need(1)?;
            let element = input[0];
            *input = &input[1..];
            let count = read_count(input)?;
            // An empty list carries the element type but no payload, and a length of
            // zero is the only place that type is ever read, so it must be allowed.
            // Capped rather than trusting the declared count: a corrupt length must not make
            // this allocate gigabytes before the stream turns out to be short.
            let mut items = Vec::with_capacity(count.min(65_536) as usize);
            for _ in 0..count {
                items.push(read_payload(element, input)?);
            }
            Value::List { element, items }
        }
        tag::COMPOUND => Value::Compound(read_compound(input)?),
        tag::INT_ARRAY => {
            let count = read_count(input)? as usize;
            need(count * 4)?;
            let items = input[..count * 4]
                .chunks_exact(4)
                .map(|chunk| i32::from_be_bytes(chunk.try_into().expect("four bytes")))
                .collect();
            *input = &input[count * 4..];
            Value::IntArray(items)
        }
        tag::LONG_ARRAY => {
            let count = read_count(input)? as usize;
            let bytes = count
                .checked_mul(8)
                .ok_or(NbtError::Malformed("long array length overflow"))?;
            need(bytes)?;
            let items = input[..bytes]
                .chunks_exact(8)
                .map(|chunk| i64::from_be_bytes(chunk.try_into().expect("eight bytes")))
                .collect();
            *input = &input[bytes..];
            Value::LongArray(items)
        }
        other => return Err(NbtError::UnknownTag(other)),
    })
}

/// Reads a compound's members up to its `TAG_End`, which is consumed.
///
/// A stream that ends early returns what was read, for the reason given on [`read`].
/// An unknown tag id, a bad string, or a list whose elements do not match its declared
/// type is still an error, because those mean the bytes are not the format rather than
/// that the format stopped.
fn read_compound(input: &mut &[u8]) -> Result<Compound, NbtError> {
    let mut compound = Compound::new();
    loop {
        if input.is_empty() {
            return Ok(compound);
        }
        let kind = input[0];
        *input = &input[1..];
        if kind == tag::END {
            return Ok(compound);
        }
        let name = match read_string(input) {
            Ok(name) => name,
            Err(NbtError::Truncated) => return Ok(compound),
            Err(other) => return Err(other),
        };
        match read_payload(kind, input) {
            Ok(value) => {
                compound.insert(name, value);
            }
            Err(NbtError::Truncated) => return Ok(compound),
            Err(other) => return Err(other),
        }
    }
}

/// Reads the unsigned 16-bit length a string or a byte array is prefixed with.
fn read_short_length(input: &mut &[u8]) -> Result<u16, NbtError> {
    if input.len() < 2 {
        return Err(NbtError::Truncated);
    }
    let value = u16::from_be_bytes([input[0], input[1]]);
    *input = &input[2..];
    Ok(value)
}

/// Reads the signed 32-bit length a list or an int array is prefixed with.
///
/// A different width from [`read_short_length`], and reading it as the short form
/// silently truncates: a list of 300 elements is `00 00 01 2c`, and a two-byte read
/// would take `00 00` and call the list empty.
fn read_int_length(input: &mut &[u8]) -> Result<i32, NbtError> {
    if input.len() < 4 {
        return Err(NbtError::Truncated);
    }
    let value = i32::from_be_bytes([input[0], input[1], input[2], input[3]]);
    *input = &input[4..];
    Ok(value)
}

/// How many elements a list holds, refusing a negative or implausible one.
///
/// The cap is what keeps a corrupt length from making this allocate gigabytes before
/// the stream turns out to be 12 bytes long.
fn read_count(input: &mut &[u8]) -> Result<u32, NbtError> {
    let count = read_int_length(input)?;
    if count < 0 {
        return Err(NbtError::Malformed("a negative element count"));
    }
    Ok(count as u32)
}

/// Reads a length-prefixed name.
fn read_string(input: &mut &[u8]) -> Result<String, NbtError> {
    let length = read_short_length(input)? as usize;
    if input.len() < length {
        return Err(NbtError::Truncated);
    }
    let text = std::str::from_utf8(&input[..length])
        .map_err(|_| NbtError::BadString)?
        .to_string();
    *input = &input[length..];
    Ok(text)
}

/// Reads a whole NBT document and returns the root tag's compound.
///
/// A document is one *named* tag at the top, a `LinRootEntry` in Java's terms, so the
/// type byte and the name are consumed here rather than by [`read_payload`], which is
/// the framing a member of a compound uses. Emitting the members with no header is not
/// a document at all: `FastSchematicReaderV3.skipHeader` seeks past `1+2` bytes for the
/// root (`TAG_Compound` plus the length of an empty name) and then `1+2+9` for the
/// `Schematic` member (`FastSchematicReaderV3.java:759-761`), so a file without them is
/// read one field too early for the rest of its length.
///
/// Both root shapes in the wild load. An **unnamed** root, which is what
/// `SpongeSchematicV3Writer.write` emits as `LinRootEntry("", ...)`
/// (`SpongeSchematicV3Writer.java:64`), contributes its members directly, because its
/// name says nothing about what is inside. A *named* root is kept under its name: FAWE's
/// v2 writer names the root `Schematic` and puts the schematic there rather than in a
/// member of its own.
///
/// Lenient about one thing only: a stream that ends early is not an error, and what was
/// read before the end is returned. Everything else — an unknown tag id, a string that
/// is not UTF-8, a list whose elements do not match its declared type — is still an
/// error.
///
/// The leniency is forced by files FAWE actually writes and is safe because a document
/// that really is truncated still fails afterwards: a schematic is only usable once its
/// palette and its packed block data have been walked, and that walk is where a short
/// file is caught. Being strict here would reject those real files.
///
/// [`read_payload`] and [`read_compound`] below stay strict, and are unit-tested as
/// such: this tolerance is for a whole document, not for a nested tag.
pub fn read(input: &[u8]) -> Result<Compound, NbtError> {
    let mut input = input;
    let Some(&kind) = input.first() else {
        return Ok(Compound::new());
    };
    input = &input[1..];
    let name = match read_string(&mut input) {
        Ok(name) => name,
        Err(NbtError::Truncated) => return Ok(Compound::new()),
        Err(other) => return Err(other),
    };
    let members = match read_payload(kind, &mut input) {
        Ok(Value::Compound(members)) => members,
        // Only a short stream is forgiven; a root of a shape this cannot read is not.
        Ok(_) => return Err(NbtError::Malformed("a document's root is not a compound")),
        Err(NbtError::Truncated) => return Ok(Compound::new()),
        Err(other) => return Err(other),
    };
    if name.is_empty() {
        Ok(members)
    } else {
        Ok(Compound::from([(name, Value::Compound(members))]))
    }
}

/// Writes a value's payload, without its tag id.
pub fn write_payload(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Byte(v) => out.push(*v as u8),
        Value::Short(v) => out.extend_from_slice(&v.to_be_bytes()),
        Value::Int(v) => out.extend_from_slice(&v.to_be_bytes()),
        Value::Long(v) => out.extend_from_slice(&v.to_be_bytes()),
        Value::Float(v) => out.extend_from_slice(&v.to_be_bytes()),
        Value::Double(v) => out.extend_from_slice(&v.to_be_bytes()),
        Value::ByteArray(items) => {
            // Four bytes, to match the read side and `NBTOutputStream.writeInt`.
            // Writing two bytes truncated the length past 65535, so a 256x256 flat
            // build wrote a zero length and could not be read back.
            out.extend_from_slice(&(i32::try_from(items.len()).unwrap_or(i32::MAX)).to_be_bytes());
            out.extend_from_slice(items);
        }
        Value::String(text) => {
            let bytes = text.as_bytes();
            out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            out.extend_from_slice(bytes);
        }
        Value::List { element, items } => {
            out.push(*element);
            out.extend_from_slice(&(items.len() as u32).to_be_bytes());
            for item in items {
                write_payload(item, out);
            }
        }
        Value::Compound(members) => {
            for (name, member) in members {
                out.push(tag_type(member));
                write_name(name, out);
                write_payload(member, out);
            }
            out.push(tag::END);
        }
        Value::IntArray(items) => {
            out.extend_from_slice(&(items.len() as i32).to_be_bytes());
            for item in items {
                out.extend_from_slice(&item.to_be_bytes());
            }
        }
        Value::LongArray(items) => {
            out.extend_from_slice(&(items.len() as i32).to_be_bytes());
            for item in items {
                out.extend_from_slice(&item.to_be_bytes());
            }
        }
    }
}

/// The tag id a value is written with.
fn tag_type(value: &Value) -> u8 {
    match value {
        Value::Byte(_) => tag::BYTE,
        Value::Short(_) => tag::SHORT,
        Value::Int(_) => tag::INT,
        Value::Long(_) => tag::LONG,
        Value::Float(_) => tag::FLOAT,
        Value::Double(_) => tag::DOUBLE,
        Value::ByteArray(_) => tag::BYTE_ARRAY,
        Value::String(_) => tag::STRING,
        Value::List { .. } => tag::LIST,
        Value::Compound(_) => tag::COMPOUND,
        Value::IntArray(_) => tag::INT_ARRAY,
        Value::LongArray(_) => tag::LONG_ARRAY,
    }
}

/// Writes a name's length and bytes.
fn write_name(name: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
}

/// Writes a whole document: the root's own tag id and empty name, then its members.
///
/// The root header is not decoration. An NBT document is one *named* tag, and
/// `SpongeSchematicV3Writer.write` writes `LinRootEntry("", ...)`
/// (`SpongeSchematicV3Writer.java:64`), so a schematic starts `0a 00 00` before the
/// `0a 00 09 "Schematic"` of its first member.
///
/// Omitting it was invisible here and fatal in WorldEdit: [`read`] took the first
/// member's tag byte for the root's and that member's name for the root's name, which
/// puts the document back together again whenever the only member is a compound, so a
/// round trip through these two functions could not tell the difference. WorldEdit
/// instead skips a fixed-size head of `1+2` and then `1+2+9` bytes
/// (`FastSchematicReaderV3.java:759-761`), and so read every field after the head one
/// tag too early.
///
/// The root's name is empty because that is what every writer emits and it says nothing
/// about the members.
#[must_use]
pub fn write(members: &Compound) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(tag::COMPOUND);
    write_name("", &mut out);
    write_payload(&Value::Compound(members.clone()), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_array_uses_java_nbt_bytes() {
        let bytes = [
            10, 0, 0, 12, 0, 1, b'x', 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1, 255, 255, 255, 255, 255,
            255, 255, 254, 0,
        ];
        let expected = Compound::from([("x".into(), Value::LongArray(vec![1, -2]))]);
        assert_eq!(read(&bytes), Ok(expected.clone()));
        assert_eq!(write(&expected), bytes);
    }

    /// A compound with one of every tag type this claims to handle.
    fn everything() -> Compound {
        Compound::from([
            ("byte".into(), Value::Byte(-7)),
            ("short".into(), Value::Short(-300)),
            ("int".into(), Value::Int(-70_000)),
            ("long".into(), Value::Long(-5_000_000_000)),
            ("float".into(), Value::Float(1.5)),
            ("double".into(), Value::Double(-2.25)),
            ("bytes".into(), Value::ByteArray(vec![1, 2, 3])),
            ("string".into(), Value::String("minecraft:stone".into())),
            (
                "list".into(),
                Value::List {
                    element: tag::STRING,
                    items: vec![Value::String("a".into()), Value::String("b".into())],
                },
            ),
            (
                "compound".into(),
                Value::Compound(Compound::from([("inner".into(), Value::Int(3))])),
            ),
            ("ints".into(), Value::IntArray(vec![1, -1, 0])),
        ])
    }

    /// Every tag type has to survive a round trip, because a schematic that reads back
    /// a different size or a different block id is a build that loads wrong.
    #[test]
    fn every_tag_type_survives_a_round_trip() {
        let original = everything();
        assert_eq!(read(&write(&original)), Ok(original));
    }

    /// Multi-byte integers are big-endian whatever produced them, so a schematic from a
    /// big-endian machine reads the same. Checked against the exact bytes rather than
    /// only through a round trip, which would pass even if both sides were wrong.
    #[test]
    fn integers_are_written_big_endian() {
        let mut out = Vec::new();
        write_payload(&Value::Int(0x0102_0304), &mut out);
        assert_eq!(out, vec![0x01, 0x02, 0x03, 0x04]);
        let mut out = Vec::new();
        write_payload(&Value::Short(0x0102), &mut out);
        assert_eq!(out, vec![0x01, 0x02]);
    }

    /// A schematic's `Offset` is an int array, and a negative coordinate has to come
    /// back negative rather than as a large positive.
    #[test]
    fn a_negative_int_array_survives() {
        let value = Value::IntArray(vec![-100, 64, -200]);
        let mut out = Vec::new();
        write_payload(&value, &mut out);
        let mut input = out.as_slice();
        assert_eq!(read_payload(tag::INT_ARRAY, &mut input), Ok(value));
    }

    /// An empty list still has to name its element type, because that is the only
    /// thing that says what a zero-length list holds.
    #[test]
    fn an_empty_list_keeps_its_element_type() {
        let value = Value::List {
            element: tag::COMPOUND,
            items: Vec::new(),
        };
        let mut out = Vec::new();
        write_payload(&value, &mut out);
        assert_eq!(out[0], tag::COMPOUND);
        let mut input = out.as_slice();
        assert_eq!(read_payload(tag::LIST, &mut input), Ok(value));
    }

    /// A list's element type is declared once for all of them, so a nested list of the
    /// wrong shape must be refused rather than read as if it were right.
    #[test]
    fn a_list_whose_elements_do_not_match_is_refused() {
        // Declares int elements, then supplies a string.
        let bytes = [tag::LIST, tag::INT, 0, 0, 0, 1, tag::STRING, 0, 1, b'x'];
        let mut input = bytes.as_slice();
        assert!(read_payload(tag::LIST, &mut input).is_err());
    }

    /// A compound whose stream ends early keeps what it read rather than reporting an
    /// empty one.
    ///
    /// Reporting it as *empty* would turn a damaged file into a blank build, which is
    /// the worst way for this to fail; refusing it outright would reject files FAWE's
    /// own writer produces. What it must not do is invent anything: only the tags that
    /// were fully present come back.
    #[test]
    fn a_compound_that_ends_early_keeps_what_it_read() {
        let bytes = [tag::INT, 0, 1, b'a', 0, 0, 0, 7];
        let mut input = bytes.as_slice();
        match read_payload(tag::COMPOUND, &mut input) {
            Ok(Value::Compound(compound)) => {
                assert_eq!(compound.len(), 1, "the one whole tag should survive");
                assert_eq!(compound.get("a"), Some(&Value::Int(7)));
            }
            other => panic!("expected the partial compound, got {other:?}"),
        }
    }

    /// A tag that is genuinely unreadable is still refused, even inside a compound that
    /// would otherwise be forgiven. This is the strictness that survives the tolerance
    /// above, and it is what catches a wrong *format* rather than a short file.
    #[test]
    fn an_unreadable_tag_inside_a_compound_is_still_refused() {
        let bytes = [99, 0, 1, b'a', 0, 1, b'z'];
        let mut input = bytes.as_slice();
        assert_eq!(
            read_payload(tag::COMPOUND, &mut input),
            Err(NbtError::UnknownTag(99))
        );
    }

    /// A tag this reader does not implement is reported, not skipped: a skipped tag
    /// is a silently dropped field.
    #[test]
    fn an_unknown_tag_type_is_reported() {
        let bytes = [99, 0, 1, b'a'];
        let mut input = bytes.as_slice();
        assert_eq!(read_payload(99, &mut input), Err(NbtError::UnknownTag(99)));
    }

    /// A string that runs off the end of the buffer is truncation, not a panic.
    #[test]
    fn a_truncated_string_is_refused() {
        let bytes = [tag::STRING, 0, 40, b's', b't'];
        let mut input = bytes.as_slice();
        assert_eq!(
            read_payload(tag::STRING, &mut input),
            Err(NbtError::Truncated)
        );
    }

    /// A string whose bytes are not UTF-8 is refused rather than replaced, because a
    /// mangled block id would resolve to a different block.
    ///
    /// The buffer starts *after* the tag byte, which is how `read_payload` is called:
    /// the caller has already consumed the tag id, and only the payload is left.
    #[test]
    fn an_invalid_utf8_string_is_refused() {
        let bytes = [0, 2, 0xFF, 0xFE];
        let mut input = bytes.as_slice();
        assert_eq!(
            read_payload(tag::STRING, &mut input),
            Err(NbtError::BadString)
        );
    }

    /// A list's length is 32 bits, unlike a string's. Reading it as two bytes would
    /// make every list longer than 32 767 elements come back empty, and a schematic
    /// with a large palette would lose blocks without any error.
    #[test]
    fn a_list_length_is_four_bytes_not_two() {
        let mut payload = vec![tag::INT, 0, 0, 0, 3];
        for index in 0..3 {
            payload.push(tag::INT);
            payload.extend_from_slice(&[0, 0, 0, index]);
        }
        let mut input = payload.as_slice();
        match read_payload(tag::LIST, &mut input) {
            Ok(Value::List { items, .. }) => assert_eq!(items.len(), 3),
            other => panic!("expected three elements, got {other:?}"),
        }
    }

    /// The other tests here build whole tags including the id, which is the easy way
    /// to get a buffer this function cannot read: it would take the tag byte as the
    /// first byte of the payload's length. Asserted rather than assumed, because every
    /// other test in this module depends on getting it right.
    #[test]
    fn read_payload_starts_after_the_tag_byte() {
        let bytes = [tag::INT, 0, 0, 0, 7];
        // Skipping the tag byte, as every caller does, gives the value. Not skipping it
        // reads the id as part of the payload, which is the mistake this asserts against.
        let mut payload = &bytes[1..];
        assert_eq!(read_payload(tag::INT, &mut payload), Ok(Value::Int(7)));

        let mut whole = bytes.as_slice();
        assert_ne!(
            read_payload(tag::INT, &mut whole),
            Ok(Value::Int(7)),
            "passing the tag byte through would misread it as the length"
        );
    }

    /// An int array's length is 32 bits too, for the same reason.
    #[test]
    fn an_int_array_length_is_four_bytes_not_two() {
        let value = Value::IntArray(vec![7, 8, 9]);
        let mut out = Vec::new();
        write_payload(&value, &mut out);
        assert_eq!(out[0..4], [0, 0, 0, 3], "length is a 32-bit count");
        let mut input = out.as_slice();
        assert_eq!(read_payload(tag::INT_ARRAY, &mut input), Ok(value));
    }
}
