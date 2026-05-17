use alloc::sync::Arc;
use buffa::MessageField;
use buffa::view::OwnedView;
use connectrpc::{ConnectError, RequestContext, ServiceResult};
use geo::{BoundingRect, Coord, LineString, Point, Polygon, Rect};
use routers_tz::{TimeZone, TimezoneResolver};
use schema::connect::routers::api::timezone::v1::TimezoneService;
use schema::proto::routers::api::timezone::v1::{
    __buffa::view::{
        BatchGetFromBoundingBoxRequestView, BatchGetFromPointsRequestView,
        BatchGetFromPolygonRequestView, GetFromBoundingBoxRequestView, GetFromPointRequestView,
        GetFromPolygonRequestView,
    },
    BatchGetFromBoundingBoxResponse, BatchGetFromPointsResponse, BatchGetFromPolygonResponse,
    GetFromBoundingBoxResponse, GetFromPointResponse, GetFromPolygonResponse,
};
use schema::proto::routers::model::v1::{
    BoundingBox as BoundingBoxMessage, Coordinate as CoordinateMessage, Polygon as PolygonMessage,
    Timezone as TimezoneMessage,
};
#[cfg(feature = "telemetry")]
use tracing::Level;

pub struct TimezoneAdapter<R> {
    pub(crate) inner: Arc<R>,
}

impl<R> TimezoneAdapter<R> {
    pub fn new(inner: Arc<R>) -> Self {
        Self { inner }
    }
}

fn timezone_message(tz: &TimeZone) -> TimezoneMessage {
    TimezoneMessage {
        iana_code: tz.name().to_string(),
        ..Default::default()
    }
}

fn point_from(c: &CoordinateMessage) -> Point {
    Point::new(c.longitude, c.latitude)
}

fn rect_from(bb: &BoundingBoxMessage) -> Option<Rect> {
    let tl = bb.top_left.as_option()?;
    let br = bb.bottom_right.as_option()?;
    Some(Rect::new(
        Coord {
            x: tl.longitude,
            y: tl.latitude,
        },
        Coord {
            x: br.longitude,
            y: br.latitude,
        },
    ))
}

fn polygon_from(p: &PolygonMessage) -> Polygon {
    let coords: Vec<Coord> = p
        .coordinates
        .iter()
        .map(|c| Coord {
            x: c.longitude,
            y: c.latitude,
        })
        .collect();
    Polygon::new(LineString::from(coords), vec![])
}

fn resolve_point<R: TimezoneResolver>(
    resolver: &R,
    coordinate: &MessageField<CoordinateMessage>,
) -> Result<Vec<TimezoneMessage>, ConnectError> {
    let point = coordinate
        .as_option()
        .map(point_from)
        .ok_or_else(|| ConnectError::invalid_argument("Missing Coordinate"))?;

    resolver
        .search(&point.bounding_rect())
        .map(|tzs| tzs.iter().map(timezone_message).collect())
        .map_err(|e| ConnectError::internal(format!("{:?}", e)))
}

fn resolve_bounding_box<R: TimezoneResolver>(
    resolver: &R,
    bounding_box: &MessageField<BoundingBoxMessage>,
) -> Result<Vec<TimezoneMessage>, ConnectError> {
    let rect = bounding_box
        .as_option()
        .and_then(rect_from)
        .ok_or_else(|| ConnectError::invalid_argument("Missing BoundingBox"))?;

    resolver
        .search(&rect)
        .map(|tzs| tzs.iter().map(timezone_message).collect())
        .map_err(|e| ConnectError::internal(format!("{:?}", e)))
}

fn resolve_polygon<R: TimezoneResolver>(
    resolver: &R,
    polygon: &MessageField<PolygonMessage>,
) -> Result<Vec<TimezoneMessage>, ConnectError> {
    let polygon = polygon
        .as_option()
        .map(polygon_from)
        .ok_or_else(|| ConnectError::invalid_argument("Missing Polygon"))?;

    resolver
        .search_polygon(&polygon)
        .map(|tzs| tzs.iter().map(timezone_message).collect())
        .map_err(|e| ConnectError::internal(format!("{:?}", e)))
}

#[allow(refining_impl_trait)]
impl<R> TimezoneService for TimezoneAdapter<R>
where
    R: TimezoneResolver + Send + Sync + 'static,
{
    #[cfg_attr(feature="telemetry", tracing::instrument(skip_all, level = Level::INFO))]
    async fn get_from_point(
        &self,
        _ctx: RequestContext,
        request: OwnedView<GetFromPointRequestView<'static>>,
    ) -> ServiceResult<GetFromPointResponse> {
        let owned = request.to_owned_message();
        let timezones = resolve_point(&*self.inner, &owned.coordinate)?;

        Ok(GetFromPointResponse {
            timezones,
            ..Default::default()
        }
        .into())
    }

    #[cfg_attr(feature="telemetry", tracing::instrument(skip_all, level = Level::INFO))]
    async fn batch_get_from_points(
        &self,
        _ctx: RequestContext,
        request: OwnedView<BatchGetFromPointsRequestView<'static>>,
    ) -> ServiceResult<BatchGetFromPointsResponse> {
        let owned = request.to_owned_message();

        let mut timezones = Vec::new();
        for c in owned.coordinates.iter() {
            let point = point_from(c);
            let found = self
                .inner
                .search(&point.bounding_rect())
                .map_err(|e| ConnectError::internal(format!("{:?}", e)))?;
            timezones.extend(found.iter().map(timezone_message));
        }

        Ok(BatchGetFromPointsResponse {
            timezones,
            ..Default::default()
        }
        .into())
    }

    #[cfg_attr(feature="telemetry", tracing::instrument(skip_all, level = Level::INFO))]
    async fn get_from_bounding_box(
        &self,
        _ctx: RequestContext,
        request: OwnedView<GetFromBoundingBoxRequestView<'static>>,
    ) -> ServiceResult<GetFromBoundingBoxResponse> {
        let owned = request.to_owned_message();
        let timezones = resolve_bounding_box(&*self.inner, &owned.bounding_box)?;

        Ok(GetFromBoundingBoxResponse {
            timezones,
            ..Default::default()
        }
        .into())
    }

    #[cfg_attr(feature="telemetry", tracing::instrument(skip_all, level = Level::INFO))]
    async fn batch_get_from_bounding_box(
        &self,
        _ctx: RequestContext,
        request: OwnedView<BatchGetFromBoundingBoxRequestView<'static>>,
    ) -> ServiceResult<BatchGetFromBoundingBoxResponse> {
        let owned = request.to_owned_message();

        let mut timezones = Vec::new();
        for bb in owned.bounding_boxes.iter() {
            let rect = rect_from(bb)
                .ok_or_else(|| ConnectError::invalid_argument("Missing BoundingBox"))?;
            let found = self
                .inner
                .search(&rect)
                .map_err(|e| ConnectError::internal(format!("{:?}", e)))?;
            timezones.extend(found.iter().map(timezone_message));
        }

        Ok(BatchGetFromBoundingBoxResponse {
            timezones,
            ..Default::default()
        }
        .into())
    }

    #[cfg_attr(feature="telemetry", tracing::instrument(skip_all, level = Level::INFO))]
    async fn get_from_polygon(
        &self,
        _ctx: RequestContext,
        request: OwnedView<GetFromPolygonRequestView<'static>>,
    ) -> ServiceResult<GetFromPolygonResponse> {
        let owned = request.to_owned_message();
        let timezones = resolve_polygon(&*self.inner, &owned.polygon)?;

        Ok(GetFromPolygonResponse {
            timezones,
            ..Default::default()
        }
        .into())
    }

    #[cfg_attr(feature="telemetry", tracing::instrument(skip_all, level = Level::INFO))]
    async fn batch_get_from_polygon(
        &self,
        _ctx: RequestContext,
        request: OwnedView<BatchGetFromPolygonRequestView<'static>>,
    ) -> ServiceResult<BatchGetFromPolygonResponse> {
        let owned = request.to_owned_message();

        let mut timezones = Vec::new();
        for p in owned.polygons.iter() {
            let polygon = polygon_from(p);
            let found = self
                .inner
                .search_polygon(&polygon)
                .map_err(|e| ConnectError::internal(format!("{:?}", e)))?;
            timezones.extend(found.iter().map(timezone_message));
        }

        Ok(BatchGetFromPolygonResponse {
            timezones,
            ..Default::default()
        }
        .into())
    }
}
