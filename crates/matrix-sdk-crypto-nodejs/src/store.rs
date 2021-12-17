use napi::bindgen_prelude::*;
use napi::{Env, JsFunction, JsObject, JsUndefined};

pub struct CryptoStore {
    do_call_fn: JsFunction,
}

impl CryptoStore {
    pub(crate) fn new(
        do_call_fn: JsFunction,
    ) -> Result<Self> {
        Ok(CryptoStore {
            do_call_fn,
        })
    }

    pub fn call_thing(&self, env: Env, s: &str) -> Result<JsUndefined> {
        // self.do_call_fn.call_without_args(Option::None)?;
        self.do_call_fn.call(Option::None, &[
            env.create_string(s)?
        ])?;
        Ok(env.get_undefined()?)
    }
}