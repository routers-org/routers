pub mod basic;
pub mod rtree;
pub mod s2cell;

/// The zones a stored index names: the zone itself, or an overlap set past the last zone.
pub fn zones<'a>(index: &'a u16, names: usize, overlaps: &'a [Vec<u16>]) -> &'a [u16] {
    match (*index as usize).checked_sub(names) {
        Some(set) => &overlaps[set],
        None => core::slice::from_ref(index),
    }
}
