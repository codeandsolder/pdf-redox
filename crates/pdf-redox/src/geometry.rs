//! Small PDF geometry primitives shared by content-analysis passes.

/// An axis-aligned rectangle represented by its lower-left and upper-right corners.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Rectangle {
    pub(crate) llx: f64,
    pub(crate) lly: f64,
    pub(crate) urx: f64,
    pub(crate) ury: f64,
}

impl Rectangle {
    pub(crate) const fn new(llx: f64, lly: f64, urx: f64, ury: f64) -> Self {
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

/// A PDF affine transformation matrix `[a b c d e f]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Matrix {
    pub(crate) a: f64,
    pub(crate) b: f64,
    pub(crate) c: f64,
    pub(crate) d: f64,
    pub(crate) e: f64,
    pub(crate) f: f64,
}

impl Default for Matrix {
    fn default() -> Self {
        Self::new(1.0, 0.0, 0.0, 1.0, 0.0, 0.0)
    }
}

impl From<[f64; 6]> for Matrix {
    fn from([a, b, c, d, e, f]: [f64; 6]) -> Self {
        Self::new(a, b, c, d, e, f)
    }
}

impl Matrix {
    pub(crate) const fn new(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Self {
        Self { a, b, c, d, e, f }
    }

    pub(crate) fn concat(&mut self, other: Self) {
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

    pub(crate) fn scale(&mut self, sx: f64, sy: f64) {
        self.concat(Self::new(sx, 0.0, 0.0, sy, 0.0, 0.0));
    }

    pub(crate) fn translate(&mut self, tx: f64, ty: f64) {
        self.concat(Self::new(1.0, 0.0, 0.0, 1.0, tx, ty));
    }

    pub(crate) fn rotatex90(&mut self, angle: i32) {
        match angle {
            90 => self.concat(Self::new(0.0, 1.0, -1.0, 0.0, 0.0, 0.0)),
            180 => self.concat(Self::new(-1.0, 0.0, 0.0, -1.0, 0.0, 0.0)),
            270 => self.concat(Self::new(0.0, -1.0, 1.0, 0.0, 0.0, 0.0)),
            _ => {}
        }
    }

    pub(crate) fn transform(self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a.mul_add(x, self.c.mul_add(y, self.e)),
            self.b.mul_add(x, self.d.mul_add(y, self.f)),
        )
    }

    pub(crate) fn transform_rectangle(self, rectangle: Rectangle) -> Rectangle {
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

    pub(crate) fn unparse(self) -> String {
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
