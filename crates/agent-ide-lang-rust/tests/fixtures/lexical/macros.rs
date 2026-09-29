macro_rules! make {
    ($name:ident) => {
        pub fn $name() {}
    };
}

make!(generated);

make! { other }

thread_local! {
    static COUNTER: u8 = 0;
}

pub struct Holder;

impl Holder {
    make!(in_impl);

    pub fn real() {}
}

pub trait Shape {
    make!(in_trait);
    fn area(&self) -> u8;
}

pub fn host() {
    make!(in_body);
    let _ = vec![1, 2];
}
