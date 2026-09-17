use a::combine;

/// Calls crate `a`'s function; never opened or edited by the acceptance scenario itself.
pub fn use_it() -> i32 {
    combine(1, 2)
}
