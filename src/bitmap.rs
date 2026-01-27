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
