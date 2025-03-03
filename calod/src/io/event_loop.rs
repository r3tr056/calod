use std::arch::x86_64::*;
use std::mem::MaybeUninit;
use std::ptr;

#[cfg(target_arch="x86_64")]
mod simd_impl {
	use super::*;

	pub struct SimdIO;

	impl SimdIO {
		pub fn new() -> Self {
			verify_cpu_features();
			SimdIO
		}
	}

	pub mod mem {
		use super::*;

		pub fn fast_copy(dest: &mut [u8], src: &[u8]) {
			assert_eq!(dest.len(), src.len());

			if is_x86_feature_detected!("avx2") {
				unsafe { avx2_copy(dest.as_mut_ptr(), src.as_ptr(), dest.len()) }
			} else {
				fallback_copy(dest, src);
			}
		}

		#[target_feature(enable="avx2")]
		unsafe fn avx2_copy(dest: *mut u8, src: *const u8, len: usize) {
			let mut offset = 0;
			while offset + 32 <= len {
				let data = _mm256_loadu_si256(src.add(offset) as *const __m256i);
				_mm256_storeu_si256(dest.add(offset) as *mut __m256i, data);
				offset += 32;
			}

			// handle remaining bytes
			ptr::copy_nonoverlapping(src.add(offset), dest.add(offset), len - offset);
		}

		fn fallback_copy(dest: &mut [u8], src: &[u8]) {
			dest.copy_from_slice(src);
		}
	}

	// parsing utilities module
	pub mod parse {
		use super::*;

		// find first occurence of byte using SIMD
		pub fn find_byte(haystack: &[u8], needle: u8) -> Option<usize> {
			if haystack.len() < 32 {
				return fallback_find(haystack, needle);
			}

			if is_x86_feature_detected!("avx2") {
				unsafe { avx2_find_byte(haystack, needle) }
			} else {
				fallback_find(haystack, needle)
			}
		}

		#[target_feature(enable="avx2")]
		unsafe fn avx2_find_byte(haystack: &[u8], needle: u8) -> Option<usize> {
			let needle_vec = _mm256_set1_epi8(needle as i8);
			let mut offset = 0;

			while offset + 32 <= haystack.len() {
				let chunk = _mm256_loadu_si256(haystack.as_ptr().add(offset) as *const __m256i);
				let eq = _mm256_cmpeq_epi8(chunk, needle_vec);
				let mask = _mm256_movemask_epi8(eq) as u32;

				if mask != 0 {
					return Some(offset + mask.trailing_zeros() as usize);
				}
				offset += 32;
			}

			fallback_find(&haystack[offset..], needle).map(|pos| offset + pos)
		}

		fn fallback_find(haystack: &[u8], needle: u8) -> Option<usize> {
			haystack.iter().position(|&b| b == needle)
		}
	}

	pub mod tree {
		use super::*;
		const NODE_CAPACITY: usize = 32;

		pub struct BTreeNode<K: Copy + Default, V> {
			keys: [K; NODE_CAPACITY],
			values: [V; NODE_CAPACITY],
			children: [Option<Box<BTreeNode<K, V>>>; NODE_CAPACITY + 1],
			len: u8
		}

		impl<K: Copy + Default + PartialOrd, V> BTreeNode<K, V> {
			pub fn new() -> Self {
				let keys = unsafe {
					let mut arr: [K; NODE_CAPACITY] = MaybeUninit::uninit().assume_init();
					for elem in &mut arr {
						ptr::write(elem, K::default());
					}
					arr
				};

				let values = unsafe {
					let mut arr: [V; NODE_CAPACITY] = MaybeUninit::uninit().assume_init();
					for elem in &mut arr {
						ptr::write(elem, MaybeUninit::uninit().assume_init());
					}
					arr
				};

				BTreeNode {
					keys,
					values,
					children: Default::default(),
					len: 0,
				}
			}

			pub fn find_key(&self, key: &K) -> Option<usize> {
				if std::mem::size_of::<K>() == 4 && is_x86_feature_detected!("avx2") {
					unsafe { self.avx2_search(key) }
				} else {
					self.scalar_search(key)
				}
			}

			#[target_feature(enable = "avx2")]
			unsafe fn avx2_search(&self, key: &K) -> Option<usize> {
				let key_vec = _mm256_set1_epi32(ptr::read_unaligned(key as *const _ as *const i32));
				let mut i = 0;
				
				while i + 8 <= self.len as usize {
					let keys_chunk = _mm256_loadu_si256(
						self.keys.as_ptr().add(i) as *const __m256i
					);
					let cmp = _mm256_cmpgt_epi32(keys_chunk, key_vec);
					let mask = _mm256_movemask_epi8(cmp) as u32;

					if mask != 0 {
						return Some(i + mask.trailing_zeros() as usize / 4);
					}
					i += 8;
				}

				self.scalar_search_range(key, i..self.len as usize)
			}

			fn scalar_search(&self, key: &K) -> Option<usize> {
				self.keys[..self.len as usize]
					.iter()
					.position(|k| k >= key)
			}

			fn scalar_search_range(&self, key: &K, range: std::ops::Range<usize>) -> Option<usize> {
				self.keys[range]
					.iter()
					.position(|k| k >= key)
			}
		}


	}

	fn verify_cpu_features() {
		if !is_x86_feature_detected!("sse2") {
			panic!("CPU does not meet min requirements (SSE2 needed)")
		}
	}
}

#[cfg(not(target_arch = "x86_64"))]
compile_error!("This module is only available for x86_64 targets");

pub use simd_impl::{SimdIO, mem, parse, tree};