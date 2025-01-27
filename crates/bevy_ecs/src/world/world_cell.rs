use bevy_utils::tracing::error;

use crate::{
    archetype::ArchetypeComponentId,
    event::{Event, Events},
    storage::SparseSet,
    system::Resource,
    world::{Mut, World},
};
use std::{
    any::TypeId,
    cell::RefCell,
    ops::{Deref, DerefMut},
    rc::Rc,
};

use super::unsafe_world_cell::UnsafeWorldCell;

/// Exposes safe mutable access to multiple resources at a time in a World. Attempting to access
/// World in a way that violates Rust's mutability rules will panic thanks to runtime checks.
pub struct WorldCell<'w> {
    pub(crate) world: UnsafeWorldCell<'w>,
    pub(crate) access: Rc<RefCell<ArchetypeComponentAccess>>,
}

pub(crate) struct ArchetypeComponentAccess {
    access: SparseSet<ArchetypeComponentId, usize>,
}

impl Default for ArchetypeComponentAccess {
    fn default() -> Self {
        Self {
            access: SparseSet::new(),
        }
    }
}

const UNIQUE_ACCESS: usize = 0;
const BASE_ACCESS: usize = 1;
impl ArchetypeComponentAccess {
    const fn new() -> Self {
        Self {
            access: SparseSet::new(),
        }
    }

    fn read(&mut self, id: ArchetypeComponentId) -> bool {
        let id_access = self.access.get_or_insert_with(id, || BASE_ACCESS);
        if *id_access == UNIQUE_ACCESS {
            false
        } else {
            *id_access += 1;
            true
        }
    }

    fn drop_read(&mut self, id: ArchetypeComponentId) {
        let id_access = self.access.get_or_insert_with(id, || BASE_ACCESS);
        *id_access -= 1;
    }

    fn write(&mut self, id: ArchetypeComponentId) -> bool {
        let id_access = self.access.get_or_insert_with(id, || BASE_ACCESS);
        if *id_access == BASE_ACCESS {
            *id_access = UNIQUE_ACCESS;
            true
        } else {
            false
        }
    }

    fn drop_write(&mut self, id: ArchetypeComponentId) {
        let id_access = self.access.get_or_insert_with(id, || BASE_ACCESS);
        *id_access = BASE_ACCESS;
    }
}

impl<'w> Drop for WorldCell<'w> {
    fn drop(&mut self) {
        let mut access = self.access.borrow_mut();

        {
            // SAFETY: we only swap `archetype_component_access`
            let world = unsafe { self.world.world() };
            // SAFETY: the WorldCell has exclusive world access
            let world_cached_access = unsafe { &mut *world.archetype_component_access.get() };

            // give world ArchetypeComponentAccess back to reuse allocations
            std::mem::swap(world_cached_access, &mut *access);
        }
    }
}

// NOTE: this has to be a separate struct, because `map` and `try_map` consumes `self`, moves this
// witness to the new borrow, and needs to bypass dropping `self` altogether. Therefore, it is
// important that `WorldBorrow[Mut]` does not implement `Drop` manually.
struct WorldBorrowWitness<const UNIQUE: bool> {
    archetype_component_id: ArchetypeComponentId,
    access: Rc<RefCell<ArchetypeComponentAccess>>,
}

impl<const UNIQUE: bool> Drop for WorldBorrowWitness<UNIQUE> {
    fn drop(&mut self) {
        let mut access = self.access.borrow_mut();
        if UNIQUE {
            access.drop_write(self.archetype_component_id);
        } else {
            access.drop_read(self.archetype_component_id);
        }
    }
}

pub struct WorldBorrow<'w, T> {
    value: &'w T,
    witness: WorldBorrowWitness<false>,
}

impl<'w, T> WorldBorrow<'w, T> {
    fn try_new(
        value: impl FnOnce() -> Option<&'w T>,
        archetype_component_id: ArchetypeComponentId,
        access: Rc<RefCell<ArchetypeComponentAccess>>,
    ) -> Option<Self> {
        assert!(
            access.borrow_mut().read(archetype_component_id),
            "Attempted to immutably access {}, but it is already mutably borrowed",
            std::any::type_name::<T>(),
        );
        let value = value()?;
        Some(Self {
            value,
            witness: WorldBorrowWitness {
                archetype_component_id,
                access,
            },
        })
    }

    /// Maps a `WorldBorrow<T>` to `WorldBorrow<U>` by applying a function to derive the borrow.
    pub fn map<U>(this: Self, f: impl FnOnce(&T) -> &U) -> WorldBorrow<'w, U> {
        WorldBorrow {
            value: f(this.value),
            witness: this.witness,
        }
    }

    /// Attempt to map a `WorldBorrow<T>` to `WorldBorrow<U>` by applying a fallible function to
    /// derive the borrow. If the transform fails, the original borrow is returned.
    pub fn try_map<U>(
        this: Self,
        f: impl FnOnce(&T) -> Option<&U>,
    ) -> Result<WorldBorrow<'w, U>, Self> {
        match f(this.value) {
            None => Err(this),
            Some(value) => Ok(WorldBorrow {
                value,
                witness: this.witness,
            }),
        }
    }
}

impl<'w, T> Clone for WorldBorrow<'w, T> {
    fn clone(&self) -> Self {
        // NOTE: technically there is a redundant assert in the following call.
        // `self.witness` guarantees that assertion is never violated.
        // It should be cheap enough so let's keep it this way until profiling proves otherwise.
        WorldBorrow::try_new(
            || Some(self.value),
            self.witness.archetype_component_id,
            self.witness.access.clone(),
        )
        .unwrap()
    }
}

impl<'w, T> Deref for WorldBorrow<'w, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.value
    }
}

pub struct WorldBorrowMut<'w, T> {
    value: Mut<'w, T>,
    witness: WorldBorrowWitness<true>,
}

impl<'w, T> WorldBorrowMut<'w, T> {
    fn try_new(
        value: impl FnOnce() -> Option<Mut<'w, T>>,
        archetype_component_id: ArchetypeComponentId,
        access: Rc<RefCell<ArchetypeComponentAccess>>,
    ) -> Option<Self> {
        assert!(
            access.borrow_mut().write(archetype_component_id),
            "Attempted to mutably access {}, but it is already mutably borrowed",
            std::any::type_name::<T>(),
        );
        let value = value()?;
        Some(Self {
            value,
            witness: WorldBorrowWitness {
                archetype_component_id,
                access,
            },
        })
    }

    /// Maps a `WorldBorrow<T>` to `WorldBorrow<U>` by applying a function to derive the borrow.
    pub fn map<U>(this: Self, f: impl FnOnce(&mut T) -> &mut U) -> WorldBorrowMut<'w, U> {
        WorldBorrowMut {
            value: this.value.map_unchanged(f),
            witness: this.witness,
        }
    }

    /// Attempt to map a `WorldBorrow<T>` to `WorldBorrow<U>` by applying a fallible function to derive the borrow.
    pub fn try_map<U>(
        this: Self,
        f: impl FnOnce(&mut T) -> Option<&mut U>,
    ) -> Option<WorldBorrowMut<'w, U>> {
        Some(WorldBorrowMut {
            value: this.value.try_map_unchanged(f)?,
            witness: this.witness,
        })
    }
}

impl<'w, T> Deref for WorldBorrowMut<'w, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.value.deref()
    }
}

impl<'w, T> DerefMut for WorldBorrowMut<'w, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

impl<'w> WorldCell<'w> {
    pub(crate) fn new(world: &'w mut World) -> Self {
        // this is cheap because ArchetypeComponentAccess::new() is const / allocation free
        let access = std::mem::replace(
            world.archetype_component_access.get_mut(),
            ArchetypeComponentAccess::new(),
        );
        // world's ArchetypeComponentAccess is recycled to cut down on allocations
        Self {
            world: world.as_unsafe_world_cell(),
            access: Rc::new(RefCell::new(access)),
        }
    }

    /// Gets a reference to the resource of the given type
    pub fn get_resource<T: Resource>(&self) -> Option<WorldBorrow<'_, T>> {
        let component_id = self.world.components().get_resource_id(TypeId::of::<T>())?;

        let archetype_component_id = self.world.storages().resources.get(component_id)?.id();

        WorldBorrow::try_new(
            // SAFETY: access is checked by WorldBorrow
            || unsafe { self.world.get_resource::<T>() },
            archetype_component_id,
            self.access.clone(),
        )
    }

    /// Gets a reference to the resource of the given type
    ///
    /// # Panics
    ///
    /// Panics if the resource does not exist. Use [`get_resource`](WorldCell::get_resource) instead
    /// if you want to handle this case.
    pub fn resource<T: Resource>(&self) -> WorldBorrow<'_, T> {
        match self.get_resource() {
            Some(x) => x,
            None => panic!(
                "Requested resource {} does not exist in the `World`. 
                Did you forget to add it using `app.insert_resource` / `app.init_resource`? 
                Resources are also implicitly added via `app.add_event`,
                and can be added by plugins.",
                std::any::type_name::<T>()
            ),
        }
    }

    /// Gets a mutable reference to the resource of the given type
    pub fn get_resource_mut<T: Resource>(&self) -> Option<WorldBorrowMut<'_, T>> {
        let component_id = self.world.components().get_resource_id(TypeId::of::<T>())?;
        let archetype_component_id = self.world.storages().resources.get(component_id)?.id();
        WorldBorrowMut::try_new(
            // SAFETY: access is checked by WorldBorrowMut
            || unsafe { self.world.get_resource_mut::<T>() },
            archetype_component_id,
            self.access.clone(),
        )
    }

    /// Gets a mutable reference to the resource of the given type
    ///
    /// # Panics
    ///
    /// Panics if the resource does not exist. Use [`get_resource_mut`](WorldCell::get_resource_mut)
    /// instead if you want to handle this case.
    pub fn resource_mut<T: Resource>(&self) -> WorldBorrowMut<'_, T> {
        match self.get_resource_mut() {
            Some(x) => x,
            None => panic!(
                "Requested resource {} does not exist in the `World`. 
                Did you forget to add it using `app.insert_resource` / `app.init_resource`? 
                Resources are also implicitly added via `app.add_event`,
                and can be added by plugins.",
                std::any::type_name::<T>()
            ),
        }
    }

    /// Gets an immutable reference to the non-send resource of the given type, if it exists.
    pub fn get_non_send_resource<T: 'static>(&self) -> Option<WorldBorrow<'_, T>> {
        let component_id = self.world.components().get_resource_id(TypeId::of::<T>())?;
        let archetype_component_id = self
            .world
            .storages()
            .non_send_resources
            .get(component_id)?
            .id();
        WorldBorrow::try_new(
            // SAFETY: access is checked by WorldBorrowMut
            || unsafe { self.world.get_non_send_resource::<T>() },
            archetype_component_id,
            self.access.clone(),
        )
    }

    /// Gets an immutable reference to the non-send resource of the given type, if it exists.
    ///
    /// # Panics
    ///
    /// Panics if the resource does not exist. Use
    /// [`get_non_send_resource`](WorldCell::get_non_send_resource) instead if you want to handle
    /// this case.
    pub fn non_send_resource<T: 'static>(&self) -> WorldBorrow<'_, T> {
        match self.get_non_send_resource() {
            Some(x) => x,
            None => panic!(
                "Requested non-send resource {} does not exist in the `World`. 
                Did you forget to add it using `app.insert_non_send_resource` / `app.init_non_send_resource`? 
                Non-send resources can also be be added by plugins.",
                std::any::type_name::<T>()
            ),
        }
    }

    /// Gets a mutable reference to the non-send resource of the given type, if it exists.
    pub fn get_non_send_resource_mut<T: 'static>(&self) -> Option<WorldBorrowMut<'_, T>> {
        let component_id = self.world.components().get_resource_id(TypeId::of::<T>())?;
        let archetype_component_id = self
            .world
            .storages()
            .non_send_resources
            .get(component_id)?
            .id();
        WorldBorrowMut::try_new(
            // SAFETY: access is checked by WorldBorrowMut
            || unsafe { self.world.get_non_send_resource_mut::<T>() },
            archetype_component_id,
            self.access.clone(),
        )
    }

    /// Gets a mutable reference to the non-send resource of the given type, if it exists.
    ///
    /// # Panics
    ///
    /// Panics if the resource does not exist. Use
    /// [`get_non_send_resource_mut`](WorldCell::get_non_send_resource_mut) instead if you want to
    /// handle this case.
    pub fn non_send_resource_mut<T: 'static>(&self) -> WorldBorrowMut<'_, T> {
        match self.get_non_send_resource_mut() {
            Some(x) => x,
            None => panic!(
                "Requested non-send resource {} does not exist in the `World`. 
                Did you forget to add it using `app.insert_non_send_resource` / `app.init_non_send_resource`? 
                Non-send resources can also be be added by plugins.",
                std::any::type_name::<T>()
            ),
        }
    }

    /// Sends an [`Event`](crate::event::Event).
    #[inline]
    pub fn send_event<E: Event>(&self, event: E) {
        self.send_event_batch(std::iter::once(event));
    }

    /// Sends the default value of the [`Event`](crate::event::Event) of type `E`.
    #[inline]
    pub fn send_event_default<E: Event + Default>(&self) {
        self.send_event_batch(std::iter::once(E::default()));
    }

    /// Sends a batch of [`Event`](crate::event::Event)s from an iterator.
    #[inline]
    pub fn send_event_batch<E: Event>(&self, events: impl Iterator<Item = E>) {
        match self.get_resource_mut::<Events<E>>() {
            Some(mut events_resource) => events_resource.extend(events),
            None => error!(
                    "Unable to send event `{}`\n\tEvent must be added to the app with `add_event()`\n\thttps://docs.rs/bevy/*/bevy/app/struct.App.html#method.add_event ",
                    std::any::type_name::<E>()
                ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BASE_ACCESS;
    use crate as bevy_ecs;
    use crate::{system::Resource, world::World};
    use std::any::TypeId;

    #[derive(Resource)]
    struct A(u32);

    #[derive(Resource)]
    struct B(u64);

    #[test]
    fn world_cell() {
        let mut world = World::default();
        world.insert_resource(A(1));
        world.insert_resource(B(1));
        let cell = world.cell();
        {
            let mut a = cell.resource_mut::<A>();
            assert_eq!(1, a.0);
            a.0 = 2;
        }
        {
            let a = cell.resource::<A>();
            assert_eq!(2, a.0, "ensure access is dropped");

            let a2 = cell.resource::<A>();
            assert_eq!(
                2, a2.0,
                "ensure multiple immutable accesses can occur at the same time"
            );
        }
        {
            let a = cell.resource_mut::<A>();
            assert_eq!(
                2, a.0,
                "ensure both immutable accesses are dropped, enabling a new mutable access"
            );

            let b = cell.resource::<B>();
            assert_eq!(
                1, b.0,
                "ensure multiple non-conflicting mutable accesses can occur at the same time"
            );
        }
    }

    #[test]
    fn world_access_reused() {
        let mut world = World::default();
        world.insert_resource(A(1));
        {
            let cell = world.cell();
            {
                let mut a = cell.resource_mut::<A>();
                assert_eq!(1, a.0);
                a.0 = 2;
            }
        }

        let u32_component_id = world.components.get_resource_id(TypeId::of::<A>()).unwrap();
        let u32_archetype_component_id = world
            .get_resource_archetype_component_id(u32_component_id)
            .unwrap();
        assert_eq!(world.archetype_component_access.get_mut().access.len(), 1);
        assert_eq!(
            world
                .archetype_component_access
                .get_mut()
                .access
                .get(u32_archetype_component_id),
            Some(&BASE_ACCESS),
            "reused access count is 'base'"
        );
    }

    #[test]
    #[should_panic]
    fn world_cell_double_mut() {
        let mut world = World::default();
        world.insert_resource(A(1));
        let cell = world.cell();
        let _value_a = cell.resource_mut::<A>();
        let _value_b = cell.resource_mut::<A>();
    }

    #[test]
    #[should_panic]
    fn world_cell_ref_and_mut() {
        let mut world = World::default();
        world.insert_resource(A(1));
        let cell = world.cell();
        let _value_a = cell.resource::<A>();
        let _value_b = cell.resource_mut::<A>();
    }

    #[test]
    #[should_panic]
    fn world_cell_mut_and_ref() {
        let mut world = World::default();
        world.insert_resource(A(1));
        let cell = world.cell();
        let _value_a = cell.resource_mut::<A>();
        let _value_b = cell.resource::<A>();
    }

    #[test]
    fn world_cell_ref_and_ref() {
        let mut world = World::default();
        world.insert_resource(A(1));
        let cell = world.cell();
        let _value_a = cell.resource::<A>();
        let _value_b = cell.resource::<A>();
    }
}
