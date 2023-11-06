use std::path::Path;

use wasmtime::{
    component::{Component, Linker},
    Config, Engine, Store,
};

use crate::{
    traits::{Instance, Module},
    Result,
};

trait ModuleNativeExt<State, Bindings> {
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
    store: Store<M::State>,
    bindings: M::Bindings,
}

impl<M> Instance<M> for NativeInstance<M>
where
    M: Module + ModuleNativeExt<M::State, M::Bindings>,
{
    fn new<P>(wasm_module: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        let mut config = Config::new();
        config.wasm_component_model(true);

        let engine = Engine::new(&config)?;
        let mut store = Store::new(&engine, <M as Module>::new_state());
        let component = Component::from_file(&engine, wasm_module)?;
        let mut linker = Linker::new(&engine);
        M::link(&mut linker, |state: &mut <M as Module>::State| state)?;

        let bindings = M::instantiate_component(&mut store, &component, &linker)?;

        Ok(Self { store, bindings })
    }
}

#[cfg(test)]
mod tests {
    use wasmtime::component::bindgen;

    use super::*;

    #[test]
    fn test_instance() {
        bindgen!("timeline");

        #[derive(Default)]
        struct TimelineState {
            output: String,
        }

        impl matrix::ui_timeline::std::Host for TimelineState {
            fn print(&mut self, msg: String) -> Result<()> {
                self.output.push_str(&msg);
                self.output.push_str("\n");

                Ok(())
            }
        }

        struct TimelineModule;

        impl Module for TimelineModule {
            type State = TimelineState;
            type Bindings = Timeline;

            fn new_state() -> Self::State {
                TimelineState::default()
            }
        }

        impl ModuleNativeExt<TimelineState, Timeline> for TimelineModule {
            fn link(
                linker: &mut Linker<TimelineState>,
                get: impl Fn(&mut TimelineState) -> &mut TimelineState + Send + Sync + Copy + 'static,
            ) -> Result<()> {
                Timeline::add_to_linker(linker, get)
            }

            fn instantiate_component(
                store: &mut Store<TimelineState>,
                component: &Component,
                linker: &Linker<TimelineState>,
            ) -> Result<Timeline> {
                let (bindings, _) = Timeline::instantiate(store, component, linker)?;

                Ok(bindings)
            }
        }

        let mut instance = NativeInstance::<TimelineModule>::new("timeline.wasm").unwrap();

        instance.bindings.call_greet(&mut instance.store, "Gordon").unwrap();

        assert_eq!(instance.store.data().output, "Hello, Gordon!\n");
    }
}
