use napi::bindgen_prelude::*;
use napi::{JsFunction, JsObject, JsTypedArray, JsUndefined};
use crate::store::CryptoStore;

#[napi]
pub struct OlmMachine {
    store: CryptoStore,
}

#[napi]
impl OlmMachine {
    #[napi(constructor)]
    pub fn new(js_store: JsObject) -> Result<Self> {
        Ok(OlmMachine {
            store: CryptoStore::new(
                js_store.get::<&str, JsFunction>("doCall")?.expect("Missing doCall function"),
            )?,
        })
    }

    #[napi]
    pub fn test(&self, env: Env) -> Result<JsUndefined> {
        self.store.call_thing(env, "ping?")?;
        Ok(env.get_undefined()?)
    }
}

