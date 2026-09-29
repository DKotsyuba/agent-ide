//! Items of every kind the scanner understands, with no comments between items.

use std::collections::HashMap;
use std::fmt::{self, Display};

extern crate alloc;

pub const LIMIT: usize = 4;
pub static NAME: &str = "items — ünïcode";
static mut COUNTER: u32 = 0;
pub(crate) static mut TOTAL: [u8; 2] = [0; 2];

const TABLE: u8 = {
    fn helper() -> u8 {
        1
    }
    helper()
};

const _: () = {
    fn hidden() {}
};

pub type Alias = HashMap<String, Vec<u8>>;

pub struct Unit;

pub struct Pair(pub u32, String);

pub struct Record {
    pub id: u32,
    pub(crate) name: String,
    r#type: u8,
}

pub union Overlap {
    int: u32,
    float: f32,
}

pub enum Shape {
    Empty,
    Circle(f64),
    Rect { width: f64, height: f64 },
    Flags = 1 << 3,
    Tail {
        first: u8,
        second: u8,
    },
}

pub trait Area {
    const SIDES: usize;
    type Output: Display;
    fn area(&self) -> f64;
    fn describe(&self) -> String {
        String::new()
    }
}

impl Area for Record {
    const SIDES: usize = 0;
    type Output = u32;
    fn area(&self) -> f64 {
        0.0
    }
}

impl Record {
    pub const fn new(id: u32) -> Self {
        Self {
            id,
            name: String::new(),
            r#type: 0,
        }
    }
    pub async fn load(&self) {}
    pub unsafe extern "C" fn raw(&self) {}
    fn r#match(&self) {}
}

unsafe impl Send for Overlap {}

impl fmt::Display for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.id)
    }
}

macro_rules! square {
    ($x:expr) => {
        $x * $x
    };
}

macro_rules! paren (
    () => {};
);

pub mod outer {
    pub mod inner {
        pub fn deep() {}
    }
    mod leaf;
}

pub fn body_items() -> usize {
    let label = "} not a brace {";
    struct Local {
        value: u8,
    }
    impl Local {
        fn get(&self) -> u8 {
            self.value
        }
    }
    let closure = || {
        fn in_closure() {}
    };
    if label.is_empty() {
        fn in_branch() {}
    }
    println!("{}", label);
    macro_rules! local {
        () => {};
    }
    enum Inner {
        A,
    }
    closure();
    label.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds() {}

    #[tokio::test(flavor = "multi_thread")]
    async fn waits() {}
}
