#[cfg(feature = "basic")]
pub mod basic;
#[cfg(feature = "rtree")]
pub mod rtree;
#[cfg(feature = "s2cell")]
pub mod s2cell;

#[cfg(test)]
pub(crate) fn assert_valid_indices(indices: &[u16], zones: usize, overlaps: &[Vec<u16>]) {
    assert!((indices.iter()).all(|&index| (index as usize) < zones + overlaps.len()));
    assert!((overlaps.iter()).all(|set| {
        set.len() > 1
            && set.windows(2).all(|w| w[0] < w[1])
            && (set.iter()).all(|&zone| (zone as usize) < zones)
    }));
}
