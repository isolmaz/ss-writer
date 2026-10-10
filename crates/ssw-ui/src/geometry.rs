//! Geometry and color primitives.
//!
//! All lengths are `f32` device-independent pixels (DIPs, 1 DIP = 1/96 inch). The y axis grows
//! downwards. Conversion to physical pixels uses a dpi scale (`dpi / 96`) only at the edges
//! ([`snap_to_pixel`], [`Rect::round_to_pixels`], [`Rect::round_out`]).

use std::ops::{Add, Sub};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Rounds `value` (DIPs) to the nearest physical pixel for the dpi `scale` (`dpi / 96`).
/// A non-positive `scale` leaves the value unchanged.
#[must_use]
pub fn snap_to_pixel(value: f32, scale: f32) -> f32 {
    if scale > 0.0 {
        (value * scale).round() / scale
    } else {
        value
    }
}

/// A point in DIPs.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };

    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// Euclidean distance to `other`.
    #[must_use]
    pub fn distance(self, other: Point) -> f32 {
        (self.x - other.x).hypot(self.y - other.y)
    }
}

impl Add for Point {
    type Output = Point;

    fn add(self, rhs: Point) -> Point {
        Point::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl Sub for Point {
    type Output = Point;

    fn sub(self, rhs: Point) -> Point {
        Point::new(self.x - rhs.x, self.y - rhs.y)
    }
}

/// A size in DIPs.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

impl Size {
    pub const ZERO: Size = Size {
        width: 0.0,
        height: 0.0,
    };
    /// "No constraint" in both axes (used as the `available` size of `measure`).
    pub const UNBOUNDED: Size = Size {
        width: f32::INFINITY,
        height: f32::INFINITY,
    };

    #[must_use]
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }

    /// True when either extent is zero or negative.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }
}

/// Per-edge distances (padding, borders, margins).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Insets {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Insets {
    pub const ZERO: Insets = Insets {
        left: 0.0,
        top: 0.0,
        right: 0.0,
        bottom: 0.0,
    };

    #[must_use]
    pub const fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    #[must_use]
    pub const fn uniform(value: f32) -> Self {
        Self::new(value, value, value, value)
    }

    #[must_use]
    pub const fn symmetric(horizontal: f32, vertical: f32) -> Self {
        Self::new(horizontal, vertical, horizontal, vertical)
    }

    /// `left + right`.
    #[must_use]
    pub fn horizontal(self) -> f32 {
        self.left + self.right
    }

    /// `top + bottom`.
    #[must_use]
    pub fn vertical(self) -> f32 {
        self.top + self.bottom
    }
}

/// Per-corner radii of a rounded rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CornerRadii {
    pub top_left: f32,
    pub top_right: f32,
    pub bottom_right: f32,
    pub bottom_left: f32,
}

impl CornerRadii {
    pub const ZERO: CornerRadii = CornerRadii::uniform(0.0);

    #[must_use]
    pub const fn new(top_left: f32, top_right: f32, bottom_right: f32, bottom_left: f32) -> Self {
        Self {
            top_left,
            top_right,
            bottom_right,
            bottom_left,
        }
    }

    #[must_use]
    pub const fn uniform(radius: f32) -> Self {
        Self::new(radius, radius, radius, radius)
    }

    /// True when all four radii are equal.
    #[must_use]
    pub fn is_uniform(self) -> bool {
        self.top_left == self.top_right
            && self.top_right == self.bottom_right
            && self.bottom_right == self.bottom_left
    }

    /// True when every radius is zero or negative (a plain rectangle).
    #[must_use]
    pub fn is_zero(self) -> bool {
        self.top_left <= 0.0
            && self.top_right <= 0.0
            && self.bottom_right <= 0.0
            && self.bottom_left <= 0.0
    }

    /// Scales all radii by the same factor so that adjacent radii never overlap on a rectangle
    /// of `size` (CSS `border-radius` rule). Radii that already fit are returned unchanged.
    #[must_use]
    pub fn clamped(self, size: Size) -> Self {
        let ratio = |available: f32, sum: f32| {
            if sum > available {
                available / sum
            } else {
                1.0
            }
        };
        let factor = ratio(size.width, self.top_left + self.top_right)
            .min(ratio(size.width, self.bottom_left + self.bottom_right))
            .min(ratio(size.height, self.top_left + self.bottom_left))
            .min(ratio(size.height, self.top_right + self.bottom_right))
            .max(0.0);
        Self::new(
            self.top_left * factor,
            self.top_right * factor,
            self.bottom_right * factor,
            self.bottom_left * factor,
        )
    }
}

/// An axis-aligned rectangle in DIPs. Edges are half-open: `[left, right) x [top, bottom)`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const ZERO: Rect = Rect {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
    };

    #[must_use]
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    #[must_use]
    pub fn from_ltrb(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self::new(left, top, right - left, bottom - top)
    }

    #[must_use]
    pub const fn from_origin_size(origin: Point, size: Size) -> Self {
        Self::new(origin.x, origin.y, size.width, size.height)
    }

    #[must_use]
    pub fn left(&self) -> f32 {
        self.x
    }

    #[must_use]
    pub fn top(&self) -> f32 {
        self.y
    }

    #[must_use]
    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    #[must_use]
    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    #[must_use]
    pub fn origin(&self) -> Point {
        Point::new(self.x, self.y)
    }

    #[must_use]
    pub fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }

    #[must_use]
    pub fn center(&self) -> Point {
        Point::new(self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    /// True when the rectangle has no area.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }

    /// True when `point` lies inside (left/top edges inclusive, right/bottom exclusive).
    #[must_use]
    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.left()
            && point.x < self.right()
            && point.y >= self.top()
            && point.y < self.bottom()
    }

    /// True when `other` lies entirely inside `self` (shared edges allowed).
    #[must_use]
    pub fn contains_rect(&self, other: Rect) -> bool {
        other.left() >= self.left()
            && other.right() <= self.right()
            && other.top() >= self.top()
            && other.bottom() <= self.bottom()
    }

    /// The overlapping area, or `None` when the rectangles share no area (touching edges do not
    /// overlap).
    #[must_use]
    pub fn intersect(&self, other: Rect) -> Option<Rect> {
        let left = self.left().max(other.left());
        let top = self.top().max(other.top());
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if left < right && top < bottom {
            Some(Rect::from_ltrb(left, top, right, bottom))
        } else {
            None
        }
    }

    /// True when the rectangles share some area.
    #[must_use]
    pub fn intersects(&self, other: Rect) -> bool {
        self.intersect(other).is_some()
    }

    /// The smallest rectangle containing both. Empty rectangles are ignored, so accumulating
    /// from [`Rect::ZERO`] yields the union of the non-empty inputs.
    #[must_use]
    pub fn union(&self, other: Rect) -> Rect {
        if self.is_empty() {
            other
        } else if other.is_empty() {
            *self
        } else {
            Rect::from_ltrb(
                self.left().min(other.left()),
                self.top().min(other.top()),
                self.right().max(other.right()),
                self.bottom().max(other.bottom()),
            )
        }
    }

    /// Shrinks each edge by `insets`; the size never goes negative.
    #[must_use]
    pub fn inset(&self, insets: Insets) -> Rect {
        Rect::new(
            self.x + insets.left,
            self.y + insets.top,
            (self.width - insets.horizontal()).max(0.0),
            (self.height - insets.vertical()).max(0.0),
        )
    }

    /// Grows each edge by `insets`.
    #[must_use]
    pub fn outset(&self, insets: Insets) -> Rect {
        Rect::new(
            self.x - insets.left,
            self.y - insets.top,
            self.width + insets.horizontal(),
            self.height + insets.vertical(),
        )
    }

    #[must_use]
    pub fn translate(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.width, self.height)
    }

    /// Snaps every edge to the nearest physical pixel for the dpi `scale` (`dpi / 96`), so
    /// fills and hairlines stay crisp. Width and height are derived from the snapped edges.
    #[must_use]
    pub fn round_to_pixels(&self, scale: f32) -> Rect {
        if scale <= 0.0 {
            return *self;
        }
        Rect::from_ltrb(
            snap_to_pixel(self.left(), scale),
            snap_to_pixel(self.top(), scale),
            snap_to_pixel(self.right(), scale),
            snap_to_pixel(self.bottom(), scale),
        )
    }

    /// Expands every edge outwards to the physical pixel grid (floor for left/top, ceil for
    /// right/bottom). Used for invalidation rectangles. A non-positive `scale` leaves the
    /// rectangle unchanged.
    #[must_use]
    pub fn round_out(&self, scale: f32) -> Rect {
        if scale <= 0.0 {
            return *self;
        }
        Rect::from_ltrb(
            (self.left() * scale).floor() / scale,
            (self.top() * scale).floor() / scale,
            (self.right() * scale).ceil() / scale,
            (self.bottom() * scale).ceil() / scale,
        )
    }
}

/// A 2D affine transform in the row-vector convention of Direct2D (`Matrix3x2F`):
///
/// ```text
/// x' = x * m11 + y * m21 + dx
/// y' = x * m12 + y * m22 + dy
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform2D {
    pub m11: f32,
    pub m12: f32,
    pub m21: f32,
    pub m22: f32,
    pub dx: f32,
    pub dy: f32,
}

impl Transform2D {
    pub const IDENTITY: Transform2D = Transform2D {
        m11: 1.0,
        m12: 0.0,
        m21: 0.0,
        m22: 1.0,
        dx: 0.0,
        dy: 0.0,
    };

    #[must_use]
    pub const fn translation(dx: f32, dy: f32) -> Self {
        Self {
            m11: 1.0,
            m12: 0.0,
            m21: 0.0,
            m22: 1.0,
            dx,
            dy,
        }
    }

    #[must_use]
    pub const fn scale(sx: f32, sy: f32) -> Self {
        Self {
            m11: sx,
            m12: 0.0,
            m21: 0.0,
            m22: sy,
            dx: 0.0,
            dy: 0.0,
        }
    }

    /// Rotation about the origin. With the y axis pointing down a positive angle turns
    /// clockwise on screen.
    #[must_use]
    pub fn rotation(radians: f32) -> Self {
        let (sin, cos) = radians.sin_cos();
        Self {
            m11: cos,
            m12: sin,
            m21: -sin,
            m22: cos,
            dx: 0.0,
            dy: 0.0,
        }
    }

    /// The transform that applies `self` first and `other` second.
    #[must_use]
    pub fn compose(self, other: Transform2D) -> Transform2D {
        Transform2D {
            m11: self.m11 * other.m11 + self.m12 * other.m21,
            m12: self.m11 * other.m12 + self.m12 * other.m22,
            m21: self.m21 * other.m11 + self.m22 * other.m21,
            m22: self.m21 * other.m12 + self.m22 * other.m22,
            dx: self.dx * other.m11 + self.dy * other.m21 + other.dx,
            dy: self.dx * other.m12 + self.dy * other.m22 + other.dy,
        }
    }

    #[must_use]
    pub fn determinant(self) -> f32 {
        self.m11 * self.m22 - self.m12 * self.m21
    }

    /// The inverse transform, or `None` when the matrix is singular.
    #[must_use]
    pub fn invert(self) -> Option<Transform2D> {
        let det = self.determinant();
        if det == 0.0 || !det.is_finite() {
            return None;
        }
        let inv = 1.0 / det;
        Some(Transform2D {
            m11: self.m22 * inv,
            m12: -self.m12 * inv,
            m21: -self.m21 * inv,
            m22: self.m11 * inv,
            dx: (self.m21 * self.dy - self.m22 * self.dx) * inv,
            dy: (self.m12 * self.dx - self.m11 * self.dy) * inv,
        })
    }

    #[must_use]
    pub fn transform_point(self, point: Point) -> Point {
        Point::new(
            point.x * self.m11 + point.y * self.m21 + self.dx,
            point.x * self.m12 + point.y * self.m22 + self.dy,
        )
    }

    /// The axis-aligned bounding box of the transformed rectangle.
    #[must_use]
    pub fn transform_rect(self, rect: Rect) -> Rect {
        let corners = [
            self.transform_point(Point::new(rect.left(), rect.top())),
            self.transform_point(Point::new(rect.right(), rect.top())),
            self.transform_point(Point::new(rect.right(), rect.bottom())),
            self.transform_point(Point::new(rect.left(), rect.bottom())),
        ];
        let (mut left, mut top) = (corners[0].x, corners[0].y);
        let (mut right, mut bottom) = (left, top);
        for corner in &corners[1..] {
            left = left.min(corner.x);
            top = top.min(corner.y);
            right = right.max(corner.x);
            bottom = bottom.max(corner.y);
        }
        Rect::from_ltrb(left, top, right, bottom)
    }
}

impl Default for Transform2D {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Failure to parse a `#RRGGBB` / `#RRGGBBAA` string.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ColorParseError {
    #[error("color must start with '#'")]
    MissingHash,
    #[error("color must have 6 or 8 hex digits, found {0}")]
    InvalidLength(usize),
    #[error("color contains a non-hexadecimal digit")]
    InvalidDigit,
}

/// A non-premultiplied sRGB color with `f32` components in `0.0..=1.0`.
///
/// Serialized as `#RRGGBB` (or `#RRGGBBAA` when not opaque) so settings files stay readable.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const TRANSPARENT: Color = Color::new(0.0, 0.0, 0.0, 0.0);
    pub const BLACK: Color = Color::new(0.0, 0.0, 0.0, 1.0);
    pub const WHITE: Color = Color::new(1.0, 1.0, 1.0, 1.0);

    #[must_use]
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    #[must_use]
    pub const fn from_rgba8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self::new(
            r as f32 / 255.0,
            g as f32 / 255.0,
            b as f32 / 255.0,
            a as f32 / 255.0,
        )
    }

    /// An opaque color from `0xRRGGBB`.
    #[must_use]
    pub const fn from_rgb_u32(rgb: u32) -> Self {
        Self::from_rgba8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 255)
    }

    /// Parses `#RRGGBB` or `#RRGGBBAA` (case-insensitive).
    ///
    /// # Errors
    /// Returns a [`ColorParseError`] when the prefix, length or digits are wrong.
    pub fn from_hex(text: &str) -> Result<Color, ColorParseError> {
        let digits = text.strip_prefix('#').ok_or(ColorParseError::MissingHash)?;
        if digits.len() != 6 && digits.len() != 8 {
            return Err(ColorParseError::InvalidLength(digits.len()));
        }
        if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ColorParseError::InvalidDigit);
        }
        // All bytes are ASCII hex digits, so slicing at even offsets is on char boundaries and
        // parsing cannot fail; the fallback keeps the function total without panicking.
        let byte_at = |index: usize| u8::from_str_radix(&digits[index..index + 2], 16).unwrap_or(0);
        let alpha = if digits.len() == 8 { byte_at(6) } else { 255 };
        Ok(Color::from_rgba8(byte_at(0), byte_at(2), byte_at(4), alpha))
    }

    /// Components as 8-bit values `[r, g, b, a]` (rounded, clamped).
    #[must_use]
    pub fn to_rgba8(self) -> [u8; 4] {
        let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        [
            channel(self.r),
            channel(self.g),
            channel(self.b),
            channel(self.a),
        ]
    }

    /// `#RRGGBB`, or `#RRGGBBAA` when the color is not fully opaque.
    #[must_use]
    pub fn to_hex(self) -> String {
        let [r, g, b, a] = self.to_rgba8();
        if a == 255 {
            format!("#{r:02X}{g:02X}{b:02X}")
        } else {
            format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
        }
    }

    /// `[r * a, g * a, b * a, a]`, the form GPU blending expects.
    #[must_use]
    pub fn premultiplied(self) -> [f32; 4] {
        [self.r * self.a, self.g * self.a, self.b * self.a, self.a]
    }

    /// Component-wise interpolation in straight (non-premultiplied) RGBA; `t` is clamped to
    /// `0.0..=1.0`.
    #[must_use]
    pub fn lerp(self, other: Color, t: f32) -> Color {
        let t = t.clamp(0.0, 1.0);
        Color::new(
            self.r + (other.r - self.r) * t,
            self.g + (other.g - self.g) * t,
            self.b + (other.b - self.b) * t,
            self.a + (other.a - self.a) * t,
        )
    }

    #[must_use]
    pub fn with_alpha(self, alpha: f32) -> Color {
        Color { a: alpha, ..self }
    }

    /// Source-over compositing of `self` on `background`.
    #[must_use]
    pub fn over(self, background: Color) -> Color {
        let alpha = self.a + background.a * (1.0 - self.a);
        if alpha <= 0.0 {
            return Color::TRANSPARENT;
        }
        let blend =
            |top: f32, bottom: f32| (top * self.a + bottom * background.a * (1.0 - self.a)) / alpha;
        Color::new(
            blend(self.r, background.r),
            blend(self.g, background.g),
            blend(self.b, background.b),
            alpha,
        )
    }

    /// WCAG relative luminance (alpha ignored).
    #[must_use]
    pub fn relative_luminance(self) -> f32 {
        let linear = |channel: f32| {
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }

    /// WCAG contrast ratio (`1.0..=21.0`) between two colors; symmetric.
    #[must_use]
    pub fn contrast_ratio(self, other: Color) -> f32 {
        let a = self.relative_luminance();
        let b = other.relative_luminance();
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }
}

impl From<Color> for String {
    fn from(color: Color) -> String {
        color.to_hex()
    }
}

impl TryFrom<String> for Color {
    type Error = ColorParseError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Color::from_hex(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    fn rect_close(a: Rect, b: Rect) -> bool {
        close(a.x, b.x) && close(a.y, b.y) && close(a.width, b.width) && close(a.height, b.height)
    }

    fn point_close(a: Point, b: Point) -> bool {
        close(a.x, b.x) && close(a.y, b.y)
    }

    #[test]
    fn point_and_size_basics() {
        let a = Point::new(1.0, 2.0);
        let b = Point::new(4.0, 6.0);
        assert_eq!(a + b, Point::new(5.0, 8.0));
        assert_eq!(b - a, Point::new(3.0, 4.0));
        assert!(close(a.distance(b), 5.0));
        assert_eq!(Point::default(), Point::ZERO);
        assert!(Size::ZERO.is_empty());
        assert!(Size::new(0.0, 5.0).is_empty());
        assert!(Size::new(5.0, -1.0).is_empty());
        assert!(!Size::new(1.0, 1.0).is_empty());
        assert!(Size::UNBOUNDED.width.is_infinite() && Size::UNBOUNDED.height.is_infinite());
    }

    #[test]
    fn rect_accessors() {
        let r = Rect::new(10.0, 20.0, 30.0, 40.0);
        assert_eq!(
            (r.left(), r.top(), r.right(), r.bottom()),
            (10.0, 20.0, 40.0, 60.0)
        );
        assert_eq!(r.origin(), Point::new(10.0, 20.0));
        assert_eq!(r.size(), Size::new(30.0, 40.0));
        assert_eq!(r.center(), Point::new(25.0, 40.0));
        assert_eq!(Rect::from_ltrb(10.0, 20.0, 40.0, 60.0), r);
        assert_eq!(Rect::from_origin_size(r.origin(), r.size()), r);
        assert!(!r.is_empty());
        assert!(Rect::ZERO.is_empty());
        assert!(Rect::new(0.0, 0.0, 5.0, 0.0).is_empty());
        assert!(Rect::new(0.0, 0.0, 0.0, 5.0).is_empty());
    }

    #[test]
    fn rect_contains_is_half_open() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(r.contains(Point::new(0.0, 0.0)));
        assert!(r.contains(Point::new(9.99, 9.99)));
        assert!(!r.contains(Point::new(10.0, 5.0)));
        assert!(!r.contains(Point::new(5.0, 10.0)));
        assert!(!r.contains(Point::new(-0.01, 5.0)));
        assert!(!r.contains(Point::new(5.0, -0.01)));
        assert!(r.contains_rect(Rect::new(2.0, 2.0, 8.0, 8.0)));
        assert!(!r.contains_rect(Rect::new(2.0, 2.0, 9.0, 8.0)));
        assert!(!r.contains_rect(Rect::new(-1.0, 2.0, 3.0, 3.0)));
        assert!(!r.contains_rect(Rect::new(2.0, -1.0, 3.0, 3.0)));
        assert!(!r.contains_rect(Rect::new(2.0, 2.0, 3.0, 9.0)));
    }

    #[test]
    fn rect_intersect_and_union() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 6.0, 10.0, 10.0);
        assert_eq!(a.intersect(b), Some(Rect::new(5.0, 6.0, 5.0, 4.0)));
        assert!(a.intersects(b));
        // Touching edges share no area.
        let touching = Rect::new(10.0, 0.0, 5.0, 5.0);
        assert_eq!(a.intersect(touching), None);
        assert!(!a.intersects(touching));
        assert_eq!(a.intersect(Rect::new(0.0, 20.0, 5.0, 5.0)), None);
        assert_eq!(a.union(b), Rect::new(0.0, 0.0, 15.0, 16.0));
        // Empty rectangles are ignored by union.
        assert_eq!(Rect::ZERO.union(b), b);
        assert_eq!(b.union(Rect::ZERO), b);
        assert_eq!(Rect::ZERO.union(Rect::ZERO), Rect::ZERO);
    }

    #[test]
    fn rect_inset_outset_translate() {
        let r = Rect::new(10.0, 10.0, 100.0, 50.0);
        let insets = Insets::new(1.0, 2.0, 3.0, 4.0);
        assert_eq!(r.inset(insets), Rect::new(11.0, 12.0, 96.0, 44.0));
        assert_eq!(r.outset(insets), Rect::new(9.0, 8.0, 104.0, 56.0));
        // Over-inset clamps to an empty size instead of going negative.
        let tiny = Rect::new(0.0, 0.0, 4.0, 4.0).inset(Insets::uniform(10.0));
        assert_eq!((tiny.width, tiny.height), (0.0, 0.0));
        assert_eq!(r.translate(5.0, -5.0), Rect::new(15.0, 5.0, 100.0, 50.0));
    }

    #[test]
    fn insets_helpers() {
        assert_eq!(Insets::uniform(2.0), Insets::new(2.0, 2.0, 2.0, 2.0));
        assert_eq!(Insets::symmetric(3.0, 5.0), Insets::new(3.0, 5.0, 3.0, 5.0));
        assert_eq!(Insets::new(1.0, 2.0, 3.0, 4.0).horizontal(), 4.0);
        assert_eq!(Insets::new(1.0, 2.0, 3.0, 4.0).vertical(), 6.0);
        assert_eq!(Insets::default(), Insets::ZERO);
    }

    #[test]
    fn rect_rounds_to_physical_pixels() {
        let r = Rect::new(10.3, 20.2, 30.4, 5.1);
        // 150 % scale: edges land on multiples of 1/1.5 DIP.
        let snapped = r.round_to_pixels(1.5);
        let expected = Rect::new(10.0, 20.0, 61.0 / 1.5 - 10.0, 38.0 / 1.5 - 20.0);
        assert!(rect_close(snapped, expected), "{snapped:?}");
        // Whole-pixel rects are unchanged at 100 %.
        let whole = Rect::new(3.0, 4.0, 5.0, 6.0);
        assert_eq!(whole.round_to_pixels(1.0), whole);
        // Invalid scales are ignored.
        assert_eq!(r.round_to_pixels(0.0), r);
        assert!(close(snap_to_pixel(10.3, 2.0), 10.5));
        assert!(close(snap_to_pixel(10.3, -1.0), 10.3));
        // Rounding outwards never shrinks the rectangle.
        let out = r.round_out(2.0);
        assert!(rect_close(out, Rect::new(10.0, 20.0, 31.0, 5.5)), "{out:?}");
        assert!(out.contains_rect(r));
        assert_eq!(r.round_out(0.0), r);
    }

    #[test]
    fn corner_radii() {
        assert!(CornerRadii::uniform(4.0).is_uniform());
        assert!(!CornerRadii::new(1.0, 2.0, 3.0, 4.0).is_uniform());
        assert!(!CornerRadii::new(4.0, 4.0, 4.0, 3.0).is_uniform());
        assert!(!CornerRadii::new(4.0, 4.0, 3.0, 4.0).is_uniform());
        assert!(CornerRadii::ZERO.is_zero());
        assert!(!CornerRadii::new(0.0, 0.0, 0.0, 1.0).is_zero());
        assert!(!CornerRadii::new(0.0, 0.0, 1.0, 0.0).is_zero());
        assert!(!CornerRadii::new(0.0, 1.0, 0.0, 0.0).is_zero());
        assert!(!CornerRadii::new(1.0, 0.0, 0.0, 0.0).is_zero());
        assert_eq!(CornerRadii::default(), CornerRadii::ZERO);
        // Fits: unchanged.
        let fits = CornerRadii::uniform(4.0).clamped(Size::new(100.0, 100.0));
        assert_eq!(fits, CornerRadii::uniform(4.0));
        // 10-wide square with radius 10 scales to a circle (radius 5).
        let circle = CornerRadii::uniform(10.0).clamped(Size::new(10.0, 10.0));
        assert_eq!(circle, CornerRadii::uniform(5.0));
        // The tightest edge decides the factor for every corner.
        let pill = CornerRadii::uniform(20.0).clamped(Size::new(100.0, 10.0));
        assert_eq!(pill, CornerRadii::uniform(5.0));
        let tall = CornerRadii::uniform(20.0).clamped(Size::new(10.0, 100.0));
        assert_eq!(tall, CornerRadii::uniform(5.0));
        // Degenerate sizes collapse to zero.
        assert_eq!(
            CornerRadii::uniform(4.0).clamped(Size::ZERO),
            CornerRadii::ZERO
        );
    }

    #[test]
    fn transform_apply_compose_and_invert() {
        let p = Point::new(3.0, 4.0);
        assert_eq!(Transform2D::IDENTITY.transform_point(p), p);
        assert_eq!(Transform2D::default(), Transform2D::IDENTITY);
        assert_eq!(
            Transform2D::translation(10.0, -2.0).transform_point(p),
            Point::new(13.0, 2.0)
        );
        assert_eq!(
            Transform2D::scale(2.0, 3.0).transform_point(p),
            Point::new(6.0, 12.0)
        );
        // A positive quarter turn maps +x to +y (clockwise on a y-down screen).
        let quarter = Transform2D::rotation(std::f32::consts::FRAC_PI_2);
        assert!(point_close(
            quarter.transform_point(Point::new(1.0, 0.0)),
            Point::new(0.0, 1.0)
        ));

        // compose: scale first, then translate.
        let scale = Transform2D::scale(2.0, 2.0);
        let translate = Transform2D::translation(5.0, 7.0);
        assert_eq!(
            scale.compose(translate).transform_point(p),
            Point::new(11.0, 15.0)
        );
        // ... and the reverse order differs.
        assert_eq!(
            translate.compose(scale).transform_point(p),
            Point::new(16.0, 22.0)
        );
        // Composition equals applying the parts one after another.
        let combined = quarter.compose(translate).compose(scale);
        let stepwise = scale.transform_point(translate.transform_point(quarter.transform_point(p)));
        assert!(point_close(combined.transform_point(p), stepwise));

        // invert round-trips.
        let m = Transform2D::rotation(0.7)
            .compose(Transform2D::scale(2.0, 0.5))
            .compose(Transform2D::translation(12.0, -3.0));
        let inverse = m.invert().expect("invertible");
        assert!(point_close(
            inverse.transform_point(m.transform_point(p)),
            p
        ));
        assert!(point_close(
            m.transform_point(inverse.transform_point(p)),
            p
        ));
        let identityish = m.compose(inverse);
        assert!(close(identityish.m11, 1.0) && close(identityish.m22, 1.0));
        assert!(close(identityish.m12, 0.0) && close(identityish.m21, 0.0));
        assert!(close(identityish.dx, 0.0) && close(identityish.dy, 0.0));
        assert!(close(m.determinant(), 1.0));
    }

    #[test]
    fn transform_singular_and_non_finite_do_not_invert() {
        assert_eq!(Transform2D::scale(0.0, 1.0).invert(), None);
        assert_eq!(Transform2D::scale(f32::INFINITY, 1.0).invert(), None);
        assert_eq!(Transform2D::scale(f32::NAN, 1.0).invert(), None);
    }

    #[test]
    fn transform_rect_is_bounding_box() {
        let r = Rect::new(0.0, 0.0, 10.0, 20.0);
        assert_eq!(
            Transform2D::translation(5.0, 5.0).transform_rect(r),
            Rect::new(5.0, 5.0, 10.0, 20.0)
        );
        assert_eq!(
            Transform2D::scale(-1.0, 1.0).transform_rect(r),
            Rect::new(-10.0, 0.0, 10.0, 20.0)
        );
        let turned = Transform2D::rotation(std::f32::consts::FRAC_PI_2).transform_rect(r);
        assert!(
            rect_close(turned, Rect::new(-20.0, 0.0, 20.0, 10.0)),
            "{turned:?}"
        );
    }

    #[test]
    fn color_hex_parse_and_format() {
        let accent = Color::from_hex("#1874D3").expect("valid");
        assert_eq!(accent.to_rgba8(), [0x18, 0x74, 0xD3, 0xFF]);
        assert_eq!(accent, Color::from_rgb_u32(0x1874D3));
        assert_eq!(accent.to_hex(), "#1874D3");
        let translucent = Color::from_hex("#00000080").expect("valid");
        assert_eq!(translucent.to_rgba8(), [0, 0, 0, 0x80]);
        assert_eq!(translucent.to_hex(), "#00000080");
        assert_eq!(
            Color::from_hex("#abcdef").expect("valid").to_hex(),
            "#ABCDEF"
        );
        assert_eq!(Color::from_hex("1874D3"), Err(ColorParseError::MissingHash));
        assert_eq!(
            Color::from_hex("#1874D"),
            Err(ColorParseError::InvalidLength(5))
        );
        assert_eq!(
            Color::from_hex("#1874D3F"),
            Err(ColorParseError::InvalidLength(7))
        );
        assert_eq!(
            Color::from_hex("#18G4D3"),
            Err(ColorParseError::InvalidDigit)
        );
        assert_eq!(
            Color::from_hex("#+874D3"),
            Err(ColorParseError::InvalidDigit)
        );
        assert_eq!(Color::from_hex("#ççç"), Err(ColorParseError::InvalidDigit));
        assert!(
            ColorParseError::InvalidDigit
                .to_string()
                .contains("hexadecimal")
        );
    }

    #[test]
    fn color_channels_and_clamping() {
        assert_eq!(
            Color::from_rgba8(255, 0, 51, 255),
            Color::new(1.0, 0.0, 0.2, 1.0)
        );
        assert_eq!(
            Color::new(2.0, -1.0, 0.5, 1.0).to_rgba8(),
            [255, 0, 128, 255]
        );
        assert_eq!(Color::TRANSPARENT.to_rgba8(), [0, 0, 0, 0]);
        assert_eq!(Color::WHITE.to_hex(), "#FFFFFF");
        assert_eq!(
            Color::BLACK.with_alpha(0.5).premultiplied(),
            [0.0, 0.0, 0.0, 0.5]
        );
        assert_eq!(
            Color::new(1.0, 0.5, 0.0, 0.5).premultiplied(),
            [0.5, 0.25, 0.0, 0.5]
        );
        assert_eq!(Color::WHITE.with_alpha(0.25).a, 0.25);
    }

    #[test]
    fn color_lerp() {
        let mid = Color::BLACK.lerp(Color::WHITE, 0.5);
        assert!(close(mid.r, 0.5) && close(mid.g, 0.5) && close(mid.b, 0.5) && close(mid.a, 1.0));
        assert_eq!(Color::BLACK.lerp(Color::WHITE, 0.0), Color::BLACK);
        assert_eq!(Color::BLACK.lerp(Color::WHITE, 1.0), Color::WHITE);
        // t is clamped.
        assert_eq!(Color::BLACK.lerp(Color::WHITE, -3.0), Color::BLACK);
        assert_eq!(Color::BLACK.lerp(Color::WHITE, 7.0), Color::WHITE);
        let alpha = Color::BLACK.lerp(Color::TRANSPARENT, 0.25);
        assert!(close(alpha.a, 0.75));
    }

    #[test]
    fn color_over() {
        let half_black = Color::BLACK.with_alpha(0.5);
        let result = half_black.over(Color::WHITE);
        assert!(close(result.r, 0.5) && close(result.a, 1.0));
        assert_eq!(Color::WHITE.over(Color::BLACK), Color::WHITE);
        assert_eq!(
            Color::TRANSPARENT.over(Color::TRANSPARENT),
            Color::TRANSPARENT
        );
        let both_translucent = half_black.over(Color::WHITE.with_alpha(0.5));
        assert!(close(both_translucent.a, 0.75));
        assert!(close(both_translucent.r, 0.25 / 0.75));
    }

    #[test]
    fn color_contrast_matches_wcag() {
        assert!(close(Color::BLACK.relative_luminance(), 0.0));
        assert!(close(Color::WHITE.relative_luminance(), 1.0));
        assert!(close(Color::WHITE.contrast_ratio(Color::BLACK), 21.0));
        assert!(close(Color::BLACK.contrast_ratio(Color::WHITE), 21.0));
        assert!(close(Color::WHITE.contrast_ratio(Color::WHITE), 1.0));
        // Reference value: #777777 on white is the classic 4.48:1 (just below AA).
        let grey = Color::from_rgb_u32(0x777777);
        let ratio = grey.contrast_ratio(Color::WHITE);
        assert!((ratio - 4.48).abs() < 0.01, "{ratio}");
        // Low-end linear segment of the sRGB curve.
        assert!(Color::from_rgb_u32(0x0A0A0A).relative_luminance() < 0.01);
    }

    #[test]
    fn color_serde_uses_hex_strings() {
        let json = serde_json::to_string(&Color::from_rgb_u32(0x1874D3)).expect("serialize");
        assert_eq!(json, "\"#1874D3\"");
        let back: Color = serde_json::from_str("\"#D6E5FA80\"").expect("deserialize");
        assert_eq!(back.to_rgba8(), [0xD6, 0xE5, 0xFA, 0x80]);
        assert!(serde_json::from_str::<Color>("\"blue\"").is_err());
        assert_eq!(String::from(Color::BLACK), "#000000");
    }
}
