#[derive(Debug, Clone)]
#[repr(C)]
pub struct Packed {
    #[doc = "a [bracket"]
    pub first: u8,
    #[cfg_attr(feature = "serde", serde(rename = "second[0]"))]
    pub second: u8,
}

#[cfg(feature = "legacy")] // drop after 2.0
pub fn legacy() {}

#[inline] pub fn same_line() {}

#[cfg_attr(
    feature = "serde",
    derive(Debug)
)]
pub enum Error {
    #[error("index {0} outside [0, {1})")]
    Index(usize, usize),
    #[error("closed ]")]
    Closed,
}

/// Documented.
#[must_use]
/// Continued after the attribute.
pub fn documented() -> u8 {
    1
}

#[allow(dead_code)]
// A plain comment between the attribute and the item.
fn commented_inside() {}

#[inline]
/* a block comment between the attribute and the item */
fn block_inside() {}

#[cfg(test)]
mod tests {
    #[test] // regression #812
    fn regression() {}

    #[test]
    #[should_panic(expected = "[boom")]
    fn panics() {}
}
