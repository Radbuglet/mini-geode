#![deny(unsafe_code)] // Unsafe code is only permitted in `core`.

pub mod behavior;
pub mod core;
mod database;
pub mod debug;
pub mod entity;
pub mod event;
pub mod obj;
pub mod query;
mod util;

cfgenius::define! {
    pub HAS_SADDLE_SUPPORT = cfg(feature = "saddle")
}

cfgenius::cond! {
    if macro(HAS_SADDLE_SUPPORT) {
        pub mod saddle;
    }
}

pub mod prelude {
    pub use crate::{
        behavior::{
            behavior, delegate, Behavior, BehaviorProvider, BehaviorRegistry, ComponentInjector,
            InitializerBehaviorList, PartialEntity, SimpleBehaviorList,
        },
        entity::{storage, CompMut, CompRef, Entity, OwnedEntity, Storage},
        event::{
            CountingEvent, EventGroup, EventGroupMarkerWith, EventGroupMarkerWithSeparated,
            EventTarget, ProcessableEventList, QueryableEventList, VecEventList,
        },
        obj::{Obj, OwnedObj},
        query::{
            flush, query, GlobalTag, GlobalVirtualTag, HasGlobalManagedTag, HasGlobalVirtualTag,
            RawTag, Tag, VirtualTag,
        },
    };

    cfgenius::cond! {
        if macro(super::HAS_SADDLE_SUPPORT) {
            pub use crate::saddle::{alias, Cx, cx, Scope, ScopeExt, scope, saddle_delegate};
        }
    }
}

pub use prelude::*;
