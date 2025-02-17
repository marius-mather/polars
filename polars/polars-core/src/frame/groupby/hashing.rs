use std::hash::{BuildHasher, Hash};

use hashbrown::hash_map::{Entry, RawEntryMut};
use hashbrown::HashMap;
use polars_utils::sync::SyncPtr;
use polars_utils::HashSingle;
use rayon::prelude::*;

use super::GroupsProxy;
use crate::datatypes::PlHashMap;
use crate::frame::groupby::GroupsIdx;
use crate::hashing::{
    df_rows_to_hashes_threaded_vertical, this_partition, AsU64, IdBuildHasher, IdxHash,
};
use crate::prelude::compare_inner::PartialEqInner;
use crate::prelude::*;
use crate::utils::{split_df, CustomIterTools};
use crate::POOL;

fn finish_group_order_vecs(
    mut vecs: Vec<(Vec<IdxSize>, Vec<Vec<IdxSize>>)>,
    sorted: bool,
) -> GroupsProxy {
    if sorted {
        if vecs.len() == 1 {
            let (first, all) = vecs.pop().unwrap();
            return GroupsProxy::Idx(GroupsIdx::new(first, all, true));
        }

        let cap = vecs.iter().map(|v| v.0.len()).sum::<usize>();
        let offsets = vecs
            .iter()
            .scan(0_usize, |acc, v| {
                let out = *acc;
                *acc += v.0.len();
                Some(out)
            })
            .collect::<Vec<_>>();

        // we write (first, all) tuple because of sorting
        let mut items = Vec::with_capacity(cap);
        let items_ptr = unsafe { SyncPtr::new(items.as_mut_ptr()) };

        POOL.install(|| {
            vecs.into_par_iter()
                .zip(offsets)
                .for_each(|((first, all), offset)| {
                    // pre-sort every array not needed as items are already sorted
                    // this is due to using an index hashmap

                    unsafe {
                        let mut items_ptr: *mut (IdxSize, Vec<IdxSize>) = items_ptr.get();
                        items_ptr = items_ptr.add(offset);

                        // give the compiler some info
                        // maybe it may elide some loop counters
                        assert_eq!(first.len(), all.len());
                        for (i, (first, all)) in first.into_iter().zip(all.into_iter()).enumerate()
                        {
                            std::ptr::write(items_ptr.add(i), (first, all))
                        }
                    }
                });
        });
        unsafe {
            items.set_len(cap);
        }
        // sort again
        items.sort_unstable_by_key(|g| g.0);

        let mut idx = GroupsIdx::from_iter(items.into_iter());
        idx.sorted = true;
        GroupsProxy::Idx(idx)
    } else {
        // this materialization is parallel in the from impl.
        GroupsProxy::Idx(GroupsIdx::from(vecs))
    }
}

// We must strike a balance between cache coherence and resizing costs.
// Overallocation seems a lot more expensive than resizing so we start reasonable small.
pub(crate) const HASHMAP_INIT_SIZE: usize = 512;

pub(crate) fn groupby<T>(a: impl Iterator<Item = T>, sorted: bool) -> GroupsProxy
where
    T: Hash + Eq,
{
    let mut hash_tbl: PlHashMap<T, (IdxSize, Vec<IdxSize>)> =
        PlHashMap::with_capacity(HASHMAP_INIT_SIZE);
    let mut cnt = 0;
    a.for_each(|k| {
        let idx = cnt;
        cnt += 1;
        let entry = hash_tbl.entry(k);

        match entry {
            Entry::Vacant(entry) => {
                entry.insert((idx, vec![idx]));
            }
            Entry::Occupied(mut entry) => {
                let v = entry.get_mut();
                v.1.push(idx);
            }
        }
    });
    if sorted {
        let mut groups = hash_tbl
            .into_iter()
            .map(|(_k, v)| v)
            .collect_trusted::<Vec<_>>();
        groups.sort_unstable_by_key(|g| g.0);
        let mut idx: GroupsIdx = groups.into_iter().collect();
        idx.sorted = true;
        GroupsProxy::Idx(idx)
    } else {
        GroupsProxy::Idx(hash_tbl.into_values().collect())
    }
}

pub(crate) fn groupby_threaded_num2<T, I>(
    keys: &[I],
    n_partitions: u64,
    sorted: bool,
) -> GroupsProxy
where
    I: IntoIterator<Item = T> + Send + Sync + Copy,
    I::IntoIter: ExactSizeIterator,
    T: Send + Hash + Eq + Sync + Copy + AsU64,
{
    assert!(n_partitions.is_power_of_two());

    // We will create a hashtable in every thread.
    // We use the hash to partition the keys to the matching hashtable.
    // Every thread traverses all keys/hashes and ignores the ones that doesn't fall in that partition.
    let v = POOL.install(|| {
        (0..n_partitions)
            .into_par_iter()
            .map(|thread_no| {
                let mut hash_tbl: PlHashMap<T, IdxSize> =
                    PlHashMap::with_capacity(HASHMAP_INIT_SIZE);
                let mut first_vals = Vec::with_capacity(HASHMAP_INIT_SIZE);
                let mut all_vals = Vec::with_capacity(HASHMAP_INIT_SIZE);

                let mut offset = 0;
                for keys in keys {
                    let keys = keys.into_iter();
                    let len = keys.len() as IdxSize;
                    let hasher = hash_tbl.hasher().clone();

                    let mut cnt = 0;
                    keys.for_each(|k| {
                        let row_idx = cnt + offset;
                        cnt += 1;

                        if this_partition(k.as_u64(), thread_no, n_partitions) {
                            let hash = hasher.hash_single(k);
                            let entry = hash_tbl.raw_entry_mut().from_key_hashed_nocheck(hash, &k);

                            match entry {
                                RawEntryMut::Vacant(entry) => {
                                    let offset_idx = first_vals.len() as IdxSize;

                                    let tuples = vec![row_idx];
                                    all_vals.push(tuples);
                                    first_vals.push(row_idx);

                                    entry.insert_with_hasher(hash, k, offset_idx, |k| {
                                        hasher.hash_single(k)
                                    });
                                }
                                RawEntryMut::Occupied(entry) => {
                                    let offset_idx = *entry.get();
                                    unsafe {
                                        let buf = all_vals.get_unchecked_mut(offset_idx as usize);
                                        buf.push(row_idx)
                                    }
                                }
                            }
                        }
                    });
                    offset += len;
                }
                (first_vals, all_vals)
            })
            .collect::<Vec<_>>()
    });
    finish_group_order_vecs(v, sorted)
}

/// Utility function used as comparison function in the hashmap.
/// The rationale is that equality is an AND operation and therefore its probability of success
/// declines rapidly with the number of keys. Instead of first copying an entire row from both
/// sides and then do the comparison, we do the comparison value by value catching early failures
/// eagerly.
///
/// # Safety
/// Doesn't check any bounds
#[inline]
pub(crate) unsafe fn compare_df_rows(keys: &DataFrame, idx_a: usize, idx_b: usize) -> bool {
    for s in keys.get_columns() {
        if !s.equal_element(idx_a, idx_b, s) {
            return false;
        }
    }
    true
}

/// Populate a multiple key hashmap with row indexes.
/// Instead of the keys (which could be very large), the row indexes are stored.
/// To check if a row is equal the original DataFrame is also passed as ref.
/// When a hash collision occurs the indexes are ptrs to the rows and the rows are compared
/// on equality.
pub(crate) fn populate_multiple_key_hashmap<V, H, F, G>(
    hash_tbl: &mut HashMap<IdxHash, V, H>,
    // row index
    idx: IdxSize,
    // hash
    original_h: u64,
    // keys of the hash table (will not be inserted, the indexes will be used)
    // the keys are needed for the equality check
    keys: &DataFrame,
    // value to insert
    vacant_fn: G,
    // function that gets a mutable ref to the occupied value in the hash table
    mut occupied_fn: F,
) where
    G: Fn() -> V,
    F: FnMut(&mut V),
    H: BuildHasher,
{
    let entry = hash_tbl
        .raw_entry_mut()
        // uses the idx to probe rows in the original DataFrame with keys
        // to check equality to find an entry
        // this does not invalidate the hashmap as this equality function is not used
        // during rehashing/resize (then the keys are already known to be unique).
        // Only during insertion and probing an equality function is needed
        .from_hash(original_h, |idx_hash| {
            // first check the hash values
            // before we incur a cache miss
            idx_hash.hash == original_h && {
                let key_idx = idx_hash.idx;
                // Safety:
                // indices in a groupby operation are always in bounds.
                unsafe { compare_df_rows(keys, key_idx as usize, idx as usize) }
            }
        });
    match entry {
        RawEntryMut::Vacant(entry) => {
            entry.insert_hashed_nocheck(original_h, IdxHash::new(idx, original_h), vacant_fn());
        }
        RawEntryMut::Occupied(mut entry) => {
            let (_k, v) = entry.get_key_value_mut();
            occupied_fn(v);
        }
    }
}

#[inline]
pub(crate) unsafe fn compare_keys<'a>(
    keys_cmp: &'a [Box<dyn PartialEqInner + 'a>],
    idx_a: usize,
    idx_b: usize,
) -> bool {
    for cmp in keys_cmp {
        if !cmp.eq_element_unchecked(idx_a, idx_b) {
            return false;
        }
    }
    true
}

// Differs in the because this one uses the PartialEqInner trait objects
// is faster when multiple chunks. Not yet used in join.
pub(crate) fn populate_multiple_key_hashmap2<'a, V, H, F, G>(
    hash_tbl: &mut HashMap<IdxHash, V, H>,
    // row index
    idx: IdxSize,
    // hash
    original_h: u64,
    // keys of the hash table (will not be inserted, the indexes will be used)
    // the keys are needed for the equality check
    keys_cmp: &'a [Box<dyn PartialEqInner + 'a>],
    // value to insert
    vacant_fn: G,
    // function that gets a mutable ref to the occupied value in the hash table
    occupied_fn: F,
) where
    G: Fn() -> V,
    F: Fn(&mut V),
    H: BuildHasher,
{
    let entry = hash_tbl
        .raw_entry_mut()
        // uses the idx to probe rows in the original DataFrame with keys
        // to check equality to find an entry
        // this does not invalidate the hashmap as this equality function is not used
        // during rehashing/resize (then the keys are already known to be unique).
        // Only during insertion and probing an equality function is needed
        .from_hash(original_h, |idx_hash| {
            // first check the hash values before we incur
            // cache misses
            original_h == idx_hash.hash && {
                let key_idx = idx_hash.idx;
                // Safety:
                // indices in a groupby operation are always in bounds.
                unsafe { compare_keys(keys_cmp, key_idx as usize, idx as usize) }
            }
        });
    match entry {
        RawEntryMut::Vacant(entry) => {
            entry.insert_hashed_nocheck(original_h, IdxHash::new(idx, original_h), vacant_fn());
        }
        RawEntryMut::Occupied(mut entry) => {
            let (_k, v) = entry.get_key_value_mut();
            occupied_fn(v);
        }
    }
}

pub(crate) fn groupby_threaded_multiple_keys_flat(
    mut keys: DataFrame,
    n_partitions: usize,
    sorted: bool,
) -> PolarsResult<GroupsProxy> {
    let dfs = split_df(&mut keys, n_partitions).unwrap();
    let (hashes, _random_state) = df_rows_to_hashes_threaded_vertical(&dfs, None)?;
    let n_partitions = n_partitions as u64;

    // trait object to compare inner types.
    let keys_cmp = keys
        .iter()
        .map(|s| s.into_partial_eq_inner())
        .collect::<Vec<_>>();

    // We will create a hashtable in every thread.
    // We use the hash to partition the keys to the matching hashtable.
    // Every thread traverses all keys/hashes and ignores the ones that doesn't fall in that partition.
    let v = POOL.install(|| {
        (0..n_partitions)
            .into_par_iter()
            .map(|thread_no| {
                let hashes = &hashes;

                let mut hash_tbl: HashMap<IdxHash, IdxSize, IdBuildHasher> =
                    HashMap::with_capacity_and_hasher(HASHMAP_INIT_SIZE, Default::default());
                let mut first_vals = Vec::with_capacity(HASHMAP_INIT_SIZE);
                let mut all_vals = Vec::with_capacity(HASHMAP_INIT_SIZE);

                // put the buffers behind a pointer so we can access them from as the bchk doesn't allow
                // 2 mutable borrows (this is safe as we don't alias)
                // even if the vecs reallocate, we have a pointer to the stack vec, and thus always
                // access the proper data.
                let all_buf_ptr =
                    &mut all_vals as *mut Vec<Vec<IdxSize>> as *const Vec<Vec<IdxSize>>;
                let first_buf_ptr = &mut first_vals as *mut Vec<IdxSize> as *const Vec<IdxSize>;

                let mut offset = 0;
                for hashes in hashes {
                    let len = hashes.len() as IdxSize;

                    let mut idx = 0;
                    for hashes_chunk in hashes.data_views() {
                        for &h in hashes_chunk {
                            // partition hashes by thread no.
                            // So only a part of the hashes go to this hashmap
                            if this_partition(h, thread_no, n_partitions) {
                                let row_idx = idx + offset;
                                populate_multiple_key_hashmap2(
                                    &mut hash_tbl,
                                    row_idx,
                                    h,
                                    &keys_cmp,
                                    || unsafe {
                                        let first_vals = &mut *(first_buf_ptr as *mut Vec<IdxSize>);
                                        let all_vals =
                                            &mut *(all_buf_ptr as *mut Vec<Vec<IdxSize>>);
                                        let offset_idx = first_vals.len() as IdxSize;

                                        let tuples = vec![row_idx];
                                        all_vals.push(tuples);
                                        first_vals.push(row_idx);
                                        offset_idx
                                    },
                                    |v| unsafe {
                                        let all_vals =
                                            &mut *(all_buf_ptr as *mut Vec<Vec<IdxSize>>);
                                        let offset_idx = *v;
                                        let buf = all_vals.get_unchecked_mut(offset_idx as usize);
                                        buf.push(row_idx)
                                    },
                                );
                            }
                            idx += 1;
                        }
                    }

                    offset += len;
                }
                (first_vals, all_vals)
            })
            .collect::<Vec<_>>()
    });
    Ok(finish_group_order_vecs(v, sorted))
}
