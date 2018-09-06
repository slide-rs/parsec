use std::sync::atomic::{AtomicUsize, Ordering};

use hibitset::{AtomicBitSet, BitSet, BitSetOr};
use shred::Read;

use error::WrongGeneration;
use join::{Join, ParJoin};
use storage::WriteStorage;
use world::Component;

/// An index is basically the id of an `Entity`.
pub type Index = u32;

/// A wrapper for a read `Entities` resource.
/// Note that this is just `Read<Entities>`, so
/// you can easily use it in your system:
///
/// ```
/// # use specs::prelude::*;
/// # struct Sys;
/// # impl<'a> System<'a> for Sys {
/// type SystemData = (Entities<'a>, /* ... */);
/// # fn run(&mut self, _: Self::SystemData) {}
/// # }
/// ```
///
/// Please note that you should call `World::maintain`
/// after creating / deleting entities with this resource.
///
/// When `.join`ing on `Entities`, you will need to do it like this:
///
/// ```
/// use specs::prelude::*;
///
/// # struct Pos; impl Component for Pos { type Storage = VecStorage<Self>; }
/// # let mut world = World::new(); world.register::<Pos>();
/// # let entities = world.entities(); let positions = world.write_storage::<Pos>();
/// for (e, pos) in (&*entities, &positions).join() {
///     // Do something
/// #   let _ = e;
/// #   let _ = pos;
/// }
/// ```
pub type Entities<'a> = Read<'a, EntitiesRes>;

/// Internally used structure for `Entity` allocation.
#[derive(Default, Debug)]
pub(crate) struct Allocator {
    pub(crate) generations: Vec<Generation>,

    alive: BitSet,
    raised: AtomicBitSet,
    killed: AtomicBitSet,
    cache: EntityCache,
    max_id: AtomicUsize,
}

impl Allocator {
    /// Kills a list of entities immediately.
    pub fn kill(&mut self, delete: &[Entity]) -> Result<(), WrongGeneration> {
        for &entity in delete {
            let id = entity.id() as usize;

            if !self.is_alive(entity) {
                return self.del_err(entity);
            }

            self.alive.remove(entity.id());

            while self.generations.len() <= id as usize {
                self.generations.push(Generation(0));
            }

            if self.raised.remove(entity.id()) {
                self.generations[id] = self.generations[id].raised();
            }
            self.generations[id].die();
        }

        self.cache.extend(delete.iter().map(|e| e.0));

        Ok(())
    }

    /// Kills and entity atomically (will be updated when the allocator is maintained).
    pub fn kill_atomic(&self, e: Entity) -> Result<(), WrongGeneration> {
        if !self.is_alive(e) {
            return self.del_err(e);
        }

        self.killed.add_atomic(e.id());

        Ok(())
    }

    pub(crate) fn del_err(&self, e: Entity) -> Result<(), WrongGeneration> {
        Err(WrongGeneration {
            action: "delete",
            actual_gen: self.generations[e.id() as usize],
            entity: e,
        })
    }

    /// Return `true` if the entity is alive.
    pub fn is_alive(&self, e: Entity) -> bool {
        e.gen() == match self.generations.get(e.id() as usize) {
            Some(g) if !g.is_alive() && self.raised.contains(e.id()) => g.raised(),
            Some(g) => *g,
            None => Generation(1),
        }
    }

    /// Returns the current alive entity with the given `Index`.
    pub fn entity(&self, id: Index) -> Entity {
        let gen = match self.generations.get(id as usize) {
            Some(g) if !g.is_alive() && self.raised.contains(id) => g.raised(),
            Some(g) => *g,
            None => Generation(1),
        };

        Entity(id, gen)
    }

    /// Allocate a new entity
    pub fn allocate_atomic(&self) -> Entity {
        let id = match self.cache.pop_atomic() {
            Some(x) => x,
            None => atomic_add1(&self.max_id).expect("No entity left to allocate") as Index,
        };

        self.raised.add_atomic(id);
        let gen = self
            .generations
            .get(id as usize)
            .map(|&gen| {
                if gen.is_alive() {
                    gen
                } else {
                    gen.raised()
                }
            })
            .unwrap_or(Generation(1));
        Entity(id, gen)
    }

    /// Allocate a new entity
    pub fn allocate(&mut self) -> Entity {
        let id = self.cache.pop_atomic().unwrap_or_else(|| {
            let id = self.max_id.load(Ordering::Relaxed);
            if id as Index == <Index>::max_value() {
                panic!("No entity left to allocate");
            }
            self.max_id.store(id as usize + 1, Ordering::Relaxed);
            id as Index
        });

        while self.generations.len() <= id as usize {
            self.generations.push(Generation(0));
        }
        self.alive.add(id as Index);

        self.generations[id as usize] = self.generations[id as usize].raised();

        Entity(id as Index, self.generations[id as usize])
    }

    /// Maintains the allocated entities, mainly dealing with atomically
    /// allocated or killed entities.
    pub fn merge(&mut self) -> Vec<Entity> {
        use hibitset::BitSetLike;

        let mut deleted = vec![];

        for i in (&self.raised).iter() {
            while self.generations.len() <= i as usize {
                self.generations.push(Generation(0));
            }
            self.generations[i as usize] = self.generations[i as usize].raised();
            self.alive.add(i);
        }
        self.raised.clear();

        for i in (&self.killed).iter() {
            self.alive.remove(i);
            deleted.push(Entity(i, self.generations[i as usize]));
            self.generations[i as usize].die();
        }
        self.killed.clear();

        self.cache.extend(deleted.iter().map(|e| e.0));

        deleted
    }
}

/// An iterator for entity creation.
/// Please note that you have to consume
/// it because iterators are lazy.
///
/// Returned from `Entities::create_iter`.
pub struct CreateIterAtomic<'a>(&'a Allocator);

impl<'a> Iterator for CreateIterAtomic<'a> {
    type Item = Entity;

    fn next(&mut self) -> Option<Entity> {
        Some(self.0.allocate_atomic())
    }
}

/// `Entity` type, as seen by the user.
#[derive(Clone, Copy, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct Entity(Index, Generation);

impl Entity {
    /// Creates a new entity (externally from ECS).
    #[cfg(test)]
    pub fn new(index: Index, gen: Generation) -> Entity {
        Entity(index, gen)
    }

    /// Returns the index of the `Entity`.
    #[inline]
    pub fn id(&self) -> Index {
        self.0
    }

    /// Returns the `Generation` of the `Entity`.
    #[inline]
    pub fn gen(&self) -> Generation {
        self.1
    }
}

/// The entities of this ECS. This is a resource, stored in the `World`.
/// If you just want to access it in your system, you can also use the `Entities`
/// type def.
///
/// **Please note that you should never get
/// this mutably in a system, because it would
/// block all the other systems.**
///
/// You need to call `World::maintain` after creating / deleting
/// entities with this struct.
#[derive(Debug, Default)]
pub struct EntitiesRes {
    pub(crate) alloc: Allocator,
}

impl EntitiesRes {
    /// Creates a new entity atomically.
    /// This will be persistent as soon
    /// as you call `World::maintain`.
    ///
    /// If you want a lazy entity builder, take a look
    /// at `LazyUpdate::create_entity`.
    ///
    /// In case you have access to the `World`,
    /// you can also use `World::create_entity` which
    /// creates the entity and the components immediately.
    pub fn create(&self) -> Entity {
        self.alloc.allocate_atomic()
    }

    /// Returns an iterator which creates
    /// new entities atomically.
    /// They will be persistent as soon
    /// as you call `World::maintain`.
    pub fn create_iter(&self) -> CreateIterAtomic {
        CreateIterAtomic(&self.alloc)
    }

    /// Similar to the `create` method above this
    /// creates an entity atomically, and then returns a
    /// builder which can be used to insert components into
    /// various storages if available.
    pub fn build_entity(&self) -> EntityResBuilder {
        let entity = self.create();
        EntityResBuilder {
            entity,
            entities: self,
            built: false,
        }
    }

    /// Deletes an entity atomically.
    /// The associated components will be
    /// deleted as soon as you call `World::maintain`.
    pub fn delete(&self, e: Entity) -> Result<(), WrongGeneration> {
        self.alloc.kill_atomic(e)
    }

    /// Returns an entity with a given `id`. There's no guarantee for validity,
    /// meaning the entity could be not alive.
    pub fn entity(&self, id: Index) -> Entity {
        self.alloc.entity(id)
    }

    /// Returns `true` if the specified entity is alive.
    #[inline]
    pub fn is_alive(&self, e: Entity) -> bool {
        self.alloc.is_alive(e)
    }
}

impl<'a> Join for &'a EntitiesRes {
    type Type = Entity;
    type Value = Self;
    type Mask = BitSetOr<&'a BitSet, &'a AtomicBitSet>;

    unsafe fn open(self) -> (Self::Mask, Self) {
        (BitSetOr(&self.alloc.alive, &self.alloc.raised), self)
    }

    unsafe fn get(v: &mut &'a EntitiesRes, idx: Index) -> Entity {
        let gen = v
            .alloc
            .generations
            .get(idx as usize)
            .map(|&gen| {
                if gen.is_alive() {
                    gen
                } else {
                    gen.raised()
                }
            })
            .unwrap_or(Generation(1));
        Entity(idx, gen)
    }
}

unsafe impl<'a> ParJoin for &'a EntitiesRes {}

/// An entity builder from `EntitiesRes`.  Allows building an entity with its
/// components if you have mutable access to the component storages.
#[must_use = "Please call .build() on this to finish building it."]
pub struct EntityResBuilder<'a> {
    /// The entity being built
    pub entity: Entity,
    /// The active borrow to `EntitiesRes`, used to delete the entity if the
    /// builder is dropped without called `build()`.
    pub entities: &'a EntitiesRes,
    built: bool,
}

impl<'a> EntityResBuilder<'a> {
    /// Appends a component and associates it with the entity.
    pub fn with<T: Component>(self, c: T, storage: &mut WriteStorage<T>) -> Self {
        storage.insert(self.entity, c).unwrap();
        self
    }

    /// Finishes the building and returns the entity.
    pub fn build(mut self) -> Entity {
        self.built = true;
        self.entity
    }
}

impl<'a> Drop for EntityResBuilder<'a> {
    fn drop(&mut self) {
        if !self.built {
            self.entities.delete(self.entity).unwrap();
        }
    }
}

/// Index generation. When a new entity is placed at an old index,
/// it bumps the `Generation` by 1. This allows to avoid using components
/// from the entities that were deleted.
#[derive(Clone, Copy, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct Generation(pub(crate) i32);

impl Generation {
    #[cfg(test)]
    pub fn new(v: i32) -> Self {
        Generation(v)
    }

    /// Returns the id of the generation.
    #[inline]
    pub fn id(&self) -> i32 {
        self.0
    }

    /// Returns `true` if entities of this `Generation` are alive.
    #[inline]
    pub fn is_alive(&self) -> bool {
        self.0 > 0
    }

    /// Kills this `Generation`.
    ///
    /// # Panics
    ///
    /// Panics in debug mode if it's not alive.
    fn die(&mut self) {
        debug_assert!(self.is_alive());
        self.0 = -self.0;
    }

    /// Revives and increments a dead `Generation`.
    ///
    /// # Panics
    ///
    /// Panics in debug mode if it is alive.
    fn raised(self) -> Generation {
        debug_assert!(!self.is_alive());
        Generation(1 - self.0)
    }
}

#[derive(Default, Debug)]
struct EntityCache {
    cache: Vec<Index>,
    len: AtomicUsize,
}

impl EntityCache {
    fn pop_atomic(&self) -> Option<Index> {
        let mut prev = self.len.load(Ordering::Relaxed);
        while prev != 0 {
            match self.len.compare_exchange_weak(
                prev,
                prev - 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(x) => return Some(self.cache[x - 1]),
                Err(next_prev) => prev = next_prev,
            }
        }
        None
    }

    fn maintain(&mut self) {
        self.cache.truncate(self.len.load(Ordering::Relaxed));
    }
}

impl Extend<Index> for EntityCache {
    fn extend<T: IntoIterator<Item = Index>>(&mut self, iter: T) {
        self.maintain();
        self.cache.extend(iter);
        self.len.store(self.cache.len(), Ordering::Relaxed);
    }
}

fn atomic_add1(i: &AtomicUsize) -> Option<usize> {
    let mut prev = i.load(Ordering::Relaxed);
    while prev != <usize>::max_value() {
        match i.compare_exchange_weak(prev, prev + 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(x) => return Some(x),
            Err(next_prev) => prev = next_prev,
        }
    }
    return None;
}
