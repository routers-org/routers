use routers_fixtures::{DISTRICT_OF_COLUMBIA, fixture_path};

use routers_codec::osm::element::item::Element;
use routers_codec::osm::element::item::ProcessedElement;
use routers_codec::osm::element::iterator::ElementIterator;
use routers_codec::osm::element::processed_iterator::ProcessedElementIterator;
use routers_codec::osm::parallel::Parallel;

use log::info;
use std::time::Instant;

#[test]
fn try_into_iter() {
    let path = fixture_path(DISTRICT_OF_COLUMBIA);
    let iter = ElementIterator::new(path).expect("Could not create iterator");

    iter.for_each(|item| {
        info!("Element: {}", item.str_type());
    });
}

#[test]
fn iter_count() {
    let path = fixture_path(DISTRICT_OF_COLUMBIA);
    let iter = ElementIterator::new(path).expect("Could not create iterator");
    let now = Instant::now();

    let nodes = iter.map_red(
        |item| match item {
            Element::Way(_) => 0,
            Element::Node(_) => 1,
            Element::Relation(_) => 0,
            Element::DenseNodes(_) => 0,
        },
        |a, b| a + b,
        || 0,
    );

    println!("There are {nodes} nodes");
    println!("Took: {}ms", now.elapsed().as_micros() / 1000)
}

#[test]
fn processed_iter_count() {
    let path = fixture_path(DISTRICT_OF_COLUMBIA);
    let iter = ProcessedElementIterator::new(path).expect("Could not create iterator");

    let now = Instant::now();

    let nodes = iter.map_red(
        |item| match item {
            ProcessedElement::Node(_) => 1,
            _ => 0,
        },
        |a, b| a + b,
        || 0,
    );

    println!("There are {nodes} nodes");
    println!("Took: {}ms", now.elapsed().as_micros() / 1000)
}
