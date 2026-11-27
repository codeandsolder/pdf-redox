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
        let (end_x, end_y) = (x + width, y + height);
        let rect = Self::new(x, y, end_x, end_y);
        (end_x.is_finite() && end_y.is_finite() && rect.x1 > rect.x0 && rect.y1 > rect.y0)
            .then_some(rect)
    }

    /// Validate corners before normalization: floating-point min/max can discard NaN.
    pub fn try_from_corners(x0: f64, y0: f64, x1: f64, y1: f64) -> Option<Self> {
        if ![x0, y0, x1, y1].into_iter().all(f64::is_finite) {
            return None;
        }
        Some(Self::new(x0, y0, x1, y1))
    }

    pub fn from_rectangle(rectangle: Rectangle) -> Option<Self> {
        Self::try_from_corners(rectangle.llx, rectangle.lly, rectangle.urx, rectangle.ury)
    }

    /// Axis-aligned bounding box after applying any affine PDF matrix.
    /// Nonrepresentable geometry must not be used to prove visibility or coverage.
    pub fn transformed_bounds(self, matrix: Matrix) -> Option<Self> {
        let points = [
            matrix.transform(self.x0, self.y0),
            matrix.transform(self.x1, self.y0),
            matrix.transform(self.x0, self.y1),
            matrix.transform(self.x1, self.y1),
        ];
        if !points.iter().all(|&(x, y)| x.is_finite() && y.is_finite()) {
            return None;
        }
        let bounds = Self {
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
        };
        bounds.area().is_finite().then_some(bounds)
    }

    pub fn scaled(self, scale: f64) -> Option<Self> {
        if !scale.is_finite() || scale <= 0.0 {
            return None;
        }
        Self::try_from_corners(
            self.x0 * scale,
            self.y0 * scale,
            self.x1 * scale,
            self.y1 * scale,
        )
        .filter(|bounds| bounds.area().is_finite())
    }

    pub fn from_ctm(ctm: Matrix) -> Option<Self> {
        Self::new(0.0, 0.0, 1.0, 1.0).transformed_bounds(ctm)
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

/// Compose two PDF affine matrix arrays with their original floating-point order.
pub fn compose_affine_matrix(a: [f64; 6], b: [f64; 6]) -> [f64; 6] {
    [
        a[2].mul_add(b[1], a[0] * b[0]),
        a[3].mul_add(b[1], a[1] * b[0]),
        a[2].mul_add(b[3], a[0] * b[2]),
        a[3].mul_add(b[3], a[1] * b[2]),
        a[2].mul_add(b[5], a[0].mul_add(b[4], a[4])),
        a[3].mul_add(b[5], a[1].mul_add(b[4], a[5])),
    ]
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

    pub const fn transform(self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a.mul_add(x, self.c.mul_add(y, self.e)),
            self.b.mul_add(x, self.d.mul_add(y, self.f)),
        )
    }

    pub fn transform_rectangle(self, rectangle: Rectangle) -> Option<Rect> {
        Rect::from_rectangle(rectangle)?.transformed_bounds(self)
    }

    pub fn is_finite(self) -> bool {
        [self.a, self.b, self.c, self.d, self.e, self.f]
            .into_iter()
            .all(f64::is_finite)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_affine_composition_preserves_pdf_matrix_order() {
        let a = [2.0, 0.0, 0.0, 3.0, 4.0, 5.0];
        let b = [0.0, 1.0, -1.0, 0.0, 7.0, 11.0];
        assert_eq!(
            compose_affine_matrix(a, b),
            [0.0, 3.0, -2.0, 0.0, 18.0, 38.0]
        );
    }

    #[test]
    fn xywh_rejects_nonrepresentable_endpoints_and_preserves_negative_extents() {
        for (x, y, width, height) in [
            (f64::MAX, 0.0, f64::MAX, 1.0),
            (0.0, -f64::MAX, 1.0, -f64::MAX),
            (1e20, 0.0, 1.0, 1.0),
            (0.0, -1e20, 1.0, -1.0),
        ] {
            assert!(Rect::from_xywh(x, y, width, height).is_none());
        }
        assert_eq!(
            Rect::from_xywh(4.0, 6.0, -3.0, -4.0),
            Some(Rect::new(1.0, 2.0, 4.0, 6.0))
        );
    }

    #[test]
    fn affine_bounds_reject_nonrepresentable_geometry_without_erasing_degeneracy() {
        let huge = Matrix::new(f64::MAX, 0.0, 0.0, 1.0, f64::MAX, 0.0);
        let invalid = Matrix::new(1.0, 0.0, f64::NAN, 1.0, 0.0, 0.0);
        assert!(Rect::from_ctm(huge).is_none());
        assert!(Rect::from_ctm(invalid).is_none());
        assert!(
            Matrix::default()
                .transform_rectangle(Rectangle::new(f64::NAN, 0.0, 1.0, 1.0))
                .is_none()
        );
        assert!(Rect::new(1.0, 0.0, 2.0, 1.0).scaled(f64::MAX).is_none());
        assert_eq!(
            Matrix::new(1e-200, 0.0, 0.0, 1e-200, 0.0, 0.0)
                .transform_rectangle(Rectangle::new(0.0, 0.0, 1e200, 1e200)),
            Some(Rect::new(0.0, 0.0, 1.0, 1.0))
        );
        assert_eq!(
            Rect::from_ctm(Matrix::new(0.0, 0.0, 0.0, 1.0, 2.0, 0.0)),
            Some(Rect::new(2.0, 0.0, 2.0, 1.0))
        );
    }

    #[test]
    fn transformed_rect_bounds_account_for_quarter_turns_and_unit_square() {
        let matrix = Matrix::new(0.0, 2.0, -3.0, 0.0, 5.0, 7.0);
        let rect = Rect::new(1.0, 2.0, 4.0, 6.0);
        assert_eq!(
            rect.transformed_bounds(matrix),
            Some(Rect::new(-13.0, 9.0, -1.0, 15.0))
        );
        assert_eq!(Rect::from_ctm(matrix), Some(Rect::new(2.0, 7.0, 5.0, 9.0)));
    }

    #[test]
    fn transformed_rect_bounds_cover_all_four_corners_under_shear() {
        let matrix = Matrix::new(1.0, 0.5, 0.25, 1.0, -10.0, 8.0);
        let rect = Rect::new(-2.0, 3.0, 4.0, 6.0);
        assert_eq!(
            rect.transformed_bounds(matrix),
            Some(Rect::new(-11.25, 10.0, -4.5, 16.0))
        );
    }
}
