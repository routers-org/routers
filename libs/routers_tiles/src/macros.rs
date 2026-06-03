pub mod tile_macro {
    #[macro_export]
    macro_rules! tile {
        ( $vec:expr ) => {
            Ok(MVTTile(Tile::from($vec)))
        };
        ( $( $x:expr ),* $(,)? ) => {
            Ok(MVTTile(Tile::from(vec![ $( $x ),* ])))
        };
    }
}

pub mod layer_macro {
    #[macro_export]
    macro_rules! layer {
        ($c:expr, $z:ident, $t:literal) => {
            crate::MVTLayer::from(($c, $z, format!("{}", $t))).0
        };
        ($c:ident, $z:ident, $t:literal) => {
            MVTLayer::from(($c, $z, format!("{}", $t))).0
        };
    }
}

pub mod cluster_macro {
    #[macro_export]
    macro_rules! cluster {
        ($c:expr, $z:ident, $t:literal) => {
            vec![
                MVTLayer::from(($c.noise, $z, format!("{}", $t))).0,
                MVTLayer::from(($c.clustered, $z, format!("{}_cluster", $t))).0,
            ]
        };
    }
}
