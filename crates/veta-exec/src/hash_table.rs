//! Open-addressing hash tables of the aggregation and of the hash join (contract §12): one
//! for fixed-width keys packed in `u64`/`u128` and one for byte strings kept in an arena.
//!
//! Both assign consecutive ids `0, 1, …` in insertion order and probe linearly with at
//! most half of the slots in use. The low bits of the hash pick the slot; the top bits
//! pick the radix partition, so the two are independent.
//!
//! [`ArrayTable`] is the aggregation by array: a single integer key whose range the part
//! statistics bound indexes its slot directly, without hashing, and assigns ids the same way.

use veta_core::hash::{hash_u64, hash_u128};

const RADIX_BITS: u32 = 6;
/// Radix partitions of the parallel merge.
pub(crate) const PARTITIONS: usize = 1 << RADIX_BITS;

#[inline]
pub(crate) fn partition_of(hash: u64) -> usize {
    (hash >> (64 - RADIX_BITS)) as usize
}

/// A packed fixed-width key.
pub(crate) trait FixedKey: Copy + Eq + Default + Send + Sync + 'static {
    fn hash64(self) -> u64;
    fn bits(self) -> u128;
    /// `self | value << shift`.
    fn or_shifted(self, value: u128, shift: u32) -> Self;
}

impl FixedKey for u64 {
    #[inline]
    fn hash64(self) -> u64 {
        hash_u64(self)
    }

    #[inline]
    fn bits(self) -> u128 {
        self as u128
    }

    #[inline]
    fn or_shifted(self, value: u128, shift: u32) -> u64 {
        self | ((value as u64) << shift)
    }
}

impl FixedKey for u128 {
    #[inline]
    fn hash64(self) -> u64 {
        hash_u128(self)
    }

    #[inline]
    fn bits(self) -> u128 {
        self
    }

    #[inline]
    fn or_shifted(self, value: u128, shift: u32) -> u128 {
        self | (value << shift)
    }
}

/// Hash table of packed keys. A slot's id is `id + 1`; 0 means empty.
#[derive(Debug)]
pub(crate) struct FixedTable<K> {
    keys: Vec<K>,
    ids: Vec<u32>,
    mask: usize,
    len: usize,
}

impl<K: FixedKey> FixedTable<K> {
    /// A table with `slots` slots rounded up to a power of two.
    pub fn with_slots(slots: usize) -> FixedTable<K> {
        let slots = slots.max(2).next_power_of_two();
        FixedTable {
            keys: vec![K::default(); slots],
            ids: vec![0; slots],
            mask: slots - 1,
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// The id of `key`, inserting it with the next id if it is new.
    #[inline]
    pub fn find_or_insert(&mut self, key: K) -> u32 {
        let hash = key.hash64();
        let mut slot = hash as usize & self.mask;
        loop {
            let id = self.ids[slot];
            if id == 0 {
                if (self.len + 1) * 2 > self.ids.len() {
                    self.grow();
                    slot = hash as usize & self.mask;
                    continue;
                }
                let new = self.len as u32;
                self.ids[slot] = new + 1;
                self.keys[slot] = key;
                self.len += 1;
                return new;
            }
            if self.keys[slot] == key {
                return id - 1;
            }
            slot = (slot + 1) & self.mask;
        }
    }

    /// The id of `key`, if the table has it.
    #[inline]
    pub fn find(&self, key: K) -> Option<u32> {
        let mut slot = key.hash64() as usize & self.mask;
        loop {
            let id = self.ids[slot];
            if id == 0 {
                return None;
            }
            if self.keys[slot] == key {
                return Some(id - 1);
            }
            slot = (slot + 1) & self.mask;
        }
    }

    fn grow(&mut self) {
        let slots = self.ids.len() * 2;
        let mask = slots - 1;
        let mut keys = vec![K::default(); slots];
        let mut ids = vec![0u32; slots];
        for (key, &id) in self.keys.iter().zip(&self.ids) {
            if id != 0 {
                let mut slot = key.hash64() as usize & mask;
                while ids[slot] != 0 {
                    slot = (slot + 1) & mask;
                }
                ids[slot] = id;
                keys[slot] = *key;
            }
        }
        self.keys = keys;
        self.ids = ids;
        self.mask = mask;
    }

    /// `(key, id)` of every key, in slot order.
    pub fn entries(&self) -> impl Iterator<Item = (K, u32)> + '_ {
        self.keys
            .iter()
            .zip(&self.ids)
            .filter(|(_, id)| **id != 0)
            .map(|(key, id)| (*key, *id - 1))
    }

    /// Bytes held by the table.
    pub fn bytes(&self) -> u64 {
        (self.keys.capacity() * size_of::<K>() + self.ids.capacity() * 4) as u64
    }
}

/// Largest key range [`ArrayTable`] takes: 4 bytes per slot, 1 MiB per worker.
pub(crate) const MAX_ARRAY_SLOTS: u64 = 1 << 18;

/// Direct-index table of a one-column packed key (the layout of `crate::group_keys`): the
/// value, `width` bytes sign-extended, picks slot `value − min`; a NULL (the flag bit at
/// `null_bit`) takes the slot after the range. Ids are assigned in insertion order, as in
/// [`FixedTable`], so converting to one keeps every id.
#[derive(Debug)]
pub(crate) struct ArrayTable {
    min: i64,
    /// Slots for the values `min..=min + span − 1`.
    span: u64,
    value_bits: u32,
    null_bit: Option<u32>,
    /// `id + 1` of each slot; 0 is empty.
    ids: Vec<u32>,
    /// Packed key of each id.
    keys: Vec<u64>,
}

impl ArrayTable {
    /// A table for values in `min..=max` of a column of `width` bytes (at most 8) whose
    /// NULL flag, if nullable, is bit `null_bit`; `None` if the range has more than
    /// [`MAX_ARRAY_SLOTS`] values.
    pub fn new(min: i64, max: i64, width: u32, null_bit: Option<u32>) -> Option<ArrayTable> {
        if max < min || width == 0 || width > 8 {
            return None;
        }
        let span = (max as i128 - min as i128 + 1) as u128;
        if span > MAX_ARRAY_SLOTS as u128 {
            return None;
        }
        let span = span as u64;
        Some(ArrayTable {
            min,
            span,
            value_bits: width * 8,
            null_bit,
            ids: vec![0; span as usize + 1],
            keys: Vec::new(),
        })
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// The slot of a packed key, `None` if its value is outside the range.
    #[inline]
    fn slot(&self, key: u64) -> Option<usize> {
        if let Some(bit) = self.null_bit
            && (key >> bit) & 1 == 1
        {
            return Some(self.span as usize);
        }
        let shift = 64 - self.value_bits;
        let value = ((key << shift) as i64) >> shift;
        let offset = (value as i128 - self.min as i128) as u128;
        (offset < self.span as u128).then_some(offset as usize)
    }

    /// The id of `key`, inserting it with the next id if it is new; `None` if its value is
    /// outside the range (the table is then left as it was).
    #[inline]
    pub fn find_or_insert(&mut self, key: u64) -> Option<u32> {
        let slot = self.slot(key)?;
        let id = self.ids[slot];
        if id != 0 {
            return Some(id - 1);
        }
        let new = self.keys.len() as u32;
        self.ids[slot] = new + 1;
        self.keys.push(key);
        Some(new)
    }

    /// `(key, id)` of every key, in id order.
    pub fn entries(&self) -> impl Iterator<Item = (u64, u32)> + '_ {
        self.keys
            .iter()
            .enumerate()
            .map(|(id, &key)| (key, id as u32))
    }

    /// The same keys with the same ids in a hash table.
    pub fn to_hash_table(&self) -> FixedTable<u64> {
        let mut table = FixedTable::with_slots(2 * self.keys.len().max(512));
        for &key in &self.keys {
            table.find_or_insert(key);
        }
        table
    }

    /// Bytes held by the table.
    pub fn bytes(&self) -> u64 {
        (self.ids.capacity() * 4 + self.keys.capacity() * 8) as u64
    }
}

/// Hash table of byte strings. Keys live in an arena; each slot keeps `id + 1` (0 is empty)
/// and the high half of the hash, so most mismatches do not touch the arena.
#[derive(Debug)]
pub(crate) struct BytesTable {
    slots: Vec<(u32, u32)>,
    mask: usize,
    hashes: Vec<u64>,
    /// End of each key in `arena`; a key starts where the previous one ends.
    ends: Vec<usize>,
    arena: Vec<u8>,
}

impl BytesTable {
    pub fn with_slots(slots: usize) -> BytesTable {
        let slots = slots.max(2).next_power_of_two();
        BytesTable {
            slots: vec![(0, 0); slots],
            mask: slots - 1,
            hashes: Vec::new(),
            ends: Vec::new(),
            arena: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    pub fn key(&self, id: usize) -> &[u8] {
        let start = if id == 0 { 0 } else { self.ends[id - 1] };
        &self.arena[start..self.ends[id]]
    }

    pub fn hash(&self, id: usize) -> u64 {
        self.hashes[id]
    }

    /// The id of `key`, whose hash is `hash`, inserting it with the next id if it is new.
    #[inline]
    pub fn find_or_insert(&mut self, key: &[u8], hash: u64) -> u32 {
        let tag = (hash >> 32) as u32;
        let mut slot = hash as usize & self.mask;
        loop {
            let (id, slot_tag) = self.slots[slot];
            if id == 0 {
                if (self.hashes.len() + 1) * 2 > self.slots.len() {
                    self.grow();
                    slot = hash as usize & self.mask;
                    continue;
                }
                let new = self.hashes.len() as u32;
                self.slots[slot] = (new + 1, tag);
                self.hashes.push(hash);
                self.arena.extend_from_slice(key);
                self.ends.push(self.arena.len());
                return new;
            }
            if slot_tag == tag && self.key(id as usize - 1) == key {
                return id - 1;
            }
            slot = (slot + 1) & self.mask;
        }
    }

    /// The id of `key`, whose hash is `hash`, if the table has it.
    #[inline]
    pub fn find(&self, key: &[u8], hash: u64) -> Option<u32> {
        let tag = (hash >> 32) as u32;
        let mut slot = hash as usize & self.mask;
        loop {
            let (id, slot_tag) = self.slots[slot];
            if id == 0 {
                return None;
            }
            if slot_tag == tag && self.key(id as usize - 1) == key {
                return Some(id - 1);
            }
            slot = (slot + 1) & self.mask;
        }
    }

    fn grow(&mut self) {
        let slots = self.slots.len() * 2;
        let mask = slots - 1;
        let mut table = vec![(0u32, 0u32); slots];
        for (i, &hash) in self.hashes.iter().enumerate() {
            let mut slot = hash as usize & mask;
            while table[slot].0 != 0 {
                slot = (slot + 1) & mask;
            }
            table[slot] = (i as u32 + 1, (hash >> 32) as u32);
        }
        self.slots = table;
        self.mask = mask;
    }

    /// Bytes held by the table.
    pub fn bytes(&self) -> u64 {
        (self.slots.capacity() * 8
            + self.hashes.capacity() * 8
            + self.ends.capacity() * 8
            + self.arena.capacity()) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use veta_core::hash::hash_bytes;

    #[test]
    fn fixed_table_assigns_stable_ids_across_growth() {
        let mut table = FixedTable::<u64>::with_slots(2);
        for round in 0..2 {
            for key in 0..10_000u64 {
                assert_eq!(table.find_or_insert(key * 7), key as u32, "round {round}");
            }
        }
        assert_eq!(table.len(), 10_000);
        let mut entries: Vec<(u64, u32)> = table.entries().collect();
        entries.sort_by_key(|e| e.1);
        assert_eq!(entries.len(), 10_000);
        assert!(entries.iter().all(|&(k, id)| k == id as u64 * 7));
    }

    #[test]
    fn zero_is_a_valid_key() {
        let mut table = FixedTable::<u128>::with_slots(8);
        assert_eq!(table.find_or_insert(0), 0);
        assert_eq!(table.find_or_insert(1u128 << 100), 1);
        assert_eq!(table.find_or_insert(0), 0);
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn bytes_table_keeps_keys_and_prefixes_apart() {
        let mut table = BytesTable::with_slots(2);
        let keys: Vec<Vec<u8>> = (0..5000u32)
            .map(|i| vec![b'k'; (i % 40) as usize + 1])
            .chain((0..5000u32).map(|i| i.to_le_bytes().to_vec()))
            .collect();
        let mut ids = Vec::new();
        for key in &keys {
            ids.push(table.find_or_insert(key, hash_bytes(key)));
        }
        for (key, &id) in keys.iter().zip(&ids) {
            assert_eq!(table.find_or_insert(key, hash_bytes(key)), id);
            assert_eq!(table.key(id as usize), key.as_slice());
            assert_eq!(table.hash(id as usize), hash_bytes(key));
        }
        assert_eq!(table.len(), 40 + 5000);
    }

    #[test]
    fn find_returns_the_inserted_ids_and_nothing_else() {
        let mut fixed = FixedTable::<u64>::with_slots(2);
        let mut bytes = BytesTable::with_slots(2);
        for key in 0..3000u64 {
            let id = fixed.find_or_insert(key * 3);
            assert_eq!(fixed.find(key * 3), Some(id));
            let text = format!("key{key}");
            let id = bytes.find_or_insert(text.as_bytes(), hash_bytes(text.as_bytes()));
            assert_eq!(
                bytes.find(text.as_bytes(), hash_bytes(text.as_bytes())),
                Some(id)
            );
        }
        for key in 0..3000u64 {
            assert_eq!(fixed.find(key * 3), Some(key as u32));
            assert_eq!(fixed.find(key * 3 + 1), None);
            let text = format!("key{key}");
            assert_eq!(
                bytes.find(text.as_bytes(), hash_bytes(text.as_bytes())),
                Some(key as u32)
            );
            let other = format!("other{key}");
            assert_eq!(
                bytes.find(other.as_bytes(), hash_bytes(other.as_bytes())),
                None
            );
        }
        assert_eq!(FixedTable::<u128>::with_slots(4).find(0), None);
    }

    #[test]
    fn specialized_tables_match_a_general_hash_map() {
        use std::collections::HashMap;
        use veta_core::rand::Rng;
        let mut rng = Rng::new(3);
        for round in 0..20 {
            let distinct = 1 + rng.below(if round % 2 == 0 { 50 } else { 20_000 });
            let mut narrow = FixedTable::<u64>::with_slots(2);
            let mut wide = FixedTable::<u128>::with_slots(2);
            let mut bytes = BytesTable::with_slots(2);
            let mut general: HashMap<u128, u32> = HashMap::new();
            for _ in 0..30_000 {
                let low = rng.below(distinct);
                let key = ((low as u128) << 64 | low as u128) ^ 0x5555;
                let expected = general.len() as u32;
                let expected = *general.entry(key).or_insert(expected);
                assert_eq!(narrow.find_or_insert(low), expected);
                assert_eq!(wide.find_or_insert(key), expected);
                let text = key.to_le_bytes();
                assert_eq!(bytes.find_or_insert(&text, hash_bytes(&text)), expected);
            }
            assert_eq!(narrow.len(), general.len());
            assert_eq!(wide.len(), general.len());
            assert_eq!(bytes.len(), general.len());
            for (&key, &id) in &general {
                assert_eq!(wide.find(key), Some(id));
                assert_eq!(narrow.find((key >> 64) as u64), Some(id));
            }
        }
    }

    #[test]
    fn array_table_matches_the_hash_table() {
        use veta_core::rand::Rng;
        let mut rng = Rng::new(11);
        // `Int32` keys packed in the low 32 bits; nullable ones with the flag at bit 32.
        for nullable in [false, true] {
            let null_bit = nullable.then_some(32);
            let mut array = ArrayTable::new(-1000, 1000, 4, null_bit).unwrap();
            let mut hash = FixedTable::<u64>::with_slots(2);
            for _ in 0..50_000 {
                let key = if nullable && rng.next_bool(0.05) {
                    1u64 << 32
                } else {
                    rng.range_i64(-1000, 1000) as i32 as u32 as u64
                };
                assert_eq!(array.find_or_insert(key), Some(hash.find_or_insert(key)));
            }
            assert_eq!(array.len(), hash.len());
            for (key, id) in array.entries() {
                assert_eq!(hash.find(key), Some(id));
            }
            let converted = array.to_hash_table();
            for (key, id) in array.entries() {
                assert_eq!(converted.find(key), Some(id));
            }
            // A value outside the range leaves the table unchanged.
            let before = array.len();
            assert_eq!(array.find_or_insert(1001u32 as u64), None);
            assert_eq!(array.find_or_insert((-1001i32) as u32 as u64), None);
            assert_eq!(array.len(), before);
        }
        assert!(ArrayTable::new(0, MAX_ARRAY_SLOTS as i64, 8, None).is_none());
        assert!(ArrayTable::new(i64::MIN, i64::MAX, 8, None).is_none());
        let mut full = ArrayTable::new(i64::MAX - 3, i64::MAX, 8, None).unwrap();
        assert_eq!(full.find_or_insert(i64::MAX as u64), Some(0));
        assert_eq!(full.find_or_insert(i64::MIN as u64), None);
    }

    #[test]
    fn partitions_cover_the_hash_range() {
        assert_eq!(partition_of(0), 0);
        assert_eq!(partition_of(u64::MAX), PARTITIONS - 1);
        assert_eq!(PARTITIONS, 64);
    }
}
