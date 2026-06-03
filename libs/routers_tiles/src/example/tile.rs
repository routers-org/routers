use crate::datasource::connectors::bigtable::{
    BigTableInput, BigTableOutput, BigTableRepositorySet,
};
use crate::datasource::date::format_date;
use crate::error::TileError;
use crate::proto::Example;
use crate::query::{DatedRange, MVTTile, QueryParams, Range};
use crate::{Fragment, Query, Repo, TileQuery, layer, tile};
use axum::extract::{Path, State};
use bigtable_rs::bigtable::RowCell;
use bigtable_rs::google::bigtable::v2::row_range::{EndKey, StartKey};
use bigtable_rs::google::bigtable::v2::{RowFilter, RowRange};
use chrono::{DateTime, Utc};
use geo::{coord, point};
use log::info;
use prost::Message;
use routers_geo::TileItem;
use routers_geo::coord::point::FeatureKey;
use schema::proto::mvt::{Tile, Value};
use serde::Deserialize;
use std::sync::Arc;
use strum::{EnumCount, EnumIter, EnumProperty, VariantArray};
use tracing::{Level, event};

const PREFIX: &str = "ex";
const STORAGE_ZOOM: u8 = 19;
const MIN_ZOOM: u8 = 16;

#[derive(EnumCount, EnumProperty, EnumIter, VariantArray, strum::Display, Copy, Clone)]
pub enum ExampleFeatureKeys {
    KeyA,
    KeyB,
}

impl FeatureKey for ExampleFeatureKeys {}

impl TileItem<Value> for Example {
    type Key = ExampleFeatureKeys;

    fn entries<'a>(&self) -> Vec<(Self::Key, Value)> {
        vec![
            (Self::Key::KeyA, Value::default().with_sint_value(self.a)),
            (Self::Key::KeyB, Value::default().with_sint_value(self.b)),
        ]
    }
}

impl Into<geo::Point> for Example {
    fn into(self) -> geo::Point {
        point!(coord! { x: self.longitude, y: self.latitude })
    }
}

impl Example {
    fn from_cell(cell: RowCell) -> Result<Self, TileError> {
        Example::decode(cell.value.as_slice()).map_err(TileError::ProtoDecode)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExampleParams {
    dates: Vec<DatedRange>,
    filter_a: Range<i64>,
    filter_b: Range<i64>,
}

impl TileQuery<Vec<RowRange>, RowFilter, MVTTile, Example> for Example {
    type Error = TileError;
    type Parameters<'a> = (ExampleParams, u8);
    type Connection<'a> = &'a Repo<BigTableInput, BigTableOutput>;

    const QUERY_TABLE: &'static str = "big_table";

    async fn query(
        input: Query<Vec<RowRange>, Option<RowFilter>>,
        (bp, zoom): Self::Parameters<'_>,
        connection: Self::Connection<'_>,
    ) -> Result<MVTTile, Self::Error> {
        tracing::event!(
            Level::INFO,
            name = "query::start",
            table = Self::QUERY_TABLE
        );
        let rows = connection.query(input).await?;
        tracing::event!(Level::INFO, name = "query::end", table = Self::QUERY_TABLE);

        if rows.len() == 0 {
            info!("Found no tile data.");
            return Err(TileError::NoTilesFound);
        }

        let points: Vec<Example> = rows
            .into_iter()
            .flat_map(|r| r.1)
            .map(Example::from_cell)
            .filter_map(|r| r.ok())
            .filter(|point| Self::filter(&(bp.clone(), zoom), point))
            .collect();

        tracing::event!(
            Level::INFO,
            name = "query::serialized",
            length = points.len()
        );

        tile! {
            layer!(points, zoom, "example_layer"),
        }
    }

    fn batch(query: Query<Self::Parameters<'_>, (u8, u32, u32)>) -> Vec<RowRange> {
        let (params, _) = query.params();
        let (z, x, y) = query.filter();

        let format_key = |date: &DateTime<Utc>, hid: u64| {
            format!("{:012}/{}/{}", hid, PREFIX, format_date(date)).into_bytes()
        };

        Fragment::new(z, x, y)
            .detail(STORAGE_ZOOM)
            .into_iter()
            .flat_map(|fragment| {
                params
                    .clone()
                    .dates
                    .into_iter()
                    .map(|t| RowRange {
                        start_key: Some(StartKey::StartKeyClosed(format_key(
                            &t.dates.start().0,
                            fragment.to_hilbert(),
                        ))),
                        end_key: Some(EndKey::EndKeyClosed(format_key(
                            &t.dates.end().0,
                            fragment.to_hilbert(),
                        ))),
                    })
                    .collect::<Vec<RowRange>>()
            })
            .collect()
    }

    fn filter((filter, _): &Self::Parameters<'_>, item: &Example) -> bool {
        filter.filter_a.within(item.a) && filter.filter_b.within(item.b)
    }
}

impl Example {
    pub async fn tile(
        State(state): State<Arc<BigTableRepositorySet>>,
        Path((z, x, y)): Path<(u8, u32, u32)>,
        QueryParams(params): QueryParams<ExampleParams>,
    ) -> Result<Tile, TileError> {
        event!(Level::TRACE, name = "query::invoke", ?z, ?x, ?y, ?params);

        if z < MIN_ZOOM || z > STORAGE_ZOOM {
            return Err(TileError::UnsupportedZoom(z));
        }

        let rows = Example::batch(Query::new((params.clone(), z), (z, x, y)));

        event!(Level::INFO, name = "query::plan", size = rows.len());
        Example::query(
            Query::new(rows, None),
            (params, z),
            state
                .get_repository(Self::QUERY_TABLE)
                .ok_or(TileError::NoMatchingRepository)?,
        )
        .await
        .map(|v: MVTTile| v.0)
    }
}
