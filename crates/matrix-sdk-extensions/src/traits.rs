use std::{fmt::Debug, ops::Deref, path::Path};

use crate::Result;

pub trait Module {
    type Environment: Default + Debug;
    type Bindings;

    fn new_environment() -> Self::Environment;
}

pub trait Instance<M>
where
    M: Module,
    Self: Sized,
{
    type EnvironmentReader<'a>: Deref<Target = M::Environment>
    where
        Self: 'a;

    fn new<P>(path_to_entry_point: P) -> Result<Self>
    where
        P: AsRef<Path>;

    fn environment<'a>(&'a self) -> Self::EnvironmentReader<'a>;
}
