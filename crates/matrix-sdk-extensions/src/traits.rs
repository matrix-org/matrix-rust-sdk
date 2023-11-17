use std::{fmt::Debug, ops::Deref, path::Path};

use crate::Result;

pub(crate) trait Module {
    type Environment: Default + Debug;
    type Bindings;

    fn new_environment() -> Self::Environment;
}

pub(crate) trait Instance<M>
where
    M: Module,
    Self: Sized,
{
    // type EnvironmentReader: Deref<Target = M::Environment>;

    fn new<P>(path_to_entry_point: P) -> Result<Self>
    where
        P: AsRef<Path>;

    // fn environment(&self) -> Self::EnvironmentReader;
}
