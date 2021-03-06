/// Raw storage for the points that make up a glyph contour
use std::collections::HashSet;
use std::ops::Range;

use super::design_space::{DPoint, DVec2};
use super::point::{EntityId, PathPoint};

use druid::kurbo::{Affine, CubicBez, Line, ParamCurve, PathSeg};
use druid::Data;

#[derive(Debug, Clone, Data)]
pub struct PathPoints {
    path_id: EntityId,
    points: protected::RawPoints,
    trailing: Option<DPoint>,
    closed: bool,
}

/// A cursor for moving through a list of points.
pub struct Cursor<'a> {
    idx: Option<usize>,
    inner: &'a mut PathPoints,
}

/// A segment in a one-or-two parameter spline.
///
/// That is: this can be either part of a cubic bezier, or part of a hyperbezier.
///
/// We do not currently support quadratics.
#[derive(Clone, Copy, PartialEq)]
pub enum Segment {
    Line(PathPoint, PathPoint),
    Cubic(PathPoint, PathPoint, PathPoint, PathPoint),
}

/// A module to hide the implementation of the RawPoints type.
///
/// The motivation for this is simple: we want to be able to index into our
/// vec of points using `EntityId`s; to do this we need to keep a map from
/// those ids ot the actual indices in the underlying vec.
///
/// By hiding this implementation, we can ensure it is only used via the declared
/// API; in that API we can ensure we always keep our map up to date.
mod protected {
    use super::{EntityId, PathPoint};
    use druid::Data;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::sync::Arc;

    #[derive(Debug, Clone, Data)]
    pub(super) struct RawPoints {
        points: Arc<Vec<PathPoint>>,
        // these two use interior mutability so that we can rebuild the indices
        // in things like getters
        #[data(ignore)]
        indices: RefCell<Arc<HashMap<EntityId, usize>>>,
        #[data(ignore)]
        needs_to_rebuild_indicies: Cell<bool>,
    }

    impl RawPoints {
        pub(super) fn new(points: Vec<PathPoint>) -> Self {
            RawPoints {
                points: Arc::new(points),
                indices: RefCell::new(Arc::new(HashMap::new())),
                needs_to_rebuild_indicies: Cell::new(true),
            }
        }
        pub(super) fn len(&self) -> usize {
            self.points.len()
        }

        pub(super) fn is_empty(&self) -> bool {
            self.len() == 0
        }

        pub(super) fn as_ref(&self) -> &[PathPoint] {
            &self.points
        }

        /// All mutable access invalidates the index map. It should be
        /// avoided unless actual mutation is going to occur.
        pub(super) fn as_mut(&mut self) -> &mut Vec<PathPoint> {
            self.set_needs_rebuild();
            Arc::make_mut(&mut self.points)
        }

        fn set_needs_rebuild(&self) {
            self.needs_to_rebuild_indicies.set(true);
        }

        fn rebuild_if_needed(&self) {
            if self.needs_to_rebuild_indicies.replace(false) {
                let mut indices = self.indices.borrow_mut();
                let indices = Arc::make_mut(&mut *indices);
                indices.clear();
                indices.extend(self.points.iter().enumerate().map(|(i, pt)| (pt.id, i)))
            }
        }

        pub(super) fn index_for_point(&self, item: EntityId) -> Option<usize> {
            self.rebuild_if_needed();
            self.indices.borrow().get(&item).copied()
        }

        pub(super) fn get(&self, item: EntityId) -> Option<&PathPoint> {
            let idx = self.index_for_point(item)?;
            self.as_ref().get(idx)
        }

        /// update a point using a closure.
        ///
        /// This cannot remove the point, or change its id; this means we don't
        /// need to invalidate our indicies.
        pub(super) fn with_mut(&mut self, item: EntityId, f: impl FnOnce(&mut PathPoint)) {
            self.rebuild_if_needed();
            if let Some(idx) = self.index_for_point(item) {
                if let Some(val) = Arc::make_mut(&mut self.points).get_mut(idx) {
                    f(val);
                    val.id = item;
                }
            }
        }
    }
}

impl PathPoints {
    pub fn new(start_point: DPoint) -> Self {
        let path_id = EntityId::next();
        let start = PathPoint::on_curve(path_id, start_point);
        PathPoints {
            path_id,
            points: protected::RawPoints::new(vec![start]),
            closed: false,
            trailing: None,
        }
    }

    pub fn from_raw_parts(
        path_id: EntityId,
        mut points: Vec<PathPoint>,
        trailing: Option<DPoint>,
        closed: bool,
    ) -> Self {
        assert!(!points.is_empty(), "path may not be empty");
        assert!(
            points.iter().all(|pt| pt.id.is_child_of(path_id)),
            "{:#?}",
            points
        );
        if !closed {
            assert!(points.first().unwrap().is_on_curve());
        }
        // normalize incoming representation
        // if the path is closed, the last point should be an on-curve point,
        // and is considered the start of the path.
        if closed && !points.last().unwrap().is_on_curve() {
            // we assume there is at least one on-curve point. One day,
            // we will find out one day that this assumption was wrong.
            let rotate_distance = points.iter().position(|p| p.is_on_curve()).unwrap() + 1;

            points.rotate_left(rotate_distance);
        }

        PathPoints {
            path_id,
            points: protected::RawPoints::new(points),
            trailing,
            closed,
        }
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn closed(&self) -> bool {
        self.closed
    }

    pub fn id(&self) -> EntityId {
        self.path_id
    }

    pub fn trailing(&self) -> Option<&DPoint> {
        self.trailing.as_ref()
    }

    fn trailing_mut(&mut self) -> Option<&mut DPoint> {
        self.trailing.as_mut()
    }

    pub fn clear_trailing(&mut self) {
        self.trailing = None;
    }

    pub fn set_trailing(&mut self, trailing: DPoint) {
        self.trailing = Some(trailing);
    }

    pub fn as_slice(&self) -> &[PathPoint] {
        self.points.as_ref()
    }

    pub(crate) fn points_mut(&mut self) -> &mut Vec<PathPoint> {
        self.points.as_mut()
    }

    /// Iterates points in order.
    pub(crate) fn iter_points(&self) -> impl Iterator<Item = PathPoint> + '_ {
        let (first, remaining_n) = if self.closed {
            (self.points.as_ref().last().copied(), self.points.len() - 1)
        } else {
            (None, self.points.len())
        };

        first
            .into_iter()
            .chain(self.points.as_ref().iter().take(remaining_n).copied())
    }

    pub fn iter_segments(&self) -> Segments {
        let prev_pt = *self.start_point();
        let idx = if self.closed() { 0 } else { 1 };
        Segments {
            points: self.points.clone(),
            prev_pt,
            idx,
        }
    }

    /// Returns a cursor useful for modifying the path.
    ///
    /// If you pass a point id, the cursor will start at that point; if not
    /// it will start at the first point.
    pub fn cursor(&mut self, id: Option<EntityId>) -> Cursor {
        let idx = match id {
            Some(id) => self.points.index_for_point(id),
            None if self.closed => self.len().checked_sub(1),
            None if self.points.is_empty() => None,
            None => Some(0),
        };
        Cursor { idx, inner: self }
    }

    pub fn close(&mut self) -> EntityId {
        assert!(!self.closed);
        self.points_mut().rotate_left(1);
        self.closed = true;
        self.points.as_ref().last().unwrap().id
    }

    pub(crate) fn reverse_contour(&mut self) {
        let last = if self.closed {
            self.points.len() - 1
        } else {
            self.points.len()
        };
        self.points_mut()[..last].reverse();
    }

    fn first_idx(&self) -> usize {
        if self.closed {
            self.len() - 1
        } else {
            0
        }
    }

    /// Push a new on-curve point onto the end of the point list.
    ///
    /// The points must not be closed.
    pub fn push_on_curve(&mut self, point: DPoint) -> EntityId {
        assert!(!self.closed);
        let point = PathPoint::on_curve(self.path_id, point);
        self.points_mut().push(point);
        point.id
    }

    pub fn split_segment(&mut self, old: Segment, pre: Segment, post: Segment) {
        let insert_idx = bail!(self
            .points
            .index_for_point(old.start_id())
            .and_then(|idx| self.next_idx(idx)));

        let (existing_control_pts, points_to_insert) = match old {
            Segment::Line(..) => (0, 1),
            Segment::Cubic(..) => (2, 5),
        };

        let iter = pre.into_iter().skip(1).chain(post.into_iter().skip(1));
        let self_id = self.id();
        self.points.as_mut().splice(
            insert_idx..insert_idx + existing_control_pts,
            iter.take(points_to_insert).map(|mut next_pt| {
                next_pt.reparent(self_id);
                next_pt
            }),
        );
    }

    pub fn replace_segment(&mut self, old: Segment, new: Segment) {
        let insert_idx = bail!(self.points.index_for_point(old.start_id()));
        let seg_len = match old {
            Segment::Line(..) => 1,
            Segment::Cubic(..) => 3,
        };
        self.points
            .as_mut()
            .splice(insert_idx..insert_idx + seg_len, new.into_iter());
    }

    fn prev_idx(&self, idx: usize) -> Option<usize> {
        if self.closed() {
            Some(((self.len() + idx) - 1) % self.len())
        } else {
            idx.checked_sub(1)
        }
    }

    fn next_idx(&self, idx: usize) -> Option<usize> {
        if self.closed() {
            Some((idx + 1) % self.len())
        } else if idx < self.len() - 1 {
            Some(idx + 1)
        } else {
            None
        }
    }

    pub(crate) fn path_point_for_id(&self, point: EntityId) -> Option<PathPoint> {
        assert!(point.is_child_of(self.path_id));
        self.points.get(point).copied()
    }

    pub(crate) fn prev_point(&self, point: EntityId) -> Option<PathPoint> {
        assert!(point.is_child_of(self.path_id));
        self.points
            .index_for_point(point)
            .and_then(|idx| self.prev_idx(idx))
            .and_then(|idx| self.points.as_ref().get(idx))
            .copied()
    }

    pub(crate) fn next_point(&self, point: EntityId) -> Option<PathPoint> {
        assert!(point.is_child_of(self.path_id));
        self.points
            .index_for_point(point)
            .and_then(|idx| self.next_idx(idx))
            .and_then(|idx| self.points.as_ref().get(idx))
            .copied()
    }

    pub fn start_point(&self) -> &PathPoint {
        assert!(!self.points.is_empty(), "empty path is not constructable");
        self.points.as_ref().get(self.first_idx()).unwrap()
    }

    /// The trailing on-curve point, if this path is not closed.
    pub fn trailing_point_in_open_path(&self) -> Option<&PathPoint> {
        if !self.closed {
            self.points.as_ref().last()
        } else {
            None
        }
    }

    /// Returns the 'last' on-curve point.
    ///
    /// In a closed path, this is the on-curve point preceding the start point.
    /// In an open path, this is the point at the end of the array.
    /// In a length-one path, this is the only point.
    pub(crate) fn last_on_curve_point(&self) -> PathPoint {
        assert!(!self.points.is_empty(), "empty path is not constructable");
        let idx = if self.closed {
            self.points.len().saturating_sub(2)
        } else {
            self.points.len() - 1
        };
        self.points.as_ref()[idx]
    }

    pub fn transform_all(&mut self, affine: Affine, anchor: DPoint) {
        let anchor = anchor.to_dvec2();
        self.points_mut()
            .iter_mut()
            .for_each(|pt| pt.transform(affine, anchor));

        if let Some(trailing) = self.trailing_mut() {
            //FIXME: what about the anchor?
            let new_trailing = affine * trailing.to_raw();
            *trailing = DPoint::from_raw(new_trailing);
        }
    }

    /// Apply the provided transform to all selected points, updating handles as
    /// appropriate.
    ///
    /// The `anchor` argument is a point that should be treated as the origin
    /// when applying the transform, which is used for things like scaling from
    /// a fixed point.
    pub fn transform_points(&mut self, points: &[EntityId], affine: Affine, anchor: DPoint) {
        let to_xform = self.points_for_points(points);
        let anchor = anchor.to_dvec2();
        for point in &to_xform {
            self.points
                .with_mut(*point, |pt| pt.transform(affine, anchor));
            if let Some((on_curve, handle)) = self.tangent_handle(*point) {
                if !to_xform.contains(&handle) {
                    self.adjust_handle_angle(*point, on_curve, handle);
                }
            }
        }
    }

    /// For a list of points, returns a set including those points and any
    /// adjacent off-curve points.
    fn points_for_points(&mut self, points: &[EntityId]) -> HashSet<EntityId> {
        let mut to_xform = HashSet::new();
        for point in points {
            let cursor = self.cursor(Some(*point));
            if cursor.point().is_some() {
                to_xform.insert(*point);
                if let Some(prev) = cursor.peek_prev().filter(|pp| pp.is_off_curve()) {
                    to_xform.insert(prev.id);
                }

                if let Some(next) = cursor.peek_next().filter(|pp| pp.is_off_curve()) {
                    to_xform.insert(next.id);
                }
            }
        }
        to_xform
    }

    pub fn update_handle(&mut self, bcp1: EntityId, mut dpt: DPoint, is_locked: bool) {
        if let Some((on_curve, bcp2)) = self.tangent_handle_opt(bcp1) {
            if is_locked {
                dpt = dpt.axis_locked_to(bail!(self.points.get(on_curve)).point);
            }
            self.points.with_mut(bcp1, |p| p.point = dpt);
            if let Some(bcp2) = bcp2 {
                self.adjust_handle_angle(bcp1, on_curve, bcp2);
            }
        }
    }

    /// Update a tangent handle in response to the movement of the partner handle.
    /// `bcp1` is the handle that has moved, and `bcp2` is the handle that needs
    /// to be adjusted.
    fn adjust_handle_angle(&mut self, bcp1: EntityId, on_curve: EntityId, bcp2: EntityId) {
        let p1 = bail!(self.points.get(bcp1));
        let p2 = bail!(self.points.get(on_curve));
        let p3 = bail!(self.points.get(bcp2));
        let raw_angle = (p1.point - p2.point).to_raw();
        if raw_angle.hypot() == 0.0 {
            return;
        }

        // that angle is in the opposite direction, so flip it
        let norm_angle = raw_angle.normalize() * -1.0;
        let handle_len = (p3.point - p2.point).hypot();

        let new_handle_offset = DVec2::from_raw(norm_angle * handle_len);
        let new_pos = p2.point + new_handle_offset;
        self.points.with_mut(bcp2, |pt| pt.point = new_pos)
    }

    /// Given the idx of an off-curve point, check if that point has a tangent
    /// handle; that is, if the nearest on-curve point's other neighbour is
    /// also an off-curve point, and the on-curve point is smooth.
    ///
    /// Returns the index for the on_curve point and the 'other' handle
    /// for an offcurve point, if it exists.
    fn tangent_handle(&mut self, point: EntityId) -> Option<(EntityId, EntityId)> {
        if let Some((on_curve, Some(bcp2))) = self.tangent_handle_opt(point) {
            Some((on_curve, bcp2))
        } else {
            None
        }
    }

    /// Given the idx of an off-curve point, return its neighbouring on-curve
    /// point; if that point is smooth and its other neighbour is also an
    /// off-curve, it returns that as well.
    fn tangent_handle_opt(&mut self, point: EntityId) -> Option<(EntityId, Option<EntityId>)> {
        let cursor = self.cursor(Some(point));
        if cursor.point().map(|pp| pp.is_off_curve()).unwrap_or(false) {
            let on_curve = cursor
                .peek_next()
                .filter(|p| p.is_on_curve())
                .or_else(|| cursor.peek_prev().filter(|p| p.is_on_curve()))
                .copied()
                .unwrap(); // all off curve points have one on_curve neighbour
            if on_curve.is_smooth() {
                let cursor = self.cursor(Some(on_curve.id));
                let other_off_curve = cursor
                    .peek_next()
                    .filter(|p| p.is_off_curve() && p.id != point)
                    .or_else(|| {
                        cursor
                            .peek_prev()
                            .filter(|p| p.is_off_curve() && p.id != point)
                    })
                    .map(|p| p.id);
                Some((on_curve.id, other_off_curve))
            } else {
                Some((on_curve.id, None))
            }
        } else {
            None
        }
    }

    pub fn delete_points(&mut self, points: &[EntityId]) {
        eprintln!("deleting {:?}", points);
        let mut to_delete = HashSet::with_capacity(points.len());
        for point in points {
            self.points_to_delete(*point, &mut to_delete)
        }

        self.points_mut().retain(|p| !to_delete.contains(&p.id));

        // normalize our representation
        let len = self.points.len();
        if len > 2
            && !self.points.as_ref()[0].is_on_curve()
            && !self.points.as_ref()[len - 1].is_on_curve()
        {
            self.points_mut().rotate_left(1);
        }

        // if we have fewer than three on_curve points we are open.
        if self.points.len() < 3 {
            self.closed = false;
        }

        // fixup smooth/corner types
        let mut cursor = self.cursor(None);
        let start_id = cursor.point().map(|pp| pp.id);

        loop {
            let next_is_on = cursor.peek_next().map(PathPoint::is_on_curve);
            let prev_is_on = cursor.peek_prev().map(PathPoint::is_on_curve);

            if cursor.point().map(|pp| pp.is_smooth()).unwrap_or(false)
                && prev_is_on.unwrap_or(false)
                && next_is_on.unwrap_or(false)
            {
                cursor.point_mut().unwrap().toggle_type();
            }

            cursor.move_next();
            if cursor.point().map(|pp| pp.id) == start_id {
                break;
            }
        }
    }

    //FIXME: this is currently buggy :(
    fn points_to_delete(&mut self, point_id: EntityId, to_delete: &mut HashSet<EntityId>) {
        let (point, prev, next) = {
            let cursor = self.cursor(Some(point_id));
            // if *this* point doesn't exist we should bail
            let point = bail!(cursor.point().copied());
            (
                point,
                cursor.peek_prev().copied(),
                cursor.peek_next().copied(),
            )
        };

        let prev_and_next_both_off_curve = prev.map(|pp| pp.is_off_curve()).unwrap_or(false)
            && next.map(|pp| pp.is_off_curve()).unwrap_or(false);

        to_delete.insert(point.id);
        if point.is_off_curve() {
            if let Some(other_off_curve) = prev
                .filter(|pp| pp.is_off_curve())
                .or_else(|| next.filter(|pp| pp.is_off_curve()))
            {
                to_delete.insert(other_off_curve.id);
            }
        } else if prev_and_next_both_off_curve {
            to_delete.extend(prev.map(|pp| pp.id));
            to_delete.extend(next.map(|pp| pp.id));
        }
    }

    pub fn last_segment_is_curve(&self) -> bool {
        let len = self.points.len();
        len > 2 && !self.points.as_ref()[len - 2].is_on_curve()
    }
}

impl Cursor<'_> {
    pub fn point(&self) -> Option<&PathPoint> {
        self.idx.map(|idx| &self.inner.points.as_ref()[idx])
    }

    pub fn point_mut(&mut self) -> Option<&mut PathPoint> {
        let idx = self.idx?;
        self.inner.points.as_mut().get_mut(idx)
    }

    pub fn move_next(&mut self) {
        self.idx = self.idx.and_then(|idx| self.inner.next_idx(idx))
    }

    pub fn move_prev(&mut self) {
        if let Some(idx) = self.idx {
            self.idx = self.inner.prev_idx(idx);
        }
    }

    pub fn peek_next(&self) -> Option<&PathPoint> {
        self.idx
            .and_then(|idx| self.inner.next_idx(idx))
            .and_then(|idx| self.inner.points.as_ref().get(idx))
    }

    pub fn peek_prev(&self) -> Option<&PathPoint> {
        self.idx
            .and_then(|idx| self.inner.prev_idx(idx))
            .and_then(|idx| self.inner.points.as_ref().get(idx))
    }
}

impl Segment {
    pub(crate) fn start(&self) -> PathPoint {
        match self {
            Segment::Line(p1, _) => *p1,
            Segment::Cubic(p1, ..) => *p1,
        }
    }

    pub(crate) fn end(&self) -> PathPoint {
        match self {
            Segment::Line(_, p2) => *p2,
            Segment::Cubic(.., p2) => *p2,
        }
    }

    pub(crate) fn start_id(&self) -> EntityId {
        self.start().id
    }

    pub(crate) fn end_id(&self) -> EntityId {
        self.end().id
    }

    pub(crate) fn points(&self) -> impl Iterator<Item = PathPoint> {
        let mut idx = 0;
        let seg = *self;
        std::iter::from_fn(move || {
            idx += 1;
            match (&seg, idx) {
                (_, 1) => Some(seg.start()),
                (Segment::Line(_, p2), 2) => Some(*p2),
                (Segment::Cubic(_, p2, _, _), 2) => Some(*p2),
                (Segment::Cubic(_, _, p3, _), 3) => Some(*p3),
                (Segment::Cubic(_, _, _, p4), 4) => Some(*p4),
                _ => None,
            }
        })
    }

    // FIXME: why a vec? was I just lazy?
    pub(crate) fn ids(&self) -> Vec<EntityId> {
        match self {
            Segment::Line(p1, p2) => vec![p1.id, p2.id],
            Segment::Cubic(p1, p2, p3, p4) => vec![p1.id, p2.id, p3.id, p4.id],
        }
    }

    pub(crate) fn to_kurbo(self) -> PathSeg {
        match self {
            Segment::Line(p1, p2) => PathSeg::Line(Line::new(p1.point.to_raw(), p2.point.to_raw())),
            Segment::Cubic(p1, p2, p3, p4) => PathSeg::Cubic(CubicBez::new(
                p1.point.to_raw(),
                p2.point.to_raw(),
                p3.point.to_raw(),
                p4.point.to_raw(),
            )),
        }
    }

    pub(crate) fn subsegment(self, range: Range<f64>) -> Self {
        let subseg = self.to_kurbo().subsegment(range);
        let path_id = self.start_id().parent();
        match subseg {
            PathSeg::Line(Line { p0, p1 }) => Segment::Line(
                PathPoint::on_curve(path_id, DPoint::from_raw(p0)),
                PathPoint::on_curve(path_id, DPoint::from_raw(p1)),
            ),
            PathSeg::Cubic(CubicBez { p0, p1, p2, p3 }) => {
                let p0 = PathPoint::on_curve(path_id, DPoint::from_raw(p0));
                let p1 = PathPoint::off_curve(path_id, DPoint::from_raw(p1));
                let p2 = PathPoint::off_curve(path_id, DPoint::from_raw(p2));
                let p3 = PathPoint::on_curve(path_id, DPoint::from_raw(p3));
                Segment::Cubic(p0, p1, p2, p3)
            }
            PathSeg::Quad(_) => panic!("quads are not supported"),
        }
    }
}

/// An iterator over the segments in a path list.
pub struct Segments {
    points: protected::RawPoints,
    prev_pt: PathPoint,
    idx: usize,
}

impl Iterator for Segments {
    type Item = Segment;

    fn next(&mut self) -> Option<Segment> {
        if self.idx >= self.points.len() {
            return None;
        }
        let seg_start = self.prev_pt;
        let seg = if !self.points.as_ref()[self.idx].is_on_curve() {
            let p1 = self.points.as_ref()[self.idx];
            let p2 = self.points.as_ref()[self.idx + 1];
            self.prev_pt = self.points.as_ref()[self.idx + 2];
            self.idx += 3;
            assert!(
                self.prev_pt.typ.is_on_curve(),
                "{:#?} idx{}",
                &self.points,
                self.idx
            );
            Segment::Cubic(seg_start, p1, p2, self.prev_pt)
        } else {
            self.prev_pt = self.points.as_ref()[self.idx];
            self.idx += 1;
            Segment::Line(seg_start, self.prev_pt)
        };
        Some(seg)
    }
}

/// An iterator over the points in a segment
pub struct SegmentPointIter {
    seg: Segment,
    idx: usize,
}

impl std::iter::IntoIterator for Segment {
    type Item = PathPoint;
    type IntoIter = SegmentPointIter;
    fn into_iter(self) -> Self::IntoIter {
        SegmentPointIter { seg: self, idx: 0 }
    }
}

impl Iterator for SegmentPointIter {
    type Item = PathPoint;

    fn next(&mut self) -> Option<PathPoint> {
        self.idx += 1;
        match (self.idx, self.seg) {
            (1, Segment::Line(p1, _)) => Some(p1),
            (2, Segment::Line(_, p2)) => Some(p2),
            (1, Segment::Cubic(p1, ..)) => Some(p1),
            (2, Segment::Cubic(_, p2, ..)) => Some(p2),
            (3, Segment::Cubic(_, _, p3, ..)) => Some(p3),
            (4, Segment::Cubic(_, _, _, p4)) => Some(p4),
            _ => None,
        }
    }
}

impl std::fmt::Debug for Segment {
    #[allow(clippy::many_single_char_names)]
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Segment::Line(one, two) => write!(
                f,
                "({}->{}) Line({:?}, {:?})",
                self.start_id(),
                self.end_id(),
                one.point,
                two.point
            ),
            Segment::Cubic(a, b, c, d) => write!(
                f,
                "Cubic({:?}, {:?}, {:?}, {:?})",
                a.point, b.point, c.point, d.point
            ),
        }
    }
}
