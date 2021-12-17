use napi_derive::napi;
use napi::{CallContext, Env, JsFunction, JsObject, JsTypedArray, JsUndefined, Result};
use crate::store::CryptoStore;

#[napi]
pub struct OlmMachine {
    store: CryptoStore,
}

#[napi]
impl OlmMachine {
    #[napi(constructor)]
    pub fn new(env: Env, js_store: JsTypedArray) -> Result<Self> {
        Ok(OlmMachine {
            store: CryptoStore::new(
                env,
                js_store.get_element(0).expect("Missing doCall function"),
            )?,
        })
    }

    #[napi]
    pub fn test1(&self, env: Env) -> Result<JsUndefined> {
        self.store.call_thing("ping?")?;
        Ok(env.get_undefined()?)
    }

    #[napi]
    pub fn test2(&self) {
        println!("from rust");
    }
}

