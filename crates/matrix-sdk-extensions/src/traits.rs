use std::path::Path;

use crate::Result;

pub(crate) trait Module {
    type State;
    type Bindings;

    fn new_state() -> Self::State;
}

pub(crate) trait Instance<M>
where
    M: Module,
    Self: Sized,
{
    fn new<P>(path_to_entry_point: P) -> Result<Self>
    where
        P: AsRef<Path>;
}
