use std::borrow::Cow;
use std::collections::HashMap;

pub struct Cache<'a, T: Clone = u8> {
    pub map: HashMap<String, Vec<u8>>,
    pub text: Cow<'a, str>,
    handlers: HashMap<String,
        Handler>,
    pub callback: Box<dyn Fn(u8) -> Vec<u8>>,
    pub pairs: Vec<(u8, u16)>,
    pub nested: Option<HashMap<u8, Vec<Vec<T>>>>,
    pub last: [u8; 4]
}

pub struct Handler;

pub struct Where<T>
where
    T: Clone,
{
    pub value: T,
}

pub struct Wrap<F>(F);

pub struct ArrayVec<T, const N: usize>([T; N]);

pub trait Run {
    fn run(&self) -> u8;
}

impl<F: Fn() -> u8> Run for Wrap<F> {
    fn run(&self) -> u8 {
        (self.0)()
    }
}

impl<T> Run for Where<T>where T: Clone {
    fn run(&self) -> u8 {
        0
    }
}

impl<T: Clone> Where<T> where T: Default {
    pub fn fresh() -> T {
        T::default()
    }
}

impl<'a, T> From<Cache<'a, T>> for Handler
where
    T: Clone + Into<u8>,
{
    fn from(_: Cache<'a, T>) -> Self {
        Handler
    }
}

pub fn make() -> ArrayVec<u8, { 4 * 1024 }> {
    fn nested() {}
    todo!()
}

pub fn bounded<T, U>(items: Vec<T>, f: impl Fn(T) -> U) -> Vec<U>
where
    T: Clone,
    U: Default,
{
    items.into_iter().map(f).collect()
}

pub fn returns_closure() -> impl Fn(u8) -> Box<dyn Fn() -> u8> {
    |x| Box::new(move || x)
}

pub fn shifts(a: u32) -> bool {
    a << 2 > 3 && a >> 1 < 4
}

pub enum Tagged<T> {
    One(Vec<T>),
    Two { map: HashMap<u8, T>, set: Vec<Option<T>> },
}
