//! Thrift compact protocol, as described by `thrift-compact-protocol.md` of Thrift 0.25.0
//! (contract §9). Only what Parquet metadata needs: structs, lists, sets and the base
//! types; maps and `uuid` are only skipped.
//!
//! A field header is `ddddtttt` (id delta 1–15 and type) or, in the long form, `0000tttt`
//! followed by the zigzag varint `i16` id. Booleans that are fields live in the header type
//! (1 true, 2 false); as list elements they are one byte (1 true, 2 false; a reader accepts
//! 0 as false too). Lists and sets are `sssstttt` with size 0–14 or `1111tttt` plus a
//! varint size. `i16`/`i32`/`i64` are zigzag varints, `double` is 8 bytes little-endian and
//! `binary` is a varint length followed by the bytes.

use crate::invalid;
use veta_core::Result;

/// Type codes shared by field headers and list elements. A boolean field reads as
/// [`BOOL`] whatever its value.
pub mod ttype {
    pub const STOP: u8 = 0;
    pub const BOOL: u8 = 1;
    pub const BOOL_FALSE: u8 = 2;
    pub const I8: u8 = 3;
    pub const I16: u8 = 4;
    pub const I32: u8 = 5;
    pub const I64: u8 = 6;
    pub const DOUBLE: u8 = 7;
    pub const BINARY: u8 = 8;
    pub const LIST: u8 = 9;
    pub const SET: u8 = 10;
    pub const MAP: u8 = 11;
    pub const STRUCT: u8 = 12;
    pub const UUID: u8 = 13;
}

/// Deepest nesting of structs and containers a reader accepts, so a hostile footer cannot
/// overflow the stack.
pub const MAX_DEPTH: usize = 64;

fn zigzag_encode(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn zigzag_decode(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Decoder over a byte slice.
pub struct CompactReader<'a> {
    buf: &'a [u8],
    pos: usize,
    /// Last field id of each enclosing struct.
    stack: Vec<i16>,
    last_id: i16,
    /// Value of a boolean field whose header was just read.
    pending_bool: Option<bool>,
}

impl<'a> CompactReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        CompactReader {
            buf,
            pos: 0,
            stack: Vec::new(),
            last_id: 0,
            pending_bool: None,
        }
    }

    /// Bytes consumed so far.
    pub fn position(&self) -> usize {
        self.pos
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(invalid("truncated thrift data"));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// Unsigned LEB128 varint of at most 64 bits.
    pub fn read_varint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            if shift == 63 && b > 1 {
                return Err(invalid("thrift varint overflows 64 bits"));
            }
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
            if shift > 63 {
                return Err(invalid("thrift varint overflows 64 bits"));
            }
        }
    }

    /// Enters a struct: field ids restart at 0.
    pub fn struct_begin(&mut self) -> Result<()> {
        if self.stack.len() >= MAX_DEPTH {
            return Err(invalid("thrift nesting too deep"));
        }
        self.stack.push(self.last_id);
        self.last_id = 0;
        Ok(())
    }

    /// Leaves a struct whose stop field [`Self::field_header`] already consumed.
    pub fn struct_end(&mut self) {
        self.last_id = self.stack.pop().unwrap_or(0);
    }

    /// Next field `(id, type)`, or `None` at the stop field. Boolean fields return
    /// [`ttype::BOOL`]; their value is then taken by [`Self::read_bool`] or [`Self::skip`].
    pub fn field_header(&mut self) -> Result<Option<(i16, u8)>> {
        let b = self.byte()?;
        if b == ttype::STOP {
            return Ok(None);
        }
        let mut t = b & 0x0f;
        let delta = b >> 4;
        let id = if delta != 0 {
            self.last_id
                .checked_add(i16::from(delta))
                .ok_or_else(|| invalid("thrift field id overflows i16"))?
        } else {
            self.read_i16()?
        };
        self.last_id = id;
        self.pending_bool = None;
        match t {
            ttype::BOOL | ttype::BOOL_FALSE => {
                self.pending_bool = Some(t == ttype::BOOL);
                t = ttype::BOOL;
            }
            ttype::I8..=ttype::UUID => {}
            _ => return Err(invalid(format!("unknown thrift field type {t}"))),
        }
        Ok(Some((id, t)))
    }

    /// A boolean field (from its header) or a boolean list element (one byte).
    pub fn read_bool(&mut self) -> Result<bool> {
        if let Some(v) = self.pending_bool.take() {
            return Ok(v);
        }
        match self.byte()? {
            1 => Ok(true),
            0 | 2 => Ok(false),
            b => Err(invalid(format!("invalid thrift boolean {b}"))),
        }
    }

    pub fn read_i8(&mut self) -> Result<i8> {
        Ok(self.byte()? as i8)
    }

    pub fn read_i16(&mut self) -> Result<i16> {
        i16::try_from(zigzag_decode(self.read_varint()?))
            .map_err(|_| invalid("thrift i16 out of range"))
    }

    pub fn read_i32(&mut self) -> Result<i32> {
        i32::try_from(zigzag_decode(self.read_varint()?))
            .map_err(|_| invalid("thrift i32 out of range"))
    }

    pub fn read_i64(&mut self) -> Result<i64> {
        Ok(zigzag_decode(self.read_varint()?))
    }

    pub fn read_double(&mut self) -> Result<f64> {
        let b = self.take(8)?;
        Ok(f64::from_le_bytes(b.try_into().expect("8 bytes")))
    }

    pub fn read_binary(&mut self) -> Result<&'a [u8]> {
        let len = self.read_varint()?;
        if len > u64::from(u32::MAX) {
            return Err(invalid("thrift binary length out of range"));
        }
        self.take(len as usize)
    }

    pub fn read_string(&mut self) -> Result<String> {
        let b = self.read_binary()?;
        String::from_utf8(b.to_vec()).map_err(|_| invalid("thrift string is not UTF-8"))
    }

    /// List or set header: `(element type, size)`. Boolean elements report [`ttype::BOOL`].
    pub fn list_header(&mut self) -> Result<(u8, usize)> {
        let b = self.byte()?;
        let mut t = b & 0x0f;
        let short = b >> 4;
        let size = if short == 15 {
            let s = self.read_varint()?;
            if s > i32::MAX as u64 {
                return Err(invalid("thrift list size out of range"));
            }
            s as usize
        } else {
            usize::from(short)
        };
        if t == ttype::BOOL_FALSE {
            t = ttype::BOOL;
        }
        if !(ttype::BOOL..=ttype::UUID).contains(&t) {
            return Err(invalid(format!("unknown thrift element type {t}")));
        }
        // Every element takes at least one byte; a larger size is a corrupt header.
        if size > self.remaining() {
            return Err(invalid("thrift list longer than its data"));
        }
        Ok((t, size))
    }

    /// Skips one value of type `t` (a field after its header, or a container element).
    pub fn skip(&mut self, t: u8) -> Result<()> {
        self.skip_depth(t, self.stack.len())
    }

    fn skip_depth(&mut self, t: u8, depth: usize) -> Result<()> {
        if depth >= MAX_DEPTH {
            return Err(invalid("thrift nesting too deep"));
        }
        match t {
            ttype::BOOL | ttype::BOOL_FALSE => {
                self.read_bool()?;
            }
            ttype::I8 => {
                self.take(1)?;
            }
            ttype::I16 | ttype::I32 | ttype::I64 => {
                self.read_varint()?;
            }
            ttype::DOUBLE => {
                self.take(8)?;
            }
            ttype::BINARY => {
                self.read_binary()?;
            }
            ttype::UUID => {
                self.take(16)?;
            }
            ttype::LIST | ttype::SET => {
                let (et, n) = self.list_header()?;
                for _ in 0..n {
                    self.skip_depth(et, depth + 1)?;
                }
            }
            ttype::MAP => {
                let n = self.read_varint()?;
                if n > 0 {
                    if n > self.remaining() as u64 {
                        return Err(invalid("thrift map longer than its data"));
                    }
                    let kv = self.byte()?;
                    let (kt, vt) = (kv >> 4, kv & 0x0f);
                    for _ in 0..n {
                        self.skip_depth(kt, depth + 1)?;
                        self.skip_depth(vt, depth + 1)?;
                    }
                }
            }
            ttype::STRUCT => {
                self.struct_begin()?;
                while let Some((_, ft)) = self.field_header()? {
                    self.skip_depth(ft, depth + 1)?;
                }
                self.struct_end();
            }
            _ => return Err(invalid(format!("cannot skip thrift type {t}"))),
        }
        Ok(())
    }
}

/// Encoder into a growing buffer. Field headers and list headers always use the short
/// form when it applies.
#[derive(Default)]
pub struct CompactWriter {
    buf: Vec<u8>,
    stack: Vec<i16>,
    last_id: i16,
}

impl CompactWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn write_varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }

    pub fn struct_begin(&mut self) {
        self.stack.push(self.last_id);
        self.last_id = 0;
    }

    /// Writes the stop field and leaves the struct.
    pub fn struct_end(&mut self) {
        self.buf.push(ttype::STOP);
        self.last_id = self.stack.pop().unwrap_or(0);
    }

    /// Field header of type `t` (for booleans use [`Self::field_bool`]).
    pub fn field_header(&mut self, id: i16, t: u8) {
        let delta = i32::from(id) - i32::from(self.last_id);
        if (1..=15).contains(&delta) {
            self.buf.push(((delta as u8) << 4) | t);
        } else {
            self.buf.push(t);
            self.write_i16(id);
        }
        self.last_id = id;
    }

    pub fn field_bool(&mut self, id: i16, v: bool) {
        let t = if v { ttype::BOOL } else { ttype::BOOL_FALSE };
        self.field_header(id, t);
    }

    pub fn field_i8(&mut self, id: i16, v: i8) {
        self.field_header(id, ttype::I8);
        self.write_i8(v);
    }

    pub fn field_i16(&mut self, id: i16, v: i16) {
        self.field_header(id, ttype::I16);
        self.write_i16(v);
    }

    pub fn field_i32(&mut self, id: i16, v: i32) {
        self.field_header(id, ttype::I32);
        self.write_i32(v);
    }

    pub fn field_i64(&mut self, id: i16, v: i64) {
        self.field_header(id, ttype::I64);
        self.write_i64(v);
    }

    pub fn field_double(&mut self, id: i16, v: f64) {
        self.field_header(id, ttype::DOUBLE);
        self.write_double(v);
    }

    pub fn field_binary(&mut self, id: i16, v: &[u8]) {
        self.field_header(id, ttype::BINARY);
        self.write_binary(v);
    }

    /// Header of a struct field; follow with [`Self::struct_begin`], the fields and
    /// [`Self::struct_end`].
    pub fn field_struct(&mut self, id: i16) {
        self.field_header(id, ttype::STRUCT);
    }

    /// Header of a list field; follow with the `len` elements.
    pub fn field_list(&mut self, id: i16, elem: u8, len: usize) {
        self.field_header(id, ttype::LIST);
        self.list_header(elem, len);
    }

    /// List or set header. Booleans are announced as [`ttype::BOOL`].
    pub fn list_header(&mut self, elem: u8, len: usize) {
        let elem = if elem == ttype::BOOL_FALSE {
            ttype::BOOL
        } else {
            elem
        };
        if len < 15 {
            self.buf.push(((len as u8) << 4) | elem);
        } else {
            self.buf.push(0xf0 | elem);
            self.write_varint(len as u64);
        }
    }

    /// Boolean list element: 1 true, 2 false.
    pub fn write_bool(&mut self, v: bool) {
        self.buf.push(if v { 1 } else { 2 });
    }

    pub fn write_i8(&mut self, v: i8) {
        self.buf.push(v as u8);
    }

    pub fn write_i16(&mut self, v: i16) {
        self.write_varint(zigzag_encode(i64::from(v)));
    }

    pub fn write_i32(&mut self, v: i32) {
        self.write_varint(zigzag_encode(i64::from(v)));
    }

    pub fn write_i64(&mut self, v: i64) {
        self.write_varint(zigzag_encode(v));
    }

    pub fn write_double(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn write_binary(&mut self, v: &[u8]) {
        self.write_varint(v.len() as u64);
        self.buf.extend_from_slice(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zigzag_matches_the_spec() {
        for (v, z) in [
            (0i64, 0u64),
            (-1, 1),
            (1, 2),
            (-2, 3),
            (i64::MAX, u64::MAX - 1),
        ] {
            assert_eq!(zigzag_encode(v), z);
            assert_eq!(zigzag_decode(z), v);
        }
        assert_eq!(zigzag_decode(u64::MAX), i64::MIN);
    }

    #[test]
    fn varint_example_from_the_spec() {
        let mut w = CompactWriter::new();
        w.write_varint(50399);
        assert_eq!(w.bytes(), &[0xdf, 0x89, 0x03]);
        let mut r = CompactReader::new(w.bytes());
        assert_eq!(r.read_varint().unwrap(), 50399);
    }

    #[test]
    fn varint_overflow_and_truncation_fail() {
        assert!(CompactReader::new(&[0xff; 11]).read_varint().is_err());
        assert!(CompactReader::new(&[0x80, 0x80]).read_varint().is_err());
        let mut max = vec![0xff; 9];
        max.push(0x01);
        assert_eq!(CompactReader::new(&max).read_varint().unwrap(), u64::MAX);
    }

    /// struct { 1: i32 = -3, 2: string = "ab", 3: list<i32> = [1, 2], 4: bool = true,
    /// 100: i64 = 5 } written by hand: the last header is in the long form.
    fn hand_struct() -> Vec<u8> {
        vec![
            0x15, 0x05, // delta 1, I32; zigzag(-3) = 5
            0x18, 0x02, b'a', b'b', // delta 1, BINARY; length 2
            0x19, 0x25, 0x02, 0x04, // delta 1, LIST; 2 elements of I32; 1, 2
            0x11, // delta 1, BOOLEAN_TRUE
            0x06, 0xc8, 0x01, 0x0a, // long form I64, id zigzag(100) = 200; zigzag(5) = 10
            0x00, // stop
        ]
    }

    #[test]
    fn reads_a_hand_built_struct() {
        let bytes = hand_struct();
        let mut r = CompactReader::new(&bytes);
        r.struct_begin().unwrap();
        assert_eq!(r.field_header().unwrap(), Some((1, ttype::I32)));
        assert_eq!(r.read_i32().unwrap(), -3);
        assert_eq!(r.field_header().unwrap(), Some((2, ttype::BINARY)));
        assert_eq!(r.read_string().unwrap(), "ab");
        assert_eq!(r.field_header().unwrap(), Some((3, ttype::LIST)));
        assert_eq!(r.list_header().unwrap(), (ttype::I32, 2));
        assert_eq!(r.read_i32().unwrap(), 1);
        assert_eq!(r.read_i32().unwrap(), 2);
        assert_eq!(r.field_header().unwrap(), Some((4, ttype::BOOL)));
        assert!(r.read_bool().unwrap());
        assert_eq!(r.field_header().unwrap(), Some((100, ttype::I64)));
        assert_eq!(r.read_i64().unwrap(), 5);
        assert_eq!(r.field_header().unwrap(), None);
        r.struct_end();
        assert_eq!(r.position(), bytes.len());
    }

    #[test]
    fn writer_produces_the_hand_built_bytes() {
        let mut w = CompactWriter::new();
        w.struct_begin();
        w.field_i32(1, -3);
        w.field_binary(2, b"ab");
        w.field_list(3, ttype::I32, 2);
        w.write_i32(1);
        w.write_i32(2);
        w.field_bool(4, true);
        w.field_i64(100, 5);
        w.struct_end();
        assert_eq!(w.into_bytes(), hand_struct());
    }

    #[test]
    fn long_form_for_backward_and_zero_deltas() {
        let mut w = CompactWriter::new();
        w.struct_begin();
        w.field_i32(5, 1);
        w.field_i32(2, 2); // negative delta → long form
        w.field_bool(-1, false);
        w.struct_end();
        let bytes = w.into_bytes();
        assert_eq!(bytes, vec![0x55, 0x02, 0x05, 0x04, 0x04, 0x02, 0x01, 0x00]);
        let mut r = CompactReader::new(&bytes);
        r.struct_begin().unwrap();
        assert_eq!(r.field_header().unwrap(), Some((5, ttype::I32)));
        assert_eq!(r.read_i32().unwrap(), 1);
        assert_eq!(r.field_header().unwrap(), Some((2, ttype::I32)));
        assert_eq!(r.read_i32().unwrap(), 2);
        assert_eq!(r.field_header().unwrap(), Some((-1, ttype::BOOL)));
        assert!(!r.read_bool().unwrap());
        assert_eq!(r.field_header().unwrap(), None);
    }

    #[test]
    fn base_types_round_trip() {
        let mut w = CompactWriter::new();
        w.write_i8(-7);
        w.write_i16(i16::MIN);
        w.write_i32(i32::MAX);
        w.write_i64(i64::MIN);
        w.write_double(-1.5);
        w.write_binary(&[0, 255, 3]);
        w.write_bool(true);
        w.write_bool(false);
        let bytes = w.into_bytes();
        let mut r = CompactReader::new(&bytes);
        assert_eq!(r.read_i8().unwrap(), -7);
        assert_eq!(r.read_i16().unwrap(), i16::MIN);
        assert_eq!(r.read_i32().unwrap(), i32::MAX);
        assert_eq!(r.read_i64().unwrap(), i64::MIN);
        assert_eq!(r.read_double().unwrap(), -1.5);
        assert_eq!(r.read_binary().unwrap(), &[0, 255, 3]);
        assert!(r.read_bool().unwrap());
        assert!(!r.read_bool().unwrap());
        assert_eq!(r.position(), bytes.len());
    }

    #[test]
    fn out_of_range_integers_fail() {
        let mut w = CompactWriter::new();
        w.write_i64(i64::from(i32::MAX) + 1);
        w.write_i64(i64::from(i16::MIN) - 1);
        let bytes = w.into_bytes();
        let mut r = CompactReader::new(&bytes);
        assert!(r.read_i32().is_err());
        assert!(r.read_i16().is_err());
    }

    #[test]
    fn lists_short_and_long_form() {
        for n in [0usize, 1, 14, 15, 300] {
            let mut w = CompactWriter::new();
            w.list_header(ttype::I64, n);
            for i in 0..n {
                w.write_i64(i as i64 - 7);
            }
            let bytes = w.into_bytes();
            if n < 15 {
                assert_eq!(bytes[0], ((n as u8) << 4) | ttype::I64);
            } else {
                assert_eq!(bytes[0], 0xf0 | ttype::I64);
            }
            let mut r = CompactReader::new(&bytes);
            assert_eq!(r.list_header().unwrap(), (ttype::I64, n));
            for i in 0..n {
                assert_eq!(r.read_i64().unwrap(), i as i64 - 7);
            }
            assert_eq!(r.position(), bytes.len());
        }
    }

    #[test]
    fn bool_lists_accept_both_element_types() {
        // Element type 2 (the original spec) and 1 (the de facto one); values 1, 2 and 0.
        for header in [0x31u8, 0x32] {
            let bytes = [header, 1, 2, 0];
            let mut r = CompactReader::new(&bytes);
            assert_eq!(r.list_header().unwrap(), (ttype::BOOL, 3));
            assert!(r.read_bool().unwrap());
            assert!(!r.read_bool().unwrap());
            assert!(!r.read_bool().unwrap());
        }
    }

    #[test]
    fn list_larger_than_data_fails() {
        let bytes = [0xf5, 0xff, 0xff, 0xff, 0x07];
        assert!(CompactReader::new(&bytes).list_header().is_err());
    }

    #[test]
    fn skips_every_type() {
        let mut w = CompactWriter::new();
        w.struct_begin();
        w.field_bool(1, true);
        w.field_i8(2, 1);
        w.field_i16(3, -300);
        w.field_i32(4, 1 << 20);
        w.field_i64(5, -1);
        w.field_double(6, 2.0);
        w.field_binary(7, b"xyz");
        w.field_list(8, ttype::BOOL, 2);
        w.write_bool(true);
        w.write_bool(false);
        // set<binary> of 1
        w.field_header(9, ttype::SET);
        w.list_header(ttype::BINARY, 1);
        w.write_binary(b"s");
        // map<i32, struct> of 1
        w.field_header(10, ttype::MAP);
        w.write_varint(1);
        let mut tail = vec![(ttype::I32 << 4) | ttype::STRUCT];
        tail.extend_from_slice(&[0x02, 0x15, 0x04, 0x00]); // key 1; struct { 1: i32 2 }
        let mut bytes = w.into_bytes();
        bytes.extend_from_slice(&tail);
        // empty map, uuid, then the field that must survive
        bytes.extend_from_slice(&[(1 << 4) | ttype::MAP, 0x00]);
        bytes.push((1 << 4) | ttype::UUID);
        bytes.extend_from_slice(&[0xaa; 16]);
        bytes.extend_from_slice(&[0x15, 0x54, 0x00]); // 13: i32 42; stop
        let mut r = CompactReader::new(&bytes);
        r.struct_begin().unwrap();
        let mut seen = None;
        while let Some((id, t)) = r.field_header().unwrap() {
            if id == 13 {
                seen = Some(r.read_i32().unwrap());
            } else {
                r.skip(t).unwrap();
            }
        }
        r.struct_end();
        assert_eq!(seen, Some(42));
        assert_eq!(r.position(), bytes.len());
    }

    #[test]
    fn truncated_and_too_deep_input_fails() {
        let bytes = [0x18, 0x05, b'a'];
        let mut r = CompactReader::new(&bytes);
        r.struct_begin().unwrap();
        let (_, t) = r.field_header().unwrap().unwrap();
        assert!(r.skip(t).is_err());

        let mut deep = vec![0x1c; MAX_DEPTH + 1];
        deep.extend(vec![0; MAX_DEPTH + 2]);
        let mut r = CompactReader::new(&deep);
        assert!(r.skip(ttype::STRUCT).is_err());
    }

    #[test]
    fn unknown_field_type_fails() {
        let mut r = CompactReader::new(&[0x1e]);
        assert!(r.field_header().is_err());
    }
}
