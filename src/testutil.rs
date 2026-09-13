//! Test-only helpers shared by the module test suites.

/// Deterministic xorshift generator for property tests.
///
/// A fixed seed keeps any failure reproducible and costs no dev-dependency.
/// The properties asserted with it concern survival — no panic, bounded
/// output — never the values themselves.
pub struct Fuzz(pub u64);

impl Fuzz {
    /// Next raw word. Named to stay clear of `Iterator::next`.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn byte(&mut self) -> u8 {
        (self.next_u64() >> 24) as u8
    }

    /// A value in `0..n`. Panics if `n` is zero, like the `%` it wraps.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// A buffer of up to `max` random bytes.
    pub fn bytes(&mut self, max: usize) -> Vec<u8> {
        let len = self.below(max);
        (0..len).map(|_| self.byte()).collect()
    }
}
