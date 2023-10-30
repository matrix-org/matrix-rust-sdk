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
                ctx: &JSContext,
                constructor: &JSObject,
                _arguments: &[JSValue],
            ) -> Result<JSValue, JSException> {
                #[function_callback]
                fn encode_into(
                    ctx: &JSContext,
                    _function: Option<&JSObject>,
                    _this_object: Option<&JSObject>,
                    arguments: &[JSValue],
                ) -> Result<JSValue, JSException> {
                    let string = arguments
                        .get(0)
                        .ok_or_else(|| -> JSException {
                            JSValue::new_string(ctx, "The first argument `string` is missing")
                                .into()
                        })
                        .and_then(|string| {
                            if string.is_string() {
                                string.as_string()
                            } else {
                                Err(JSValue::new_string(
                                    ctx,
                                    "The first argument `string` is not a string",
                                )
                                .into())
                            }
                        })?;

                    let mut utf8_array = arguments
                        .get(1)
                        .ok_or_else(|| -> JSException {
                            JSValue::new_string(ctx, "The second argument `utf8Array` is missing")
                                .into()
                        })
                        .and_then(|class| {
                            if class.is_typed_array()
                                && class.as_typed_array()?.ty()? == JSTypedArrayType::Uint8Array
                            {
                                class.as_typed_array()
                            } else {
                                Err(JSValue::new_string(
                                    ctx,
                                    "The second argument `uint8Array` is not a `Uint8Array`",
                                )
                                .into())
                            }
                        })?;

                    let position = arguments
                        .get(2)
                        .map(|number| -> Result<usize, JSException> {
                            if number.is_number() {
                                Ok(number.as_number()? as usize)
                            } else {
                                Err(JSValue::new_string(
                                    ctx,
                                    "The third argument `position` is not a `number`",
                                )
                                .into())
                            }
                        })
                        .transpose()?
                        .unwrap_or_default();

                    let utf8_array_slice = unsafe { utf8_array.as_mut_slice() }?;

                    for (nth, char) in string.to_string().as_bytes().iter().enumerate() {
                        *utf8_array_slice.get_mut(nth + position).unwrap() = *char;
                    }

                    let result = JSValue::new_from_json(ctx, "{\"read\": 0, \"written\": 0}")
                        .unwrap()
                        .as_object()?;

                    let len = f64::from(string.len() as u32);
                    result.set_property("read", JSValue::new_number(ctx, len))?;
                    result.set_property("written", JSValue::new_number(ctx, len))?;

                    Ok(result.into())
                }

                constructor.set_property(
                    "encodeInto",
                    JSValue::new_function(ctx, "encodeInto", Some(encode_into)),
                )?;

                Ok(constructor.into())
            }

            #[constructor_callback]
            fn text_decoder(
                ctx: &JSContext,
                constructor: &JSObject,
                _arguments: &[JSValue],
            ) -> Result<JSValue, JSException> {
                #[function_callback]
                fn decode(
                    ctx: &JSContext,
                    _function: Option<&JSObject>,
                    _this_object: Option<&JSObject>,
                    arguments: &[JSValue],
                ) -> Result<JSValue, JSException> {
                    let mut utf8_array = arguments
                        .get(0)
                        .ok_or_else(|| -> JSException {
                            JSValue::new_string(ctx, "The first argument `utf8Array` is missing")
                                .into()
                        })
                        .and_then(|class| {
                            if class.is_typed_array()
                                && class.as_typed_array()?.ty()? == JSTypedArrayType::Uint8Array
                            {
                                class.as_typed_array()
                            } else {
                                Err(JSValue::new_string(
                                    ctx,
                                    "The first argument `uint8Array` is not a `Uint8Array`",
                                )
                                .into())
                            }
                        })?;

                    let utf8_array_as_vec = utf8_array.to_vec()?;
                    let string = String::from_utf8_lossy(&utf8_array_as_vec).into_owned();

                    Ok(JSValue::new_string(ctx, string))
                }

                constructor
                    .set_property("decode", JSValue::new_function(ctx, "decode", Some(decode)))?;

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

        let compile_core_function = {
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
                let webassembly_module = webassembly.get_property("Module").as_object()?;

                let wasm_bytes_buffer =
                    unsafe { JSValue::new_typed_array_with_bytes(ctx, wasm_bytes.as_mut_slice()) }?;

                let compiled_module =
                    webassembly_module.call_as_constructor(&[wasm_bytes_buffer])?;

                Ok(compiled_module)
            }

            JSValue::new_function(&context, "compile_core", Some(compile))
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

            global_object.set_property(
                "my_print",
                JSValue::new_function(&context, "my_print", Some(print)),
            )?;

            imports
        };

        let instantiate_core_function = {
            #[function_callback]
            fn instantiate(
                ctx: &JSContext,
                _function: Option<&JSObject>,
                _this_object: Option<&JSObject>,
                arguments: &[JSValue],
            ) -> Result<JSValue, JSException> {
                if arguments.len() > 2 {
                    return Err(JSValue::new_string(
                        ctx,
                        "`instantiate` expects at most 2 arguments",
                    )
                    .into());
                }

                let global_object = ctx.global_object()?;
                let webassembly = global_object.get_property("WebAssembly").as_object()?;
                let webassembly_instance = webassembly.get_property("Instance").as_object()?;

                webassembly_instance.call_as_constructor(arguments)
            }

            JSValue::new_function(&context, "instantiate_core", Some(instantiate))
        };

        // Run the script.
        {
            let timeline_script = fs::read_to_string(TIMELINE_SCRIPT_PATH)
                .unwrap()
                .replace("export async function", "function")
                .replace(" await ", " ")
                .replace("(await ", "(");

            let _result =
                evaluate_script(&context, timeline_script, None, TIMELINE_SCRIPT_PATH, 1)?;
        }

        // Run the `instantiate` function from the script.
        {
            let instantiate_function = global_object.get_property("instantiate");

            assert!(instantiate_function.is_object(), "`instantiate` does not exist");

            let exports = instantiate_function
                .as_object()?
                .call_as_function(
                    None,
                    &[compile_core_function, imports, instantiate_core_function],
                )?
                .as_object()?;

            let greet = exports.get_property("greet").as_object()?;
            let result =
                greet.call_as_function(None, &[JSValue::new_string(&context, "from Rust")]);

            // panic!("{}", result.unwrap().as_string().unwrap().to_string());
        }

        Ok(())
    }
}
