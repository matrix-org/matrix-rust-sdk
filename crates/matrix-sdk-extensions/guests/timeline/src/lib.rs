wit_bindgen::generate!({
    world: "timeline",
    exports: {
        world: Timeline,
    },
});

struct Timeline;

impl Guest for Timeline {
    fn greet(who: String) {
        matrix::ui_timeline::std::print(&format!("Hello, {who}!"))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        use wasmtime::{component::*, Config, Engine, Result, Store};

        bindgen!("timeline");

        #[derive(Debug, Default)]
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

        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).unwrap();
        let component = Component::from_file(&engine, "timeline.wasm").unwrap();

        let mut linker = Linker::new(&engine);
        Timeline::add_to_linker(&mut linker, |state: &mut TimelineState| state).unwrap();

        let mut store = Store::new(&engine, TimelineState::default());
        let (bindings, _) = Timeline::instantiate(&mut store, &component, &linker).unwrap();

        bindings.call_greet(&mut store, "Gordon").unwrap();

        assert_eq!(store.data().output, "Hello, Gordon!\n");
    }
}
