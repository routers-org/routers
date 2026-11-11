use alloc::sync::Arc;
use buffa::MessageField;
use buffa_types::google::protobuf::Timestamp;
use connectrpc::{ConnectError, RequestContext, ServiceRequest, ServiceResult};
use geo::{BoundingRect, Coord, LineString, Point, Polygon, Rect};
use routers_tz::{TimeZone, Timestamp as Instant, TimezoneResolver};
use schema::connect::routers::api::timezone::v1::TimezoneService;
use schema::proto::routers::api::timezone::v1::{
    BatchGetFromBoundingBoxRequest, BatchGetFromBoundingBoxResponse, BatchGetFromPointsRequest,
    BatchGetFromPointsResponse, BatchGetFromPolygonRequest, BatchGetFromPolygonResponse,
    GetFromBoundingBoxResponse, GetFromPointResponse, GetFromPolygonResponse,
};
use schema::proto::routers::api::timezone::v1::{
    GetFromBoundingBoxRequest, GetFromPointRequest, GetFromPolygonRequest,
};
use schema::proto::routers::model::v1::{
    BoundingBox as BoundingBoxMessage, Coordinate as CoordinateMessage, Polygon as PolygonMessage,
    Timezone as TimezoneMessage, UtcOffset,
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

fn timezone_message(tz: TimeZone, instant: Instant) -> TimezoneMessage {
    let observed = tz.at(instant);
    let offset = observed.offset().seconds();

    TimezoneMessage {
        iana_code: tz.name().to_string(),
        utc_offset: MessageField::some(UtcOffset {
            seconds: offset % 60,
            minutes: offset / 60 % 60,
            hours: offset / 3600,
            ..Default::default()
        }),
        is_dst: observed.dst().is_dst(),
        ..Default::default()
    }
}

/// The requested instant, or now when the request leaves it unset.
fn instant_of(timestamp: &MessageField<Timestamp>) -> Result<Instant, ConnectError> {
    let Some(ts) = timestamp.as_option() else {
        return Ok(Instant::now());
    };

    Instant::new(ts.seconds, ts.nanos)
        .map_err(|_| ConnectError::invalid_argument("Timestamp out of range"))
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

#[allow(refining_impl_trait)]
impl<R> TimezoneService for TimezoneAdapter<R>
where
    R: TimezoneResolver + Send + Sync + 'static,
{
    #[cfg_attr(feature="telemetry", tracing::instrument(skip_all, level = Level::INFO))]
    async fn get_from_point(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetFromPointRequest>,
    ) -> ServiceResult<GetFromPointResponse> {
        let GetFromPointRequest {
            coordinate,
            timestamp,
            ..
        } = request.to_owned_message();
        let instant = instant_of(&timestamp)?;
        let point = coordinate
            .as_option()
            .map(point_from)
            .ok_or_else(|| ConnectError::invalid_argument("Missing Coordinate"))?;

        let timezones = self
            .inner
            .search(&point.bounding_rect())
            .map(|tzs| {
                tzs.into_iter()
                    .map(|tz| timezone_message(tz, instant))
                    .collect()
            })
            .map_err(|e| ConnectError::internal(format!("{:?}", e)))?;

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
        request: ServiceRequest<'_, BatchGetFromPointsRequest>,
    ) -> ServiceResult<BatchGetFromPointsResponse> {
        let owned = request.to_owned_message();
        let instant = instant_of(&owned.timestamp)?;

        let mut timezones = Vec::new();
        for c in owned.coordinates.iter() {
            let point = point_from(c);
            let found = self
                .inner
                .search(&point.bounding_rect())
                .map_err(|e| ConnectError::internal(format!("{:?}", e)))?;
            let tz = found
                .into_iter()
                .next()
                .map(|tz| timezone_message(tz, instant))
                .ok_or_else(|| ConnectError::not_found("No timezone found for coordinate"))?;
            timezones.push(tz);
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
        request: ServiceRequest<'_, GetFromBoundingBoxRequest>,
    ) -> ServiceResult<GetFromBoundingBoxResponse> {
        let GetFromBoundingBoxRequest {
            bounding_box,
            timestamp,
            ..
        } = request.to_owned_message();
        let instant = instant_of(&timestamp)?;

        let rect = bounding_box
            .as_option()
            .and_then(rect_from)
            .ok_or_else(|| ConnectError::invalid_argument("Missing BoundingBox"))?;

        let timezones = self
            .inner
            .search(&rect)
            .map(|tzs| {
                tzs.into_iter()
                    .map(|tz| timezone_message(tz, instant))
                    .collect()
            })
            .map_err(|e| ConnectError::internal(format!("{:?}", e)))?;

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
        request: ServiceRequest<'_, BatchGetFromBoundingBoxRequest>,
    ) -> ServiceResult<BatchGetFromBoundingBoxResponse> {
        let owned = request.to_owned_message();
        let instant = instant_of(&owned.timestamp)?;

        let mut timezones = Vec::new();
        for bb in owned.bounding_boxes.iter() {
            let rect = rect_from(bb)
                .ok_or_else(|| ConnectError::invalid_argument("Missing BoundingBox"))?;
            let found = self
                .inner
                .search(&rect)
                .map_err(|e| ConnectError::internal(format!("{:?}", e)))?;
            let tz = found
                .into_iter()
                .next()
                .map(|tz| timezone_message(tz, instant))
                .ok_or_else(|| ConnectError::not_found("No timezone found for bounding box"))?;
            timezones.push(tz);
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
        request: ServiceRequest<'_, GetFromPolygonRequest>,
    ) -> ServiceResult<GetFromPolygonResponse> {
        let GetFromPolygonRequest {
            polygon, timestamp, ..
        } = request.to_owned_message();
        let instant = instant_of(&timestamp)?;

        let polygon = polygon
            .as_option()
            .map(polygon_from)
            .ok_or_else(|| ConnectError::invalid_argument("Missing Polygon"))?;

        let timezones = self
            .inner
            .search_polygon(&polygon)
            .map(|tzs| {
                tzs.into_iter()
                    .map(|tz| timezone_message(tz, instant))
                    .collect()
            })
            .map_err(|e| ConnectError::internal(format!("{:?}", e)))?;

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
        request: ServiceRequest<'_, BatchGetFromPolygonRequest>,
    ) -> ServiceResult<BatchGetFromPolygonResponse> {
        let owned = request.to_owned_message();
        let instant = instant_of(&owned.timestamp)?;

        let mut timezones = Vec::new();
        for p in owned.polygons.iter() {
            let polygon = polygon_from(p);
            let found = self
                .inner
                .search_polygon(&polygon)
                .map_err(|e| ConnectError::internal(format!("{:?}", e)))?;
            let tz = found
                .into_iter()
                .next()
                .map(|tz| timezone_message(tz, instant))
                .ok_or_else(|| ConnectError::not_found("No timezone found for polygon"))?;
            timezones.push(tz);
        }

        Ok(BatchGetFromPolygonResponse {
            timezones,
            ..Default::default()
        }
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offset(name: &str, instant: &str) -> (i32, i32, i32, bool) {
        let tz = TimeZone::get(name).expect("zone is in the bundled database");
        let message = timezone_message(tz, instant.parse().expect("valid timestamp"));
        let offset = message.utc_offset.as_option().expect("offset is set");
        (offset.hours, offset.minutes, offset.seconds, message.is_dst)
    }

    #[test]
    fn offset_is_the_one_in_effect_at_the_instant() {
        let (summer, winter) = ("2026-01-15T00:00:00Z", "2026-07-15T00:00:00Z");

        assert_eq!(offset("Australia/Sydney", summer), (11, 0, 0, true));
        assert_eq!(offset("Australia/Sydney", winter), (10, 0, 0, false));
    }

    #[test]
    fn offset_is_split_into_signed_clock_parts() {
        let at = "2026-01-15T00:00:00Z";

        assert_eq!(offset("Asia/Kolkata", at), (5, 30, 0, false));
        assert_eq!(offset("America/St_Johns", at), (-3, -30, 0, false));
    }

    #[test]
    fn instant_defaults_to_now() {
        let before = Instant::now();
        let instant = instant_of(&MessageField::none()).expect("now is in range");

        assert!(instant >= before && instant <= Instant::now());
    }

    #[test]
    fn instant_reads_the_timestamp() {
        let timestamp = MessageField::some(Timestamp::from_unix(1_768_435_200, 500));

        assert_eq!(
            instant_of(&timestamp).expect("in range"),
            "2026-01-15T00:00:00.0000005Z"
                .parse()
                .expect("valid timestamp")
        );
    }

    #[test]
    fn instant_rejects_out_of_range_timestamps() {
        let timestamp = MessageField::some(Timestamp::from_unix(i64::MAX, 0));

        assert!(instant_of(&timestamp).is_err());
    }
}
