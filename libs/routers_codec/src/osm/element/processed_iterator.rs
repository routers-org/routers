//! Iterator over all primitive entities in the structure
//! ignoring header blocks

use rayon::iter::ParallelIterator;
use std::path::PathBuf;

use crate::osm::block::iterator::BlockIterator;
use crate::osm::element::item::ProcessedElement;
use crate::osm::error::CodecError;
use crate::osm::parallel::Parallel;

pub struct ProcessedElementIterator {
    iter: BlockIterator,
}

impl ProcessedElementIterator {
    pub fn new(path: PathBuf) -> Result<ProcessedElementIterator, CodecError> {
        Ok(ProcessedElementIterator {
            iter: BlockIterator::new(path)?,
        })
    }
}

impl Parallel for ProcessedElementIterator {
    type Item<'a> = ProcessedElement<'a>;

    fn for_each<F>(mut self, f: F)
    where
        F: for<'a> Fn(ProcessedElement<'a>) + Send + Sync,
    {
        self.iter.par_iter().for_each(|mut block| {
            block.par_iter().for_each(&f);
        })
    }

    fn map_red<Map, Reduce, Identity, T>(
        mut self,
        map_op: Map,
        red_op: Reduce,
        ident: Identity,
    ) -> T
    where
        Map: for<'a> Fn(ProcessedElement<'a>) -> T + Send + Sync,
        Reduce: Fn(T, T) -> T + Send + Sync,
        Identity: Fn() -> T + Send + Sync,
        T: Send,
    {
        self.iter
            .par_iter()
            .map(|mut block| block.par_iter().map(&map_op).reduce(&ident, &red_op))
            .reduce(&ident, &red_op)
    }

    fn par_red<Reduce, Identity, Combine, T>(
        mut self,
        fold_op: Reduce,
        combine: Combine,
        ident: Identity,
    ) -> T
    where
        Reduce: for<'a> Fn(T, ProcessedElement<'a>) -> T + Send + Sync,
        Identity: Fn() -> T + Send + Sync,
        Combine: Fn(T, T) -> T + Send + Sync,
        T: Send,
    {
        self.iter
            .par_iter()
            .map(|mut block| {
                block
                    .par_iter()
                    .fold(&ident, &fold_op)
                    .reduce(&ident, &combine)
            })
            .reduce(&ident, &combine)
    }
}
