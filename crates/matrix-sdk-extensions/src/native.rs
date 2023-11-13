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

pub trait ModuleNativeExt<State, Bindings> {
    fn link(
        linker: &mut Linker<State>,
        get: impl Fn(&mut State) -> &mut State + Send + Sync + Copy + 'static,
    ) -> Result<()>;

    fn instantiate_component(
        store: &mut Store<State>,
        component: &Component,
        linker: &Linker<State>,
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
    M: Module + ModuleNativeExt<M::Environment, M::Bindings>,
{
    fn new<P>(wasm_module: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        let mut config = Config::new();
        config.wasm_component_model(true);

        let engine = Engine::new(&config)?;
        let mut store = Store::new(&engine, <M as Module>::new_environment());
        let component = Component::from_file(&engine, wasm_module)?;
        let mut linker = Linker::new(&engine);
        M::link(&mut linker, |state: &mut <M as Module>::Environment| state)?;

        let bindings = M::instantiate_component(&mut store, &component, &linker)?;

        Ok(Self { store, exports: bindings })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instance() {
        bindgen!({
            world: "timeline",
            module: TimelineModule,
            environment: TimelineEnvironment,
            matrix_sdk_extensions_alias: crate,
        });

        struct TimelineModule;

        #[derive(Default)]
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

        assert_eq!(instance.store.data().output, "Hello, Gordon!\n");
    }
}
