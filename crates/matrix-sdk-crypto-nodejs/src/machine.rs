use napi::bindgen_prelude::*;
use napi::{JsFunction, JsObject, JsUndefined};
use ruma::{
    UserId,
    DeviceId,
};
use crate::store::CryptoStore;

#[napi]
pub struct OlmMachine {
    // tuid: Box<UserId>,
    // tdid: Box<DeviceId>,
    // store: CryptoStore,
    do_call_fn: JsFunction,
    obj: JsObject,
}

#[napi]
impl OlmMachine {
    #[napi(constructor)]
    pub fn new(js: JsObject) -> Result<Self> {
        let do_call_fn = js.get_named_property::<JsFunction>("doCall")
            .expect("Function doCall not found");
        println!("Set1? {}", js.has_named_property("doCall")?);
        Ok(OlmMachine {
            // tuid: user_id.try_into().expect("failed to convert user ID"),
            // tdid: device_id.into(),
            // tref,
            // store: CryptoStore::new(js)?,
            do_call_fn,
            obj: js,
        })
    }

    // #[napi(getter)]
    // pub fn get_user_id(&self) -> &str {
    //     self.tuid.as_str()
    // }
    //
    // #[napi(getter)]
    // pub fn get_device_id(&self) -> &str {
    //     self.tdid.as_str()
    // }

    #[napi]
    pub fn test(&self, env: Env) -> Result<JsUndefined> {
        // self.store.call_thing(env, "keyHere")?;
        // Ok(env.get_undefined()?)
        println!("Set2? {}", self.obj.has_named_property("doCall")?);

        self.do_call_fn.call(Option::from(&self.obj), &[
            env.create_string("hello")?
        ])?;
        Ok(env.get_undefined()?)
    }
}

