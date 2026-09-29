pub struct Rng;

pub fn uses(rng: &mut Rng) -> u8 {
    let gen = 1;
    rng.gen() + gen
}

pub fn after() {
    fn nested() {}
}
