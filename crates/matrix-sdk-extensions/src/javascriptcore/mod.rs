mod web_api;

use std::{error::Error, fmt, fs, path::Path, result::Result as StdResult};

use javascriptcore::{
    evaluate_script, function_callback, JSClass, JSContext, JSException, JSObject, JSValue,
};

use crate::{
    traits::{Instance, Module},
    Result,
};

#[derive(Debug)]
struct JSError {
    error: String,
}

impl fmt::Display for JSError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "JSError: {}", self.error)
    }
}

impl Error for JSError {}

impl From<JSException> for JSError {
    fn from(exception: JSException) -> Self {
        Self { error: exception.to_string() }
    }
}

pub struct JSInstance {
    context: JSContext,
    exports: JSObject,
}

impl<M> Instance<M> for JSInstance
where
    M: Module,
{
    fn new<P>(js_file: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        Ok(Self::new_impl(js_file).map_err(JSError::from)?)
    }
}

impl JSInstance {
    fn new_impl<P>(js_file: P) -> StdResult<Self, JSException>
    where
        P: AsRef<Path>,
    {
        let js_file = js_file.as_ref();
        let context = JSContext::default();
        let global_object = context.global_object()?;

        // Set up.
        {
            let text_encoder = JSClass::builder(&context, "TextEncoder")?
                .constructor(Some(web_api::text_encoder))
                .build()?;
            let text_decoder = JSClass::builder(&context, "TextDecoder")?
                .constructor(Some(web_api::text_decoder))
                .build()?;

            global_object.set_property("TextEncoder", text_encoder.new_object().into())?;
            global_object.set_property("TextDecoder", text_decoder.new_object().into())?;
        }

        let compile_function =
            JSValue::new_function(&context, "compile_core", Some(web_api::compile_wasm));

        let imports = {
            #[function_callback]
            fn print(
                ctx: &JSContext,
                _function: Option<&JSObject>,
                _this_object: Option<&JSObject>,
                arguments: &[JSValue],
            ) -> StdResult<JSValue, JSException> {
                for argument in arguments {
                    println!("==> {}", argument.as_string()?);
                }

                Ok(JSValue::new_undefined(ctx))
            }

            let imports = JSValue::new_from_json(&context, r#"{"matrix:ui-timeline/std": {}}"#)
                // SAFETY: The JSON string is static, and is valid.
                .unwrap();

            {
                let imports_as_object = imports.as_object()?;

                let ui_timeline_std =
                    imports_as_object.get_property("matrix:ui-timeline/std").as_object()?;
                ui_timeline_std
                    .set_property("print", JSValue::new_function(&context, "print", Some(print)))?;
            }

            imports
        };

        let instantiate_function =
            JSValue::new_function(&context, "instantiate_core", Some(web_api::instantiate_wasm));

        // Run the script.
        {
            let timeline_script = fs::read_to_string(js_file).unwrap().replace("export ", "");

            let _result = evaluate_script(
                &context,
                timeline_script,
                None,
                js_file.to_string_lossy().into_owned(),
                1,
            )?;
        }

        // Run the `instantiate` function from the script.
        let instantiate = global_object.get_property("instantiate");

        assert!(instantiate.is_object(), "`instantiate` does not exist");

        let exports = instantiate
            .as_object()?
            .call_as_function(None, &[compile_function, imports, instantiate_function])?
            .as_object()?;

        Ok(Self { context, exports })
    }
}

#[cfg(test)]
mod tests {
    use std::result::Result as StdResult;

    use javascriptcore::*;
    use wasmtime::component::bindgen;

    use super::JSInstance;
    use crate::{
        traits::{Instance, Module},
        Result,
    };

    #[test]
    fn test_basics() -> StdResult<(), JSException> {
        const TIMELINE_SCRIPT_PATH: &str = "guests/timeline/js/timeline.js";

        bindgen!("timeline");

        #[derive(Default)]
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

        struct TimelineModule;

        impl Module for TimelineModule {
            type State = TimelineState;
            type Bindings = Timeline;

            fn new_state() -> Self::State {
                TimelineState::default()
            }
        }

        let instance = <JSInstance as Instance<TimelineModule>>::new(TIMELINE_SCRIPT_PATH).unwrap();

        //     let greet = exports.get_property("greet").as_object()?;
        //     let result =
        //         greet.call_as_function(None, &[JSValue::new_string(&context, "from
        // Rusty")]);

        //     // panic!("{}", result.unwrap().as_string().unwrap().to_string());
        // }

        Ok(())
    }
}
