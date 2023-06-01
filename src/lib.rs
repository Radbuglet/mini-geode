#![deny(unsafe_code)]

pub mod core;
pub mod debug;
pub mod entity;
pub mod obj;
pub mod threading;
mod util;

pub mod prelude {
    pub use crate::{
        entity::{storage, CompMut, CompRef, Entity, OwnedEntity, Storage},
        obj::{Obj, OwnedObj},
    };
}

pub use prelude::*;
