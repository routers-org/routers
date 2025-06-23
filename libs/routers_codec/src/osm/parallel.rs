/// Defines the set of functions available on a
/// parallel iterator. This allows for more
/// efficient traversal of elements within
/// a file.
///
/// Requires defining the item being traversed
/// over, with a <'a> lifetime.
pub trait Parallel {
    type Item<'a>;

    /// Allows immutable linear traversal over the given iterator.
    ///
    /// The traversing function must be
    /// both `Send` and `Sync`.
    fn for_each<F>(self, f: F)
    where
        F: for<'a> Fn(Self::Item<'_>) + Send + Sync;

    /// Allows for a map and reduce over the provided iterator.
    ///
    /// There are three functions required as input:
    /// - The mapping function, `fn(item) -> T`
    /// - The reducing function, `fn(T, T) -> T`
    /// - Identity function, `fn() -> T`
    ///
    /// It works as follows. We iterate in parallel, utilising the identity function
    /// to create a basis for the reduction. We then recursively reduce all the
    /// `Iter<T>` streams (as an iterator, not a collection) into a final, `T` output.
    ///
    /// ### Example
    ///
    /// ```rust
    /// use std::path::PathBuf;
    /// use routers_fixtures::{fixture_path, DISTRICT_OF_COLUMBIA};
    /// use routers_routers_codec::osm::element::item::ProcessedElement;
    /// use routers_routers_codec::osm::{Parallel, ProcessedElementIterator};
    ///
    /// let path = fixture_path(DISTRICT_OF_COLUMBIA);
    /// let nodes = ProcessedElementIterator::new(path)
    ///     .expect("!")
    ///     .map_red(|item| {
    ///        match item {
    ///            ProcessedElement::Node(_) => 1,
    ///            _ => 0
    ///        }
    ///     }, |a, b| a + b, || 0);
    /// ```
    ///
    /// ### Idea
    /// The idea for this function was obtained from [osmpbf](https://github.com/b-r-u/osmpbf/blob/5907ca998a30ef51941bf40257ec78cf8e0b66ed/src/reader.rs#L119)
    fn map_red<Map, Reduce, Identity, T>(self, map_op: Map, red_op: Reduce, ident: Identity) -> T
    where
        Map: for<'a> Fn(Self::Item<'_>) -> T + Send + Sync,
        Reduce: Fn(T, T) -> T + Send + Sync,
        Identity: Fn() -> T + Send + Sync,
        T: Send;

    /// Allows for a reduce over the provided iterator, in parallel.
    ///
    /// There are three functions required as input:
    /// - The reduce function, `fn(T, item) -> T`
    /// - The combine function, `fn(T, T) -> T`
    /// - Identity function, `fn() -> T`
    ///
    /// We reduce, in parallel the elements contained inside, into
    /// a composited `T` result.
    ///
    /// ### Example
    ///
    /// ```rust
    /// use std::collections::BTreeMap;
    /// use std::path::PathBuf;
    /// use routers_fixtures::{fixture_path, DISTRICT_OF_COLUMBIA};
    /// use routers_routers_codec::osm::element::item::ProcessedElement;
    /// use routers_routers_codec::osm::{Parallel, ProcessedElementIterator};
    ///
    /// let path = fixture_path(DISTRICT_OF_COLUMBIA);
    /// let nodes = ProcessedElementIterator::new(path)
    ///     .expect("!")
    ///     .par_red(|tree, item| {
    ///         if let ProcessedElement::Node(node) = item {
    ///             tree.insert(node.id, node);
    ///         }
    ///
    ///         tree
    ///     }, |a, b| BTreeMap::from_iter(a.iter().chain(b.iter())), || BTreeMap::new());
    /// ```
    ///
    /// ### Idea
    /// The idea for this function was obtained from [osmpbf](https://github.com/b-r-u/osmpbf/blob/5907ca998a30ef51941bf40257ec78cf8e0b66ed/src/reader.rs#L119)
    fn par_red<Reduce, Identity, Combine, T>(
        self,
        fold_op: Reduce,
        combine: Combine,
        ident: Identity,
    ) -> T
    where
        Reduce: for<'a> Fn(T, Self::Item<'_>) -> T + Send + Sync,
        Identity: Fn() -> T + Send + Sync,
        Combine: Fn(T, T) -> T + Send + Sync,
        T: Send;
}
