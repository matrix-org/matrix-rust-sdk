pub struct NativeInstance;

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, File},
        io::Read,
    };

    use javascriptcore::*;

    #[test]
    fn test_basics() -> Result<(), JSException> {
        const TIMELINE_SCRIPT_PATH: &str = "guests/timeline/js/timeline.js";

        let context = JSContext::default();
        let global_object = context.global_object()?;

        // Set up.
        {
            #[constructor_callback]
            fn text_encoder(
                _ctx: &JSContext,
                constructor: &JSObject,
                _arguments: &[JSValue],
            ) -> Result<JSValue, JSException> {
                Ok(constructor.into())
            }

            #[constructor_callback]
            fn text_decoder(
                _ctx: &JSContext,
                constructor: &JSObject,
                _arguments: &[JSValue],
            ) -> Result<JSValue, JSException> {
                Ok(constructor.into())
            }

            let text_encoder = JSClass::builder(&context, "TextEncoder")?
                .constructor(Some(text_encoder))
                .build()?;
            let text_decoder = JSClass::builder(&context, "TextDecoder")?
                .constructor(Some(text_decoder))
                .build()?;

            global_object.set_property("TextEncoder", text_encoder.new_object().into())?;
            global_object.set_property("TextDecoder", text_decoder.new_object().into())?;
        }

        // Run the script.
        {
            let timeline_script = fs::read_to_string(TIMELINE_SCRIPT_PATH)
                .unwrap()
                .replace("export async function", "async function");

            let result = evaluate_script(&context, timeline_script, None, TIMELINE_SCRIPT_PATH, 1)?;
        }

        let compile_function = {
            #[function_callback]
            fn compile(
                ctx: &JSContext,
                _function: Option<&JSObject>,
                _this_object: Option<&JSObject>,
                arguments: &[JSValue],
            ) -> Result<JSValue, JSException> {
                if arguments.len() != 1 {
                    return Err(JSValue::new_string(ctx, "`compile` expects one argument").into());
                }

                let wasm_path = arguments[0].as_string()?.to_string();
                dbg!(&wasm_path);

                let mut wasm_file =
                    File::open(format!("guests/timeline/js/{}", wasm_path)).unwrap();
                let mut wasm_bytes = Vec::new();
                wasm_file.read_to_end(&mut wasm_bytes).unwrap();

                let global_object = ctx.global_object()?;
                let webassembly = global_object.get_property("WebAssembly").as_object()?;
                let webassembly_compile = webassembly.get_property("compile").as_object()?;

                let wasm_bytes_buffer =
                    unsafe { JSValue::new_typed_array_with_bytes(ctx, wasm_bytes.as_mut_slice()) }?;

                let compiled_module = webassembly_compile
                    .call_as_function(Some(&webassembly), &[wasm_bytes_buffer])?;

                Ok(compiled_module)
            }

            JSValue::new_function(&context, "compile", Some(compile))
        };

        let imports = {
            #[function_callback]
            fn print(
                ctx: &JSContext,
                _function: Option<&JSObject>,
                _this_object: Option<&JSObject>,
                arguments: &[JSValue],
            ) -> Result<JSValue, JSException> {
                for argument in arguments {
                    println!("==> {}", argument.as_string()?);
                }

                Ok(JSValue::new_undefined(ctx))
            }

            let imports =
                JSValue::new_from_json(&context, r#"{"matrix:ui-timeline/std": {}}"#).unwrap();

            {
                let imports_as_object = imports.as_object()?;

                let ui_timeline_std =
                    imports_as_object.get_property("matrix:ui-timeline/std").as_object()?;
                ui_timeline_std
                    .set_property("print", JSValue::new_function(&context, "print", Some(print)))?;
            }

            imports
        };

        // Run the `instantiate` function.
        {
            let instantiate_function = global_object.get_property("instantiate");

            assert!(instantiate_function.is_object(), "`instantiate` does not exist");

            let instantiate_promise = instantiate_function
                .as_object()?
                .call_as_function(None, &[compile_function, imports])?
                .as_object()?;
            let instantiate_promise_then = instantiate_promise.get_property("then").as_object()?;

            #[function_callback]
            fn on_resolved(
                ctx: &JSContext,
                _function: Option<&JSObject>,
                _this_object: Option<&JSObject>,
                arguments: &[JSValue],
            ) -> Result<JSValue, JSException> {
                todo!("on_resolved")
            }

            #[function_callback]
            fn on_rejected(
                ctx: &JSContext,
                _function: Option<&JSObject>,
                _this_object: Option<&JSObject>,
                arguments: &[JSValue],
            ) -> Result<JSValue, JSException> {
                todo!("on_rejected")
            }

            let on_resolved = JSValue::new_function(&context, "on_resolved", Some(on_resolved));
            let on_rejected = JSValue::new_function(&context, "on_rejected", Some(on_rejected));

            dbg!("here");

            assert!(instantiate_promise_then.is_object());
            let instantiate_promise_then_result = instantiate_promise_then
                .call_as_function(Some(&instantiate_promise), &[on_resolved, on_rejected])?;

            assert!(instantiate_promise_then_result.is_object());
            dbg!(instantiate_promise_then_result.as_string()?);

            // panic!("{}", result.unwrap_err().to_string());

            dbg!("thereeee");

            std::thread::sleep(std::time::Duration::from_secs(2));
        }

        Ok(())

        /*
        test script inside `timeline.js`:

        import fs from 'node:fs';

        function compileCore(wasm_path) {
          const data = fs.readFileSync('/…/guests/timeline/js/' + wasm_path);
          const module = WebAssembly.compile(data);
          return module;
        }

        function on_resolved(exports)  {
          console.log(exports)
          exports.greet("foo");
        }

        const imports = {
         'matrix:ui-timeline/std': {
            print: function(msg) { console.log(msg) }
          }
        };

        instantiate(compileCore, imports).then(on_resolved)
        */
    }
}
