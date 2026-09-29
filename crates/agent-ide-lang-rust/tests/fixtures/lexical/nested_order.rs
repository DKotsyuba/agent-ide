pub fn f() -> [u8; {
    fn a() -> usize {
        1
    }
    a()
}]
where
    [(); {
        fn b() -> usize {
            1
        }
        b()
    }]: Sized,
{
    todo!()
}
