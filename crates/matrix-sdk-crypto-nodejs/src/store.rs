use napi_derive::napi;
use napi::{CallContext, Env, JsFunction, JsObject, JsUndefined, Result};

pub struct CryptoStore {
    env: Env,
    do_call_fn: JsFunction,
}

impl CryptoStore {
    pub(crate) fn new(
        env: Env,
        do_call_fn: JsFunction,
    ) -> Result<Self> {
        Ok(CryptoStore {
            env,
            do_call_fn,
        })
    }

    pub fn call_thing(&self, s: &str) -> Result<u32> {
        // self.do_call_fn.call_without_args(Option::None)?;
        self.do_call_fn.call(None, &[
            self.env.create_string(s)?
        ])?;
        Ok(2)
    }
}