pub struct Query<T, F> {
    pub parameters: T,
    pub filter: F,
}

impl<T, F> Query<T, F> {
    pub fn new(parameters: T, filter: F) -> Self {
        Query { parameters, filter }
    }

    pub fn add_param<K>(self, new_param: K) -> Query<(T, K), F> {
        Query {
            parameters: (self.parameters, new_param),
            filter: self.filter,
        }
    }

    pub fn params(&self) -> &T {
        &self.parameters
    }

    pub fn filter(&self) -> &F {
        &self.filter
    }
}

pub trait TileQuery<In, Filter, Out, Item> {
    type Error;
    type Parameters<'a>
    where
        Self: 'a;
    type Connection<'a>
    where
        Self: 'a;

    const QUERY_TABLE: &'static str;

    fn query(
        input: Query<In, Option<Filter>>,
        params: Self::Parameters<'_>,
        conn: Self::Connection<'_>,
    ) -> impl std::future::Future<Output = Result<Out, Self::Error>> + Send;

    fn batch(query: Query<Self::Parameters<'_>, (u8, u32, u32)>) -> In;
    fn filter(filter: &Self::Parameters<'_>, item: &Item) -> bool;
}
