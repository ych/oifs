/// Read-only view over a bitmap slice
pub struct BitmapRef<'a> {
    data: &'a [u8],
}

impl<'a> BitmapRef<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    /// Check if bit is set (used)
    #[inline]
    pub fn get(&self, index: usize) -> bool {
        let byte_index = index / 8;
        let bit_index = index % 8;
        if byte_index < self.data.len() {
            (self.data[byte_index] & (1 << bit_index)) != 0
        } else {
            false
        }
    }

    /// Find first bit that is 0 (free)
    pub fn find_first_free(&self) -> Option<usize> {
        self.find_first_free_from(0)
    }

    /// Find first bit that is 0 (free) starting from `start_bit`
    pub fn find_first_free_from(&self, start_bit: usize) -> Option<usize> {
        let total_bits = self.data.len() * 8;
        if start_bit >= total_bits {
            return None;
        }

        let start_chunk = start_bit / 64;
        let bit_in_chunk = start_bit % 64;

        let (chunks, remainder) = self.data.as_chunks::<8>();
        let chunk_count = chunks.len();

        if start_chunk < chunk_count {
            let mut chunk_iter = chunks.iter().enumerate().skip(start_chunk);

            // First chunk: mask out bits before start_bit
            if let Some((chunk_idx, chunk)) = chunk_iter.next() {
                let raw_word = u64::from_le_bytes(*chunk);
                let mask = (1u64 << bit_in_chunk) - 1;
                let masked_word = raw_word | mask;
                if masked_word != u64::MAX {
                    let bit = (!masked_word).trailing_zeros() as usize;
                    return Some(chunk_idx * 64 + bit);
                }
            }

            // Subsequent chunks
            for (chunk_idx, chunk) in chunk_iter {
                let word = u64::from_le_bytes(*chunk);
                if word != u64::MAX {
                    let bit = (!word).trailing_zeros() as usize;
                    return Some(chunk_idx * 64 + bit);
                }
            }
        }

        // Remainder bytes
        let base = chunk_count * 64;
        let rem_start_byte = if start_bit > base {
            (start_bit - base) / 8
        } else {
            0
        };

        for (i, &byte) in remainder.iter().enumerate().skip(rem_start_byte) {
            let byte_bit_offset = base + i * 8;
            let mask = if start_bit > byte_bit_offset {
                (1u8 << (start_bit - byte_bit_offset)) - 1
            } else {
                0
            };
            let masked_byte = byte | mask;
            if masked_byte != 0xFF {
                let bit = (!masked_byte).trailing_zeros() as usize;
                return Some(byte_bit_offset + bit);
            }
        }

        None
    }

    /// Finds next free bit starting from `hint`. If not found after `hint`, wraps around to 0.
    pub fn find_next_free_wrapped(&self, hint: usize) -> Option<usize> {
        if let Some(idx) = self.find_first_free_from(hint) {
            Some(idx)
        } else if hint > 0 {
            self.find_first_free_from(0).filter(|&idx| idx < hint)
        } else {
            None
        }
    }

    /// Fast 64-bit word iterator over all set bits up to `max_bits`.
    /// Skips 64 zero bits in a single CPU operation.
    pub fn for_each_set_bit<F: FnMut(usize)>(&self, max_bits: usize, mut f: F) {
        let max_bytes = max_bits.div_ceil(8).min(self.data.len());
        let slice = &self.data[..max_bytes];
        let (chunks, remainder) = slice.as_chunks::<8>();
        let chunk_count = chunks.len();

        for (chunk_idx, chunk) in chunks.iter().enumerate() {
            let mut word = u64::from_le_bytes(*chunk);
            let base = chunk_idx * 64;
            while word != 0 {
                let bit = word.trailing_zeros() as usize;
                let idx = base + bit;
                if idx < max_bits {
                    f(idx);
                }
                word &= word - 1; // Clear lowest set bit
            }
        }

        let base = chunk_count * 64;
        for (i, &byte) in remainder.iter().enumerate() {
            let mut b = byte;
            let byte_base = base + i * 8;
            while b != 0 {
                let bit = b.trailing_zeros() as usize;
                let idx = byte_base + bit;
                if idx < max_bits {
                    f(idx);
                }
                b &= b - 1;
            }
        }
    }
}

pub struct Bitmap<'a> {
    data: &'a mut [u8],
}

impl<'a> Bitmap<'a> {
    pub fn new(data: &'a mut [u8]) -> Self {
        Self { data }
    }

    /// Set bit at index to 1 (used)
    #[inline]
    pub fn set(&mut self, index: usize) {
        let byte_index = index / 8;
        let bit_index = index % 8;
        if byte_index < self.data.len() {
            self.data[byte_index] |= 1 << bit_index;
        }
    }

    /// Set bit at index to 0 (free)
    #[inline]
    pub fn clear(&mut self, index: usize) {
        let byte_index = index / 8;
        let bit_index = index % 8;
        if byte_index < self.data.len() {
            self.data[byte_index] &= !(1 << bit_index);
        }
    }

    /// Check if bit is set (used)
    #[inline]
    pub fn get(&self, index: usize) -> bool {
        BitmapRef::new(self.data).get(index)
    }

    /// Find first bit that is 0 (free)
    #[inline]
    pub fn find_first_free(&self) -> Option<usize> {
        BitmapRef::new(self.data).find_first_free()
    }

    /// Find first bit that is 0 (free) starting from `start_bit`
    #[inline]
    pub fn find_first_free_from(&self, start_bit: usize) -> Option<usize> {
        BitmapRef::new(self.data).find_first_free_from(start_bit)
    }

    /// Finds next free bit starting from `hint`. If not found, wraps around to 0.
    #[inline]
    pub fn find_next_free_wrapped(&self, hint: usize) -> Option<usize> {
        BitmapRef::new(self.data).find_next_free_wrapped(hint)
    }

    /// Fast 64-bit word iterator over all set bits
    #[inline]
    pub fn for_each_set_bit<F: FnMut(usize)>(&self, max_bits: usize, f: F) {
        BitmapRef::new(self.data).for_each_set_bit(max_bits, f);
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bitmap_operations() {
        let mut data = [0u8; 2];
        let mut bitmap = Bitmap::new(&mut data);

        assert_eq!(bitmap.find_first_free(), Some(0));
        
        bitmap.set(0);
        assert!(bitmap.get(0));
        assert_eq!(bitmap.find_first_free(), Some(1));

        bitmap.set(1);
        assert!(bitmap.get(1));
        assert_eq!(bitmap.find_first_free(), Some(2));
        
        bitmap.clear(0);
        assert!(!bitmap.get(0));
        assert_eq!(bitmap.find_first_free(), Some(0));
    }

    #[test]
    fn test_bitmap_hint_and_set_bit_iteration() {
        let mut data = [0u8; 16]; // 128 bits
        let mut bm = Bitmap::new(&mut data);

        // Set bits 2, 5, 63, 64, 100
        bm.set(2);
        bm.set(5);
        bm.set(63);
        bm.set(64);
        bm.set(100);

        let mut collected = Vec::new();
        bm.for_each_set_bit(128, |idx| collected.push(idx));
        assert_eq!(collected, vec![2, 5, 63, 64, 100]);

        // find_first_free_from
        assert_eq!(bm.find_first_free_from(0), Some(0));
        assert_eq!(bm.find_first_free_from(2), Some(3)); // 2 is set, 3 is free
        assert_eq!(bm.find_first_free_from(5), Some(6));
        assert_eq!(bm.find_first_free_from(63), Some(65)); // 63 and 64 are set, 65 is free

        // Wrapped search
        // Fill all bits from 10 to 127
        for i in 10..128 {
            bm.set(i);
        }
        // Free bits are: 0, 1, 3, 4, 6, 7, 8, 9
        assert_eq!(bm.find_first_free_from(10), None);
        assert_eq!(bm.find_next_free_wrapped(10), Some(0));
        assert_eq!(bm.find_next_free_wrapped(50), Some(0));
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Prove that set followed by clear restores the original bit state.
    #[kani::proof]
    fn proof_set_clear_roundtrip() {
        let mut data = [0u8; 4]; // 32 bits
        let index: usize = kani::any();
        kani::assume(index < 32);

        let mut bitmap = Bitmap::new(&mut data);
        let before = bitmap.get(index);
        assert!(!before, "Fresh bitmap should have all bits clear");

        bitmap.set(index);
        assert!(bitmap.get(index), "Bit must be set after set()");

        bitmap.clear(index);
        assert!(!bitmap.get(index), "Bit must be clear after clear()");
    }

    /// Prove that set() only modifies the target bit, leaving others unchanged.
    #[kani::proof]
    fn proof_set_isolation() {
        let mut data = [0u8; 2]; // 16 bits
        let target: usize = kani::any();
        kani::assume(target < 16);

        // Snapshot before
        let snapshot = data;
        let mut bitmap = Bitmap::new(&mut data);
        bitmap.set(target);

        // Every other bit must remain unchanged
        for i in 0..16 {
            if i != target {
                let byte_idx = i / 8;
                let bit_idx = i % 8;
                let original = (snapshot[byte_idx] & (1 << bit_idx)) != 0;
                assert_eq!(bitmap.get(i), original, "set() must not alter other bits");
            }
        }
    }

    /// Prove that find_first_free returns a genuinely free bit.
    #[kani::proof]
    #[kani::unwind(9)] // 8 bits + 1 for loop termination
    fn proof_find_first_free_correctness() {
        let b0: u8 = kani::any();
        let mut data = [b0]; // 8 bits — keeps CBMC tractable

        let bitmap = Bitmap::new(&mut data);
        if let Some(idx) = bitmap.find_first_free() {
            // The returned index must actually be free
            assert!(idx < 8, "Index must be within bitmap bounds");
            assert!(!bitmap.get(idx), "find_first_free must return a free bit");
            // All earlier indices must be set (i.e., it's truly the *first* free)
            for i in 0..idx {
                assert!(bitmap.get(i), "All bits before find_first_free result must be set");
            }
        } else {
            // No free bit: byte must be all ones
            assert_eq!(b0, 0xFF);
        }
    }


    /// Prove that out-of-bounds get() always returns false (no panic).
    #[kani::proof]
    fn proof_get_oob_returns_false() {
        let mut data = [0xFFu8; 2]; // all set
        let index: usize = kani::any();
        kani::assume(index >= 16 && index < 64);

        let bitmap = Bitmap::new(&mut data);
        assert!(!bitmap.get(index), "Out-of-bounds get() should return false");
    }
}
