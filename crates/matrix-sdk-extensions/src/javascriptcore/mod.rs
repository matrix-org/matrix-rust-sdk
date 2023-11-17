mod web_api;

use std::{
    error::Error,
    fmt, fs,
    path::Path,
    result::Result as StdResult,
    sync::{Arc, Mutex},
};

use javascriptcore::{evaluate_script, JSClass};
pub use javascriptcore::{function_callback, JSContext, JSException, JSObject, JSValue};
pub use matrix_sdk_extensions_macros::javascriptcore_bindgen as bindgen;

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

pub trait ModuleExt<Environment> {
    fn link(
        context: &JSContext,
        imports: &JSObject,
        environment: &Arc<Mutex<Environment>>,
    ) -> Result<()>;
}

pub struct JSInstance<M>
where
    M: Module,
{
    context: JSContext,
    exports: JSObject,
    environment: Arc<Mutex<M::Environment>>,
}

impl<M> Instance<M> for JSInstance<M>
where
    M: Module + ModuleExt<M::Environment>,
{
    // type EnvironmentReader<'a> = std::sync::MutexGuard<'a, M::Environment>;

    fn new<P>(js_file: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        Ok(Self::new_impl(js_file).map_err(JSError::from)?)
    }

    // fn environment(&self) -> Self::EnvironmentReader {
    //     self.environment.lock().unwrap()
    // }
}

impl<M> JSInstance<M>
where
    M: Module + ModuleExt<M::Environment>,
{
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

        let environment = Arc::new(Mutex::new(M::Environment::default()));

        let compile_function =
            JSValue::new_function(&context, "compile_core", Some(web_api::compile_wasm));

        let imports = {
            /*
            #[function_callback]
            fn print<E>(
                ctx: &JSContext,
                _function: Option<&JSObject>,
                this_object: Option<&JSObject>,
                arguments: &[JSValue],
            ) -> StdResult<JSValue, JSException>
            where
                E: std::fmt::Debug,
            {
                let this_object = this_object.unwrap();

                let env_ptr = this_object.as_number().unwrap() as usize;
                let env = unsafe { Arc::from_raw(env_ptr as *const Mutex<E>) };

                dbg!(&env);

                for argument in arguments {
                    println!("==> {}", argument.as_string()?);
                }

                Ok(JSValue::new_undefined(ctx))
            }
            */

            let imports = JSValue::new_from_json(&context, r#"{}"#).unwrap();
            let imports_as_object = imports.as_object()?;

            M::link(&context, &imports_as_object, &environment).unwrap();

            /*
            {
                let imports_as_object = imports.as_object()?;

                imports_as_object
                    .set_property(
                        "matrix:ui-timeline/std",
                        JSValue::new_from_json(&context, "{}").unwrap(),
                    )
                    .unwrap();

                let ui_timeline_std =
                    imports_as_object.get_property("matrix:ui-timeline/std").as_object()?;

                let f = JSValue::new_function(&context, "print", Some(print::<M::Environment>))
                    .as_object()
                    .unwrap();

                ui_timeline_std.set_property(
                    "print",
                    f.get_property("bind")
                        .as_object()
                        .unwrap()
                        .call_as_function(
                            Some(&f),
                            &[JSValue::new_number(
                                &context,
                                Arc::into_raw(environment.clone()) as usize as f64,
                            )],
                        )
                        .unwrap(),
                )?;
            }
            */

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

        Ok(Self { context, exports, environment })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instance() -> Result<()> {
        bindgen!({
            world: "timeline",
            environment: TimelineEnvironment,
            matrix_sdk_extensions_alias: crate,
        });

        #[derive(Default, Debug)]
        struct TimelineEnvironment {
            output: String,
        }

        impl matrix::ui_timeline::std::Host for TimelineEnvironment {
            fn print(&mut self, msg: String) -> Result<()> {
                self.output.push_str(&msg);
                self.output.push_str("\n");

                Ok(())
            }
        }

        let instance = TimelineInstance::new("guests/timeline/js/timeline.js").unwrap();
        let greet = instance.exports.get_property("greet").as_object().unwrap();
        let result =
            greet.call_as_function(None, &[JSValue::new_string(&instance.context, "Gordon")]);

        let env = instance.environment.lock().unwrap();
        assert_eq!(env.output, "Hello, Gordon!\n");

        Ok(())
    }
}
