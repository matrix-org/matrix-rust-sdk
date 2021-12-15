use napi::bindgen_prelude::*;
use napi::{JsFunction, JsObject, JsUndefined, JsUnknown, Ref};
use ruma::{
    UserId,
    DeviceId,
};

#[napi]
pub struct OlmMachine {
    // rs: RSMachine,
    // runtime: Runtime,

    tuid: Box<UserId>,
    tdid: Box<DeviceId>,
    tref: Ref<()>,
}

#[napi]
impl OlmMachine {
    #[napi(constructor, ts_args_type="userId: string, deviceId: string, obj: any")]
    pub fn new(env: Env, user_id: String, device_id: String, obj: JsObject) -> Self {
        let tref = env.create_reference(obj).expect("failed to make ref");
        OlmMachine {
            tuid: user_id.try_into().expect("failed to convert user ID"),
            tdid: device_id.into(),
            tref,
        }
    }

    #[napi(getter)]
    pub fn get_user_id(&self) -> &str {
        self.tuid.as_str()
    }

    #[napi(getter)]
    pub fn get_device_id(&self) -> &str {
        self.tdid.as_str()
    }

    #[napi]
    pub fn test(&mut self, env: Env) -> Result<JsUndefined> {
        let tobj = env.get_reference_value::<JsObject>(&self.tref)?;
        let res = (|tobj: JsObject| -> Result<JsUnknown> {
            tobj.get::<&str, JsFunction>("doCall")
                .expect("failed function").expect("failed function")
                .call(Option::from(&tobj), &[env.create_string("test")?])
        })(tobj);
        res?;
        Ok(env.get_undefined()?)
    }
}

