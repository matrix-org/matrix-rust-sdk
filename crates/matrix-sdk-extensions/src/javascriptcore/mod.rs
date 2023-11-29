mod from_into;
mod web_api;

use std::{
    error::Error,
    fmt, fs,
    marker::PhantomData,
    ops::Deref,
    path::Path,
    result::Result as StdResult,
    sync::{Arc, Mutex, MutexGuard},
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

pub struct JSExports<M>
where
    M: Module,
{
    #[doc(hidden)]
    pub context: Arc<JSContext>,

    #[doc(hidden)]
    pub exports: JSObject,

    phantom: PhantomData<M>,
}

pub struct JSInstance<M>
where
    M: Module,
{
    #[allow(unused)]
    context: Arc<JSContext>,
    environment: Arc<Mutex<M::Environment>>,
    pub exports: JSExports<M>,
}

pub struct EnvironmentGuard<'a, M>(MutexGuard<'a, M::Environment>)
where
    M: Module;

impl<'a, M> Deref for EnvironmentGuard<'a, M>
where
    M: Module,
{
    type Target = M::Environment;

    fn deref(&self) -> &Self::Target {
        self.0.deref()
    }
}

impl<M> Instance<M> for JSInstance<M>
where
    M: Module + ModuleExt<M::Environment>,
{
    type EnvironmentRef<'a> = EnvironmentGuard<'a, M> where Self: 'a;

    fn new<P>(js_file: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        Ok(Self::new_impl(js_file).map_err(JSError::from)?)
    }

    fn environment<'a>(&'a self) -> Self::EnvironmentRef<'a> {
        EnvironmentGuard(self.environment.lock().unwrap())
    }
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

        let imports = {
            let imports = JSValue::new_from_json(&context, r#"{}"#).unwrap();
            let imports_as_object = imports.as_object()?;

            M::link(&context, &imports_as_object, &environment).unwrap();

            imports
        };

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

        let exports = instantiate.as_object()?.call_as_function(None, &[imports])?.as_object()?;
        let context = Arc::new(context);

        Ok(Self {
            context: context.clone(),
            environment,
            exports: JSExports { context, exports, phantom: Default::default() },
        })
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

        let mut instance = TimelineInstance::new("guests/timeline/js/timeline.js").unwrap();
        let result = instance.exports.greet("Gordon");

        assert!(result.is_ok());

        {
            let env = instance.environment();
            assert_eq!(env.output, "Hello, Gordon!\n");
        }

        Ok(())
    }
}
