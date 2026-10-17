use std::{
    cell::{Ref, RefCell},
    rc::Rc,
};

use log::trace;
use type_map::TypeMap;

#[derive(Debug, Clone)]
pub struct Resources(pub Rc<RefCell<TypeMap>>);

struct Revision<T>(u64, std::marker::PhantomData<T>);

impl Resources {
    /// Creates a new resource map.
    pub fn new(map: TypeMap) -> Self {
        Self(Rc::new(RefCell::new(map)))
    }

    /// Gets a ref to a resource from the resource map. Panics if the resource is not present.
    pub fn get<T: 'static>(&self) -> Ref<'_, T> {
        trace!("getting ref to resource: {:?}", std::any::type_name::<T>());
        Ref::map(self.0.borrow(), |m| {
            m.get::<T>().expect("resource not found")
        })
    }

    pub fn revision<T: 'static>(&self) -> u64 {
        self.0.borrow().get::<Revision<T>>().map_or(0, |r| r.0)
    }

    /// Sets a resource in the resource map.
    pub fn insert<T: 'static>(&self, value: T) {
        let mut map = self.0.borrow_mut();
        let revision = map.get::<Revision<T>>().map_or(1, |r| r.0.wrapping_add(1));
        map.insert(Revision::<T>(revision, std::marker::PhantomData));
        map.insert::<T>(value);
    }
}
