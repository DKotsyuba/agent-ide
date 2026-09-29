pub struct Host;

pub fn host(flag: bool) -> usize {
    match flag {
        true => {
            fn in_arm() {}
            1
        }
        false => 0,
    };
    let _ = async {
        struct InAsync;
    };
    let array: [u8; {
        const N: usize = 2;
        N
    }] = [0; 2];
    unsafe {
        fn in_unsafe() {}
    }
    loop {
        enum InLoop {}
        break;
    }
    let _ = |value: u8| {
        static IN_CLOSURE: u8 = 0;
        value
    };
    array.len()
}

impl Host {
    const _: () = {
        fn hidden_in_impl() {}
    };

    pub fn visible() {}
}

pub struct Sized2 {
    pub field: [u8; {
        fn in_field_type() -> usize {
            1
        }
        in_field_type()
    }],
}

pub enum Discriminants {
    First = {
        const BASE: isize = 4;
        BASE
    },
    Second,
}
