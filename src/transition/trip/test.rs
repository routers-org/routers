use crate::transition::Trip;
use routers_codec::osm::OsmEntryId;

use approx::{assert_relative_eq};
use geo::wkt;
use routers_codec::primitive::Node;

const SHARED_DISTANCE: f64 = 900.0;

#[test]
fn test_trip() {
    use geo::Point;

    let nodes: Vec<Node<OsmEntryId>> = vec![
        Node::new(Point::new(0.0, 0.0), OsmEntryId::null()),
        Node::new(Point::new(0.0, 1.0), OsmEntryId::null()),
        Node::new(Point::new(1.0, 1.0), OsmEntryId::null()),
        Node::new(Point::new(1.0, 0.0), OsmEntryId::null()),
        Node::new(Point::new(1.0, -1.0), OsmEntryId::null()),
    ];

    let trip = Trip::from(nodes);

    let angles = trip.headings();
    assert_relative_eq!(angles[0], 0.0);
    assert_relative_eq!(angles[1], 90.0, max_relative = 1.0);
    assert_relative_eq!(angles[2], 180.0);
    assert_relative_eq!(angles[3], 180.0);

    assert_relative_eq!(trip.total_angle(), 180.0);
}

#[test]
fn validate_segment() {
    let linestring = wkt! {
        LINESTRING (-118.618033 34.166292, -118.623419 34.164641, -118.626895 34.163434)
    };

    let nodes = linestring
        .into_points()
        .into_iter()
        .map(|p| Node::new(p, OsmEntryId::null()))
        .collect::<Vec<_>>();

    let trip = Trip::from(nodes);

    let angle = trip.total_angle();
    assert_relative_eq!(angle, 2.44, max_relative = 0.1);

    let imm_angle = trip.immediate_angle().abs();
    assert_relative_eq!(imm_angle, 0.81, max_relative = 0.1);

    let exp_angle = trip.angular_complexity();
    assert_relative_eq!(exp_angle, 0.95, max_relative = 0.1);
}

#[test]
fn validate_turning_path() {
    let linestring = wkt! {
        LINESTRING (-118.61829 34.166594, -118.623312 34.164996, -118.62329 34.164073, -118.624127 34.163896, -118.624449 34.163736, -118.625554 34.163461, -118.625929 34.163327, -118.626637 34.162928)
    };

    let nodes = linestring
        .into_points()
        .into_iter()
        .map(|p| Node::new(p, OsmEntryId::null()));

    let trip = Trip::new(nodes);

    let angle = trip.total_angle();
    assert_relative_eq!(angle, 195.30, max_relative = 0.1);

    let imm_angle = trip.immediate_angle().abs();
    assert_relative_eq!(imm_angle, 24.41, epsilon = 1e-2f64);

    let exp_angle = trip.angular_complexity();
    assert_relative_eq!(exp_angle, 0.002, epsilon = 1e-2f64);
}

#[test]
fn validate_uturn_expensive() {
    let linestring = wkt! {
        LINESTRING (-118.509833 34.170873, -118.505648 34.170891, -118.51406 34.170908, -118.509849 34.170926, -118.509865 34.172293)
    };

    let nodes = linestring
        .into_points()
        .into_iter()
        .map(|p| Node::new(p, OsmEntryId::null()));

    let trip = Trip::new(nodes);

    let length = trip.length();
    assert_relative_eq!(length, 1698.0, max_relative = 0.1);

    let angle = trip.total_angle();
    assert_relative_eq!(angle, 449.40, max_relative = 0.1);

    // Discouragingly complex
    let imm_angle = trip.angular_complexity();
    assert_relative_eq!(imm_angle, 0., max_relative = 0.1);
}

#[test]
fn validate_through_lower_cost() {
    let linestring_through_trip = wkt! {
        LINESTRING (-118.236761 33.945685, -118.236447 33.945696, -118.236341 33.945703, -118.23623 33.945723, -118.236133 33.945751, -118.236041 33.945797, -118.235908 33.945868, -118.235774 33.94596, -118.235509 33.946225, -118.235419 33.946304, -118.235322 33.946382, -118.235192 33.946447, -118.235031 33.946503, -118.234928 33.946525, -118.234797 33.946538, -118.234501 33.946542)
    };

    let linestring_around_trip = wkt! {
        LINESTRING (-118.236761 33.945685, -118.236759 33.946891, -118.236759 33.946891, -118.236758 33.947095, -118.235875 33.947112, -118.235546 33.947118, -118.235546 33.947118, -118.234899 33.947131, -118.234483 33.947139, -118.234501 33.946542)
    };

    let nodes = linestring_through_trip
        .into_points()
        .into_iter()
        .map(|p| Node::new(p, OsmEntryId::null()));
    let through = Trip::new(nodes);

    let nodes = linestring_around_trip
        .into_points()
        .into_iter()
        .map(|p| Node::new(p, OsmEntryId::null()));
    let around = Trip::new(nodes);

    let imm_angle = around.angular_complexity();
    assert_relative_eq!(imm_angle, 0.046, max_relative = 0.1);

    let imm_angle = through.angular_complexity();
    assert_relative_eq!(imm_angle, 0.899, max_relative = 0.1);

    let len = around.length();
    assert_relative_eq!(len, 433.0, max_relative = 0.1);

    let len = through.length();
    assert_relative_eq!(len, 241.52, max_relative = 0.1);
}

#[test]
fn validate_slip_road_optimality() {
    use crate::transition::Trip;

    let linestring_sliproad = wkt! {
        LINESTRING (-118.138707 33.917051, -118.13859 33.917027, -118.138402 33.916998, -118.138172 33.916897, -118.138106 33.916837, -118.138078 33.916778, -118.138076 33.916697, -118.138251 33.916449, -118.138268 33.916424)
    };

    let linestring_around = wkt! {
        LINESTRING (-118.138707 33.917051, -118.13859 33.917027, -118.138402 33.916998, -118.138174 33.916984, -118.137992 33.916977, -118.137992 33.916977, -118.137881 33.916973, -118.138076 33.916697, -118.138251 33.916449, -118.138273 33.916417)
    };

    let nodes = linestring_sliproad
        .into_points()
        .into_iter()
        .map(|p| Node::new(p, OsmEntryId::null()));
    let sliproad = Trip::new(nodes);

    let nodes = linestring_around
        .into_points()
        .into_iter()
        .map(|p| Node::new(p, OsmEntryId::null()));
    let around = Trip::new(nodes);

    let tot_angle = sliproad.total_angle();
    assert_relative_eq!(tot_angle, 114.1, max_relative = 0.1);

    let tot_angle = around.total_angle();
    assert_relative_eq!(tot_angle, 129.97, max_relative = 0.1);

    let imm_angle = around.angular_complexity();
    assert_relative_eq!(imm_angle, 0.26, max_relative = 0.1);

    let imm_angle = sliproad.angular_complexity();
    assert_relative_eq!(imm_angle, 0.497, max_relative = 0.1);

    let len = around.length();
    assert_relative_eq!(len, 148.5, max_relative = 0.1);

    let len = sliproad.length();
    assert_relative_eq!(len, 113.0, max_relative = 0.1);
}

#[test]
fn validate_on_less_than_off() {
    use crate::transition::Trip;

    let take_exit = wkt! {
        LINESTRING(-118.59729839999997 34.16952509999994,-118.59760129999997 34.16957129999994,-118.60298680000004 34.17041450000007,-118.60328260000004 34.17053870000007,-118.60476640000027 34.17089249999979,-118.6050337000002 34.170964500000125,-118.60554899999987 34.17111750000006,-118.6056053 34.17114289999975,-118.60563500000028 34.17116429999979,-118.60573580000026 34.171245199999916,-118.60574170000027 34.171851299999915,-118.60581550000006 34.171971400000075,-118.6060260999994 34.171972599999954,-118.60615740000034 34.17197259999983,-118.60639540000021 34.17197290000012,-118.6065834000002 34.17197150000012,-118.60687739999997 34.17197120000001,-118.60695699999988 34.17197200000007,-118.60706999999998 34.17197310000001,-118.60705410000008 34.17192239999984,-118.6070492999998 34.17186159999987,-118.60703870000003 34.17175670000002,-118.6070362999997 34.171568899999855,-118.60703909999971 34.171520199999854,-118.60705120000031 34.17144340000009,-118.6070660999997 34.17140009999986,-118.60708360000021 34.17135899999991,-118.60710769999972 34.17132169999985,-118.60713840000021 34.17128299999991,-118.60719320000021 34.17122819999991,-118.60724350000021 34.171188399999906,-118.60730579999971 34.17115589999985,-118.60734930000021 34.171134199999905,-118.60737319999971 34.17112179999985,-118.60741699999971 34.17110469999985,-118.60847060000022 34.170885599999906,-118.60908790000005 34.170690800000074,-118.60943160000028 34.17061079999978,-118.60978520000027 34.17051619999978,-118.61008539999995 34.17043330000008,-118.61062660000029 34.17021899999979,-118.61094349999954 34.170084900000035,-118.6113436000003 34.16989419999979,-118.61164579999954 34.16973670000004,-118.61199749999953 34.16953180000004)
    };

    let remain_on = wkt! {
      LINESTRING (-118.59729839999997 34.16952509999994, -118.597601 34.169571, -118.600072 34.169937, -118.602987 34.170414, -118.604769 34.170653, -118.605613 34.17078, -118.606072 34.170837, -118.606395 34.170865, -118.606715 34.170882, -118.607126 34.170887, -118.607523 34.170877, -118.607935 34.170846, -118.608425 34.170791, -118.609088 34.170691, -118.609432 34.170611, -118.609784 34.170516, -118.610085 34.170432, -118.610627 34.170218, -118.610943 34.170085, -118.611343 34.169894, -118.611646 34.169737, -118.61199749999953 34.16953180000004)
    };

    let nodes = take_exit
        .into_points()
        .into_iter()
        .map(|p| Node::new(p, OsmEntryId::null()));
    let exit = Trip::new(nodes);

    let nodes = remain_on
        .into_points()
        .into_iter()
        .map(|p| Node::new(p, OsmEntryId::null()));
    let remain = Trip::new(nodes);

    let tot_angle_exit = exit.total_angle();
    let tot_angle_remain = remain.total_angle();

    assert!(tot_angle_remain < tot_angle_exit);

    let inv_complexity_exit = exit.angular_complexity();
    let inv_complexity_remain = remain.angular_complexity();

    // Lower score is considered "complex", we want the highway exit
    // to be considered more complex than remaining on the highway.
    assert!(inv_complexity_exit < inv_complexity_remain);
}