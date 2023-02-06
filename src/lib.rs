use std::{
    any::{type_name, Any, TypeId},
    borrow::Borrow,
    cell::{Ref, RefCell, RefMut},
    fmt, hash, iter, mem,
    num::NonZeroU64,
    sync::atomic::{AtomicU64, Ordering},
};

// === Helpers === //

type FxHashBuilder = hash::BuildHasherDefault<rustc_hash::FxHasher>;
type FxHashMap<K, V> = hashbrown::HashMap<K, V, FxHashBuilder>;
type FxHashSet<T> = hashbrown::HashSet<T, FxHashBuilder>;

fn hash_iter<H, E, I>(state: &mut H, iter: I)
where
    H: hash::Hasher,
    E: hash::Hash,
    I: IntoIterator<Item = E>,
{
    for item in iter {
        item.hash(state);
    }
}

fn merge_iters<I, A, B>(a: A, b: B) -> impl Iterator<Item = I>
where
    I: Ord,
    A: IntoIterator<Item = I>,
    B: IntoIterator<Item = I>,
{
    let mut a_iter = a.into_iter().peekable();
    let mut b_iter = b.into_iter().peekable();

    iter::from_fn(move || {
        // Unfortunately, `Option`'s default Ord impl isn't suitable for this.
        match (a_iter.peek(), b_iter.peek()) {
            (Some(a), Some(b)) => {
                if a < b {
                    a_iter.next()
                } else {
                    b_iter.next()
                }
            }
            (Some(_), None) => a_iter.next(),
            (None, Some(_)) => b_iter.next(),
            (None, None) => None,
        }
    })
}

fn leak<T>(value: T) -> &'static T {
    Box::leak(Box::new(value))
}

// === ComponentList === //

#[derive(Copy, Clone)]
struct ComponentType {
    id: TypeId,
    name: &'static str,
    dtor: fn(Entity),
}

impl ComponentType {
    fn of<T: 'static>() -> Self {
        fn dtor<T: 'static>(entity: Entity) {
            drop(storage::<T>().remove_untracked(entity)); // (ignores missing components)
        }

        Self {
            id: TypeId::of::<T>(),
            name: type_name::<T>(),
            dtor: dtor::<T>,
        }
    }
}

impl Ord for ComponentType {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

impl PartialOrd for ComponentType {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl hash::Hash for ComponentType {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl Eq for ComponentType {}

impl PartialEq for ComponentType {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

struct ComponentList {
    comps: Box<[ComponentType]>,
    extensions: RefCell<FxHashMap<TypeId, &'static Self>>,
    de_extensions: RefCell<FxHashMap<TypeId, &'static Self>>,
}

impl hash::Hash for ComponentList {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        hash_iter(state, self.comps.iter());
    }
}

impl Eq for ComponentList {}

impl PartialEq for ComponentList {
    fn eq(&self, other: &Self) -> bool {
        self.comps == other.comps
    }
}

impl ComponentList {
    pub fn empty() -> &'static Self {
        thread_local! {
            static EMPTY: &'static ComponentList = leak(ComponentList {
                comps: Box::new([]),
                extensions: Default::default(),
                de_extensions: Default::default(),
            });
        }

        EMPTY.with(|v| *v)
    }

    pub fn run_dtors(&self, target: Entity) {
        for comp in &*self.comps {
            (comp.dtor)(target);
        }
    }

    pub fn extend(&'static self, with: ComponentType) -> &'static Self {
        if self.comps.contains(&with) {
            return self;
        }

        self.extensions
            .borrow_mut()
            .entry(with.id)
            .or_insert_with(|| Self::find_extension_in_db(&self.comps, with))
    }

    pub fn de_extend(&'static self, without: ComponentType) -> &'static Self {
        if !self.comps.contains(&without) {
            return self;
        }

        self.de_extensions
            .borrow_mut()
            .entry(without.id)
            .or_insert_with(|| Self::find_de_extension_in_db(&self.comps, without))
    }

    // === Database === //

    thread_local! {
        static COMP_LISTS: RefCell<FxHashSet<&'static ComponentList>> = RefCell::new(FxHashSet::from_iter([
            ComponentList::empty(),
        ]));
    }

    fn find_extension_in_db(base_set: &[ComponentType], with: ComponentType) -> &'static Self {
        struct ComponentListSearch<'a>(&'a [ComponentType], ComponentType);

        impl hash::Hash for ComponentListSearch<'_> {
            fn hash<H: hash::Hasher>(&self, state: &mut H) {
                hash_iter(state, merge_iters(self.0, &[self.1]));
            }
        }

        impl hashbrown::Equivalent<&'static ComponentList> for ComponentListSearch<'_> {
            fn equivalent(&self, key: &&'static ComponentList) -> bool {
                // See if the key component list without the additional component
                // is equal to the base list.
                key.comps.iter().filter(|v| **v == self.1).eq(self.0.iter())
            }
        }

        ComponentList::COMP_LISTS.with(|set| {
            *set.borrow_mut()
                .get_or_insert_with(&ComponentListSearch(base_set, with), |_| {
                    leak(Self {
                        comps: merge_iters(base_set.iter().copied(), [with])
                            .collect::<Vec<_>>()
                            .into_boxed_slice(),
                        extensions: Default::default(),
                        de_extensions: Default::default(),
                    })
                })
        })
    }

    fn find_de_extension_in_db(
        base_set: &[ComponentType],
        without: ComponentType,
    ) -> &'static Self {
        struct ComponentListSearch<'a>(&'a [ComponentType], ComponentType);

        impl hash::Hash for ComponentListSearch<'_> {
            fn hash<H: hash::Hasher>(&self, state: &mut H) {
                hash_iter(state, self.0.iter().filter(|v| **v != self.1));
            }
        }

        impl hashbrown::Equivalent<&'static ComponentList> for ComponentListSearch<'_> {
            fn equivalent(&self, key: &&'static ComponentList) -> bool {
                // See if the base component list without the removed component
                // is equal to the key list.
                self.0.iter().filter(|v| **v == self.1).eq(key.comps.iter())
            }
        }

        ComponentList::COMP_LISTS.with(|set| {
            *set.borrow_mut()
                .get_or_insert_with(&ComponentListSearch(base_set, without), |_| {
                    leak(Self {
                        comps: base_set
                            .iter()
                            .copied()
                            .filter(|v| *v != without)
                            .collect::<Vec<_>>()
                            .into_boxed_slice(),
                        extensions: Default::default(),
                        de_extensions: Default::default(),
                    })
                })
        })
    }
}

// === Storage === //

pub fn storage<T: 'static>() -> &'static Storage<T> {
    thread_local! {
        static STORAGES: RefCell<FxHashMap<TypeId, &'static dyn Any>> = Default::default();
    }

    STORAGES.with(|db| {
        db.borrow_mut()
            .entry(TypeId::of::<T>())
            .or_insert_with(|| {
                leak(Storage::<T>(RefCell::new(StorageInner {
                    free_slots: Vec::new(),
                    mappings: FxHashMap::default(),
                })))
            })
            .downcast_ref::<Storage<T>>()
            .unwrap()
    })
}

// TODO: These should likely be allocated in a bump allocator instead.
const BLOCK_SIZE: usize = 128;

type StorageSlot<T> = RefCell<Option<T>>;

#[derive(Debug)]
pub struct Storage<T: 'static>(RefCell<StorageInner<T>>);

#[derive(Debug)]
struct StorageInner<T: 'static> {
    free_slots: Vec<&'static StorageSlot<T>>,
    mappings: FxHashMap<Entity, &'static StorageSlot<T>>,
}

impl<T: 'static> Storage<T> {
    pub fn insert(&self, entity: Entity, value: T) -> Option<T> {
        ALIVE.with(|slots| {
            let mut slots = slots.borrow_mut();
            let slot = slots.get_mut(&entity).unwrap_or_else(|| {
                panic!("attempted to attach a component to the dead {:?}.", entity)
            });

            *slot = slot.extend(ComponentType::of::<T>());
        });

        self.insert_untracked(entity, value)
    }

    fn insert_untracked(&self, entity: Entity, value: T) -> Option<T> {
        let me = &mut *self.0.borrow_mut();

        let slot = match me.mappings.entry(entity) {
            hashbrown::hash_map::Entry::Occupied(entry) => entry.get(),
            hashbrown::hash_map::Entry::Vacant(entry) => {
                if me.free_slots.is_empty() {
                    let block = iter::repeat_with(StorageSlot::default)
                        .take(BLOCK_SIZE)
                        .collect::<Vec<_>>()
                        .leak();

                    me.free_slots.extend(block.into_iter().map(|v| &*v));
                }

                let slot = me.free_slots.pop().unwrap();
                entry.insert(slot);
                slot
            }
        };

        slot.borrow_mut().replace(value)
    }

    pub fn remove(&self, entity: Entity) -> Option<T> {
        ALIVE.with(|slots| {
            let mut slots = slots.borrow_mut();
            let slot = slots.get_mut(&entity).unwrap_or_else(|| {
                panic!(
                    "attempted to remove a component from the dead {:?}.",
                    entity
                )
            });

            *slot = slot.de_extend(ComponentType::of::<T>());
        });

        self.remove_untracked(entity)
    }

    fn remove_untracked(&self, entity: Entity) -> Option<T> {
        let mut me = self.0.borrow_mut();

        if let Some(slot) = me.mappings.remove(&entity) {
            me.free_slots.push(slot);
            slot.borrow_mut().take()
        } else {
            None
        }
    }

    #[inline(always)]
    fn try_get_slot(&self, entity: Entity) -> Option<&'static StorageSlot<T>> {
        self.0.borrow().mappings.get(&entity).copied()
    }

    #[inline(always)]
    fn get_slot(&self, entity: Entity) -> &'static StorageSlot<T> {
        #[cold]
        #[inline(never)]
        fn get_slot_failed<T: 'static>(entity: Entity) -> ! {
            // Try to get the component list or panic if this is a liveness error.
            let comp_list = ALIVE.with(|alive| {
                alive.borrow().get(&entity).copied().unwrap_or_else(|| {
                    panic!(
                        "failed to find component of type {} for dead {:?}",
                        type_name::<T>(),
                        entity
                    )
                })
            });

            // Otherwise, print the regular error message.
            struct CompListFmt<'a>(&'a [ComponentType]);

            impl fmt::Display for CompListFmt<'_> {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    for (i, v) in self.0.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        f.write_str(v.name)?;
                    }
                    Ok(())
                }
            }

            panic!(
                "failed to find component of type {} for {:?} (components: {})",
                type_name::<T>(),
                entity,
                CompListFmt(&comp_list.comps),
            );
        }

        self.try_get_slot(entity)
            .unwrap_or_else(|| get_slot_failed::<T>(entity))
    }

    #[inline(always)]
    pub fn try_get(&self, entity: Entity) -> Option<Ref<'static, T>> {
        self.try_get_slot(entity)
            .map(|slot| Ref::map(slot.borrow(), |v| v.as_ref().unwrap()))
    }

    #[inline(always)]
    pub fn try_get_mut(&self, entity: Entity) -> Option<RefMut<'static, T>> {
        self.try_get_slot(entity)
            .map(|slot| RefMut::map(slot.borrow_mut(), |v| v.as_mut().unwrap()))
    }

    #[inline(always)]
    pub fn get(&self, entity: Entity) -> Ref<'static, T> {
        Ref::map(self.get_slot(entity).borrow(), |v| v.as_ref().unwrap())
    }

    #[inline(always)]
    pub fn get_mut(&self, entity: Entity) -> RefMut<'static, T> {
        RefMut::map(self.get_slot(entity).borrow_mut(), |v| v.as_mut().unwrap())
    }

    #[inline(always)]
    pub fn has(&self, entity: Entity) -> bool {
        self.try_get_slot(entity).is_some()
    }
}

// === Entity === //

thread_local! {
    static ALIVE: RefCell<FxHashMap<Entity, &'static ComponentList>> = Default::default();
}

// TODO: Implement debug labels
#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
pub struct Entity(NonZeroU64);

impl Entity {
    pub fn new() -> OwnedEntity {
        OwnedEntity::new()
    }

    pub fn new_unmanaged() -> Self {
        static ID_GEN: AtomicU64 = AtomicU64::new(1);

        let me = Self(NonZeroU64::new(ID_GEN.fetch_add(1, Ordering::Relaxed)).unwrap());

        ALIVE.with(|slots| slots.borrow_mut().insert(me, ComponentList::empty()));

        me
    }

    pub fn with<T: 'static>(self, comp: T) -> Self {
        self.insert(comp);
        self
    }

    pub fn insert<T: 'static>(self, comp: T) -> Option<T> {
        storage::<T>().insert(self, comp)
    }

    pub fn remove<T: 'static>(self) -> Option<T> {
        storage::<T>().remove(self)
    }

    pub fn try_get<T: 'static>(self) -> Option<Ref<'static, T>> {
        storage::<T>().try_get(self)
    }

    pub fn try_get_mut<T: 'static>(self) -> Option<RefMut<'static, T>> {
        storage::<T>().try_get_mut(self)
    }

    pub fn get<T: 'static>(self) -> Ref<'static, T> {
        storage::<T>().get(self)
    }

    pub fn get_mut<T: 'static>(self) -> RefMut<'static, T> {
        storage::<T>().get_mut(self)
    }

    pub fn has<T: 'static>(self) -> bool {
        storage::<T>().has(self)
    }

    pub fn is_alive(self) -> bool {
        ALIVE.with(|slots| slots.borrow().contains_key(&self))
    }

    pub fn destroy(self) {
        ALIVE.with(|slots| {
            let comp_list = slots
                .borrow_mut()
                .remove(&self)
                .unwrap_or_else(|| panic!("attempted to destroy the already-dead {:?}.", self));

            comp_list.run_dtors(self);
        });
    }
}

// === OwnedEntity === //

#[derive(Debug, Hash, Eq, PartialEq)]
pub struct OwnedEntity(Entity);

impl OwnedEntity {
    // === Lifecycle === //

    pub fn new() -> Self {
        Self(Entity::new_unmanaged())
    }

    pub fn entity(&self) -> Entity {
        self.0
    }

    pub fn unmanage(self) -> Entity {
        let entity = self.0;
        mem::forget(self);

        entity
    }

    pub fn split_guard(self) -> (Self, Entity) {
        let entity = self.entity();
        (self, entity)
    }

    // === Forwards === //

    pub fn with<T: 'static>(self, comp: T) -> Self {
        self.0.insert(comp);
        self
    }

    pub fn insert<T: 'static>(&self, comp: T) -> Option<T> {
        self.0.insert(comp)
    }

    pub fn remove<T: 'static>(&self) -> Option<T> {
        self.0.remove()
    }

    pub fn try_get<T: 'static>(&self) -> Option<Ref<'static, T>> {
        self.0.try_get()
    }

    pub fn try_get_mut<T: 'static>(&self) -> Option<RefMut<'static, T>> {
        self.0.try_get_mut()
    }

    pub fn get<T: 'static>(&self) -> Ref<'static, T> {
        self.0.get()
    }

    pub fn get_mut<T: 'static>(&self) -> RefMut<'static, T> {
        self.0.get_mut()
    }

    pub fn has<T: 'static>(&self) -> bool {
        self.0.has::<T>()
    }

    pub fn is_alive(&self) -> bool {
        self.0.is_alive()
    }
}

impl Borrow<Entity> for OwnedEntity {
    fn borrow(&self) -> &Entity {
        &self.0
    }
}

impl Drop for OwnedEntity {
    fn drop(&mut self) {
        self.0.destroy();
    }
}
