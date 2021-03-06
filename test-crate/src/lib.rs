//! this is a test crate zoom zoom zoom

#![allow(unused, unused_variables)]

use rand_chacha::ChaChaCore;

/// thing
pub fn x(y: i32) -> i32 {
    y + 1
}

pub fn gen1(m: Vec<i32>) {}
pub fn gen2(m: Vec<Vec<i32>>) {}
pub fn gen3(m: Vec<Vec<ChaChaCore>>) {}

/// opaque struct
pub struct Opaque {
    member_a: Vec<rand_chacha::ChaChaCore>,
}

impl Opaque {
    fn q<T: std::fmt::Debug>(&self, t: Vec<T>) {}
}

struct Borrows<'a>(&'a [i32]);
impl Borrows<'_> {
    fn new(v: &[i32]) -> Borrows {
        Borrows(v)
    }
}

/// non-opaque struct
/// hmm...
pub struct NonOpaque {
    pub member_b: Vec<i32>,
}

/// partially-opaque struct
pub struct PartiallyOpaque {
    pub member_c: Vec<i32>,
    _nonexhaustive: (),
}

pub struct Generic<T: Sized + std::io::Write> {
    pub generic_member: T,
    pub other: Opaque,
}

#[repr(C)]
pub struct ReprC {
    pub x: i32,
    pub y: *mut (),
    pub w: i64,
}

#[path = "./renamed.rs"]
pub mod x;

pub mod z {
    pub struct InMod {
        pub n: i8,
    }
}

pub struct WackyTupleStruct(i32, pub i32);

pub use rand_chacha::ChaChaRng as ReexportedThing;

pub fn uses_other(z: rand_chacha::ChaChaCore) {}

macro_rules! expands_to_item {
    ($(($x:ty)) 'f +) => {
        pub struct ExpandedAlt {
            thing: &'static std::option::Option<i32>,
            stuff: ($($x),+)
        }
    };
    () => {
        pub struct Expanded {
            thing: &'static std::option::Option<i32>
        }
    }
}
expands_to_item!((i32) 'f (i32) 'f (f64));

expands_to_item!();

macro_rules! wacky_levels {
    ($($name:ident),+ | $($type:ty),+ | $($expr:expr),+) => {
        $(
            pub const $name: $type = $expr;
        )+
    }
}
wacky_levels!(M, N, O | i8, i32, i16 | 0, 1, 2);
