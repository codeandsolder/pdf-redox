//! Small PDF geometry primitives shared by content-analysis passes.

/// An axis-aligned rectangle represented by its lower-left and upper-right corners.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rectangle {
    pub llx: f64,
    pub lly: f64,
    pub urx: f64,
    pub ury: f64,
}

impl Rectangle {
    pub const fn new(llx: f64, lly: f64, urx: f64, ury: f64) -> Self {
        Self { llx, lly, urx, ury }
    }
}

impl From<[f64; 4]> for Rectangle {
    fn from([llx, lly, urx, ury]: [f64; 4]) -> Self {
        Self::new(llx, lly, urx, ury)
    }
}

impl From<Rectangle> for [f64; 4] {
    fn from(rectangle: Rectangle) -> Self {
        [rectangle.llx, rectangle.lly, rectangle.urx, rectangle.ury]
    }
}

/// A normalized axis-aligned bounding box used by analysis and optimization passes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect {
    /// Construct a normalized rectangle from two corners.
    pub const fn new(x0: f64, y0: f64, x1: f64, y1: f64) -> Self {
        Self {
            x0: x0.min(x1),
            y0: y0.min(y1),
            x1: x0.max(x1),
            y1: y0.max(y1),
        }
    }

    /// Construct a non-degenerate finite rectangle from PDF `x y width height` operands.
    pub fn from_xywh(x: f64, y: f64, width: f64, height: f64) -> Option<Self> {
        if ![x, y, width, height].into_iter().all(f64::is_finite)
            || width.abs() <= f64::EPSILON
            || height.abs() <= f64::EPSILON
        {
            return None;
        }
        Some(Self::new(x, y, x + width, y + height))
    }

    pub const fn from_rectangle(rectangle: Rectangle) -> Self {
        Self::new(rectangle.llx, rectangle.lly, rectangle.urx, rectangle.ury)
    }

    pub fn from_ctm(ctm: Matrix) -> Self {
        let points = [
            ctm.transform(0.0, 0.0),
            ctm.transform(1.0, 0.0),
            ctm.transform(0.0, 1.0),
            ctm.transform(1.0, 1.0),
        ];
        Self {
            x0: points
                .iter()
                .map(|point| point.0)
                .fold(f64::INFINITY, f64::min),
            y0: points
                .iter()
                .map(|point| point.1)
                .fold(f64::INFINITY, f64::min),
            x1: points
                .iter()
                .map(|point| point.0)
                .fold(f64::NEG_INFINITY, f64::max),
            y1: points
                .iter()
                .map(|point| point.1)
                .fold(f64::NEG_INFINITY, f64::max),
        }
    }

    pub fn area(self) -> f64 {
        (self.x1 - self.x0).max(0.0) * (self.y1 - self.y0).max(0.0)
    }

    pub const fn union(self, other: Self) -> Self {
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    pub fn gap(self, other: Self) -> f64 {
        let dx = if self.x1 < other.x0 {
            other.x0 - self.x1
        } else if other.x1 < self.x0 {
            self.x0 - other.x1
        } else {
            0.0
        };
        let dy = if self.y1 < other.y0 {
            other.y0 - self.y1
        } else if other.y1 < self.y0 {
            self.y0 - other.y1
        } else {
            0.0
        };
        dx.hypot(dy)
    }

    pub fn intersects(self, other: Self) -> bool {
        self.x0 < other.x1 && self.x1 > other.x0 && self.y0 < other.y1 && self.y1 > other.y0
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        let output = Self {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        };
        (output.x1 > output.x0 && output.y1 > output.y0).then_some(output)
    }

    pub fn contains(self, other: Self, epsilon: f64) -> bool {
        self.x0 <= other.x0 + epsilon
            && self.y0 <= other.y0 + epsilon
            && self.x1 + epsilon >= other.x1
            && self.y1 + epsilon >= other.y1
    }

    pub fn coverage_of(self, target: Self) -> f64 {
        let area = target.area();
        if area <= f64::EPSILON {
            return 0.0;
        }
        self.intersection(target)
            .map_or(0.0, |rect| rect.area() / area)
    }
}

/// A PDF affine transformation matrix `[a b c d e f]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Matrix {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Default for Matrix {
    fn default() -> Self {
        Self::new(1.0, 0.0, 0.0, 1.0, 0.0, 0.0)
    }
}

impl From<[f64; 6]> for Matrix {
    #[expect(
        clippy::many_single_char_names,
        reason = "PDF affine matrices are normatively written as [a b c d e f]"
    )]
    fn from([a, b, c, d, e, f]: [f64; 6]) -> Self {
        Self::new(a, b, c, d, e, f)
    }
}

impl Matrix {
    #[expect(
        clippy::many_single_char_names,
        reason = "PDF affine matrices are normatively written as [a b c d e f]"
    )]
    pub const fn new(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Self {
        Self { a, b, c, d, e, f }
    }

    pub fn concat(&mut self, other: Self) {
        let ap = self.a.mul_add(other.a, self.c * other.b);
        let bp = self.b.mul_add(other.a, self.d * other.b);
        let cp = self.a.mul_add(other.c, self.c * other.d);
        let dp = self.b.mul_add(other.c, self.d * other.d);
        let ep = self.a.mul_add(other.e, self.c.mul_add(other.f, self.e));
        let fp = self.b.mul_add(other.e, self.d.mul_add(other.f, self.f));
        self.a = ap;
        self.b = bp;
        self.c = cp;
        self.d = dp;
        self.e = ep;
        self.f = fp;
    }

    pub fn scale(&mut self, sx: f64, sy: f64) {
        self.concat(Self::new(sx, 0.0, 0.0, sy, 0.0, 0.0));
    }

    pub fn translate(&mut self, tx: f64, ty: f64) {
        self.concat(Self::new(1.0, 0.0, 0.0, 1.0, tx, ty));
    }

    pub fn rotatex90(&mut self, angle: i32) {
        match angle {
            90 => self.concat(Self::new(0.0, 1.0, -1.0, 0.0, 0.0, 0.0)),
            180 => self.concat(Self::new(-1.0, 0.0, 0.0, -1.0, 0.0, 0.0)),
            270 => self.concat(Self::new(0.0, -1.0, 1.0, 0.0, 0.0, 0.0)),
            _ => {}
        }
    }

    pub fn transform(self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a.mul_add(x, self.c.mul_add(y, self.e)),
            self.b.mul_add(x, self.d.mul_add(y, self.f)),
        )
    }

    pub fn transform_rectangle(self, rectangle: Rectangle) -> Rectangle {
        let points = [
            self.transform(rectangle.llx, rectangle.lly),
            self.transform(rectangle.llx, rectangle.ury),
            self.transform(rectangle.urx, rectangle.lly),
            self.transform(rectangle.urx, rectangle.ury),
        ];
        Rectangle::new(
            points
                .iter()
                .map(|point| point.0)
                .fold(f64::INFINITY, f64::min),
            points
                .iter()
                .map(|point| point.1)
                .fold(f64::INFINITY, f64::min),
            points
                .iter()
                .map(|point| point.0)
                .fold(f64::NEG_INFINITY, f64::max),
            points
                .iter()
                .map(|point| point.1)
                .fold(f64::NEG_INFINITY, f64::max),
        )
    }

    pub fn unparse(self) -> String {
        [self.a, self.b, self.c, self.d, self.e, self.f]
            .into_iter()
            .map(format_component)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn format_component(value: f64) -> String {
    let value = if value > -0.00001 && value < 0.00001 {
        0.0
    } else {
        value
    };
    format!("{value:.5}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}
