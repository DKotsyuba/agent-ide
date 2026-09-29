pub struct Foo<const C: char>;

pub trait Tr {}

impl Tr for Foo<'a'> {}

impl Tr for Foo<'b'> {}

impl Foo<'{'> {
    pub fn brace() {}
}

impl<const C: char> Foo<C> where Foo<C>: Tr {
    pub fn generic() {}
}

impl !Send for Foo<'c'> {}

impl Tr for [Foo<'d'>; 2] {}

impl Tr for (Foo<'e'>, fn(u8) -> u8) {}

impl Tr
    for Foo<'f'>
{
}
