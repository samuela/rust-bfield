use std::cmp::Ordering;
#[cfg(feature = "legacy")]
use std::fs::File;
#[cfg(feature = "prefetching")]
use std::intrinsics;
use std::io;
use std::path::Path;

use bincode::{deserialize, serialize, Infinite};
use mmap_bitvec::combinatorial::{rank, unrank};
use mmap_bitvec::{BitVector, MmapBitVec};
use murmurhash3::murmurhash3_x64_128;
use serde::de::DeserializeOwned;
use serde::Serialize;
#[cfg(feature = "legacy")]
use serde_json;

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct BFieldParams<T> {
    n_hashes: u8,      // k
    marker_width: u8,  // nu
    n_marker_bits: u8, // kappa
    pub(crate) other: Option<T>,
}

pub(crate) struct BFieldMember<T> {
    bitvec: MmapBitVec,
    pub(crate) params: BFieldParams<T>,
}

pub type BFieldVal = u32;
const BF_MAGIC: [u8; 2] = [0xBF, 0x1D];

#[derive(Debug, PartialEq)]
pub(crate) enum BFieldLookup {
    Indeterminate,
    Some(BFieldVal),
    None,
}

impl<T: Clone + DeserializeOwned + Serialize> BFieldMember<T> {
    pub fn create<P>(
        filename: P,
        size: usize,
        n_hashes: u8,
        marker_width: u8,
        n_marker_bits: u8,
        other_params: Option<T>,
    ) -> Result<Self, io::Error>
    where
        P: AsRef<Path>,
    {
        let bf_params = BFieldParams {
            n_hashes,
            marker_width,
            n_marker_bits,
            other: other_params,
        };

        let header: Vec<u8> = serialize(&bf_params, Infinite).unwrap();
        let bv = MmapBitVec::create(filename, size, BF_MAGIC, &header)?;

        Ok(BFieldMember {
            bitvec: bv,
            params: bf_params,
        })
    }

    pub fn open<P>(filename: P, read_only: bool) -> Result<Self, io::Error>
    where
        P: AsRef<Path>,
    {
        let bv = MmapBitVec::open(filename, Some(&BF_MAGIC), read_only)?;
        let bf_params: BFieldParams<T> = {
            let header = bv.header();
            deserialize(&header[..]).unwrap()
        };

        Ok(BFieldMember {
            bitvec: bv,
            params: bf_params,
        })
    }

    #[cfg(feature = "legacy")]
    pub fn open_legacy<P>(filename: P) -> Result<Self, io::Error>
    where
        P: AsRef<Path>,
    {
        // parse out all the bfield information we need from the params file
        // `bfield.params` is a JSON blob with the following format:
        // [%result.capacity, %result.root_filename, %result.bits_per_element,
        // %result.k, %result.nu, %result.kappa, %result.beta,
        // %result.n_secondaries, %result.max_value, %result.max_scaledown, %result.use_chunks]
        let params_path = Path::with_extension(filename.as_ref(), "params");
        let params_file = File::open(params_path)?;
        let params: serde_json::Value = serde_json::from_reader(params_file).unwrap();
        let bf_params = BFieldParams {
            n_hashes: params.get(3).unwrap().as_u64().unwrap() as u8, // k
            marker_width: params.get(4).unwrap().as_u64().unwrap() as u8, // nu
            n_marker_bits: params.get(5).unwrap().as_u64().unwrap() as u8, // kappa
            other: None,
        };
        // finally, open the bfield itself
        let bv = MmapBitVec::open_no_header(filename, 8)?;

        Ok(BFieldMember {
            bitvec: bv,
            params: bf_params,
        })
    }

    pub fn in_memory(
        size: usize,
        n_hashes: u8,
        marker_width: u8,
        n_marker_bits: u8,
    ) -> Result<Self, io::Error> {
        let bf_params = BFieldParams {
            n_hashes,
            marker_width,
            n_marker_bits,
            other: None,
        };

        let bv = MmapBitVec::from_memory(size)?;

        Ok(BFieldMember {
            bitvec: bv,
            params: bf_params,
        })
    }

    pub fn insert(&mut self, key: &[u8], value: BFieldVal) {
        // TODO: need to do a check that `value` < allowable range based on
        // self.params.marker_width and self.params.n_marker_bits
        let k = self.params.n_marker_bits;
        self.insert_raw(key, rank(value as usize, k));
    }

    #[inline]
    fn insert_raw(&mut self, key: &[u8], marker: u128) {
        let marker_width = self.params.marker_width as usize;
        let hash = murmurhash3_x64_128(key, 0);
        let aligned_marker = align_bits(marker, marker_width);

        for marker_ix in 0usize..self.params.n_hashes as usize {
            let pos = marker_pos(hash, marker_ix, self.bitvec.size(), marker_width);
            self.bitvec
                .set_range(pos..pos + marker_width, aligned_marker);
        }
    }

    /// "Removes" a key from the b-field by flipping an extra bit to make it
    /// indeterminate. Use this with caution because it can make other keys
    /// indeterminate by saturating the b-field with ones.
    ///
    /// Returns `true` if the value was inserted or was already present with
    /// the correct value; `false` if masking occured or if it was already
    /// indeterminate.
    pub fn mask_or_insert(&mut self, key: &[u8], value: BFieldVal) -> bool {
        let correct_marker = rank(value as usize, self.params.n_marker_bits);
        let k = u32::from(self.params.n_marker_bits);
        let existing_marker = self.get_raw(key, k);

        match existing_marker.count_ones().cmp(&k) {
            Ordering::Greater => false, // already indeterminate
            Ordering::Equal => {
                // value already in b-field, but is it correct?
                if existing_marker == correct_marker {
                    return true;
                }
                // try to find a new, invalid marker that has an extra
                // bit over the existing marker so that it'll become
                // indeterminate once we overwrite it
                let mut pos = 0;
                let mut new_marker = existing_marker;
                while new_marker.count_ones() == k {
                    new_marker = existing_marker | (1 << pos);
                    pos += 1;
                }
                // mask out the existing!
                self.insert_raw(key, new_marker);
                false
            }
            Ordering::Less => {
                // nothing present; insert the value
                self.insert_raw(key, correct_marker);
                true
            }
        }
    }

    #[inline]
    pub fn get(&self, key: &[u8]) -> BFieldLookup {
        let k = u32::from(self.params.n_marker_bits);
        let putative_marker = self.get_raw(key, k);
        match putative_marker.count_ones().cmp(&k) {
            Ordering::Greater => BFieldLookup::Indeterminate,
            Ordering::Equal => BFieldLookup::Some(unrank(putative_marker) as u32),
            Ordering::Less => BFieldLookup::None,
        }
    }

    #[inline]
    fn get_raw(&self, key: &[u8], k: u32) -> u128 {
        let marker_width = self.params.marker_width as usize;
        let hash = murmurhash3_x64_128(key, 0);
        let mut merged_marker = u128::max_value();
        let mut positions: [usize; 16] = [0; 16]; // support up to 16 hashes
        for marker_ix in 0usize..self.params.n_hashes as usize {
            let pos = marker_pos(hash, marker_ix, self.bitvec.size(), marker_width);
            positions[marker_ix] = pos;

            // pre-fetch the memory!
            if cfg!(feature = "prefetching") {
                unsafe {
                    let byte_idx_st = (pos >> 3) as usize;
                    #[allow(unused_variables)]
                    let ptr: *const u8 = self.bitvec.mmap.as_ptr().add(byte_idx_st);
                    #[cfg(feature = "prefetching")]
                    intrinsics::prefetch_read_data(ptr, 3);
                }
            }
        }

        assert!(self.params.n_hashes <= 16);
        for pos in positions.iter().take(self.params.n_hashes as usize) {
            let marker = self.bitvec.get_range(*pos..*pos + marker_width);
            merged_marker &= marker;
            if merged_marker.count_ones() < k {
                return 0;
            }
        }
        align_bits(merged_marker, marker_width)
    }

    pub fn info(&self) -> (usize, u8, u8, u8) {
        (
            self.bitvec.size(),
            self.params.n_hashes,
            self.params.marker_width,
            self.params.n_marker_bits,
        )
    }
}

#[cfg(not(feature = "legacy"))]
#[inline]
fn align_bits(b: u128, _: usize) -> u128 {
    // everything is normal if we're not in legacy mode (this is a noop)
    b
}

#[cfg(feature = "legacy")]
#[inline]
fn align_bits(b: u128, len: usize) -> u128 {
    // we need to reverse the bits (everything is backwards at both the byte
    // and the marker level in the existing nim implementation)
    let mut new_b = 0u128;
    for i in 0..len {
        new_b |= (b & (1 << (len - i - 1))) >> (len - i - 1) << i;
    }
    new_b
}

#[cfg(feature = "legacy")]
#[test]
fn test_align_bits() {
    assert_eq!(align_bits(0b0011, 4), 0b1100);
    assert_eq!(align_bits(0b10011, 5), 0b11001);
}

#[inline]
#[cfg(not(feature = "legacy"))]
fn marker_pos(hash: (u64, u64), n: usize, total_size: usize, marker_size: usize) -> usize {
    ((hash.0 as usize).wrapping_add(n.wrapping_mul(hash.1 as usize))) % (total_size - marker_size)
}

#[inline]
#[cfg(feature = "legacy")]
fn marker_pos(hash: (u64, u64), n: usize, total_size: usize, _: usize) -> usize {
    // this should be bit-wise the same as nim-bfield
    let mashed_hash = (hash.0 as i64)
        .overflowing_add((n as i64).overflowing_mul(hash.1 as i64).0)
        .0;
    // note that the nim implementation always uses a marker width of 64 here
    i64::abs(mashed_hash % (total_size as i64 - 64)) as usize
}

#[test]
fn test_bfield() {
    let mut bfield: BFieldMember<usize> = BFieldMember::in_memory(1024, 3, 64, 4).unwrap();
    // check that inserting keys adds new entries
    bfield.insert(b"test", 2);
    assert_eq!(bfield.get(b"test"), BFieldLookup::Some(2));

    bfield.insert(b"test2", 106);
    assert_eq!(bfield.get(b"test2"), BFieldLookup::Some(106));

    // test3 was never added
    assert_eq!(bfield.get(b"test3"), BFieldLookup::None);
}

#[test]
fn test_bfield_collisions() {
    // comically small bfield with too many (16) hashes
    // and too many bits (8) to cause saturation
    let mut bfield: BFieldMember<usize> = BFieldMember::in_memory(128, 16, 64, 8).unwrap();

    bfield.insert(b"test", 100);
    assert_eq!(bfield.get(b"test"), BFieldLookup::Indeterminate);
}

#[test]
fn test_bfield_bits_set() {
    let mut bfield: BFieldMember<usize> = BFieldMember::in_memory(128, 2, 16, 4).unwrap();

    bfield.insert(b"test", 100);
    assert_eq!(bfield.bitvec.rank(0..128), 8);
    bfield.insert(b"test2", 200);
    assert_eq!(bfield.bitvec.rank(0..128), 16);
    bfield.insert(b"test3", 300);
    assert!(bfield.bitvec.rank(0..128) < 24); // 23 bits set
}

#[test]
fn test_bfield_mask_or_insert() {
    let mut bfield: BFieldMember<usize> = BFieldMember::in_memory(1024, 2, 16, 4).unwrap();

    bfield.insert(b"test", 2);
    assert_eq!(bfield.get(b"test"), BFieldLookup::Some(2));

    // `mask_or_insert`ing the same value doesn't change anything
    assert_eq!(bfield.mask_or_insert(b"test", 2), true);
    assert_eq!(bfield.get(b"test"), BFieldLookup::Some(2));

    // `mask_or_insert`ing a new value results in an indeterminate
    assert_eq!(bfield.mask_or_insert(b"test", 3), false);
    assert_eq!(bfield.get(b"test"), BFieldLookup::Indeterminate);

    // `mask_or_insert`ing an indeterminate value is still indeterminate
    assert_eq!(bfield.mask_or_insert(b"test", 3), false);
    assert_eq!(bfield.get(b"test"), BFieldLookup::Indeterminate);

    // `mask_or_insert`ing a new key just sets that key
    assert_eq!(bfield.mask_or_insert(b"test2", 2), true);
    assert_eq!(bfield.get(b"test2"), BFieldLookup::Some(2));
}
