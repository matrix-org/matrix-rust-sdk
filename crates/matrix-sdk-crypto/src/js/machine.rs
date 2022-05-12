use std::{collections::BTreeMap, sync::Arc};

use js_sys::{Array, Map, Promise, Set};
use ruma::{DeviceKeyAlgorithm, UInt};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

use crate::js::{identifiers, sync_events};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console)]
    fn log(s: String);
}

#[wasm_bindgen]
#[derive(Debug)]
pub struct OlmMachine {
    inner: Arc<crate::OlmMachine>,
}

#[cfg_attr(feature = "js", wasm_bindgen)]
impl OlmMachine {
    #[wasm_bindgen(constructor)]
    pub fn new(user_id: &identifiers::UserId, device_id: &identifiers::DeviceId) -> Promise {
        let user_id = user_id.inner.clone();
        let device_id = device_id.inner.clone();

        future_to_promise(async move {
            Ok(JsValue::from(OlmMachine {
                inner: Arc::new(crate::OlmMachine::new(user_id.as_ref(), device_id.as_ref()).await),
            }))
        })
    }

    pub fn receive_sync_changes(
        &self,
        to_device_events: &str,
        changed_devices: &sync_events::DeviceLists,
        one_time_key_counts: &Map,
        unused_fallback_keys: &Set,
    ) -> Promise {
        let to_device_events = serde_json::from_str(to_device_events).unwrap();
        let changed_devices = changed_devices.inner.clone();
        let one_time_key_counts: BTreeMap<DeviceKeyAlgorithm, UInt> = one_time_key_counts
            .entries()
            .into_iter()
            .filter_map(|js_value| {
                let pair = Array::from(&js_value.ok()?);
                let (key, value) = (
                    DeviceKeyAlgorithm::from(pair.at(0).as_string()?),
                    UInt::new(pair.at(1).as_f64()? as u64)?,
                );

                Some((key, value))
            })
            .collect();
        let unused_fallback_keys: Option<Vec<DeviceKeyAlgorithm>> = Some(
            unused_fallback_keys
                .values()
                .into_iter()
                .filter_map(|js_value| Some(DeviceKeyAlgorithm::from(js_value.ok()?.as_string()?)))
                .collect(),
        );

        let me = self.inner.clone();

        future_to_promise(async move {
            Ok(JsValue::from(
                serde_json::to_string(
                    &me.receive_sync_changes(
                        to_device_events,
                        &changed_devices,
                        &one_time_key_counts,
                        unused_fallback_keys.as_deref(),
                    )
                    .await
                    .unwrap(),
                )
                .unwrap(),
            ))
        })
    }

    pub fn outgoing_requests(&self) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move {
            Ok(JsValue::from(
                me.outgoing_requests()
                    .await
                    .unwrap()
                    .into_iter()
                    .map(TryFrom::try_from)
                    .collect::<Result<Vec<JsValue>, _>>()
                    .unwrap()
                    .into_iter()
                    .collect::<Array>(),
            ))
        })
    }
}
