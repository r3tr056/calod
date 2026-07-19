//! Backing data structure and operations for Redis sorted sets (ZSET).
//!
//! Members are binary-safe (`Vec<u8>`); scores are `f64`. Elements are kept in a
//! total order by **(score ascending, then member bytes lexicographically
//! ascending)** — exactly matching Redis semantics, where equal scores break
//! ties by member byte order.
//!
//! Two structures are kept in sync:
//! * a [`HashMap<Vec<u8>, f64>`] for O(1) `member -> score` lookups, and
//! * a [`BTreeSet<ScoreKey>`] providing the ordered index for rank/range ops.
//!
//! [`f64::total_cmp`] is used for score comparison so the ordered index is a
//! genuine total order even in the presence of `NaN`/`-0.0` (callers are
//! expected to reject `NaN` scores before insertion). On `insert` of an existing
//! member the stale [`ScoreKey`] is removed before the new one is inserted so the
//! two structures never diverge.
//!
//! Serialization stores only the `member -> score` map; the ordered index is
//! rebuilt on deserialize, keeping the on-disk/wire form compact and the
//! invariant trivially maintained.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

/// A score-range bound. Use [`f64::NEG_INFINITY`] / [`f64::INFINITY`] for
/// `-inf` / `+inf`.
#[derive(Clone, Copy, Debug)]
pub enum ScoreBound {
    Inclusive(f64),
    Exclusive(f64),
}

/// A lexicographic-range bound (for `ZRANGEBYLEX` / `ZLEXCOUNT`).
#[derive(Clone, Debug)]
pub enum LexBound {
    NegInf,
    PosInf,
    Inclusive(Vec<u8>),
    Exclusive(Vec<u8>),
}

/// Internal ordered key: `(score, member)` with a total order.
///
/// Scores are compared with [`f64::total_cmp`] (total even for `NaN`/`-0.0`),
/// then members with their natural `Vec<u8>` lexicographic ordering.
#[derive(Clone, Debug)]
struct ScoreKey {
    score: f64,
    member: Vec<u8>,
}

impl ScoreKey {
    #[inline]
    fn new(score: f64, member: Vec<u8>) -> Self {
        ScoreKey { score, member }
    }
}

impl PartialEq for ScoreKey {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for ScoreKey {}

impl PartialOrd for ScoreKey {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoreKey {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| self.member.cmp(&other.member))
    }
}

/// The backing data structure for a Redis sorted set (ZSET).
pub struct SortedSetData {
    /// `member -> score` for O(1) score lookup.
    map: HashMap<Vec<u8>, f64>,
    /// Ordered index over `(score, member)`.
    index: BTreeSet<ScoreKey>,
}

impl SortedSetData {
    /// Create an empty sorted set.
    pub fn new() -> Self {
        SortedSetData {
            map: HashMap::new(),
            index: BTreeSet::new(),
        }
    }

    /// Number of members in the set.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// `true` iff the set has no members.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Score of `member`, or `None` if it is not present.
    pub fn score(&self, member: &[u8]) -> Option<f64> {
        self.map.get(member).copied()
    }

    /// Insert or update `member -> score`.
    ///
    /// Returns `true` iff the member was **newly added** (i.e. did not exist
    /// before). On update, the stale ordered key is removed first so the index
    /// stays consistent.
    pub fn insert(&mut self, member: Vec<u8>, score: f64) -> bool {
        match self.map.insert(member.clone(), score) {
            Some(old_score) => {
                // Existing member: drop the stale ScoreKey, insert the fresh one.
                if old_score.to_bits() != score.to_bits() {
                    self.index.remove(&ScoreKey::new(old_score, member.clone()));
                    self.index.insert(ScoreKey::new(score, member));
                }
                false
            }
            None => {
                self.index.insert(ScoreKey::new(score, member));
                true
            }
        }
    }

    /// Remove `member`. Returns `true` iff it existed.
    pub fn remove(&mut self, member: &[u8]) -> bool {
        match self.map.remove(member) {
            Some(score) => {
                self.index.remove(&ScoreKey::new(score, member.to_vec()));
                true
            }
            None => false,
        }
    }

    /// 0-based ascending rank (by `(score, member)` order); `None` if absent.
    pub fn rank(&self, member: &[u8]) -> Option<usize> {
        let score = self.score(member)?;
        let key = ScoreKey::new(score, member.to_vec());
        Some(self.index.range(..&key).count())
    }

    /// 0-based descending rank; `None` if absent.
    pub fn rev_rank(&self, member: &[u8]) -> Option<usize> {
        let asc = self.rank(member)?;
        Some(self.len() - 1 - asc)
    }

    /// Members by 0-based inclusive index range, supporting negative indices
    /// (like Redis: `-1` = last). `rev` walks high->low. Returns
    /// `(member, score)` in the produced order. Out-of-range yields empty.
    pub fn range_by_index(&self, start: i64, stop: i64, rev: bool) -> Vec<(Vec<u8>, f64)> {
        let (lo, hi) = match normalize_index_range(start, stop, self.len()) {
            Some(range) => range,
            None => return Vec::new(),
        };

        // `lo..=hi` are positions in *ascending* order.
        if rev {
            // Walk descending, mapping ascending positions to from-the-end ones.
            let len = self.len();
            self.index
                .iter()
                .rev()
                .skip(len - 1 - hi)
                .take(hi - lo + 1)
                .map(|k| (k.member.clone(), k.score))
                .collect()
        } else {
            self.index
                .iter()
                .skip(lo)
                .take(hi - lo + 1)
                .map(|k| (k.member.clone(), k.score))
                .collect()
        }
    }

    /// Members whose score is within `[min, max]` honoring exclusivity. `rev`
    /// returns highest-first. `offset`/`count` are applied AFTER ordering
    /// (`count == None` means no limit; `offset` skips from the start of the
    /// produced order).
    pub fn range_by_score(
        &self,
        min: ScoreBound,
        max: ScoreBound,
        rev: bool,
        offset: usize,
        count: Option<usize>,
    ) -> Vec<(Vec<u8>, f64)> {
        let iter = self
            .index
            .iter()
            .filter(|k| score_in_bounds(k.score, &min, &max))
            .map(|k| (k.member.clone(), k.score));

        if rev {
            apply_limit(iter.collect::<Vec<_>>().into_iter().rev(), offset, count)
        } else {
            apply_limit(iter, offset, count)
        }
    }

    /// Number of members whose score is within `[min, max]` (honoring
    /// exclusivity).
    pub fn count_by_score(&self, min: ScoreBound, max: ScoreBound) -> usize {
        self.index
            .iter()
            .filter(|k| score_in_bounds(k.score, &min, &max))
            .count()
    }

    /// Lex range (Redis assumes all members share the same score; ordering is by
    /// member bytes). `rev` returns reverse order. `offset`/`count` as above.
    pub fn range_by_lex(
        &self,
        min: LexBound,
        max: LexBound,
        rev: bool,
        offset: usize,
        count: Option<usize>,
    ) -> Vec<Vec<u8>> {
        let iter = self
            .index
            .iter()
            .filter(|k| lex_in_bounds(&k.member, &min, &max))
            .map(|k| k.member.clone());

        if rev {
            apply_limit(iter.collect::<Vec<_>>().into_iter().rev(), offset, count)
        } else {
            apply_limit(iter, offset, count)
        }
    }

    /// Number of members within the lexicographic range `[min, max]`.
    pub fn count_by_lex(&self, min: LexBound, max: LexBound) -> usize {
        self.index
            .iter()
            .filter(|k| lex_in_bounds(&k.member, &min, &max))
            .count()
    }

    /// Remove and return up to `count` lowest-score members (ascending order).
    pub fn pop_min(&mut self, count: usize) -> Vec<(Vec<u8>, f64)> {
        let victims: Vec<(Vec<u8>, f64)> = self
            .index
            .iter()
            .take(count)
            .map(|k| (k.member.clone(), k.score))
            .collect();
        for (member, score) in &victims {
            self.map.remove(member);
            self.index.remove(&ScoreKey::new(*score, member.clone()));
        }
        victims
    }

    /// Remove and return up to `count` highest-score members (descending order).
    pub fn pop_max(&mut self, count: usize) -> Vec<(Vec<u8>, f64)> {
        let victims: Vec<(Vec<u8>, f64)> = self
            .index
            .iter()
            .rev()
            .take(count)
            .map(|k| (k.member.clone(), k.score))
            .collect();
        for (member, score) in &victims {
            self.map.remove(member);
            self.index.remove(&ScoreKey::new(*score, member.clone()));
        }
        victims
    }

    /// Remove members in the inclusive 0-based (negative-capable) rank range.
    /// Returns the number of members removed.
    pub fn remove_range_by_rank(&mut self, start: i64, stop: i64) -> usize {
        let victims = self.range_by_index(start, stop, false);
        for (member, score) in &victims {
            self.map.remove(member);
            self.index.remove(&ScoreKey::new(*score, member.clone()));
        }
        victims.len()
    }

    /// Remove all members whose score is within `[min, max]` (honoring
    /// exclusivity). Returns the number removed.
    pub fn remove_range_by_score(&mut self, min: ScoreBound, max: ScoreBound) -> usize {
        let victims: Vec<(Vec<u8>, f64)> = self
            .index
            .iter()
            .filter(|k| score_in_bounds(k.score, &min, &max))
            .map(|k| (k.member.clone(), k.score))
            .collect();
        for (member, score) in &victims {
            self.map.remove(member);
            self.index.remove(&ScoreKey::new(*score, member.clone()));
        }
        victims.len()
    }

    /// Remove all members within the lexicographic range `[min, max]`. Returns
    /// the number removed.
    pub fn remove_range_by_lex(&mut self, min: LexBound, max: LexBound) -> usize {
        let victims: Vec<(Vec<u8>, f64)> = self
            .index
            .iter()
            .filter(|k| lex_in_bounds(&k.member, &min, &max))
            .map(|k| (k.member.clone(), k.score))
            .collect();
        for (member, score) in &victims {
            self.map.remove(member);
            self.index.remove(&ScoreKey::new(*score, member.clone()));
        }
        victims.len()
    }

    /// All members in ascending `(score, member)` order (used by `ZSCAN`).
    pub fn entries(&self) -> Vec<(Vec<u8>, f64)> {
        self.index
            .iter()
            .map(|k| (k.member.clone(), k.score))
            .collect()
    }

    /// Random members.
    ///
    /// * `count >= 0`: up to `count` **distinct** members (arbitrary order).
    /// * `count < 0`: exactly `|count|` members **with possible repeats**.
    pub fn random_members(&self, count: i64) -> Vec<(Vec<u8>, f64)> {
        use rand::seq::SliceRandom;
        use rand::Rng;

        if self.is_empty() || count == 0 {
            return Vec::new();
        }

        let pool: Vec<(Vec<u8>, f64)> = self.map.iter().map(|(m, s)| (m.clone(), *s)).collect();
        let mut rng = rand::rng();

        if count > 0 {
            // Distinct: shuffle and take up to `count`.
            let want = (count as usize).min(pool.len());
            let mut pool = pool;
            pool.shuffle(&mut rng);
            pool.truncate(want);
            pool
        } else {
            // With repeats: pick exactly `|count|` independently.
            let want = count.unsigned_abs() as usize;
            let mut out = Vec::with_capacity(want);
            for _ in 0..want {
                let idx = rng.random_range(0..pool.len());
                out.push(pool[idx].clone());
            }
            out
        }
    }

    /// Rebuild the ordered index from the `member -> score` map. Used after
    /// deserialization.
    fn rebuild_index(&mut self) {
        self.index.clear();
        for (member, score) in &self.map {
            self.index.insert(ScoreKey::new(*score, member.clone()));
        }
    }

    /// Construct from a `member -> score` map, building the ordered index.
    fn from_map(map: HashMap<Vec<u8>, f64>) -> Self {
        let mut s = SortedSetData {
            map,
            index: BTreeSet::new(),
        };
        s.rebuild_index();
        s
    }
}

impl Default for SortedSetData {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for SortedSetData {
    fn clone(&self) -> Self {
        SortedSetData {
            map: self.map.clone(),
            index: self.index.clone(),
        }
    }
}

impl std::fmt::Debug for SortedSetData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SortedSetData")
            .field("len", &self.len())
            .finish()
    }
}

// --- Serde: serialize only the member->score map, rebuild the index on load ---

impl Serialize for SortedSetData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Serialize as a sequence of (member_bytes, score) pairs so that binary
        // member keys round-trip through serializers that require string map
        // keys (e.g. JSON) as well as binary formats (e.g. bincode).
        let pairs: Vec<(&Vec<u8>, f64)> = self.map.iter().map(|(m, s)| (m, *s)).collect();
        pairs.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SortedSetData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let pairs = Vec::<(Vec<u8>, f64)>::deserialize(deserializer)?;
        let map: HashMap<Vec<u8>, f64> = pairs.into_iter().collect();
        Ok(SortedSetData::from_map(map))
    }
}

// --- Free helpers --------------------------------------------------------------

/// Normalize a Redis-style inclusive index range against `len`, returning
/// ascending `(lo, hi)` positions, or `None` if the range is empty.
fn normalize_index_range(start: i64, stop: i64, len: usize) -> Option<(usize, usize)> {
    if len == 0 {
        return None;
    }
    let len_i = len as i64;

    let mut lo = if start < 0 { len_i + start } else { start };
    let mut hi = if stop < 0 { len_i + stop } else { stop };

    if lo < 0 {
        lo = 0;
    }
    if hi < 0 {
        hi = 0;
    }
    if hi >= len_i {
        hi = len_i - 1;
    }

    // Empty if start runs past the end or the window is inverted.
    if lo > hi || lo >= len_i {
        return None;
    }

    Some((lo as usize, hi as usize))
}

/// Apply `offset`/`count` to an already-ordered iterator.
fn apply_limit<I, T>(iter: I, offset: usize, count: Option<usize>) -> Vec<T>
where
    I: Iterator<Item = T>,
{
    match count {
        Some(c) => iter.skip(offset).take(c).collect(),
        None => iter.skip(offset).collect(),
    }
}

/// Whether `score` falls within `[min, max]` honoring bound exclusivity.
fn score_in_bounds(score: f64, min: &ScoreBound, max: &ScoreBound) -> bool {
    let lower_ok = match *min {
        ScoreBound::Inclusive(v) => score >= v,
        ScoreBound::Exclusive(v) => score > v,
    };
    let upper_ok = match *max {
        ScoreBound::Inclusive(v) => score <= v,
        ScoreBound::Exclusive(v) => score < v,
    };
    lower_ok && upper_ok
}

/// Whether `member` falls within the lexicographic `[min, max]` honoring bound
/// exclusivity and infinities.
fn lex_in_bounds(member: &[u8], min: &LexBound, max: &LexBound) -> bool {
    let lower_ok = match min {
        LexBound::NegInf => true,
        LexBound::PosInf => false, // nothing is >= +inf
        LexBound::Inclusive(v) => member >= v.as_slice(),
        LexBound::Exclusive(v) => member > v.as_slice(),
    };
    let upper_ok = match max {
        LexBound::PosInf => true,
        LexBound::NegInf => false, // nothing is <= -inf
        LexBound::Inclusive(v) => member <= v.as_slice(),
        LexBound::Exclusive(v) => member < v.as_slice(),
    };
    lower_ok && upper_ok
}

/// Parse a Redis score-range token: `-inf`/`+inf`/`inf`, a plain float
/// (inclusive), or a `(` prefix for exclusive (e.g. `(1.5`). `Err` on malformed.
#[allow(clippy::result_unit_err)]
pub fn parse_score_bound(s: &str) -> Result<ScoreBound, ()> {
    let (exclusive, body) = match s.strip_prefix('(') {
        Some(rest) => (true, rest),
        None => (false, s),
    };

    let value = parse_score_value(body)?;
    if exclusive {
        Ok(ScoreBound::Exclusive(value))
    } else {
        Ok(ScoreBound::Inclusive(value))
    }
}

/// Parse the numeric portion of a score bound, accepting Redis infinity tokens.
fn parse_score_value(body: &str) -> Result<f64, ()> {
    match body.trim() {
        "-inf" => Ok(f64::NEG_INFINITY),
        "+inf" | "inf" => Ok(f64::INFINITY),
        other => {
            let v: f64 = other.parse().map_err(|_| ())?;
            if v.is_nan() {
                Err(())
            } else {
                Ok(v)
            }
        }
    }
}

/// Parse a Redis lex-range token: `-` (NegInf), `+` (PosInf), `[x` (inclusive
/// `x`), `(x` (exclusive `x`). `Err` on malformed (missing `[`/`(` prefix for a
/// value).
#[allow(clippy::result_unit_err)]
pub fn parse_lex_bound(s: &str) -> Result<LexBound, ()> {
    match s {
        "-" => Ok(LexBound::NegInf),
        "+" => Ok(LexBound::PosInf),
        _ => {
            let mut chars = s.chars();
            match chars.next() {
                Some('[') => Ok(LexBound::Inclusive(chars.as_str().as_bytes().to_vec())),
                Some('(') => Ok(LexBound::Exclusive(chars.as_str().as_bytes().to_vec())),
                _ => Err(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    fn members(v: &[(Vec<u8>, f64)]) -> Vec<Vec<u8>> {
        v.iter().map(|(member, _)| member.clone()).collect()
    }

    fn build(pairs: &[(&str, f64)]) -> SortedSetData {
        let mut z = SortedSetData::new();
        for (member, score) in pairs {
            z.insert(m(member), *score);
        }
        z
    }

    #[test]
    fn new_is_empty() {
        let z = SortedSetData::new();
        assert!(z.is_empty());
        assert_eq!(z.len(), 0);
        assert_eq!(z.score(b"x"), None);
    }

    #[test]
    fn default_equals_new() {
        let z = SortedSetData::default();
        assert!(z.is_empty());
    }

    #[test]
    fn insert_new_vs_update() {
        let mut z = SortedSetData::new();
        // New insert returns true.
        assert!(z.insert(m("a"), 1.0));
        assert_eq!(z.len(), 1);
        assert_eq!(z.score(b"a"), Some(1.0));

        // Update returns false, score changes.
        assert!(!z.insert(m("a"), 2.5));
        assert_eq!(z.len(), 1);
        assert_eq!(z.score(b"a"), Some(2.5));

        // Index stays consistent after update: single ordered entry.
        let entries = z.entries();
        assert_eq!(entries, vec![(m("a"), 2.5)]);
    }

    #[test]
    fn insert_same_score_is_noop_update() {
        let mut z = SortedSetData::new();
        assert!(z.insert(m("a"), 1.0));
        assert!(!z.insert(m("a"), 1.0));
        assert_eq!(z.entries(), vec![(m("a"), 1.0)]);
    }

    #[test]
    fn remove() {
        let mut z = build(&[("a", 1.0), ("b", 2.0)]);
        assert!(z.remove(b"a"));
        assert!(!z.remove(b"a"));
        assert_eq!(z.len(), 1);
        assert_eq!(z.entries(), vec![(m("b"), 2.0)]);
    }

    #[test]
    fn tie_break_by_member_bytes() {
        // Equal scores must order by member bytes ascending.
        let z = build(&[("c", 1.0), ("a", 1.0), ("b", 1.0)]);
        assert_eq!(members(&z.entries()), vec![m("a"), m("b"), m("c")]);
    }

    #[test]
    fn ordering_by_score_then_member() {
        let z = build(&[("b", 2.0), ("a", 2.0), ("z", 1.0)]);
        assert_eq!(
            z.entries(),
            vec![(m("z"), 1.0), (m("a"), 2.0), (m("b"), 2.0)]
        );
    }

    #[test]
    fn rank_and_rev_rank() {
        let z = build(&[("a", 1.0), ("b", 2.0), ("c", 3.0)]);
        assert_eq!(z.rank(b"a"), Some(0));
        assert_eq!(z.rank(b"b"), Some(1));
        assert_eq!(z.rank(b"c"), Some(2));
        assert_eq!(z.rank(b"missing"), None);

        assert_eq!(z.rev_rank(b"a"), Some(2));
        assert_eq!(z.rev_rank(b"b"), Some(1));
        assert_eq!(z.rev_rank(b"c"), Some(0));
        assert_eq!(z.rev_rank(b"missing"), None);
    }

    #[test]
    fn rank_with_tie_break() {
        let z = build(&[("a", 1.0), ("b", 1.0), ("c", 1.0)]);
        assert_eq!(z.rank(b"a"), Some(0));
        assert_eq!(z.rank(b"b"), Some(1));
        assert_eq!(z.rank(b"c"), Some(2));
    }

    #[test]
    fn range_by_index_basic() {
        let z = build(&[("a", 1.0), ("b", 2.0), ("c", 3.0), ("d", 4.0)]);
        assert_eq!(
            z.range_by_index(0, 1, false),
            vec![(m("a"), 1.0), (m("b"), 2.0)]
        );
        assert_eq!(
            z.range_by_index(0, -1, false),
            vec![(m("a"), 1.0), (m("b"), 2.0), (m("c"), 3.0), (m("d"), 4.0)]
        );
    }

    #[test]
    fn range_by_index_negatives() {
        let z = build(&[("a", 1.0), ("b", 2.0), ("c", 3.0), ("d", 4.0)]);
        assert_eq!(
            z.range_by_index(-2, -1, false),
            vec![(m("c"), 3.0), (m("d"), 4.0)]
        );
        // start clamps below zero
        assert_eq!(z.range_by_index(-100, 0, false), vec![(m("a"), 1.0)]);
    }

    #[test]
    fn range_by_index_out_of_range() {
        let z = build(&[("a", 1.0), ("b", 2.0)]);
        assert!(z.range_by_index(5, 10, false).is_empty());
        // inverted
        assert!(z.range_by_index(1, 0, false).is_empty());
        // empty set
        let empty = SortedSetData::new();
        assert!(empty.range_by_index(0, -1, false).is_empty());
    }

    #[test]
    fn range_by_index_rev() {
        let z = build(&[("a", 1.0), ("b", 2.0), ("c", 3.0), ("d", 4.0)]);
        // rev over full range -> descending
        assert_eq!(
            z.range_by_index(0, -1, true),
            vec![(m("d"), 4.0), (m("c"), 3.0), (m("b"), 2.0), (m("a"), 1.0)]
        );
        // rev over first two ascending positions (a, b) returns them high->low.
        assert_eq!(
            z.range_by_index(0, 1, true),
            vec![(m("b"), 2.0), (m("a"), 1.0)]
        );
        // rev with negatives: last two positions (c, d) high->low.
        assert_eq!(
            z.range_by_index(-2, -1, true),
            vec![(m("d"), 4.0), (m("c"), 3.0)]
        );
    }

    #[test]
    fn range_by_score_inclusive() {
        let z = build(&[("a", 1.0), ("b", 2.0), ("c", 3.0)]);
        let r = z.range_by_score(
            ScoreBound::Inclusive(1.0),
            ScoreBound::Inclusive(2.0),
            false,
            0,
            None,
        );
        assert_eq!(r, vec![(m("a"), 1.0), (m("b"), 2.0)]);
    }

    #[test]
    fn range_by_score_exclusive() {
        let z = build(&[("a", 1.0), ("b", 2.0), ("c", 3.0)]);
        let r = z.range_by_score(
            ScoreBound::Exclusive(1.0),
            ScoreBound::Exclusive(3.0),
            false,
            0,
            None,
        );
        assert_eq!(r, vec![(m("b"), 2.0)]);
    }

    #[test]
    fn range_by_score_infinities() {
        let z = build(&[("a", -10.0), ("b", 0.0), ("c", 10.0)]);
        let r = z.range_by_score(
            ScoreBound::Inclusive(f64::NEG_INFINITY),
            ScoreBound::Inclusive(f64::INFINITY),
            false,
            0,
            None,
        );
        assert_eq!(members(&r), vec![m("a"), m("b"), m("c")]);
    }

    #[test]
    fn range_by_score_limit_offset() {
        let z = build(&[("a", 1.0), ("b", 2.0), ("c", 3.0), ("d", 4.0)]);
        // offset 1, count 2 over full range
        let r = z.range_by_score(
            ScoreBound::Inclusive(f64::NEG_INFINITY),
            ScoreBound::Inclusive(f64::INFINITY),
            false,
            1,
            Some(2),
        );
        assert_eq!(r, vec![(m("b"), 2.0), (m("c"), 3.0)]);
    }

    #[test]
    fn range_by_score_rev() {
        let z = build(&[("a", 1.0), ("b", 2.0), ("c", 3.0)]);
        let r = z.range_by_score(
            ScoreBound::Inclusive(f64::NEG_INFINITY),
            ScoreBound::Inclusive(f64::INFINITY),
            true,
            0,
            None,
        );
        assert_eq!(r, vec![(m("c"), 3.0), (m("b"), 2.0), (m("a"), 1.0)]);
        // rev with offset/count: skip from start of produced (descending) order
        let r2 = z.range_by_score(
            ScoreBound::Inclusive(f64::NEG_INFINITY),
            ScoreBound::Inclusive(f64::INFINITY),
            true,
            1,
            Some(1),
        );
        assert_eq!(r2, vec![(m("b"), 2.0)]);
    }

    #[test]
    fn count_by_score_works() {
        let z = build(&[("a", 1.0), ("b", 2.0), ("c", 3.0)]);
        assert_eq!(
            z.count_by_score(ScoreBound::Inclusive(1.0), ScoreBound::Inclusive(3.0)),
            3
        );
        assert_eq!(
            z.count_by_score(ScoreBound::Exclusive(1.0), ScoreBound::Inclusive(3.0)),
            2
        );
        assert_eq!(
            z.count_by_score(ScoreBound::Exclusive(1.0), ScoreBound::Exclusive(3.0)),
            1
        );
    }

    #[test]
    fn range_by_lex_brackets() {
        // All same score: pure lex ordering.
        let z = build(&[("a", 0.0), ("b", 0.0), ("c", 0.0), ("d", 0.0)]);
        // [b .. (d  => b, c
        let r = z.range_by_lex(
            LexBound::Inclusive(m("b")),
            LexBound::Exclusive(m("d")),
            false,
            0,
            None,
        );
        assert_eq!(r, vec![m("b"), m("c")]);

        // ( b .. [d => c, d
        let r2 = z.range_by_lex(
            LexBound::Exclusive(m("b")),
            LexBound::Inclusive(m("d")),
            false,
            0,
            None,
        );
        assert_eq!(r2, vec![m("c"), m("d")]);
    }

    #[test]
    fn range_by_lex_infinities() {
        let z = build(&[("a", 0.0), ("b", 0.0), ("c", 0.0)]);
        let r = z.range_by_lex(LexBound::NegInf, LexBound::PosInf, false, 0, None);
        assert_eq!(r, vec![m("a"), m("b"), m("c")]);
    }

    #[test]
    fn range_by_lex_rev_and_limit() {
        let z = build(&[("a", 0.0), ("b", 0.0), ("c", 0.0), ("d", 0.0)]);
        let r = z.range_by_lex(LexBound::NegInf, LexBound::PosInf, true, 0, None);
        assert_eq!(r, vec![m("d"), m("c"), m("b"), m("a")]);

        let r2 = z.range_by_lex(LexBound::NegInf, LexBound::PosInf, false, 1, Some(2));
        assert_eq!(r2, vec![m("b"), m("c")]);
    }

    #[test]
    fn count_by_lex_works() {
        let z = build(&[("a", 0.0), ("b", 0.0), ("c", 0.0)]);
        assert_eq!(z.count_by_lex(LexBound::NegInf, LexBound::PosInf), 3);
        assert_eq!(
            z.count_by_lex(LexBound::Inclusive(m("b")), LexBound::PosInf),
            2
        );
        assert_eq!(
            z.count_by_lex(LexBound::Exclusive(m("a")), LexBound::Exclusive(m("c"))),
            1
        );
    }

    #[test]
    fn pop_min_and_max() {
        let mut z = build(&[("a", 1.0), ("b", 2.0), ("c", 3.0), ("d", 4.0)]);
        assert_eq!(z.pop_min(2), vec![(m("a"), 1.0), (m("b"), 2.0)]);
        assert_eq!(z.len(), 2);
        assert_eq!(z.pop_max(1), vec![(m("d"), 4.0)]);
        assert_eq!(z.len(), 1);
        assert_eq!(z.entries(), vec![(m("c"), 3.0)]);
    }

    #[test]
    fn pop_more_than_len() {
        let mut z = build(&[("a", 1.0)]);
        assert_eq!(z.pop_min(10), vec![(m("a"), 1.0)]);
        assert!(z.is_empty());
        assert!(z.pop_max(5).is_empty());
    }

    #[test]
    fn remove_range_by_rank_count() {
        let mut z = build(&[("a", 1.0), ("b", 2.0), ("c", 3.0), ("d", 4.0)]);
        assert_eq!(z.remove_range_by_rank(1, 2), 2);
        assert_eq!(z.entries(), vec![(m("a"), 1.0), (m("d"), 4.0)]);

        // negative-capable
        let mut z2 = build(&[("a", 1.0), ("b", 2.0), ("c", 3.0)]);
        assert_eq!(z2.remove_range_by_rank(-2, -1), 2);
        assert_eq!(z2.entries(), vec![(m("a"), 1.0)]);

        // out of range removes nothing
        let mut z3 = build(&[("a", 1.0)]);
        assert_eq!(z3.remove_range_by_rank(5, 10), 0);
        assert_eq!(z3.len(), 1);
    }

    #[test]
    fn remove_range_by_score_count() {
        let mut z = build(&[("a", 1.0), ("b", 2.0), ("c", 3.0)]);
        assert_eq!(
            z.remove_range_by_score(ScoreBound::Inclusive(2.0), ScoreBound::Inclusive(3.0)),
            2
        );
        assert_eq!(z.entries(), vec![(m("a"), 1.0)]);
    }

    #[test]
    fn remove_range_by_lex_count() {
        let mut z = build(&[("a", 0.0), ("b", 0.0), ("c", 0.0), ("d", 0.0)]);
        assert_eq!(
            z.remove_range_by_lex(LexBound::Inclusive(m("b")), LexBound::Inclusive(m("c"))),
            2
        );
        assert_eq!(members(&z.entries()), vec![m("a"), m("d")]);
    }

    #[test]
    fn random_members_distinct() {
        let z = build(&[("a", 1.0), ("b", 2.0), ("c", 3.0)]);
        // count >= 0 -> distinct, capped at len
        let r = z.random_members(2);
        assert_eq!(r.len(), 2);
        let mut got = members(&r);
        got.sort();
        got.dedup();
        assert_eq!(got.len(), 2, "members must be distinct");

        // request more than len: capped at len, still distinct
        let r2 = z.random_members(100);
        assert_eq!(r2.len(), 3);
        let mut got2 = members(&r2);
        got2.sort();
        got2.dedup();
        assert_eq!(got2.len(), 3);

        // every returned member must be a real member with correct score
        for (member, score) in &r2 {
            assert_eq!(z.score(member), Some(*score));
        }
    }

    #[test]
    fn random_members_with_repeats() {
        let z = build(&[("a", 1.0), ("b", 2.0)]);
        // count < 0 -> exactly |count|, repeats allowed
        let r = z.random_members(-5);
        assert_eq!(r.len(), 5);
        for (member, score) in &r {
            assert_eq!(z.score(member), Some(*score));
        }
    }

    #[test]
    fn random_members_empty_and_zero() {
        let empty = SortedSetData::new();
        assert!(empty.random_members(3).is_empty());
        assert!(empty.random_members(-3).is_empty());

        let z = build(&[("a", 1.0)]);
        assert!(z.random_members(0).is_empty());
    }

    #[test]
    fn parse_score_bound_happy() {
        assert!(matches!(
            parse_score_bound("1.5"),
            Ok(ScoreBound::Inclusive(v)) if v == 1.5
        ));
        assert!(matches!(
            parse_score_bound("(1.5"),
            Ok(ScoreBound::Exclusive(v)) if v == 1.5
        ));
        assert!(matches!(
            parse_score_bound("-inf"),
            Ok(ScoreBound::Inclusive(v)) if v == f64::NEG_INFINITY
        ));
        assert!(matches!(
            parse_score_bound("+inf"),
            Ok(ScoreBound::Inclusive(v)) if v == f64::INFINITY
        ));
        assert!(matches!(
            parse_score_bound("inf"),
            Ok(ScoreBound::Inclusive(v)) if v == f64::INFINITY
        ));
        assert!(matches!(
            parse_score_bound("(-inf"),
            Ok(ScoreBound::Exclusive(v)) if v == f64::NEG_INFINITY
        ));
        assert!(matches!(
            parse_score_bound("-3"),
            Ok(ScoreBound::Inclusive(v)) if v == -3.0
        ));
    }

    #[test]
    fn parse_score_bound_errors() {
        assert!(parse_score_bound("abc").is_err());
        assert!(parse_score_bound("(abc").is_err());
        assert!(parse_score_bound("").is_err());
        assert!(parse_score_bound("nan").is_err());
        assert!(parse_score_bound("(").is_err());
    }

    #[test]
    fn parse_lex_bound_happy() {
        assert!(matches!(parse_lex_bound("-"), Ok(LexBound::NegInf)));
        assert!(matches!(parse_lex_bound("+"), Ok(LexBound::PosInf)));
        assert!(matches!(
            parse_lex_bound("[abc"),
            Ok(LexBound::Inclusive(v)) if v == b"abc".to_vec()
        ));
        assert!(matches!(
            parse_lex_bound("(abc"),
            Ok(LexBound::Exclusive(v)) if v == b"abc".to_vec()
        ));
        // empty value after prefix is valid (empty member bytes)
        assert!(matches!(
            parse_lex_bound("["),
            Ok(LexBound::Inclusive(v)) if v.is_empty()
        ));
    }

    #[test]
    fn parse_lex_bound_errors() {
        assert!(parse_lex_bound("abc").is_err());
        assert!(parse_lex_bound("").is_err());
        assert!(parse_lex_bound("x").is_err());
    }

    #[test]
    fn serde_round_trip_json() {
        let z = build(&[("c", 3.0), ("a", 1.0), ("b", 2.0)]);
        let json = serde_json::to_string(&z).expect("serialize");
        let back: SortedSetData = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.len(), z.len());
        // Ordering rebuilt correctly.
        assert_eq!(
            back.entries(),
            vec![(m("a"), 1.0), (m("b"), 2.0), (m("c"), 3.0)]
        );
        // Scores preserved.
        assert_eq!(back.score(b"a"), Some(1.0));
        assert_eq!(back.score(b"b"), Some(2.0));
        assert_eq!(back.score(b"c"), Some(3.0));
    }

    #[test]
    fn serde_round_trip_bincode() {
        let z = build(&[("c", 3.0), ("a", 1.0), ("b", 2.0)]);
        let bytes = bincode::serialize(&z).expect("serialize");
        let back: SortedSetData = bincode::deserialize(&bytes).expect("deserialize");

        assert_eq!(
            back.entries(),
            vec![(m("a"), 1.0), (m("b"), 2.0), (m("c"), 3.0)]
        );
    }

    #[test]
    fn serde_round_trip_binary_members() {
        // Non-UTF8 binary members must round-trip.
        let mut z = SortedSetData::new();
        z.insert(vec![0u8, 159, 146, 150], 1.0);
        z.insert(vec![255, 0, 1], 2.0);
        let json = serde_json::to_string(&z).expect("serialize");
        let back: SortedSetData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.score(&[0u8, 159, 146, 150]), Some(1.0));
        assert_eq!(back.score(&[255, 0, 1]), Some(2.0));
    }

    #[test]
    fn clone_is_independent() {
        let mut z = build(&[("a", 1.0), ("b", 2.0)]);
        let c = z.clone();
        z.remove(b"a");
        assert_eq!(c.len(), 2);
        assert_eq!(c.entries(), vec![(m("a"), 1.0), (m("b"), 2.0)]);
    }
}
