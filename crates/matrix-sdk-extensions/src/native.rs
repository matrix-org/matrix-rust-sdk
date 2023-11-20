use std::path::Path;

pub use matrix_sdk_extensions_macros::wasmtime_bindgen as bindgen;
use wasmtime::{
    component::{Component, Linker},
    Config, Engine, Store,
};

use crate::{
    traits::{Instance, Module},
    Result,
};

pub trait ModuleExt<Environment, Bindings> {
    fn link(
        linker: &mut Linker<Environment>,
        get: impl Fn(&mut Environment) -> &mut Environment + Send + Sync + Copy + 'static,
    ) -> Result<()>;

    fn instantiate_component(
        store: &mut Store<Environment>,
        component: &Component,
        linker: &Linker<Environment>,
    ) -> Result<Bindings>;
}

pub struct NativeInstance<M>
where
    M: Module,
{
    store: Store<M::Environment>,
    exports: M::Bindings,
}

impl<M> Instance<M> for NativeInstance<M>
where
    M: Module + ModuleExt<M::Environment, M::Bindings>,
{
    type EnvironmentReader<'a> = &'a M::Environment where Self: 'a;

    fn new<P>(wasm_module: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        let mut config = Config::new();
        config.wasm_component_model(true);

        let engine = Engine::new(&config)?;
        let mut store = Store::new(&engine, M::new_environment());
        let component = Component::from_file(&engine, wasm_module)?;
        let mut linker = Linker::new(&engine);
        M::link(&mut linker, |state: &mut M::Environment| state)?;

        let bindings = M::instantiate_component(&mut store, &component, &linker)?;

        Ok(Self { store, exports: bindings })
    }

    fn environment<'a>(&'a self) -> Self::EnvironmentReader<'a> {
        self.store.data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instance() {
        bindgen!({
            world: "timeline",
            environment: TimelineEnvironment,
            matrix_sdk_extensions_alias: crate,
        });

        #[derive(Default, Debug)]
        struct TimelineEnvironment {
            output: String,
        }

        impl matrix::ui_timeline::std::Host for TimelineEnvironment {
            fn print(&mut self, msg: String) -> Result<()> {
                self.output.push_str(&msg);
                self.output.push_str("\n");

                Ok(())
            }
        }

        let mut instance = TimelineInstance::new("timeline.wasm").unwrap();

        instance.exports.call_greet(&mut instance.store, "Gordon").unwrap();

        let env = instance.environment();
        assert_eq!(env.output, "Hello, Gordon!\n");
    }
}
