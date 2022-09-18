#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
// We only use a select few functions
#![allow(unused)]
// Don't care about clippy on auto-generated code
#![allow(clippy::all)]
#![allow(clippy::nursery)]
#![allow(clippy::pedantic)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
