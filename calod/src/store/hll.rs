//! Self-contained dense HyperLogLog used to back Redis `PFADD` / `PFCOUNT` /
//! `PFMERGE`.
//!
//! # Representation
//!
//! This is a **dense** HyperLogLog with `m = 2^14 = 16384` registers, each a
//! single [`u8`] holding the maximum observed *rank* for that register (a value
//! in `0..=51` in practice). Registers start at `0` (the empty / never-seen
//! value).
//!
//! The serialized form returned by [`Hll::to_bytes`] is the 4-byte magic
//! `b"HYLL"` followed by exactly `HLL_REGISTERS` register bytes, for a total of
//! `4 + 16384 = 16388` bytes. [`Hll::from_bytes`] parses that form back.
//!
//! Because HLLs are persisted as the bytes of [`Hll::to_bytes`] inside an
//! ordinary string key, `TYPE` reports `"string"` — matching how real Redis
//! stores HLLs. The magic prefix happens to coincide with Redis's own `HYLL`
//! marker, but this format is intentionally **not** byte-compatible with
//! Redis's RDB HLL encoding: it is only required to be *internally* consistent
//! (round-trips through `to_bytes`/`from_bytes`, and [`Hll::merge`] of HLLs
//! produced by this module behaves correctly).
//!
//! # Hashing
//!
//! Elements are hashed with [`twox_hash::XxHash64`] using a **fixed seed** of
//! `0`, so the hash — and therefore every cardinality estimate — is fully
//! deterministic across calls and across process runs.
//!
//! # Algorithm
//!
//! For each element we compute a 64-bit hash `h`:
//! * the register **index** is the top `14` bits of `h`;
//! * the **rank** is derived from the remaining low `50` bits as
//!   `1 + (number of leading zero bits among those 50 bits)`, with rank `51`
//!   when all `50` bits are zero.
//!
//! The register is updated to the maximum of its current value and the new
//! rank. The cardinality [`Hll::count`] uses the standard HLL estimator with
//! `alpha_m = 0.7213 / (1 + 1.079/m)` and linear-counting small-range
//! correction. The 64-bit large-range correction is unnecessary for `u64`
//! cardinalities and is intentionally omitted.

use twox_hash::XxHash64;

/// Number of registers in the dense representation: `2^14`.
pub const HLL_REGISTERS: usize = 16384;

/// Number of high bits of the hash used to select a register: `log2(16384)`.
const HLL_INDEX_BITS: u32 = 14;

/// Number of low bits of the hash used to compute the rank.
const HLL_RANK_BITS: u32 = 64 - HLL_INDEX_BITS; // 50

/// Maximum possible rank: every one of the `HLL_RANK_BITS` low bits is zero,
/// giving `HLL_RANK_BITS + 1`.
const HLL_MAX_RANK: u8 = (HLL_RANK_BITS + 1) as u8; // 51

/// Fixed hash seed. Keeping this constant is what makes counts deterministic
/// across calls and process runs.
const HLL_SEED: u64 = 0;

/// 4-byte magic prefix written by [`Hll::to_bytes`].
const HLL_MAGIC: &[u8; 4] = b"HYLL";

/// Total length of the serialized form: magic + one byte per register.
const HLL_SERIALIZED_LEN: usize = HLL_MAGIC.len() + HLL_REGISTERS;

/// A dense HyperLogLog sketch.
#[derive(Clone)]
pub struct Hll {
    /// Exactly [`HLL_REGISTERS`] register bytes, each the max observed rank.
    registers: Vec<u8>,
}

/// Compute the `(index, rank)` pair for a 64-bit hash.
///
/// * `index` is the top [`HLL_INDEX_BITS`] bits (always `< HLL_REGISTERS`).
/// * `rank` is `1 + leading_zeros(low 50 bits)`, capped at [`HLL_MAX_RANK`]
///   (which is also the value produced when all 50 low bits are zero).
#[inline]
fn index_and_rank(hash: u64) -> (usize, u8) {
    let index = (hash >> HLL_RANK_BITS) as usize;

    // Isolate the low `HLL_RANK_BITS` bits, then shift them up to the top of the
    // word so a normal 64-bit `leading_zeros` counts from the MSB side of those
    // bits. If the low bits are all zero the shifted value is zero and
    // `leading_zeros() == 64`, which we clamp to the maximum rank.
    let low = hash & ((1u64 << HLL_RANK_BITS) - 1);
    let shifted = low << HLL_INDEX_BITS; // move the 50 low bits to the MSB end

    let rank = if shifted == 0 {
        HLL_MAX_RANK
    } else {
        // leading_zeros() here is in 0..=49 (bit 63 down to bit 14 may be set),
        // so rank = leading_zeros + 1 is in 1..=50.
        (shifted.leading_zeros() + 1) as u8
    };

    (index, rank)
}

impl Hll {
    /// Create an empty HLL with all registers zero.
    pub fn new() -> Self {
        Hll {
            registers: vec![0u8; HLL_REGISTERS],
        }
    }

    /// Add an element to the sketch.
    ///
    /// Returns `true` if a register's value increased (so the cardinality
    /// estimate may have changed), `false` otherwise.
    pub fn add(&mut self, element: &[u8]) -> bool {
        let hash = XxHash64::oneshot(HLL_SEED, element);
        let (index, rank) = index_and_rank(hash);

        // `index` is the top 14 bits of a 64-bit value, hence always in range,
        // but index defensively rather than risk a panic on a corrupt sketch.
        let slot = &mut self.registers[index];
        if rank > *slot {
            *slot = rank;
            true
        } else {
            false
        }
    }

    /// Estimate the cardinality (number of distinct elements added).
    pub fn count(&self) -> u64 {
        let m = HLL_REGISTERS as f64;
        let alpha_m = 0.7213 / (1.0 + 1.079 / m);

        // Harmonic mean denominator: sum of 2^-reg[j]; also count empty
        // registers for the small-range linear-counting correction.
        let mut sum = 0.0f64;
        let mut zeros = 0u32;
        for &reg in &self.registers {
            // 2^-reg == 1 / (1 << reg) for reg in 0..=63; reg never exceeds
            // HLL_MAX_RANK (51) here, so the shift is always well-defined.
            sum += 1.0 / ((1u64 << reg) as f64);
            if reg == 0 {
                zeros += 1;
            }
        }

        let raw_estimate = alpha_m * m * m / sum;

        // Small-range correction: when the raw estimate is small and some
        // registers are still empty, linear counting is more accurate.
        let estimate = if raw_estimate <= 2.5 * m && zeros != 0 {
            m * (m / zeros as f64).ln()
        } else {
            raw_estimate
        };

        estimate.round() as u64
    }

    /// Merge another HLL into `self` by taking the register-wise maximum.
    pub fn merge(&mut self, other: &Hll) {
        for (dst, &src) in self.registers.iter_mut().zip(other.registers.iter()) {
            if src > *dst {
                *dst = src;
            }
        }
    }

    /// Serialize to `b"HYLL"` followed by the [`HLL_REGISTERS`] register bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HLL_SERIALIZED_LEN);
        out.extend_from_slice(HLL_MAGIC);
        out.extend_from_slice(&self.registers);
        out
    }

    /// Parse the serialized form produced by [`Hll::to_bytes`].
    ///
    /// Returns `None` if the magic prefix is wrong or the length is not exactly
    /// [`HLL_SERIALIZED_LEN`].
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if !Self::is_hll(b) {
            return None;
        }
        let registers = b[HLL_MAGIC.len()..].to_vec();
        debug_assert_eq!(registers.len(), HLL_REGISTERS);
        Some(Hll { registers })
    }

    /// Quick check that `b` looks like a serialized HLL: correct magic prefix
    /// and exactly [`HLL_SERIALIZED_LEN`] bytes.
    pub fn is_hll(b: &[u8]) -> bool {
        b.len() == HLL_SERIALIZED_LEN && &b[..HLL_MAGIC.len()] == HLL_MAGIC
    }
}

impl Default for Hll {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_count_is_zero() {
        let h = Hll::new();
        assert_eq!(h.count(), 0);
        assert_eq!(h.registers.len(), HLL_REGISTERS);
        assert!(h.registers.iter().all(|&r| r == 0));
    }

    #[test]
    fn default_matches_new() {
        let a = Hll::default();
        let b = Hll::new();
        assert_eq!(a.registers, b.registers);
    }

    #[test]
    fn duplicate_add_returns_false() {
        let mut h = Hll::new();
        // First insertion of a fresh element must increase a register.
        assert!(h.add(b"hello"));
        // Adding the exact same element again cannot increase any register.
        assert!(!h.add(b"hello"));
        // Cardinality of a single distinct element estimates as ~1.
        assert_eq!(h.count(), 1);
    }

    #[test]
    fn count_within_three_percent_of_10000() {
        let n: usize = 10_000;
        let mut h = Hll::new();
        for i in 0..n {
            h.add(format!("e{i}").as_bytes());
        }
        let est = h.count() as f64;
        let target = n as f64;
        let err = (est - target).abs() / target;
        assert!(
            err <= 0.03,
            "estimate {est} not within 3% of {target} (error {err})"
        );
    }

    #[test]
    fn merge_overlapping_sets() {
        // A: elements 0..5000, B: elements 2500..7500; union is 0..7500.
        let mut a = Hll::new();
        for i in 0..5000 {
            a.add(format!("e{i}").as_bytes());
        }
        let mut b = Hll::new();
        for i in 2500..7500 {
            b.add(format!("e{i}").as_bytes());
        }

        a.merge(&b);

        let est = a.count() as f64;
        let target = 7500.0;
        let err = (est - target).abs() / target;
        assert!(
            err <= 0.03,
            "merged estimate {est} not within 3% of {target} (error {err})"
        );
    }

    #[test]
    fn merge_is_register_wise_max() {
        let mut a = Hll::new();
        let mut b = Hll::new();
        a.add(b"only-in-a");
        b.add(b"only-in-b");

        let mut expected = a.clone();
        for (e, &s) in expected.registers.iter_mut().zip(b.registers.iter()) {
            *e = (*e).max(s);
        }

        a.merge(&b);
        assert_eq!(a.registers, expected.registers);
    }

    #[test]
    fn to_from_bytes_round_trip() {
        let mut h = Hll::new();
        for i in 0..2000 {
            h.add(format!("item-{i}").as_bytes());
        }

        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), HLL_SERIALIZED_LEN);

        let restored = Hll::from_bytes(&bytes).expect("valid serialized HLL");
        assert_eq!(restored.registers, h.registers);
        assert_eq!(restored.count(), h.count());
    }

    #[test]
    fn is_hll_recognition() {
        let h = Hll::new();
        let bytes = h.to_bytes();
        assert!(Hll::is_hll(&bytes));

        // Wrong magic.
        assert!(!Hll::is_hll(b"nothll"));

        // Right magic, wrong length.
        let mut wrong_len = HLL_MAGIC.to_vec();
        wrong_len.extend_from_slice(&[0u8; 10]);
        assert!(!Hll::is_hll(&wrong_len));

        // Correct length, wrong magic.
        let mut wrong_magic = vec![0u8; HLL_SERIALIZED_LEN];
        wrong_magic[..4].copy_from_slice(b"XXXX");
        assert!(!Hll::is_hll(&wrong_magic));
    }

    #[test]
    fn from_bytes_rejects_bad_input() {
        assert!(Hll::from_bytes(b"nothll").is_none());
        assert!(Hll::from_bytes(&[]).is_none());

        let mut wrong_len = HLL_MAGIC.to_vec();
        wrong_len.push(0);
        assert!(Hll::from_bytes(&wrong_len).is_none());
    }

    #[test]
    fn index_and_rank_known_values() {
        // Index is the top 14 bits; rank derives from the low 50 bits.

        // hash = 0 -> index 0, all 50 low bits zero -> max rank (51).
        assert_eq!(index_and_rank(0), (0, HLL_MAX_RANK));

        // Top 14 bits all set -> index 0x3FFF (16383); low 50 bits zero -> 51.
        // (0x3FFF is exactly 14 set bits; shifting by 50 stays within the u64.)
        let top_set = 0x3FFFu64 << HLL_RANK_BITS;
        let (idx, rank) = index_and_rank(top_set);
        assert_eq!(idx, HLL_REGISTERS - 1);
        assert_eq!(rank, HLL_MAX_RANK);

        // Index 0, with the highest of the 50 low bits set (bit 49) -> rank 1.
        let high_low_bit = 1u64 << (HLL_RANK_BITS - 1); // bit 49
        assert_eq!(index_and_rank(high_low_bit), (0, 1));

        // Index 0, with only the lowest bit set (bit 0). The 50 low bits have 49
        // leading zeros, so rank = 49 + 1 = 50.
        assert_eq!(index_and_rank(1), (0, 50));

        // Index 0, second-lowest of the 50 low bits set (bit 48) -> rank 2.
        let second_high = 1u64 << (HLL_RANK_BITS - 2); // bit 48
        assert_eq!(index_and_rank(second_high), (0, 2));

        // Distinct index check: put a known value in the index field and a known
        // rank in the low bits simultaneously.
        let idx_val = 12345u64;
        let combined = (idx_val << HLL_RANK_BITS) | high_low_bit;
        let (i2, r2) = index_and_rank(combined);
        assert_eq!(i2, idx_val as usize);
        assert_eq!(r2, 1);
    }

    #[test]
    fn rank_never_exceeds_max() {
        // Exhaustively sanity-check that ranks stay in 1..=HLL_MAX_RANK for a
        // spread of hash values.
        for k in 0..64u64 {
            let (_idx, rank) = index_and_rank(1u64 << k);
            assert!(
                (1..=HLL_MAX_RANK).contains(&rank),
                "bad rank {rank} for bit {k}"
            );
        }
    }
}
