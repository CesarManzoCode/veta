//! Validity bitmap, LSB-first: bit `i` is `bytes[i / 8] >> (i % 8) & 1`, 1 = valid (§5.3).
//!
//! Invariant: the unused high bits of the last byte are always zero, so equality and
//! counting work on whole bytes.

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Bitmap {
    bytes: Vec<u8>,
    len: usize,
}

impl Bitmap {
    /// `len` bits, all set (every row valid).
    pub fn new_set(len: usize) -> Self {
        let mut bitmap = Bitmap {
            bytes: vec![0xFF; len.div_ceil(8)],
            len,
        };
        bitmap.clear_tail();
        bitmap
    }

    /// `len` bits, all clear (every row NULL).
    pub fn new_unset(len: usize) -> Self {
        Bitmap {
            bytes: vec![0; len.div_ceil(8)],
            len,
        }
    }

    pub fn from_bools(bits: &[bool]) -> Self {
        let mut bitmap = Bitmap::new_unset(bits.len());
        for (i, &bit) in bits.iter().enumerate() {
            if bit {
                bitmap.bytes[i / 8] |= 1 << (i % 8);
            }
        }
        bitmap
    }

    /// Takes `ceil(len / 8)` bytes as stored on disk; bits past `len` are ignored.
    /// Returns `None` if `bytes` is shorter than that.
    pub fn from_bytes(bytes: &[u8], len: usize) -> Option<Self> {
        let needed = len.div_ceil(8);
        if bytes.len() < needed {
            return None;
        }
        let mut bitmap = Bitmap {
            bytes: bytes[..needed].to_vec(),
            len,
        };
        bitmap.clear_tail();
        Some(bitmap)
    }

    fn clear_tail(&mut self) {
        let rem = self.len % 8;
        if rem != 0
            && let Some(last) = self.bytes.last_mut()
        {
            *last &= (1u8 << rem) - 1;
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn get(&self, i: usize) -> bool {
        assert!(i < self.len, "bit {i} out of range {}", self.len);
        (self.bytes[i / 8] >> (i % 8)) & 1 == 1
    }

    pub fn set(&mut self, i: usize, value: bool) {
        assert!(i < self.len, "bit {i} out of range {}", self.len);
        if value {
            self.bytes[i / 8] |= 1 << (i % 8);
        } else {
            self.bytes[i / 8] &= !(1 << (i % 8));
        }
    }

    pub fn push(&mut self, value: bool) {
        if self.len % 8 == 0 {
            self.bytes.push(0);
        }
        self.len += 1;
        self.set(self.len - 1, value);
    }

    pub fn count_set(&self) -> usize {
        self.bytes.iter().map(|b| b.count_ones() as usize).sum()
    }

    pub fn count_unset(&self) -> usize {
        self.len - self.count_set()
    }

    pub fn all_set(&self) -> bool {
        self.count_set() == self.len
    }

    pub fn slice(&self, offset: usize, len: usize) -> Bitmap {
        assert!(offset + len <= self.len, "slice out of range");
        let mut out = Bitmap::new_unset(len);
        for i in 0..len {
            if self.get(offset + i) {
                out.bytes[i / 8] |= 1 << (i % 8);
            }
        }
        out
    }

    pub fn gather(&self, indices: &[u32]) -> Bitmap {
        let mut out = Bitmap::new_unset(indices.len());
        for (i, &idx) in indices.iter().enumerate() {
            if self.get(idx as usize) {
                out.bytes[i / 8] |= 1 << (i % 8);
            }
        }
        out
    }

    pub fn extend(&mut self, other: &Bitmap) {
        for i in 0..other.len {
            self.push(other.get(i));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsb_first_layout() {
        let bitmap =
            Bitmap::from_bools(&[true, false, true, true, false, false, false, false, true]);
        assert_eq!(bitmap.as_bytes(), &[0b0000_1101, 0b0000_0001]);
        assert_eq!(bitmap.len(), 9);
        assert!(bitmap.get(0) && !bitmap.get(1) && bitmap.get(8));
        assert_eq!(bitmap.count_set(), 4);
        assert_eq!(bitmap.count_unset(), 5);
    }

    #[test]
    fn new_set_keeps_tail_clear() {
        let bitmap = Bitmap::new_set(10);
        assert_eq!(bitmap.as_bytes(), &[0xFF, 0b11]);
        assert!(bitmap.all_set());
        assert_eq!(Bitmap::new_unset(10).count_set(), 0);
        assert_eq!(Bitmap::new_set(0).as_bytes(), &[] as &[u8]);
    }

    #[test]
    fn from_bytes_ignores_bits_past_len() {
        let bitmap = Bitmap::from_bytes(&[0xFF, 0xFF], 12).unwrap();
        assert_eq!(bitmap.as_bytes(), &[0xFF, 0x0F]);
        assert_eq!(bitmap, Bitmap::new_set(12));
        assert!(Bitmap::from_bytes(&[0xFF], 12).is_none());
    }

    #[test]
    fn set_push_slice_gather() {
        let mut bitmap = Bitmap::new_unset(3);
        bitmap.set(1, true);
        bitmap.push(true);
        bitmap.push(false);
        assert_eq!(
            bitmap,
            Bitmap::from_bools(&[false, true, false, true, false])
        );
        bitmap.set(1, false);
        assert!(!bitmap.get(1));
        let bits: Vec<bool> = (0..20).map(|i| i % 3 == 0).collect();
        let bitmap = Bitmap::from_bools(&bits);
        assert_eq!(bitmap.slice(5, 10), Bitmap::from_bools(&bits[5..15]));
        assert_eq!(
            bitmap.gather(&[0, 1, 18, 3]),
            Bitmap::from_bools(&[true, false, true, true])
        );
        let mut joined = bitmap.slice(0, 7);
        joined.extend(&bitmap.slice(7, 13));
        assert_eq!(joined, bitmap);
    }
}
