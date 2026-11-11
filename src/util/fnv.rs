//! FNV-1a 64-bit hash for content hashes, checksums and ids. Not cryptographic.

const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

/// Incremental FNV-1a 64 hasher.
#[derive(Clone, Copy, Debug)]
pub struct Fnv64(u64);

impl Fnv64 {
    /// A hasher in the initial state.
    pub fn new() -> Fnv64 {
        Fnv64(OFFSET)
    }

    /// Feeds bytes.
    pub fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(PRIME);
        }
    }

    /// The hash of everything fed so far.
    pub fn finish(self) -> u64 {
        self.0
    }
}

impl Default for Fnv64 {
    fn default() -> Fnv64 {
        Fnv64::new()
    }
}

/// Hash of `bytes`.
pub fn hash(bytes: &[u8]) -> u64 {
    let mut hasher = Fnv64::new();
    hasher.update(bytes);
    hasher.finish()
}

/// Hash of `bytes` as 16 lowercase hex digits.
pub fn hex(bytes: &[u8]) -> String {
    format!("{:016x}", hash(bytes))
}

#[cfg(test)]
mod tests {
    use super::hex;

    #[test]
    fn known_values() {
        assert_eq!(hex(b""), "cbf29ce484222325");
        assert_eq!(hex(b"a"), "af63dc4c8601ec8c");
        assert_eq!(hex(b"foobar"), "85944171f73967e8");
    }
}
