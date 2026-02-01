//! The `Element` item, provides distinction for
//! Nodes, DenseNodes, ... by reference to their
//! derived item, in the primitive entity.

use crate::osm;
use crate::osm::element::variants::{OsmEntryId, Relation, Way};
use routers_network::Node;
use std::vec;
#[cfg(feature = "tracing")]
use tracing::debug;

#[derive(Clone)]
pub enum Element<'a> {
    Node(&'a osm::Node),
    Way(&'a osm::Way),
    DenseNodes(&'a osm::DenseNodes),
    Relation(&'a osm::Relation),
}

#[derive(Clone)]
pub enum ProcessedElement {
    Node(Node<OsmEntryId>),
    Way(Way),
    Relation(Relation),
}

impl ProcessedElement {
    #[inline]
    pub(crate) fn from_raw(element: Element, block: &osm::PrimitiveBlock) -> Vec<ProcessedElement> {
        #[cfg(feature = "tracing")]
        if block.lat_offset.is_some() || block.lon_offset.is_some() || block.granularity.is_some() {
            debug!(
                "BlockHasOffset! +Lon={:?}, +Lat={:?}, Granularity={:?}",
                block.lon_offset, block.lat_offset, block.granularity
            );
        }

        // Default Scaling Factor: https://wiki.openstreetmap.org/wiki/PBF_Format
        let granularity = block.granularity.unwrap_or(100);

        match element {
            Element::DenseNodes(dense_nodes) => dense_nodes
                .nodes(granularity)
                .map(ProcessedElement::Node)
                .collect(),
            Element::Node(node) => vec![ProcessedElement::Node(Node::from(node))],
            Element::Way(way) => vec![ProcessedElement::Way(Way::from_raw(way, block))],
            Element::Relation(_relation) => {
                vec![]
                // ProcessedElement::Relation(Relation::from_raw(
                //     relation, block,
                // ))
            }
        }
    }
}

impl<'a> Element<'a> {
    #[inline]
    pub(crate) fn from_group(group: &'a osm::PrimitiveGroup) -> Vec<Element<'a>> {
        let mut elements: Vec<Element<'a>> = Vec::new();

        elements.extend(group.ways.iter().map(Element::Way));
        elements.extend(group.nodes.iter().map(Element::Node));
        elements.extend(group.relations.iter().map(Element::Relation));

        if let Some(nodes) = &group.dense {
            elements.push(Element::DenseNodes(nodes));
        }

        elements
    }

    pub fn str_type(&self) -> &str {
        match self {
            Element::Node(_) => "node",
            Element::Way(_) => "way",
            Element::Relation(_) => "relation",
            Element::DenseNodes(_) => "node set",
        }
    }
}
