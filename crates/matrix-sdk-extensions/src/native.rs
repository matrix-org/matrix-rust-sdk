use std::{
    ops::Deref,
    path::Path,
    sync::{Arc, RwLock, RwLockReadGuard},
};

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

pub struct NativeExports<M>
where
    M: Module,
{
    store: Arc<RwLock<Store<M::Environment>>>,
    bindings: M::Bindings,
}

pub struct NativeInstance<M>
where
    M: Module,
{
    store: Arc<RwLock<Store<M::Environment>>>,
    pub exports: NativeExports<M>,
}

pub struct EnvironmentGuard<'a, M>(RwLockReadGuard<'a, Store<M::Environment>>)
where
    M: Module;

impl<'a, M> Deref for EnvironmentGuard<'a, M>
where
    M: Module,
{
    type Target = M::Environment;

    fn deref(&self) -> &Self::Target {
        self.0.deref().data()
    }
}

impl<M> Instance<M> for NativeInstance<M>
where
    M: Module + ModuleExt<M::Environment, M::Bindings>,
{
    type EnvironmentRef<'a> = EnvironmentGuard<'a, M> where Self: 'a;

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
        let store = Arc::new(RwLock::new(store));

        Ok(Self { store: store.clone(), exports: NativeExports { store, bindings } })
    }

    fn environment<'a>(&'a self) -> Self::EnvironmentRef<'a> {
        EnvironmentGuard(self.store.read().unwrap())
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

        pub trait TimelineExports {
            fn greet(&mut self, arg0: &str) -> crate::Result<()>;
        }

        impl TimelineExports for NativeExports<TimelineModule> {
            fn greet(&mut self, arg0: &str) -> crate::Result<()> {
                let mut store = self.store.write().unwrap();
                let store: &mut Store<TimelineEnvironment> = &mut store;

                self.bindings.call_greet(store, arg0)
            }
        }

        let mut instance = TimelineInstance::new("timeline.wasm").unwrap();

        instance.exports.greet("Gordon").unwrap();

        let env = instance.environment();
        assert_eq!(env.output, "Hello, Gordon!\n");
    }
}
