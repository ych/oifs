pub struct Bitmap<'a> {
    data: &'a mut [u8],
}

impl<'a> Bitmap<'a> {
    pub fn new(data: &'a mut [u8]) -> Self {
        Self { data }
    }

    /// Set bit at index to 1 (used)
    pub fn set(&mut self, index: usize) {
        let byte_index = index / 8;
        let bit_index = index % 8;
        if byte_index < self.data.len() {
            self.data[byte_index] |= 1 << bit_index;
        }
    }

    /// Set bit at index to 0 (free)
    pub fn clear(&mut self, index: usize) {
        let byte_index = index / 8;
        let bit_index = index % 8;
        if byte_index < self.data.len() {
            self.data[byte_index] &= !(1 << bit_index);
        }
    }

    /// Check if bit is set (used)
    pub fn get(&self, index: usize) -> bool {
        let byte_index = index / 8;
        let bit_index = index % 8;
        if byte_index < self.data.len() {
            (self.data[byte_index] & (1 << bit_index)) != 0
        } else {
            false // Out of bounds is considered "not set" or should be error? 
                  // For safety, let's say false, but caller should check bounds.
        }
    }

    /// Find first bit that is 0 (free)
    pub fn find_first_free(&self) -> Option<usize> {
        for (i, &byte) in self.data.iter().enumerate() {
            if byte != 0xFF {
                // Found a byte with at least one free bit
                for bit in 0..8 {
                    if (byte & (1 << bit)) == 0 {
                        return Some(i * 8 + bit);
                    }
                }
            }
        }
        None
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
