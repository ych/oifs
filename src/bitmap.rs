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

    /// Find first range of `count` contiguous free bits (0s) starting from `start_bit`.
    pub fn find_contiguous_free_from(&self, mut start_bit: usize, count: usize) -> Option<usize> {
        let total_bits = self.data.len() * 8;
        if count == 0 {
            return if start_bit <= total_bits {
                Some(start_bit)
            } else {
                None
            };
        }
        if count > total_bits || start_bit >= total_bits {
            return None;
        }
        if count == 1 {
            return self.find_first_free_from(start_bit);
        }

        let (chunks, remainder) = self.data.as_chunks::<8>();
        let chunk_count = chunks.len();

        let is_bit_free = |idx: usize| -> bool {
            let chunk_idx = idx / 64;
            let bit_idx = idx % 64;
            if chunk_idx < chunk_count {
                let word = u64::from_le_bytes(chunks[chunk_idx]);
                (word & (1u64 << bit_idx)) == 0
            } else {
                let rem_idx = (idx - chunk_count * 64) / 8;
                let rem_bit = idx % 8;
                if rem_idx < remainder.len() {
                    (remainder[rem_idx] & (1u8 << rem_bit)) == 0
                } else {
                    false
                }
            }
        };

        while start_bit.saturating_add(count) <= total_bits {
            let candidate = self.find_first_free_from(start_bit)?;
            if candidate.saturating_add(count) > total_bits {
                return None;
            }

            let mut all_free = true;
            let mut i = 1;
            while i < count {
                let check_bit = candidate + i;
                if !is_bit_free(check_bit) {
                    start_bit = check_bit + 1;
                    all_free = false;
                    break;
                }
                i += 1;
            }

            if all_free {
                return Some(candidate);
            }
        }

        None
    }

    /// Find first range of `count` contiguous free bits starting from 0.
    #[inline]
    pub fn find_contiguous_free(&self, count: usize) -> Option<usize> {
        self.find_contiguous_free_from(0, count)
    }

    /// Find `count` contiguous free bits starting from `hint`.
    /// If not found after `hint`, wraps around to 0.
    pub fn find_contiguous_free_wrapped(&self, hint: usize, count: usize) -> Option<usize> {
        if let Some(idx) = self.find_contiguous_free_from(hint, count) {
            Some(idx)
        } else if hint > 0 {
            self.find_contiguous_free_from(0, count)
                .filter(|&idx| idx < hint)
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

    /// Set a contiguous range of `count` bits starting at `start_bit` to 1 (used).
    pub fn set_range(&mut self, start_bit: usize, count: usize) {
        if count == 0 {
            return;
        }
        let total_bits = self.data.len() * 8;
        let end_bit = match start_bit.checked_add(count) {
            Some(e) => e.min(total_bits),
            None => total_bits,
        };
        if start_bit >= end_bit {
            return;
        }

        let start_byte = start_bit / 8;
        let end_byte = end_bit / 8;

        if start_byte == end_byte {
            let num_bits = end_bit - start_bit;
            let mask = (((1u16 << num_bits) - 1) as u8) << (start_bit % 8);
            self.data[start_byte] |= mask;
            return;
        }

        let head_rem = start_bit % 8;
        if head_rem != 0 {
            let mask = 0xFFu8 << head_rem;
            self.data[start_byte] |= mask;
        }

        let mid_start = if head_rem != 0 {
            start_byte + 1
        } else {
            start_byte
        };
        if mid_start < end_byte {
            self.data[mid_start..end_byte].fill(0xFF);
        }

        let tail_rem = end_bit % 8;
        if tail_rem != 0 && end_byte < self.data.len() {
            let mask = (1u8 << tail_rem) - 1;
            self.data[end_byte] |= mask;
        }
    }

    /// Clear a contiguous range of `count` bits starting at `start_bit` to 0 (free).
    pub fn clear_range(&mut self, start_bit: usize, count: usize) {
        if count == 0 {
            return;
        }
        let total_bits = self.data.len() * 8;
        let end_bit = match start_bit.checked_add(count) {
            Some(e) => e.min(total_bits),
            None => total_bits,
        };
        if start_bit >= end_bit {
            return;
        }

        let start_byte = start_bit / 8;
        let end_byte = end_bit / 8;

        if start_byte == end_byte {
            let num_bits = end_bit - start_bit;
            let mask = (((1u16 << num_bits) - 1) as u8) << (start_bit % 8);
            self.data[start_byte] &= !mask;
            return;
        }

        let head_rem = start_bit % 8;
        if head_rem != 0 {
            let mask = 0xFFu8 << head_rem;
            self.data[start_byte] &= !mask;
        }

        let mid_start = if head_rem != 0 {
            start_byte + 1
        } else {
            start_byte
        };
        if mid_start < end_byte {
            self.data[mid_start..end_byte].fill(0x00);
        }

        let tail_rem = end_bit % 8;
        if tail_rem != 0 && end_byte < self.data.len() {
            let mask = (1u8 << tail_rem) - 1;
            self.data[end_byte] &= !mask;
        }
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

    /// Find first range of `count` contiguous free bits starting from 0.
    #[inline]
    pub fn find_contiguous_free(&self, count: usize) -> Option<usize> {
        BitmapRef::new(self.data).find_contiguous_free(count)
    }

    /// Find first range of `count` contiguous free bits starting from `start_bit`.
    #[inline]
    pub fn find_contiguous_free_from(&self, start_bit: usize, count: usize) -> Option<usize> {
        BitmapRef::new(self.data).find_contiguous_free_from(start_bit, count)
    }

    /// Find `count` contiguous free bits starting from `hint`. If not found after `hint`, wraps around to 0.
    #[inline]
    pub fn find_contiguous_free_wrapped(&self, hint: usize, count: usize) -> Option<usize> {
        BitmapRef::new(self.data).find_contiguous_free_wrapped(hint, count)
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

    #[test]
    fn test_bitmap_contiguous_operations() {
        let mut data = [0u8; 32]; // 256 bits
        let mut bm = Bitmap::new(&mut data);

        // Initially all 256 bits are free
        assert_eq!(bm.find_contiguous_free(10), Some(0));
        assert_eq!(bm.find_contiguous_free(256), Some(0));
        assert_eq!(bm.find_contiguous_free(257), None);

        // Set bits 0..5 (bits 0,1,2,3,4)
        bm.set_range(0, 5);
        for i in 0..5 {
            assert!(bm.get(i));
        }
        assert!(!bm.get(5));
        assert_eq!(bm.find_contiguous_free(5), Some(5));

        // Set bit 10
        bm.set(10);
        // Free intervals: [5..10) has 5 bits, [11..256) has 245 bits
        assert_eq!(bm.find_contiguous_free(5), Some(5));
        assert_eq!(bm.find_contiguous_free(6), Some(11));

        // Test multi-byte set_range spanning word boundaries
        // Set range 60..130 (70 bits)
        bm.set_range(60, 70);
        for i in 60..130 {
            assert!(bm.get(i), "Bit {} should be set", i);
        }
        assert!(!bm.get(59));
        assert!(!bm.get(130));

        // Clear range 70..80 (10 bits)
        bm.clear_range(70, 10);
        for i in 70..80 {
            assert!(!bm.get(i), "Bit {} should be cleared", i);
        }
        assert!(bm.get(69));
        assert!(bm.get(80));

        // Contiguous search should find the hole [70..80) for count <= 10
        assert_eq!(bm.find_contiguous_free_from(60, 10), Some(70));
        assert_eq!(bm.find_contiguous_free_from(60, 11), Some(130));

        // Wrapped search
        // Fill all bits 0..70 except 70..80, and fill 130..256
        bm.set_range(0, 70);
        bm.set_range(130, 126);
        // Now free intervals: [70..80) (10 bits), and nothing after 80
        assert_eq!(bm.find_contiguous_free_from(100, 10), None);
        assert_eq!(bm.find_contiguous_free_wrapped(100, 10), Some(70));
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
                assert!(
                    bitmap.get(i),
                    "All bits before find_first_free result must be set"
                );
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
        assert!(
            !bitmap.get(index),
            "Out-of-bounds get() should return false"
        );
    }

    /// Prove that find_contiguous_free returns a range where all bits are genuinely free.
    #[kani::proof]
    #[kani::unwind(9)]
    fn proof_find_contiguous_free_correctness() {
        let b0: u8 = kani::any();
        let mut data = [b0]; // 8 bits
        let count: usize = kani::any();
        kani::assume(count > 0 && count <= 8);

        let bitmap = Bitmap::new(&mut data);
        if let Some(start) = bitmap.find_contiguous_free(count) {
            assert!(start + count <= 8);
            for i in start..start + count {
                assert!(!bitmap.get(i), "All bits in returned range must be free");
            }
        }
    }
}
