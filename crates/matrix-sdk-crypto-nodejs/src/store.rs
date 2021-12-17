use napi::bindgen_prelude::*;
use napi::{Env, JsFunction, JsObject, JsUndefined};

pub struct CryptoStore {
    do_call_fn: JsFunction,
    obj: JsObject,
}

impl CryptoStore {
    pub(crate) fn new(js: JsObject) -> Result<Self> {
        let do_call_fn = js.get::<&str, JsFunction>("doCall")?
            .expect("Function doCall not found");
        println!("Set1? {}", js.has_own_property("doCall")?);
        Ok(CryptoStore {
            do_call_fn,
            obj: js,
        })
    }

    pub fn call_thing(&self, env: Env, s: &str) -> Result<JsUndefined> {
        println!("Set2? {}", self.obj.has_own_property("doCall")?);
        self.do_call_fn.call(Option::from(&self.obj), &[
            env.create_string(s)?
        ])?;
        Ok(env.get_undefined()?)
    }
}