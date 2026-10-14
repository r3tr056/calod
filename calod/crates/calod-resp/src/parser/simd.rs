//! SIMD acceleration for RESP parsing

use std::arch::is_x86_feature_detected;

/// Find CRLF in a buffer using the best available method
#[inline(always)]
pub fn find_crlf(buf: &[u8]) -> Option<usize> {
    if is_x86_feature_detected!("avx2") && buf.len() >= 32 {
        unsafe { find_crlf_avx2(buf) }
    } else if is_x86_feature_detected!("sse2") && buf.len() >= 16 {
        unsafe { find_crlf_sse2(buf) }
    } else {
        find_crlf_fallback(buf)
    }
}

/// AVX2-accelerated CRLF search
#[target_feature(enable = "avx2")]
#[cfg(target_arch = "x86_64")]
unsafe fn find_crlf_avx2(buf: &[u8]) -> Option<usize> {
    use std::arch::x86_64::{
        __m256i, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_set1_epi8,
    };

    let mut offset = 0;
    let len = buf.len();

    // Must have enough space for at least one full vector plus one extra byte for the \n
    while offset + 33 <= len {
        let chunk = _mm256_loadu_si256(buf.as_ptr().add(offset) as *const __m256i);
        let cr_mask = _mm256_movemask_epi8(_mm256_cmpeq_epi8(chunk, _mm256_set1_epi8(b'\r' as i8)));

        if cr_mask != 0 {
            // Check each CR position for a following LF
            let mut bit = 1;
            for i in 0..32 {
                if (cr_mask & bit) != 0 && offset + i + 1 < len && buf[offset + i + 1] == b'\n' {
                    return Some(offset + i);
                }
                bit <<= 1;
            }
        }

        offset += 32;
    }

    // Fall back to standard search for remaining bytes
    find_crlf_fallback(&buf[offset..]).map(|pos| offset + pos)
}

/// SSE2-accelerated CRLF search
#[target_feature(enable = "sse2")]
#[cfg(target_arch = "x86_64")]
unsafe fn find_crlf_sse2(buf: &[u8]) -> Option<usize> {
    use std::arch::x86_64::{
        __m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8,
    };

    let mut offset = 0;
    let len = buf.len();

    // Must have enough space for at least one full vector plus one extra byte for the \n
    while offset + 17 <= len {
        let chunk = _mm_loadu_si128(buf.as_ptr().add(offset) as *const __m128i);
        let cr_mask = _mm_movemask_epi8(_mm_cmpeq_epi8(chunk, _mm_set1_epi8(b'\r' as i8)));

        if cr_mask != 0 {
            // Check each CR position for a following LF
            let mut bit = 1;
            for i in 0..16 {
                if (cr_mask & bit) != 0 && offset + i + 1 < len && buf[offset + i + 1] == b'\n' {
                    return Some(offset + i);
                }
                bit <<= 1;
            }
        }

        offset += 16;
    }

    // Fall back to standard search for remaining bytes
    find_crlf_fallback(&buf[offset..]).map(|pos| offset + pos)
}

// NOTE: A hand-written NEON path previously lived here but used a non-existent
// intrinsic (`vcreq_u8`) and a runtime lane index with `vgetq_lane_u8` (which
// requires a const lane), so it never compiled on aarch64 and was never
// dispatched to from `find_crlf`. It has been removed. On non-x86_64 targets
// `find_crlf` uses `find_crlf_fallback`, whose `memchr` backend is itself
// NEON-accelerated on aarch64, so no real throughput is lost.

/// Standard fallback implementation for CRLF search
#[inline(always)]
fn find_crlf_fallback(buf: &[u8]) -> Option<usize> {
    use memchr::memmem;
    let finder = memmem::Finder::new(b"\r\n");
    finder.find(buf)
}
