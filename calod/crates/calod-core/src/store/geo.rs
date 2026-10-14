//! Geohash encoding/decoding, great-circle distance, and radius search support
//! backing the Redis GEO command family (`GEOADD`, `GEOPOS`, `GEODIST`,
//! `GEOHASH`, `GEOSEARCH`, …).
//!
//! # How GEO maps onto sorted sets
//!
//! Redis does not have a dedicated geo type. Instead, GEO commands store points
//! as members of an ordinary **sorted set** whose score is a 52-bit geohash
//! integer reinterpreted as an [`f64`]. A 52-bit integer is exactly
//! representable in an `f64` mantissa, so the geohash survives the round trip
//! through the sorted-set score with no loss.
//!
//! The 52-bit integer is produced by [`encode`]: longitude is normalized into
//! `[0, 1)` over the Redis GEO longitude range and latitude over the (clamped)
//! GEO latitude range, the top [`GEO_STEP`] = 26 bits of each are taken, and the
//! two 26-bit values are *bit-interleaved* — latitude bits in the even
//! positions, longitude bits in the odd positions — to form a single 52-bit
//! Morton code. [`decode`] reverses this, returning the **center** of the
//! addressed cell.
//!
//! # Ranges and the world latitude quirk
//!
//! Internally GEO clamps latitude to `±85.05112878°` (the Web-Mercator limit),
//! which keeps the lon/lat cells roughly square. The user-visible `GEOHASH`
//! command, however, re-encodes against the *world* latitude range `[-90, 90]`
//! before emitting the 11-character base32 string — see [`geohash_string`] — so
//! its output matches the documented Redis values.
//!
//! # Radius search
//!
//! [`search_ranges`] mirrors `geohashGetAreasByRadius`: it estimates the geohash
//! `step` (bits per coordinate) whose cell comfortably covers the requested
//! radius, then returns the inclusive 52-bit score ranges for that center cell
//! plus its eight neighbours. A caller scans the sorted set for members whose
//! score lands in any returned range, then filters survivors with an exact
//! [`haversine_m`] distance check.
//!
//! This module is self-contained and depends only on `std`.

/// Minimum valid longitude (degrees) for Redis GEO.
pub const GEO_LON_MIN: f64 = -180.0;
/// Maximum valid longitude (degrees) for Redis GEO.
pub const GEO_LON_MAX: f64 = 180.0;
/// Minimum valid latitude (degrees) for Redis GEO (Web-Mercator clamp).
pub const GEO_LAT_MIN: f64 = -85.05112878;
/// Maximum valid latitude (degrees) for Redis GEO (Web-Mercator clamp).
pub const GEO_LAT_MAX: f64 = 85.05112878;
/// Bits of precision per coordinate. The interleaved score is `2 * GEO_STEP`
/// = 52 bits, which is exactly representable in an `f64`.
pub const GEO_STEP: u8 = 26;

/// Earth radius in meters used by Redis for haversine distance.
const EARTH_RADIUS_M: f64 = 6372797.560856;

/// World latitude range used only by the `GEOHASH` string command.
const GEOHASH_LAT_MIN: f64 = -90.0;
const GEOHASH_LAT_MAX: f64 = 90.0;

/// The base32 alphabet Redis uses for the human-readable geohash string.
const GEOALPHABET: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";

/// Spread the low 26 bits of `v` so that bit `i` of `v` lands at bit `2*i` of
/// the result (i.e. into the even positions), leaving the odd positions zero.
///
/// This is the 32-bit "Morton spread" used by Redis's `interleave64`.
fn spread_low26(v: u32) -> u64 {
    let mut x = v as u64 & 0x03FF_FFFF; // keep 26 bits
    x = (x | (x << 16)) & 0x0000_FFFF_0000_FFFF;
    x = (x | (x << 8)) & 0x00FF_00FF_00FF_00FF;
    x = (x | (x << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
    x = (x | (x << 2)) & 0x3333_3333_3333_3333;
    x = (x | (x << 1)) & 0x5555_5555_5555_5555;
    x
}

/// Inverse of [`spread_low26`]: gather the even-position bits of `x` back into
/// the low 26 bits of a `u32`.
fn squash_even(mut x: u64) -> u32 {
    x &= 0x5555_5555_5555_5555;
    x = (x | (x >> 1)) & 0x3333_3333_3333_3333;
    x = (x | (x >> 2)) & 0x0F0F_0F0F_0F0F_0F0F;
    x = (x | (x >> 4)) & 0x00FF_00FF_00FF_00FF;
    x = (x | (x >> 8)) & 0x0000_FFFF_0000_FFFF;
    x = (x | (x >> 16)) & 0x0000_0000_FFFF_FFFF;
    (x & 0x03FF_FFFF) as u32
}

/// Interleave two 26-bit integers into a 52-bit value, with the bits of `lat`
/// occupying the even positions and the bits of `lon` the odd positions —
/// matching Redis's `interleave64(ilato, ilono)`.
fn interleave(lat_bits: u32, lon_bits: u32) -> u64 {
    spread_low26(lat_bits) | (spread_low26(lon_bits) << 1)
}

/// Deinterleave a 52-bit value into `(lat_bits, lon_bits)`, the inverse of
/// [`interleave`].
fn deinterleave(bits: u64) -> (u32, u32) {
    let lat_bits = squash_even(bits);
    let lon_bits = squash_even(bits >> 1);
    (lat_bits, lon_bits)
}

/// Take the top 26 bits of `value`'s position within `[min, max)`, returning the
/// integer cell index in `0..2^26`. The result is clamped to a valid index even
/// for `value == max`.
fn coord_to_bits(value: f64, min: f64, max: f64) -> u32 {
    let frac = (value - min) / (max - min);
    let scaled = frac * (1u64 << GEO_STEP) as f64;
    let idx = scaled as i64;
    let max_idx = (1i64 << GEO_STEP) - 1;
    idx.clamp(0, max_idx) as u32
}

/// Center of the cell addressed by 26-bit `idx` within `[min, max)`.
fn bits_to_center(idx: u32, min: f64, max: f64) -> f64 {
    let cells = (1u64 << GEO_STEP) as f64;
    let lo = min + (idx as f64 / cells) * (max - min);
    let hi = min + ((idx as f64 + 1.0) / cells) * (max - min);
    (lo + hi) / 2.0
}

/// Interleave 26-bit lon + 26-bit lat into a 52-bit score.
///
/// Returns `None` if `lon` or `lat` falls outside the valid Redis GEO range
/// (`[-180, 180]` for longitude, `[-85.05112878, 85.05112878]` for latitude),
/// or if either is not finite.
pub fn encode(lon: f64, lat: f64) -> Option<u64> {
    if !lon.is_finite() || !lat.is_finite() {
        return None;
    }
    if !(GEO_LON_MIN..=GEO_LON_MAX).contains(&lon) || !(GEO_LAT_MIN..=GEO_LAT_MAX).contains(&lat) {
        return None;
    }
    let lon_bits = coord_to_bits(lon, GEO_LON_MIN, GEO_LON_MAX);
    let lat_bits = coord_to_bits(lat, GEO_LAT_MIN, GEO_LAT_MAX);
    Some(interleave(lat_bits, lon_bits))
}

/// Decode a 52-bit score back to the `(lon, lat)` of the cell **center**.
///
/// This is the inverse of [`encode`] to within cell precision (about
/// `360 / 2^26 ≈ 5.4e-6` degrees of longitude).
pub fn decode(bits: u64) -> (f64, f64) {
    let (lat_bits, lon_bits) = deinterleave(bits);
    let lon = bits_to_center(lon_bits, GEO_LON_MIN, GEO_LON_MAX);
    let lat = bits_to_center(lat_bits, GEO_LAT_MIN, GEO_LAT_MAX);
    (lon, lat)
}

/// Great-circle distance in **meters** between two lon/lat points using the
/// haversine formula and an earth radius of `6372797.560856 m` (matching
/// Redis).
pub fn haversine_m(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    let lat1r = lat1.to_radians();
    let lat2r = lat2.to_radians();
    let u = ((lat2r - lat1r) / 2.0).sin();
    let v = ((lon2 - lon1).to_radians() / 2.0).sin();
    let a = u * u + lat1r.cos() * lat2r.cos() * v * v;
    2.0 * EARTH_RADIUS_M * a.sqrt().asin()
}

/// The standard 11-character base32 geohash string emitted by the Redis
/// `GEOHASH` command.
///
/// Redis quirk: this re-encodes against the **world** latitude range
/// `[-90, 90]` (not the clamped GEO range) before producing the base32 string,
/// using the alphabet `"0123456789bcdefghjkmnpqrstuvwxyz"`. Inputs are clamped
/// into their valid ranges so the function always returns 11 characters.
pub fn geohash_string(lon: f64, lat: f64) -> String {
    let lon = lon.clamp(GEO_LON_MIN, GEO_LON_MAX);
    let lat = lat.clamp(GEOHASH_LAT_MIN, GEOHASH_LAT_MAX);

    // Re-encode at full 26-bit precision against the world latitude range.
    let lon_bits = coord_to_bits(lon, GEO_LON_MIN, GEO_LON_MAX) as u64;
    let lat_bits = coord_to_bits(lat, GEOHASH_LAT_MIN, GEOHASH_LAT_MAX) as u64;
    let mut bits = interleave(lat_bits as u32, lon_bits as u32);

    // The 52-bit value is left-aligned into a 55-bit field (11 * 5) so that the
    // first base32 char carries the most-significant 5 bits, then any trailing
    // bits beyond 52 are zero. Redis shifts the 52-bit value left by 3.
    bits <<= 55 - (2 * GEO_STEP as u32); // 55 - 52 = 3

    let mut out = String::with_capacity(11);
    for i in 0..11u32 {
        // Most-significant 5-bit group first.
        let shift = 50 - i * 5;
        let idx = ((bits >> shift) & 0x1F) as usize;
        out.push(GEOALPHABET[idx] as char);
    }
    out
}

/// Estimate the geohash `step` (bits per coordinate, `1..=26`) whose cell is at
/// least as large as `radius_m` near the given `lat`, following Redis's
/// `geohashEstimateStepsByRadius`.
fn estimate_steps_by_radius(radius_m: f64, lat: f64) -> u8 {
    if radius_m == 0.0 {
        return 26;
    }
    let mut step: i32 = 1;
    let mut r = radius_m;
    while r < MERCATOR_MAX {
        r *= 2.0;
        step += 1;
    }
    // Each step doubled the radius; back off so the cell comfortably exceeds the
    // requested radius. Mirrors Redis's constant adjustments.
    step -= 2;

    // Near the poles, longitudinal cells shrink, so widen the search.
    let alat = lat.abs();
    if alat > 66.0 {
        step -= 1;
        if alat > 80.0 {
            step -= 1;
        }
    }

    step.clamp(1, 26) as u8
}

/// Half the circumference of the Mercator projection in meters — the scale at
/// which a step-1 cell spans the whole map, used by [`estimate_steps_by_radius`].
const MERCATOR_MAX: f64 = 20037726.37;

/// A geohash cell identified by its `(lat_idx, lon_idx)` prefix at a given
/// `step` (bits per coordinate).
#[derive(Clone, Copy)]
struct Cell {
    lat_idx: u32,
    lon_idx: u32,
    step: u8,
}

impl Cell {
    /// The inclusive 52-bit score range `[min, max]` covered by this cell.
    fn score_range(&self) -> (u64, u64) {
        let step = self.step as u32;
        let prefix = interleave_step(self.lat_idx, self.lon_idx, step);
        let shift = 52 - 2 * step;
        let min = prefix << shift;
        let max = ((prefix + 1) << shift) - 1;
        (min, max)
    }
}

/// Interleave `step`-bit lat/lon indices into a `2*step`-bit prefix (lat in even
/// positions, lon in odd positions), consistent with [`interleave`].
fn interleave_step(lat_idx: u32, lon_idx: u32, step: u32) -> u64 {
    let mask = if step >= 32 {
        u32::MAX
    } else {
        (1u32 << step) - 1
    };
    interleave(lat_idx & mask, lon_idx & mask)
}

/// Inclusive 52-bit score ranges covering a circle of `radius_m` around
/// `(lon, lat)`.
///
/// The function picks a geohash `step` whose cell is at least `radius_m` across,
/// computes the center cell at that step, and returns the score ranges for that
/// cell and its eight neighbours (N, S, E, W, NE, NW, SE, SW). Longitude wraps
/// around the antimeridian; neighbours whose latitude index falls outside the
/// valid range are dropped. Adjacent ranges are merged where possible, so the
/// result holds at most nine entries and never fewer than one.
///
/// Each range is expressed at the full 52-bit score scale: a cell prefix `p`
/// with `2*step` bits maps to `[p << (52 - 2*step), ((p+1) << (52 - 2*step)) - 1]`.
/// Callers scan the sorted set for members whose score lands in any range, then
/// filter by an exact `haversine_m(..) <= radius_m` test.
pub fn search_ranges(lon: f64, lat: f64, radius_m: f64) -> Vec<(u64, u64)> {
    let lon = lon.clamp(GEO_LON_MIN, GEO_LON_MAX);
    let lat = lat.clamp(GEO_LAT_MIN, GEO_LAT_MAX);

    let step = estimate_steps_by_radius(radius_m.max(0.0), lat);
    let step_u = step as u32;
    let cells = 1u32 << step_u; // number of cells per axis at this step
    let max_idx = cells - 1;

    // Center cell indices at this step: take the top `step` bits of the full
    // 26-bit coordinate index.
    let shift = GEO_STEP as u32 - step_u;
    let center_lat = coord_to_bits(lat, GEO_LAT_MIN, GEO_LAT_MAX) >> shift;
    let center_lon = coord_to_bits(lon, GEO_LON_MIN, GEO_LON_MAX) >> shift;

    let mut ranges: Vec<(u64, u64)> = Vec::with_capacity(9);

    // Neighbour offsets: (d_lon, d_lat) for the 3x3 block centered on the cell.
    for d_lat in [-1i32, 0, 1] {
        for d_lon in [-1i32, 0, 1] {
            // Latitude does not wrap: drop cells off the top/bottom of the map.
            let lat_i = center_lat as i64 + d_lat as i64;
            if lat_i < 0 || lat_i > max_idx as i64 {
                continue;
            }
            // Longitude wraps around the antimeridian.
            let lon_i = (center_lon as i64 + d_lon as i64).rem_euclid(cells as i64);

            let cell = Cell {
                lat_idx: lat_i as u32,
                lon_idx: lon_i as u32,
                step,
            };
            ranges.push(cell.score_range());
        }
    }

    merge_ranges(&mut ranges);
    ranges
}

/// Sort `ranges` and coalesce overlapping or directly adjacent intervals in
/// place.
fn merge_ranges(ranges: &mut Vec<(u64, u64)>) {
    if ranges.len() <= 1 {
        return;
    }
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for &(min, max) in ranges.iter() {
        if let Some(last) = merged.last_mut() {
            // Adjacent when `min == last.1 + 1`; guard against overflow.
            if min <= last.1 || (last.1 != u64::MAX && min == last.1 + 1) {
                if max > last.1 {
                    last.1 = max;
                }
                continue;
            }
        }
        merged.push((min, max));
    }
    *ranges = merged;
}

#[cfg(test)]
mod tests {
    use super::*;

    // Well-known reference points (from the Redis GEO documentation).
    const PALERMO: (f64, f64) = (13.361389, 38.115556);
    const CATANIA: (f64, f64) = (15.087269, 37.502669);

    fn assert_close(a: f64, b: f64, tol: f64, what: &str) {
        assert!(
            (a - b).abs() <= tol,
            "{what}: {a} not within {tol} of {b} (diff {})",
            (a - b).abs()
        );
    }

    #[test]
    fn encode_decode_round_trip_cities() {
        for &(name, (lon, lat)) in &[("Palermo", PALERMO), ("Catania", CATANIA)] {
            let bits = encode(lon, lat).expect("in range");
            let (dlon, dlat) = decode(bits);
            assert_close(dlon, lon, 0.001, &format!("{name} lon"));
            assert_close(dlat, lat, 0.001, &format!("{name} lat"));
        }
    }

    #[test]
    fn encode_decode_round_trip_extremes_and_origin() {
        for &(lon, lat) in &[
            (0.0, 0.0),
            (-180.0, -85.05112878),
            (180.0, 85.05112878),
            (-122.4194, 37.7749), // San Francisco
            (139.6917, 35.6895),  // Tokyo
        ] {
            let bits = encode(lon, lat).expect("in range");
            let (dlon, dlat) = decode(bits);
            assert_close(dlon, lon, 0.01, "lon");
            assert_close(dlat, lat, 0.01, "lat");
        }
    }

    #[test]
    fn encode_rejects_out_of_range() {
        assert!(encode(0.0, 90.0).is_none(), "lat 90 out of GEO range");
        assert!(encode(0.0, -90.0).is_none(), "lat -90 out of GEO range");
        assert!(encode(200.0, 0.0).is_none(), "lon 200 out of range");
        assert!(encode(-200.0, 0.0).is_none(), "lon -200 out of range");
        assert!(encode(f64::NAN, 0.0).is_none(), "NaN lon");
        assert!(encode(0.0, f64::INFINITY).is_none(), "inf lat");
    }

    #[test]
    fn encode_accepts_range_boundaries() {
        assert!(encode(180.0, 85.05112878).is_some());
        assert!(encode(-180.0, -85.05112878).is_some());
    }

    #[test]
    fn haversine_palermo_catania() {
        let d = haversine_m(PALERMO.0, PALERMO.1, CATANIA.0, CATANIA.1);
        // Redis reports ~166274.15 m for this pair.
        assert_close(d, 166274.0, 166274.0 * 0.01, "Palermo<->Catania");
    }

    #[test]
    fn haversine_zero_for_same_point() {
        let d = haversine_m(PALERMO.0, PALERMO.1, PALERMO.0, PALERMO.1);
        assert!(d < 1e-6, "distance to self should be ~0, got {d}");
    }

    #[test]
    fn haversine_is_symmetric() {
        let a = haversine_m(PALERMO.0, PALERMO.1, CATANIA.0, CATANIA.1);
        let b = haversine_m(CATANIA.0, CATANIA.1, PALERMO.0, PALERMO.1);
        assert_close(a, b, 1e-6, "symmetry");
    }

    #[test]
    fn geohash_string_palermo() {
        let h = geohash_string(PALERMO.0, PALERMO.1);
        assert_eq!(h.len(), 11, "geohash must be 11 chars, got {h:?}");
        // Redis documents "sqc8b49rny0"; be tolerant of the last chars.
        assert!(
            h.starts_with("sqc8b49"),
            "geohash {h:?} should start with sqc8b49"
        );
    }

    #[test]
    fn geohash_string_catania() {
        let h = geohash_string(CATANIA.0, CATANIA.1);
        assert_eq!(h.len(), 11);
        // Redis documents "sqdtr74hyu0".
        assert!(
            h.starts_with("sqdtr74"),
            "geohash {h:?} should start with sqdtr74"
        );
    }

    #[test]
    fn geohash_string_uses_only_alphabet() {
        let h = geohash_string(PALERMO.0, PALERMO.1);
        for c in h.bytes() {
            assert!(
                GEOALPHABET.contains(&c),
                "char {} not in geohash alphabet",
                c as char
            );
        }
    }

    #[test]
    fn search_ranges_contains_center_score() {
        let (lon, lat) = PALERMO;
        let score = encode(lon, lat).expect("in range");
        let ranges = search_ranges(lon, lat, 1000.0);
        assert!(!ranges.is_empty(), "expected at least one range");
        assert!(ranges.len() <= 9, "at most 9 ranges, got {}", ranges.len());
        assert!(
            ranges
                .iter()
                .any(|&(min, max)| score >= min && score <= max),
            "center score {score} not covered by any range {ranges:?}"
        );
    }

    #[test]
    fn search_ranges_merged_and_sorted() {
        let ranges = search_ranges(PALERMO.0, PALERMO.1, 5000.0);
        for w in ranges.windows(2) {
            assert!(w[0].0 <= w[1].0, "ranges not sorted: {ranges:?}");
            // After merging, consecutive ranges must not touch or overlap.
            assert!(
                w[0].1 < w[1].0,
                "ranges overlap/adjacent after merge: {ranges:?}"
            );
        }
        for &(min, max) in &ranges {
            assert!(min <= max, "range min>max: {min}..{max}");
        }
    }

    #[test]
    fn search_ranges_neighbors_also_covered_for_small_radius() {
        // For a tiny radius the step is fine and nearby members should still be
        // discoverable: members within the radius must fall inside some range.
        let (lon, lat) = CATANIA;
        let ranges = search_ranges(lon, lat, 2000.0);
        // A point ~500m east should be encodable and covered.
        let near = encode(lon + 0.006, lat).expect("in range");
        assert!(
            ranges.iter().any(|&(min, max)| near >= min && near <= max),
            "nearby point not covered by search ranges"
        );
    }

    #[test]
    fn estimate_steps_monotonic_in_radius() {
        // Larger radius => coarser (smaller) step.
        let s_small = estimate_steps_by_radius(100.0, 0.0);
        let s_large = estimate_steps_by_radius(100_000.0, 0.0);
        assert!(
            s_large <= s_small,
            "expected coarser step for larger radius: {s_small} vs {s_large}"
        );
        assert!((1..=26).contains(&s_small));
        assert!((1..=26).contains(&s_large));
    }

    #[test]
    fn interleave_round_trips() {
        let lat = 0x02AB_CDEF & 0x03FF_FFFF;
        let lon = 0x0155_4321 & 0x03FF_FFFF;
        let bits = interleave(lat, lon);
        let (dlat, dlon) = deinterleave(bits);
        assert_eq!(dlat, lat);
        assert_eq!(dlon, lon);
    }

    #[test]
    fn antimeridian_wrap_produces_valid_ranges() {
        // Near the antimeridian, longitude neighbours must wrap, not panic.
        let ranges = search_ranges(179.999, 0.0, 50_000.0);
        assert!(!ranges.is_empty());
        for &(min, max) in &ranges {
            assert!(min <= max);
            assert!(max < (1u64 << 52), "range exceeds 52-bit scale");
        }
    }
}
