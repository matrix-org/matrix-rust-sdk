use napi::bindgen_prelude::*;
use napi::{JsFunction, JsObject, JsUndefined};

#[napi]
pub struct DemoMachine {
    write_fn: JsFunction,
    store_obj: JsObject
}

#[napi]
impl DemoMachine {
    #[napi(constructor)]
    pub fn new(env: Env, store: JsObject) -> Result<Self> {
        let write_fn = store.get::<&str, JsFunction>("writeValue")?
            .expect("Function writeValue is required");

        println!("Set1? {}", store.has_own_property("writeValue")?); // true

        Ok(DemoMachine {
            write_fn,
            store_obj: store,
        })
    }

    #[napi]
    pub fn do_work(&self, env: Env, key: String, val: String) -> Result<JsUndefined> {
        println!("Set2? {}", self.store_obj.has_own_property("writeValue")?); // false (!!)

        // Try calling the function anyways
        self.write_fn.call(Option::from(&self.store_obj), &[
            env.create_string(&key.as_str())?,
            env.create_string(&val.as_str())?,
        ])?;

        Ok(env.get_undefined()?)
    }
}