use std::{any::TypeId, fmt, marker::PhantomData};

use derive_where::derive_where;

use crate::{
    core::token::MainThreadToken,
    database::{get_global_tag, DbRoot, DbStorage, InertTag, RecursiveQueryGuardTy},
    entity::Storage,
    util::misc::NamedTypeId,
};

// === Tag === //

#[derive_where(Debug, Copy, Clone, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub struct Tag<T: 'static> {
    _ty: PhantomData<fn() -> T>,
    raw: RawTag,
}

impl<T> Tag<T> {
    pub fn new() -> Self {
        Self {
            _ty: PhantomData,
            raw: RawTag::new(NamedTypeId::of::<T>()),
        }
    }

    pub fn global<G: HasGlobalManagedTag<Component = T>>() -> Self {
        Self {
            _ty: PhantomData,
            raw: get_global_tag(NamedTypeId::of::<G>(), NamedTypeId::of::<T>()),
        }
    }

    pub fn raw(self) -> RawTag {
        self.raw
    }
}

impl<T> From<Tag<T>> for RawTag {
    fn from(value: Tag<T>) -> Self {
        value.raw
    }
}

impl<T> Default for Tag<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub struct VirtualTag {
    raw: RawTag,
}

impl VirtualTag {
    pub fn new() -> Self {
        Self {
            raw: RawTag::new(InertTag::inert_ty_id()),
        }
    }

    pub fn global<T: HasGlobalVirtualTag>() -> Self {
        Self {
            raw: get_global_tag(NamedTypeId::of::<T>(), InertTag::inert_ty_id()),
        }
    }

    pub fn raw(self) -> RawTag {
        self.raw
    }
}

impl From<VirtualTag> for RawTag {
    fn from(value: VirtualTag) -> Self {
        value.raw
    }
}

impl Default for VirtualTag {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Copy, Clone, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub struct RawTag(pub(crate) InertTag);

impl RawTag {
    pub fn new(ty: NamedTypeId) -> Self {
        DbRoot::get(MainThreadToken::acquire_fmt("create tag"))
            .spawn_tag(ty)
            .into_dangerous_tag()
    }

    pub fn ty(self) -> TypeId {
        self.0.ty().raw()
    }

    pub fn unerase<T: 'static>(self) -> Option<Tag<T>> {
        (self.0.ty() == NamedTypeId::of::<T>()).then_some(Tag {
            _ty: PhantomData,
            raw: self,
        })
    }
}

impl fmt::Debug for RawTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawTag")
            .field("id", &self.0.id())
            .field("ty", &self.0.ty())
            .finish()
    }
}

// === Global Tags === //

// Traits
pub trait HasGlobalManagedTag: Sized + 'static {
    type Component: 'static;
}

pub trait HasGlobalVirtualTag: Sized + 'static {}

// Aliases
mod tag_alias_sealed {
    use std::marker::PhantomData;

    use derive_where::derive_where;

    #[derive(Debug, Copy, Clone)]
    pub enum Never {}

    #[derive_where(Debug, Copy, Clone)]
    pub enum GlobalTag<T> {
        _Phantom(Never, PhantomData<fn() -> T>),
        GlobalTag,
    }

    #[derive_where(Debug, Copy, Clone)]
    pub enum GlobalVirtualTag<T> {
        _Phantom(Never, PhantomData<fn() -> T>),
        GlobalVirtualTag,
    }

    pub mod globs {
        pub use super::{GlobalTag::GlobalTag, GlobalVirtualTag::GlobalVirtualTag};
    }
}

pub use tag_alias_sealed::{globs::*, GlobalTag, GlobalVirtualTag};

impl<T: HasGlobalManagedTag> From<GlobalTag<T>> for Tag<T::Component> {
    fn from(_: GlobalTag<T>) -> Self {
        Tag::global::<T>()
    }
}

impl<T: HasGlobalManagedTag> From<GlobalTag<T>> for RawTag {
    fn from(value: GlobalTag<T>) -> Self {
        Tag::from(value).into()
    }
}

impl<T: HasGlobalVirtualTag> From<GlobalVirtualTag<T>> for VirtualTag {
    fn from(_: GlobalVirtualTag<T>) -> Self {
        VirtualTag::global::<T>()
    }
}

impl<T: HasGlobalVirtualTag> From<GlobalVirtualTag<T>> for RawTag {
    fn from(value: GlobalVirtualTag<T>) -> Self {
        VirtualTag::from(value).into()
    }
}

// === Flushing === //

#[must_use]
pub fn try_flush() -> bool {
    let token = MainThreadToken::acquire_fmt("flush entity archetypes");
    DbRoot::get(token).flush_archetypes(token).is_ok()
}

fn flush_with_custom_msg(msg: &'static str) {
    autoken::assert_mutably_borrowable::<RecursiveQueryGuardTy>();
    assert!(try_flush(), "{msg}");
}

pub fn flush() {
    flush_with_custom_msg("attempted to flush the entity database while a query was active");
}

// === Queries === //

#[doc(hidden)]
pub mod query_internals {
    use std::hash;

    use crate::{
        core::{
            cell::{OptRef, OptRefMut},
            heap::{array_chunks, HeapSlotBlock},
            token::Token,
            token_cell::NMainCell,
        },
        database::InertEntity,
        entity::Entity,
    };

    use super::*;

    // === Re-exports === //

    pub use {
        crate::{
            core::{cell::MultiRefCellIndex, token::MainThreadToken},
            database::{DbRoot, InertTag, ReifiedTagList},
            entity::{CompMut, CompRef},
            event::QueryableEventList,
            obj::Obj,
            query::try_flush,
        },
        autoken::{PotentialImmutableBorrow, PotentialMutableBorrow},
        std::{
            compile_error,
            convert::Into,
            hint::unreachable_unchecked,
            iter::{empty, IntoIterator, Iterator},
            mem::drop,
            ops::ControlFlow,
            option::Option,
            vec::Vec,
        },
    };

    // === Helpers === //

    pub fn get_tag<T: 'static>(tag: impl Into<Tag<T>>) -> (Tag<T>, InertTag) {
        let tag = tag.into();
        (tag, tag.raw.0)
    }

    pub fn get_storage<T: 'static>(
        db: &mut DbRoot,
        token: &'static MainThreadToken,
        _infer: (Tag<T>, InertTag),
    ) -> &'static DbStorage<T> {
        db.get_storage::<T>(token)
    }

    pub fn inner_storage_to_api_storage<T: 'static>(
        token: &'static MainThreadToken,
        inner: &'static DbStorage<T>,
    ) -> Storage<T> {
        Storage::from_database(token, inner)
    }

    pub fn split_entities_into_chunks(
        entities: &[NMainCell<InertEntity>],
    ) -> &[[NMainCell<InertEntity>; MultiRefCellIndex::COUNT]] {
        array_chunks(entities)
    }

    pub fn fake_oref_without_entity<T: 'static>(
        _heap: &HeapSlotBlock<'_, T, impl Token>,
    ) -> impl Iterator<Item = OptRef<'static, T>> {
        None.into_iter()
    }

    pub fn fake_oref_with_entity<T: 'static>(
        _heap: &HeapSlotBlock<'_, T, impl Token>,
    ) -> impl Iterator<Item = CompRef<'static, T>> {
        None.into_iter()
    }

    pub fn fake_omut_without_entity<T: 'static>(
        _heap: &HeapSlotBlock<'_, T, impl Token>,
    ) -> impl Iterator<Item = OptRefMut<'static, T>> {
        None.into_iter()
    }

    pub fn fake_omut_with_entity<T: 'static>(
        _heap: &HeapSlotBlock<'_, T, impl Token>,
    ) -> impl Iterator<Item = CompMut<'static, T>> {
        None.into_iter()
    }

    pub trait ExtraTagConverter {
        fn into_single(self, extra: &mut Vec<InertTag>) -> Option<InertTag>;
    }

    impl ExtraTagConverter for VirtualTag {
        fn into_single(self, _extra: &mut Vec<InertTag>) -> Option<InertTag> {
            Some(self.raw.0)
        }
    }

    impl<T: 'static> ExtraTagConverter for Tag<T> {
        fn into_single(self, _extra: &mut Vec<InertTag>) -> Option<InertTag> {
            Some(self.raw.0)
        }
    }

    impl ExtraTagConverter for RawTag {
        fn into_single(self, _extra: &mut Vec<InertTag>) -> Option<InertTag> {
            Some(self.0)
        }
    }

    impl<T: HasGlobalManagedTag> ExtraTagConverter for GlobalTag<T> {
        fn into_single(self, _extra: &mut Vec<InertTag>) -> Option<InertTag> {
            Some(Tag::from(self).raw().0)
        }
    }

    impl<T: HasGlobalVirtualTag> ExtraTagConverter for GlobalVirtualTag<T> {
        fn into_single(self, _extra: &mut Vec<InertTag>) -> Option<InertTag> {
            Some(VirtualTag::from(self).raw().0)
        }
    }

    impl<I: IntoIterator> ExtraTagConverter for I
    where
        I::Item: Into<RawTag>,
    {
        fn into_single(self, extra: &mut Vec<InertTag>) -> Option<InertTag> {
            let mut iter = self.into_iter();

            if let Some(first) = iter.next() {
                let first = first.into().0;
                extra.extend(iter.map(|v| v.into().0));

                Some(first)
            } else {
                None
            }
        }
    }

    // === QueryableEventListCallSyntaxHelper === //

    pub struct QueryableEventListCallSyntaxHelperDisamb;

    pub struct QueryableEventListCallSyntaxHelperProof {
        _private: (),
    }

    pub trait QueryableEventListCallSyntaxHelper: QueryableEventList {
        fn query_raw_call_syntax_helper<K, I, F>(
            &self,
            _disamb: QueryableEventListCallSyntaxHelperDisamb,
            version_id: K,
            tags: I,
            handler: F,
        ) -> QueryableEventListCallSyntaxHelperProof
        where
            K: 'static + Send + Sync + hash::Hash + PartialEq,
            I: IntoIterator<Item = RawTag>,
            I::IntoIter: Clone,
            F: FnMut(Entity, &Self::Event) -> ControlFlow<()>,
        {
            self.query_raw(version_id, tags, handler);

            QueryableEventListCallSyntaxHelperProof { _private: () }
        }
    }

    impl<T: ?Sized + QueryableEventList> QueryableEventListCallSyntaxHelper for T {}
}

#[macro_export]
macro_rules! query {
    // === Event query === //
    (
        $(as[$discriminator:expr])?
        for (
            $event_name:ident in $event_src:expr

            $(;
                $(@$entity:ident $(,)?)?
                $($prefix:ident $name:ident in $tag:expr),*
                $(,)?
            )?
        )
        $(+ [$($vtag:expr),*$(,)?])?
        {
            $($body:tt)*
        }
    ) => {'__query: {
        use $crate::query::query_internals::QueryableEventListCallSyntaxHelper as _;

        // Evaluate our tag expressions
        $($( let $name = $crate::query::query_internals::get_tag($tag); )*)?

        // Determine tag list
        let mut virtual_tags_dyn = Vec::<$crate::query::query_internals::InertTag>::new();
        let virtual_tags_static = [
            $($($crate::query::query_internals::Option::Some($name.1),)*)?
            $($($crate::query::query_internals::ExtraTagConverter::into_single($vtag, &mut virtual_tags_dyn),)*)?
        ];

        let tag_list = $crate::query::query_internals::ReifiedTagList {
            static_tags: &virtual_tags_static,
            dynamic_tags: &virtual_tags_dyn,
        };

        // Acquire storages for all tags we care about
        let token = $crate::query::query_internals::MainThreadToken::acquire_fmt("query entities");
        let mut db = $crate::query::query_internals::DbRoot::get(token);

        $($(
            let $name = $crate::query::query_internals::get_storage(&mut db, token, $name);
            let $name = $crate::query::query_internals::inner_storage_to_api_storage(token, $name);
        )*)?
        $crate::query::query_internals::drop(db);

        // Run the event handler
        let _: $crate::query::query_internals::QueryableEventListCallSyntaxHelperProof
            = $event_src.query_raw_call_syntax_helper(
                $crate::query::query_internals::QueryableEventListCallSyntaxHelperDisamb,
                #[allow(unused)]
                {
                    #[derive(Hash, Eq, PartialEq)]
                    struct QueryDiscriminator;

                    let discriminator = QueryDiscriminator;
                    $(let discriminator = $discriminator;)?
                    discriminator
                },
                $crate::query::query_internals::Iterator::map(
                    tag_list.iter(),
                    |inert_tag| inert_tag.into_dangerous_tag()
                ),
                |entity, $event_name| {
                    // Inject all the necessary context
                    $($(
                        let $name = $name.get_slot(entity);
                        $crate::query::query!(@__internal_xform entity; $prefix $name token;);
                    )*)?
                    $($( let $entity = entity; )?)?

                    // Handle breaks
                    // TODO: Can we also handle `return`s?
                    let mut did_run = false;
                    loop {
                        if did_run {
                            // The user must have used `continue`.
                            return $crate::query::query_internals::ControlFlow::Continue(());
                        }
                        did_run = true;

                        let _: () = {
                            $($body)*
                        };

                        // The user completed the loop.
                        #[allow(unreachable_code)]
                        {
                            return $crate::query::query_internals::ControlFlow::Continue(());
                        }
                    }

                    // The user broke out of the loop.
                    #[allow(unreachable_code)]
                    {
                        $crate::query::query_internals::ControlFlow::Continue(())
                    }
                }
            );
    }};

    // === Global query === //
    (
        for (
            $(@$entity:ident $(,)?)?
            $($prefix:ident $name:ident in $tag:expr),*
            $(,)?
        )
        $(+ [$($vtag:expr),*$(,)?])?
        {
            $($body:tt)*
        }
    ) => {'__query: {
        // Evaluate our tag expressions
        $( let $name = $crate::query::query_internals::get_tag($tag); )*

        // Determine tag list
        let mut virtual_tags_dyn = $crate::query::query_internals::Vec::<$crate::query::query_internals::InertTag>::new();
        let virtual_tags_static = [
            $($crate::query::query_internals::Option::Some($name.1),)*
            $($($crate::query::query_internals::ExtraTagConverter::into_single($vtag, &mut virtual_tags_dyn),)*)?
        ];

        // Acquire the main thread token used for our query
        let token = $crate::query::query_internals::MainThreadToken::acquire_fmt("query entities");

        // Acquire the database
        let mut db = $crate::query::query_internals::DbRoot::get(token);

        // Collect the necessary storages and tags
        $( let $name = $crate::query::query_internals::get_storage(&mut db, token, $name); )*

        // Acquire a chunk iterator
        let chunks = $crate::query::query!(
            @__internal_switch;
            cond: {$(@$entity)?}
            true: {
                db.prepare_named_entity_query($crate::query::query_internals::ReifiedTagList {
                    static_tags: &virtual_tags_static,
                    dynamic_tags: &virtual_tags_dyn,
                })
            }
            false: {
                db.prepare_anonymous_entity_query($crate::query::query_internals::ReifiedTagList {
                    static_tags: &virtual_tags_static,
                    dynamic_tags: &virtual_tags_dyn,
                })
            }
        );

        // Acquire a query guard to prevent flushing
        let _guard = db.borrow_query_guard(token);

        // Drop the database to allow safe userland code involving Bort to run
        $crate::query::query_internals::drop(db);

        // For each chunk...
        for chunk in chunks {
            // Fetch the entity iter if it was requested
            $(
                let (chunk, $entity) = chunk.split();
                let mut $entity = $entity.into_iter();
            )?

            // Collect the heaps for each storage
            $( let mut $name = chunk.heaps(&$name.borrow(token)).into_iter(); )*

            // Handle all our heaps
            let mut i = chunk.heap_count();

            // N.B. the following while loop's pattern may be empty if no components
            // are being borrowed or entities are being read, which could cause an
            // underflow when `i` is subtracted. We guard against that scenario here.
            if i == 0 {
                continue;
            }

            // For each heap in that chunk...
            $crate::query::query! {
                @__internal_zip_iter for ($($name in $name,)* $($entity in $entity)?) {
                    // Determine whether we're the last heap of the chunk
                    i -= 1;
                    let is_last = i == 0;

                    // Determine the actual length of our iterator
                    let len = 0;
                    $(let len = $name.len();)*

                    // Construct iterators
                    $(let mut $name = $crate::query::query_internals::Iterator::take(
                        $name.values_and_slots(token),
                        if is_last {
                            $crate::query::query_internals::MultiRefCellIndex::blocks_needed(chunk.last_heap_len())
                        } else {
                            $name.len()
                        },
                    );)*

                    $(
                        let mut $entity = $crate::query::query_internals::split_entities_into_chunks(
                            if is_last {
                                &$entity[..chunk.last_heap_len()]
                            } else {
                                &$entity
                            },
                        )
                        .iter();
                    )?

                    // For each block in that heap...
                    $crate::query::query! {
                        @__internal_zip_iter
                        '__query_block: for ($($name in $name,)* $($entity in $entity)?) {
                            // Attempt to run the fast path.
                            '__fast_path: {
                                // Create iterators for the entire block.
                                $(let mut $entity = $entity.iter();)?
                                $crate::query::query!(
                                    @__internal_acquire_group
                                    token '__fast_path $($entity)?;
                                    $($prefix $name;)*
                                );

                                // For every element in the block...
                                $crate::query::query! {
                                    @__internal_zip_iter
                                    '__query_ent: for ($($name in $name,)* $($entity in $entity)?) {
                                        // Convert the entity cell into a regular entity.
                                        $(let $entity = $entity.get(token).into_dangerous_entity();)?

                                        // Run userland code.
                                        let mut did_run = false;
                                        loop {
                                            if did_run {
                                                // The user must have used `continue`.
                                                continue '__query_ent;
                                            }
                                            did_run = true;
                                            let _: () = {
                                                $($body)*
                                            };
                                            // The user completed the loop.
                                            #[allow(unreachable_code)]
                                            {
                                                continue '__query_ent;
                                            }
                                        }
                                        // The user broke out of the loop.
                                        #[allow(unreachable_code)]
                                        {
                                            break '__query;
                                        }
                                    }
                                }
                                continue '__query_block;
                            };

                            // Otherwise, we're running the slow path.
                            // TODO
                        }
                    }
                }
            }
        }
    }};

    // === Helpers === //
    (
        @__internal_switch;
        cond: {}
        true: {$($true:tt)*}
        false: {$($false:tt)*}
    ) => {
        $($false)*
    };
    (
        @__internal_switch;
        cond: {$($there:tt)+}
        true: {$($true:tt)*}
        false: {$($false:tt)*}
    ) => {
        $($true)*
    };

    // N.B. these work on both `Slot`s and `DirectSlot`s
    (@__internal_xform $($entity:ident)?;) => {};
    (@__internal_xform $($entity:ident)?; slot $name:ident $token:ident; $($rest:tt)*) => {
        $crate::query::query!(@__internal_xform $($entity)?; $($rest)*);
    };
    (@__internal_xform $($entity:ident)?; ref $name:ident $token:ident; $($rest:tt)*) => {
        let $name = &*$name.borrow($token);
        $crate::query::query!(@__internal_xform $($entity)?; $($rest)*);
    };
    (@__internal_xform $($entity:ident)?; mut $name:ident $token:ident; $($rest:tt)*) => {
        let $name = &mut *$name.borrow_mut($token);
        $crate::query::query!(@__internal_xform $($entity)?; $($rest)*);
    };
    (@__internal_xform; oref $name:ident $token:ident; $($rest:tt)*) => {
        let $name = $name.borrow($token);
        $crate::query::query!(@__internal_xform; $($rest)*);
    };
    (@__internal_xform; omut $name:ident $token:ident; $($rest:tt)*) => {
        let mut $name =  $name.borrow_mut($token);
        $crate::query::query!(@__internal_xform; $($rest)*);
    };
    (@__internal_xform $entity:ident; oref $name:ident $token:ident; $($rest:tt)*) => {
        let $name = $crate::query::query_internals::CompRef::new(
            $crate::query::query_internals::Obj::from_raw_parts(
                $entity,
                $crate::query::query_internals::Into::into($name),
            ),
            $name.borrow($token),
        );
        $crate::query::query!(@__internal_xform $entity; $($rest)*);
    };
    (@__internal_xform $entity:ident; omut $name:ident $token:ident; $($rest:tt)*) => {
        let mut $name = $crate::query::query_internals::CompMut::new(
            $crate::query::query_internals::Obj::from_raw_parts(
                $entity,
                $crate::query::query_internals::Into::into($name),
            ),
            $name.borrow_mut($token),
        );
        $crate::query::query!(@__internal_xform $entity; $($rest)*);
    };
    (@__internal_xform; obj $name:ident $token:ident; $($rest:tt)*) => {
        $crate::query::query_internals::compile_error!("`obj` qualifier can only be used on queries which also request the entity ID");
    };
    (@__internal_xform $entity:ident; obj $name:ident $token:ident; $($rest:tt)*) => {
        let $name = $crate::query::query_internals::Obj::from_raw_parts(
            $entity,
            $crate::query::query_internals::Into::into($name),
        );
        $crate::query::query!(@__internal_xform $entity; $($rest)*);
    };

    (@__internal_acquire_group $token:ident $label:lifetime $($entity:ident)?;) => {};
    (@__internal_acquire_group $token:ident $label:lifetime $($entity:ident)?; slot $name:ident; $($rest:tt)*) => {
        let mut $name = $name.slots();
        $crate::query::query!(@__internal_acquire_group $token $label $($entity)?; $($rest)*);
    };
    (@__internal_acquire_group $token:ident $label:lifetime $($entity:ident)?; ref $name:ident; $($rest:tt)*) => {
        let loaner = $crate::query::query_internals::PotentialImmutableBorrow::new();
        let $name = match $name.values().try_borrow_all($token, &loaner) {
            $crate::query::query_internals::Option::Some(v) => v,
            $crate::query::query_internals::Option::None => break $label (),
        };
        let mut $name = $name.iter();
        $crate::query::query!(@__internal_acquire_group $token $label $($entity)?; $($rest)*);
    };
    (@__internal_acquire_group $token:ident $label:lifetime $($entity:ident)?; mut $name:ident; $($rest:tt)*) => {
        let mut loaner = $crate::query::query_internals::PotentialMutableBorrow::new();
        let mut $name = match $name.values().try_borrow_all_mut($token, &mut loaner) {
            $crate::query::query_internals::Option::Some(v) => v,
            $crate::query::query_internals::Option::None => break $label (),
        };
        let mut $name = $name.iter_mut();
        $crate::query::query!(@__internal_acquire_group $token $label $($entity)?; $($rest)*);
    };
    (@__internal_acquire_group $token:ident $label:lifetime $entity:ident; oref $name:ident; $($rest:tt)*) => {
        let mut $name = $crate::query::query_internals::fake_oref_with_entity(&$name);
        if true {
            break $label ();
        }
        $crate::query::query!(@__internal_acquire_group $token $label $entity; $($rest)*);
    };
    (@__internal_acquire_group $token:ident $label:lifetime; oref $name:ident; $($rest:tt)*) => {
        let mut $name = $crate::query::query_internals::fake_oref_without_entity(&$name);
        if true {
            break $label ();
        }
        $crate::query::query!(@__internal_acquire_group $token $label; $($rest)*);
    };
    (@__internal_acquire_group $token:ident $label:lifetime $entity:ident; omut $name:ident; $($rest:tt)*) => {
        let mut $name = $crate::query::query_internals::fake_omut_with_entity(&$name);
        if true {
            break $label ();
        }
        $crate::query::query!(@__internal_acquire_group $token $label $entity; $($rest)*);
    };
    (@__internal_acquire_group $token:ident $label:lifetime; omut $name:ident; $($rest:tt)*) => {
        let mut $name = $crate::query::query_internals::fake_omut_without_entity(&$name);
        if true {
            break $label ();
        }
        $crate::query::query!(@__internal_acquire_group $token $label; $($rest)*);
    };
    (@__internal_acquire_group $token:ident $label:lifetime $entity:ident; obj $name:ident; $($rest:tt)*) => {
        let mut $name = $crate::query::query_internals::Iterator::map(
            $crate::query::query_internals::Iterator::zip($name.slots(), $entity),
            |(slot, entity)| $crate::query::query_internals::Obj::from_raw_parts(
                entity.get($token).into_dangerous_entity(),
                slot.slot(),
            ),
        );
        $crate::query::query!(@__internal_acquire_group $token $label $entity; $($rest)*);
    };
    (@__internal_acquire_group $token:ident $label:lifetime; obj $name:ident; $($rest:tt)*) => {
        $crate::query::query_internals::compile_error!("`obj` qualifier can only be used on queries which also request the entity ID");
        $crate::query::query!(@__internal_acquire_group $token $label; $($rest)*);
    };
    (
        @__internal_zip_iter
        for () {
            $($body:tt)*
        }
    ) => {{
        if false {
            $($body)*
        }
    }};
    (
        @__internal_zip_iter
        $($label:lifetime: )?
        for ($first_name:ident in $first_iter:expr $(, $name:ident in $iter:expr)*$(,)?) {
            $($body:tt)*
        }
    ) => {{
        let iter = $crate::query::query_internals::IntoIterator::into_iter($first_iter);
        $(let iter = $crate::query::query_internals::Iterator::zip(
            $crate::query::query_internals::IntoIterator::into_iter($iter),
            iter,
        );)*

        $($label:)? for decomposed in iter {
            // Get the `$name`s in reverse order.
            $(let ($name, decomposed) = decomposed;)*

            // This, however, is fine.
            let $first_name = decomposed;

            // Construct another cons-list.
            let recomposed = ();
            $(let recomposed = ($name, recomposed);)*
            $(let ($name, recomposed) = recomposed;)*

            $($body)*
        }
    }};
}

pub use query;
