//! Unified display layout: every machine's displays in one coordinate space, in logical
//! points, like macOS arranges its own displays. Each machine's displays move as a rigid group
//! (an offset from its local coordinates); the cursor crosses wherever displays of different
//! groups touch.

use serde::{Deserialize, Serialize};

use crate::proto::DisplayInfo;

/// Displays closer than this (in points) count as touching.
const TOUCH: f64 = 1.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn offset(self, by: Point) -> Point {
        Point::new(self.x + by.x, self.y + by.y)
    }

    pub fn minus(self, by: Point) -> Point {
        Point::new(self.x - by.x, self.y - by.y)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f64 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }

    pub fn centre(&self) -> Point {
        Point::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// Half-open: the right and bottom edges belong to the neighbour.
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }

    /// Nearest point inside the rectangle.
    pub fn clamp(&self, p: Point) -> Point {
        // max/min rather than f64::clamp, which panics on tiny rects or NaN.
        Point::new(
            p.x.max(self.x).min((self.right() - 1.0).max(self.x)),
            p.y.max(self.y).min((self.bottom() - 1.0).max(self.y)),
        )
    }

    pub fn translate(&self, by: Point) -> Rect {
        Rect::new(self.x + by.x, self.y + by.y, self.w, self.h)
    }

    /// True if the rectangles share any area (touching edges don't count).
    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    pub fn union(&self, other: &Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Rect::new(
            x,
            y,
            self.right().max(other.right()) - x,
            self.bottom().max(other.bottom()) - y,
        )
    }

    pub fn bounding(rects: impl IntoIterator<Item = Rect>) -> Option<Rect> {
        rects.into_iter().reduce(|a, b| a.union(&b))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Above,
    Below,
}

impl Side {
    pub const ALL: [Side; 4] = [Side::Left, Side::Right, Side::Above, Side::Below];
}

impl std::str::FromStr for Side {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "left" => Side::Left,
            "right" => Side::Right,
            "above" | "top" | "up" => Side::Above,
            "below" | "bottom" | "down" => Side::Below,
            _ => anyhow::bail!("side must be left, right, above or below"),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Machine {
    pub id: String,
    pub displays: Vec<DisplayInfo>,
    /// Added to machine-local coordinates to get unified coordinates.
    pub offset: Point,
}

impl Machine {
    pub fn bounds(&self) -> Option<Rect> {
        Rect::bounding(self.displays.iter().map(|d| d.rect))
    }

    pub fn primary(&self) -> Option<usize> {
        self.displays
            .iter()
            .position(|d| d.primary)
            .or((!self.displays.is_empty()).then_some(0))
    }
}

/// A display within the layout: (machine index, display index).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayRef {
    pub machine: usize,
    pub display: usize,
}

/// Where the cursor lands after crossing an edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Crossing {
    pub to: DisplayRef,
    /// Unified coordinates, just inside the destination display.
    pub point: Point,
}

#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub machines: Vec<Machine>,
}

impl Layout {
    pub fn machine_index(&self, id: &str) -> Option<usize> {
        self.machines.iter().position(|m| m.id == id)
    }

    pub fn rect(&self, r: DisplayRef) -> Rect {
        let m = &self.machines[r.machine];
        m.displays[r.display].rect.translate(m.offset)
    }

    pub fn to_local(&self, machine: usize, p: Point) -> Point {
        p.minus(self.machines[machine].offset)
    }

    fn displays(&self) -> impl Iterator<Item = DisplayRef> + '_ {
        self.machines.iter().enumerate().flat_map(|(mi, m)| {
            (0..m.displays.len()).map(move |di| DisplayRef {
                machine: mi,
                display: di,
            })
        })
    }

    /// The display on `machine` containing `p` (unified coordinates).
    pub fn locate_on(&self, machine: usize, p: Point) -> Option<DisplayRef> {
        self.displays()
            .filter(|r| r.machine == machine)
            .find(|r| self.rect(*r).contains(p))
    }

    /// The display on `machine` nearest to `p`.
    pub fn nearest_on(&self, machine: usize, p: Point) -> Option<DisplayRef> {
        self.displays()
            .filter(|r| r.machine == machine)
            .min_by(|a, b| {
                let da = dist2(self.rect(*a).clamp(p), p);
                let db = dist2(self.rect(*b).clamp(p), p);
                da.total_cmp(&db)
            })
    }

    /// Where pushing off `from`'s edge `side` at position `along` (y for left/right, x for
    /// above/below) leads.
    ///
    /// A display touching that part of the edge wins. Otherwise the cursor jumps any gap to
    /// the nearest display of *another computer* in that direction that spans `along`: desks
    /// rarely line screens up exactly, and "off the left of this screen goes to the computer
    /// on the left" is what people expect.
    pub fn neighbour(&self, from: DisplayRef, side: Side, along: f64) -> Option<Crossing> {
        self.touching(from, side, along)
            .or_else(|| self.across_gap(from, side, along))
    }

    fn touching(&self, from: DisplayRef, side: Side, along: f64) -> Option<Crossing> {
        let a = self.rect(from);
        self.displays().filter(|r| *r != from).find_map(|r| {
            let b = self.rect(r);
            let touching = match side {
                Side::Left => (b.right() - a.x).abs() <= TOUCH,
                Side::Right => (b.x - a.right()).abs() <= TOUCH,
                Side::Above => (b.bottom() - a.y).abs() <= TOUCH,
                Side::Below => (b.y - a.bottom()).abs() <= TOUCH,
            };
            (touching && spans(&b, side, along)).then(|| Crossing {
                to: r,
                point: entry(&b, side, along),
            })
        })
    }

    fn across_gap(&self, from: DisplayRef, side: Side, along: f64) -> Option<Crossing> {
        let a = self.rect(from);
        self.displays()
            .filter(|r| r.machine != from.machine)
            .filter_map(|r| {
                let b = self.rect(r);
                let gap = match side {
                    Side::Left => a.x - b.right(),
                    Side::Right => b.x - a.right(),
                    Side::Above => a.y - b.bottom(),
                    Side::Below => b.y - a.bottom(),
                };
                (gap >= -TOUCH && spans(&b, side, along)).then_some((gap, r, b))
            })
            .min_by(|x, y| x.0.total_cmp(&y.0))
            .map(|(_, r, b)| Crossing {
                to: r,
                point: entry(&b, side, along),
            })
    }

    /// Which sides of `machine`'s displays lead to another computer somewhere along them
    /// (display index, side). A capture backend watches these edges.
    pub fn exit_sides(&self, machine: usize) -> Vec<(usize, Side)> {
        let mut out = vec![];
        let Some(m) = self.machines.get(machine) else {
            return out;
        };
        for d in 0..m.displays.len() {
            let from = DisplayRef {
                machine,
                display: d,
            };
            for side in Side::ALL {
                if self.side_leads(from, side, |m| m != machine) {
                    out.push((d, side));
                }
            }
        }
        out
    }

    /// Does pushing off some part of `from`'s `side` edge reach a machine matching `to`?
    fn side_leads(&self, from: DisplayRef, side: Side, to: impl Fn(usize) -> bool) -> bool {
        let a = self.rect(from);
        let (lo, hi) = match side {
            Side::Left | Side::Right => (a.y, a.bottom()),
            Side::Above | Side::Below => (a.x, a.right()),
        };
        let mut v = lo;
        while v < hi {
            if self
                .neighbour(from, side, v)
                .is_some_and(|c| to(c.to.machine))
            {
                return true;
            }
            v += 4.0;
        }
        false
    }

    /// Is machine `m` somewhere the cursor can use: overlapping nothing, and reachable straight
    /// from machine `anchor`?
    pub fn well_placed(&self, m: usize, anchor: usize) -> bool {
        !self.overlaps(m, self.machines[m].offset)
            && (0..self.machines[anchor].displays.len()).any(|d| {
                let from = DisplayRef {
                    machine: anchor,
                    display: d,
                };
                Side::ALL
                    .into_iter()
                    .any(|side| self.side_leads(from, side, |to| to == m))
            })
    }

    /// Stretches of display edge where the cursor passes to another computer, for drawing in
    /// the arrangement view. Each is (start, end) in layout coordinates.
    pub fn crossing_edges(&self) -> Vec<(Point, Point)> {
        let edges = self.edges_where(|from, to| from != to);
        edges.into_iter().map(|(_, _, a, b)| (a, b)).collect()
    }

    /// Stretches of machine `a`'s display edges that lead to machine `b`.
    pub fn edges_between(&self, a: usize, b: usize) -> Vec<(Point, Point)> {
        let edges = self.edges_where(|from, to| from == a && to == b);
        edges.into_iter().map(|(_, _, a, b)| (a, b)).collect()
    }

    /// Stretches of `machine`'s display edges that lead to another computer, with the display
    /// (its index) and side each is on: where a capture backend that sets barriers puts them.
    pub fn exit_edges(&self, machine: usize) -> Vec<(usize, Side, Point, Point)> {
        let edges = self.edges_where(|from, to| from == machine && to != machine);
        edges
            .into_iter()
            .map(|(d, side, a, b)| (d.display, side, a, b))
            .collect()
    }

    /// Stretches of display edge leading from one machine to another, for which `leads`
    /// (from machine, to machine) holds.
    fn edges_where(
        &self,
        leads: impl Fn(usize, usize) -> bool,
    ) -> Vec<(DisplayRef, Side, Point, Point)> {
        const STEP: f64 = 2.0;
        let mut out = vec![];
        for from in self.displays().collect::<Vec<_>>() {
            let a = self.rect(from);
            for side in Side::ALL {
                let (lo, hi) = match side {
                    Side::Left | Side::Right => (a.y, a.bottom()),
                    Side::Above | Side::Below => (a.x, a.right()),
                };
                let at = |v: f64| match side {
                    Side::Left => Point::new(a.x, v),
                    Side::Right => Point::new(a.right(), v),
                    Side::Above => Point::new(v, a.y),
                    Side::Below => Point::new(v, a.bottom()),
                };
                let mut run: Option<f64> = None;
                let mut v = lo;
                while v < hi + STEP {
                    let crosses = v < hi
                        && self
                            .neighbour(from, side, v)
                            .is_some_and(|c| leads(from.machine, c.to.machine));
                    match (crosses, run) {
                        (true, None) => run = Some(v),
                        (false, Some(start)) => {
                            out.push((from, side, at(start), at(v.min(hi))));
                            run = None;
                        }
                        _ => {}
                    }
                    v += STEP;
                }
            }
        }
        out
    }
    /// The valid position nearest to `desired` for machine `m`'s displays: touching at least
    /// one display of machine `anchor_machine` along a usable stretch of edge, and overlapping
    /// nothing. This is what dropping a machine in the arrangement view snaps to.
    pub fn snap(&self, m: usize, anchor_machine: usize, desired: Point) -> Option<Point> {
        const MIN_SHARED: f64 = 100.0;
        if !desired.x.is_finite() || !desired.y.is_finite() {
            return None;
        }
        let fit = |v: f64, lo: f64, hi: f64| v.max(lo).min(hi.max(lo));
        let group = &self.machines[m];
        let anchors: Vec<Rect> = self.machines[anchor_machine]
            .displays
            .iter()
            .map(|d| d.rect.translate(self.machines[anchor_machine].offset))
            .collect();
        let mut best: Option<(f64, Point)> = None;
        for a in &anchors {
            for b in group.displays.iter().map(|d| d.rect) {
                // `b` is in m-local coordinates; offset o puts it at b + o.
                let want = Point::new(desired.x + b.x, desired.y + b.y);
                let min_x = a.x - b.w + MIN_SHARED.min(b.w).min(a.w);
                let max_x = a.right() - MIN_SHARED.min(b.w).min(a.w);
                let min_y = a.y - b.h + MIN_SHARED.min(b.h).min(a.h);
                let max_y = a.bottom() - MIN_SHARED.min(b.h).min(a.h);
                let candidates = [
                    Point::new(a.x - b.w, fit(want.y, min_y, max_y)),
                    Point::new(a.right(), fit(want.y, min_y, max_y)),
                    Point::new(fit(want.x, min_x, max_x), a.y - b.h),
                    Point::new(fit(want.x, min_x, max_x), a.bottom()),
                ];
                for c in candidates {
                    let offset = Point::new(c.x - b.x, c.y - b.y);
                    if self.overlaps(m, offset) {
                        continue;
                    }
                    let d = dist2(offset, desired);
                    if best.is_none_or(|(bd, _)| d < bd) {
                        best = Some((d, offset));
                    }
                }
            }
        }
        best.map(|(_, p)| p)
    }

    /// Would machine `m` at `offset` overlap any other machine's displays?
    fn overlaps(&self, m: usize, offset: Point) -> bool {
        let mine: Vec<Rect> = self.machines[m]
            .displays
            .iter()
            .map(|d| d.rect.translate(offset))
            .collect();
        self.machines
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != m)
            .flat_map(|(_, other)| {
                other
                    .displays
                    .iter()
                    .map(|d| d.rect.translate(other.offset))
            })
            .any(|r| mine.iter().any(|a| a.intersects(&r)))
    }

    /// Offset that puts machine `m` beside display `anchor`, centred along the shared edge.
    pub fn offset_beside(&self, anchor: DisplayRef, m: usize, side: Side) -> Option<Point> {
        let a = self.rect(anchor);
        let b = self.machines[m].bounds()?;
        let (ac, bc) = (a.centre(), b.centre());
        Some(match side {
            Side::Left => Point::new(a.x - b.right(), ac.y - bc.y),
            Side::Right => Point::new(a.right() - b.x, ac.y - bc.y),
            Side::Above => Point::new(ac.x - bc.x, a.y - b.bottom()),
            Side::Below => Point::new(ac.x - bc.x, a.bottom() - b.y),
        })
    }
}

/// Does `b` cover position `along` on the axis running along a `side` edge?
fn spans(b: &Rect, side: Side, along: f64) -> bool {
    match side {
        Side::Left | Side::Right => along >= b.y && along < b.bottom(),
        Side::Above | Side::Below => along >= b.x && along < b.right(),
    }
}

/// Just inside `b`'s edge facing a cursor that arrives moving towards `side`.
fn entry(b: &Rect, side: Side, along: f64) -> Point {
    match side {
        Side::Left => Point::new(b.right() - 1.0, along),
        Side::Right => Point::new(b.x, along),
        Side::Above => Point::new(along, b.bottom() - 1.0),
        Side::Below => Point::new(along, b.y),
    }
}

fn dist2(a: Point, b: Point) -> f64 {
    (a.x - b.x).powi(2) + (a.y - b.y).powi(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn display(id: &str, x: f64, y: f64, w: f64, h: f64, primary: bool) -> DisplayInfo {
        DisplayInfo {
            id: id.into(),
            name: id.into(),
            rect: Rect::new(x, y, w, h),
            scale: 1.0,
            primary,
        }
    }

    /// A desk: MacBook (main) with an external display above it, iMac left of the MacBook.
    pub fn desk() -> Layout {
        let mut layout = Layout {
            machines: vec![
                Machine {
                    id: "mac".into(),
                    displays: vec![
                        display("builtin", 0.0, 0.0, 1512.0, 982.0, true),
                        display("dell", -587.0, -1440.0, 2560.0, 1440.0, false),
                    ],
                    offset: Point::default(),
                },
                Machine {
                    id: "imac".into(),
                    displays: vec![display("eDP-1", 0.0, 0.0, 1920.0, 1080.0, true)],
                    offset: Point::default(),
                },
            ],
        };
        let anchor = DisplayRef {
            machine: 0,
            display: 0,
        };
        layout.machines[1].offset = layout.offset_beside(anchor, 1, Side::Left).unwrap();
        layout
    }

    #[test]
    fn places_imac_left_of_builtin_centred() {
        let l = desk();
        assert_eq!(l.machines[1].offset, Point::new(-1920.0, -49.0));
    }

    #[test]
    fn crosses_left_edge_of_builtin_into_imac() {
        let l = desk();
        let from = DisplayRef {
            machine: 0,
            display: 0,
        };
        let c = l.neighbour(from, Side::Left, 500.0).unwrap();
        assert_eq!(c.to.machine, 1);
        assert_eq!(c.point, Point::new(-1.0, 500.0));
        assert_eq!(l.to_local(1, c.point), Point::new(1919.0, 549.0));
    }

    #[test]
    fn dell_left_edge_leads_nowhere() {
        let l = desk();
        let dell = DisplayRef {
            machine: 0,
            display: 1,
        };
        assert!(l.neighbour(dell, Side::Left, -700.0).is_none());
    }

    #[test]
    fn imac_right_edge_returns_only_along_shared_segment() {
        let l = desk();
        let imac = DisplayRef {
            machine: 1,
            display: 0,
        };
        let back = l.neighbour(imac, Side::Right, 10.0).unwrap();
        assert_eq!(back.to.machine, 0);
        assert_eq!(back.point, Point::new(0.0, 10.0));
        // Above the MacBook's top edge (y < 0) there's nothing to the right of the iMac.
        assert!(l.neighbour(imac, Side::Right, -20.0).is_none());
    }

    #[test]
    fn snap_pulls_a_loose_drop_onto_the_nearest_edge() {
        let l = desk();
        // Dropped a little away from the MacBook's left edge, a bit low.
        let p = l.snap(1, 0, Point::new(-1990.0, 100.0)).unwrap();
        assert_eq!(p, Point::new(-1920.0, 100.0));
    }

    #[test]
    fn snap_never_overlaps() {
        let l = desk();
        // Dropped right on top of the MacBook: must end up beside something, not over it.
        let p = l.snap(1, 0, Point::new(0.0, 0.0)).unwrap();
        let imac = Rect::new(p.x, p.y, 1920.0, 1080.0);
        for d in &l.machines[0].displays {
            assert!(!imac.intersects(&d.rect), "{imac:?} overlaps {:?}", d.rect);
        }
    }

    #[test]
    fn snap_keeps_a_usable_shared_edge() {
        let l = desk();
        // Dropped far below and to the left: lands below the MacBook, slid right until at
        // least 100 pt of edge is shared.
        let p = l.snap(1, 0, Point::new(-1920.0, 5000.0)).unwrap();
        assert_eq!(p, Point::new(-1820.0, 982.0));
        // Dropped low on the left: stays left, slid up to share 100 pt.
        let p = l.snap(1, 0, Point::new(-2100.0, 1000.0)).unwrap();
        assert_eq!(p, Point::new(-1920.0, 882.0));
    }

    /// A real desk: the iMac beside the external display, reaching down level with the top of the
    /// MacBook, which sits further right with a gap between them.
    fn real_desk() -> Layout {
        let mut l = desk();
        l.machines[1].offset = Point::new(-2507.0, -484.0);
        l
    }

    #[test]
    fn crosses_a_gap_to_the_other_computer() {
        let l = real_desk();
        let builtin = DisplayRef {
            machine: 0,
            display: 0,
        };
        // Left off the MacBook, level with the iMac: jumps the gap.
        let c = l.neighbour(builtin, Side::Left, 300.0).unwrap();
        assert_eq!(c.to.machine, 1);
        assert_eq!(c.point, Point::new(-588.0, 300.0));
        // Below the iMac's bottom edge there's nothing to the left.
        assert!(l.neighbour(builtin, Side::Left, 700.0).is_none());
        // And back: right off the iMac below the Dell lands on the MacBook's left edge.
        let imac = DisplayRef {
            machine: 1,
            display: 0,
        };
        let back = l.neighbour(imac, Side::Right, 300.0).unwrap();
        assert_eq!(back.to, builtin);
        assert_eq!(back.point, Point::new(0.0, 300.0));
        // Higher up, the Dell touches the iMac, so that wins.
        let dell = l.neighbour(imac, Side::Right, -100.0).unwrap();
        assert_eq!(dell.to.display, 1);
    }

    #[test]
    fn exit_sides_are_where_other_computers_are() {
        let l = real_desk();
        // The Mac: MacBook's left edge (across the gap) and the external display's left edge.
        assert_eq!(l.exit_sides(0), vec![(0, Side::Left), (1, Side::Left)]);
        // The iMac: its right edge.
        assert_eq!(l.exit_sides(1), vec![(0, Side::Right)]);
    }

    #[test]
    fn gaps_never_link_screens_of_the_same_computer() {
        let l = real_desk();
        let dell = DisplayRef {
            machine: 0,
            display: 1,
        };
        // Right off the Dell: nothing of another computer there.
        assert!(l.neighbour(dell, Side::Right, -500.0).is_none());
    }

    #[test]
    fn crossing_edges_cover_both_screens() {
        let l = real_desk();
        let edges = l.crossing_edges();
        // The Dell's left edge from the iMac's top down to the Dell's bottom...
        assert!(edges.contains(&(Point::new(-587.0, -484.0), Point::new(-587.0, 0.0))));
        // ...and the MacBook's left edge down to the iMac's bottom.
        assert!(edges.contains(&(Point::new(0.0, 0.0), Point::new(0.0, 596.0))));
    }

    #[test]
    fn exit_edges_say_which_display_and_side() {
        let l = real_desk();
        assert_eq!(
            l.exit_edges(0),
            vec![
                (0, Side::Left, Point::new(0.0, 0.0), Point::new(0.0, 596.0)),
                (
                    1,
                    Side::Left,
                    Point::new(-587.0, -484.0),
                    Point::new(-587.0, 0.0)
                ),
            ]
        );
        // The iMac's right edge leads to the Mac all the way down: the Dell, then the MacBook.
        assert_eq!(
            l.exit_edges(1),
            vec![(
                0,
                Side::Right,
                Point::new(-587.0, -484.0),
                Point::new(-587.0, 596.0)
            )]
        );
    }

    #[test]
    fn locate_and_nearest() {
        let l = desk();
        assert_eq!(
            l.locate_on(0, Point::new(100.0, -100.0)),
            Some(DisplayRef {
                machine: 0,
                display: 1
            })
        );
        assert_eq!(l.locate_on(0, Point::new(-10.0, 500.0)), None);
        assert_eq!(
            l.nearest_on(0, Point::new(-10.0, 500.0)),
            Some(DisplayRef {
                machine: 0,
                display: 0
            })
        );
    }
}
