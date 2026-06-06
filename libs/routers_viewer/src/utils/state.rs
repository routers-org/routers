use std::cell::RefCell;

use geo::Coord;
use wkt::ToWkt as _;

use super::MatchData;

#[derive(Default)]
pub struct DrawState {
    pub active: RefCell<bool>,
    pub points: RefCell<Vec<Coord>>,
}

impl DrawState {
    pub fn is_active(&self) -> bool {
        *self.active.borrow()
    }

    pub fn enter(&self) {
        *self.active.borrow_mut() = true;
        self.points.borrow_mut().clear();
    }

    pub fn exit(&self) {
        *self.active.borrow_mut() = false;
    }

    pub fn commit(&self, coord: Coord) -> Option<geo::LineString> {
        self.points.borrow_mut().push(coord);
        let pts = self.points.borrow();
        (pts.len() >= 2).then(|| geo::LineString::new(pts.clone()))
    }
}

#[derive(Default)]
pub struct CursorState {
    pub pos: RefCell<Option<Coord>>,
    pub preview: RefCell<Option<MatchData>>,
}

impl CursorState {
    pub fn pos(&self) -> Option<Coord> {
        *self.pos.borrow()
    }

    pub fn set(&self, coord: Coord) {
        *self.pos.borrow_mut() = Some(coord);
    }

    pub fn clear(&self) {
        *self.pos.borrow_mut() = None;
        *self.preview.borrow_mut() = None;
    }

    pub fn set_preview(&self, data: Option<MatchData>) {
        *self.preview.borrow_mut() = data;
    }

    pub fn take_preview(&self) -> Option<MatchData> {
        self.preview.borrow_mut().take()
    }
}

#[derive(Default)]
pub struct SelectionState {
    pub layer: RefCell<Option<usize>>,
    pub candidate: RefCell<Option<usize>>,
}

impl SelectionState {
    pub fn clear(&self) {
        *self.layer.borrow_mut() = None;
        *self.candidate.borrow_mut() = None;
    }
}

#[derive(Default)]
pub struct ResultState {
    pub data: RefCell<Option<MatchData>>,
    pub error: RefCell<Option<String>>,
}

impl ResultState {
    pub fn set(&self, data: MatchData) {
        *self.data.borrow_mut() = Some(data);
        *self.error.borrow_mut() = None;
    }

    pub fn set_error(&self, msg: String) {
        *self.error.borrow_mut() = Some(msg);
    }

    pub fn clear(&self) {
        *self.data.borrow_mut() = None;
        *self.error.borrow_mut() = None;
    }
}

#[derive(Default)]
pub struct State {
    pub input: RefCell<String>,
    pub draw: DrawState,
    pub cursor: CursorState,
    pub selection: SelectionState,
    pub result: ResultState,
}

impl State {
    pub fn exit_draw_mode(&self) {
        if let Some(preview) = self.cursor.take_preview() {
            self.result.set(preview);
        }
        self.draw.exit();
        self.cursor.clear();
    }

    pub fn commit_drawn_point(&self, coord: Coord) {
        if let Some(ls) = self.draw.commit(coord) {
            *self.input.borrow_mut() = ls.wkt_string();
        }
    }
}
