//! Minimal protobuf wire-format encoder. Just enough to emit Perfetto
//! traces; no schema validation, no reflection, no codegen. Reference:
//! <https://protobuf.dev/programming-guides/encoding/>.

use std::io::{self, Write};

/// Wire types we use. Skipping 32-bit fixed because Perfetto's trace format
/// doesn't need it for the messages we emit.
#[derive(Copy, Clone)]
#[repr(u32)]
pub enum WireType {
    Varint = 0,
    Fixed64 = 1,
    LengthDelimited = 2,
}

#[inline]
fn tag(field_number: u32, wire_type: WireType) -> u32 {
    (field_number << 3) | (wire_type as u32)
}

#[inline]
pub fn write_varint<W: Write>(w: &mut W, mut value: u64) -> io::Result<()> {
    while value >= 0x80 {
        w.write_all(&[(value as u8) | 0x80])?;
        value >>= 7;
    }
    w.write_all(&[value as u8])
}

#[inline]
pub fn write_tag<W: Write>(w: &mut W, field: u32, wt: WireType) -> io::Result<()> {
    write_varint(w, tag(field, wt) as u64)
}

#[inline]
pub fn write_uint64<W: Write>(w: &mut W, field: u32, value: u64) -> io::Result<()> {
    write_tag(w, field, WireType::Varint)?;
    write_varint(w, value)
}

#[inline]
pub fn write_uint32<W: Write>(w: &mut W, field: u32, value: u32) -> io::Result<()> {
    write_uint64(w, field, value as u64)
}

#[inline]
pub fn write_int64<W: Write>(w: &mut W, field: u32, value: i64) -> io::Result<()> {
    // Non-negative `int64` values encode as varint of the unsigned bit
    // pattern; we don't emit negatives, so this is fine.
    write_uint64(w, field, value as u64)
}

#[inline]
pub fn write_fixed64<W: Write>(w: &mut W, field: u32, value: u64) -> io::Result<()> {
    write_tag(w, field, WireType::Fixed64)?;
    w.write_all(&value.to_le_bytes())
}

#[inline]
pub fn write_bytes<W: Write>(w: &mut W, field: u32, value: &[u8]) -> io::Result<()> {
    write_tag(w, field, WireType::LengthDelimited)?;
    write_varint(w, value.len() as u64)?;
    w.write_all(value)
}

#[inline]
pub fn write_string<W: Write>(w: &mut W, field: u32, value: &str) -> io::Result<()> {
    write_bytes(w, field, value.as_bytes())
}

/// Write a packed `repeated uint64` field. Useful for arrays-of-iids.
pub fn write_packed_uint64<W: Write>(
    w: &mut W,
    field: u32,
    values: &[u64],
) -> io::Result<()> {
    let mut payload: Vec<u8> = Vec::with_capacity(values.len() * 2);
    for &v in values {
        write_varint(&mut payload, v)?;
    }
    write_bytes(w, field, &payload)
}

/// Encode a sub-message. The closure builds the message body into a Vec
/// (so we can prefix with its length); then we write a length-delimited
/// field. Perfetto traces are deeply nested so this gets called a lot;
/// callers reuse a scratch buffer to avoid allocating per call when it
/// matters.
pub fn write_message<W: Write, F>(w: &mut W, field: u32, build: F) -> io::Result<()>
where
    F: FnOnce(&mut Vec<u8>) -> io::Result<()>,
{
    let mut buf = Vec::new();
    build(&mut buf)?;
    write_bytes(w, field, &buf)
}
