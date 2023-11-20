use std::{env, path::PathBuf};

use heck::{ToSnakeCase as _, ToUpperCamelCase as _};
use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;
use syn::{
    braced,
    parse::{Error, Parse, ParseStream, Result},
    parse_macro_input,
    punctuated::Punctuated,
    LitStr, Token, TypePath,
};
use wit_parser::{Resolve, Results, Type, WorldId, WorldItem};

struct Configuration {
    resolve: Resolve,
    world: WorldId,
    // module_type: TypePath,
    environment_type: TypePath,
    matrix_sdk_extensions_alias: Option<TypePath>,
}

mod keyword {
    use syn::custom_keyword;

    custom_keyword!(world);
    custom_keyword!(module);
    custom_keyword!(environment);
    custom_keyword!(matrix_sdk_extensions_alias);
}

impl Parse for Configuration {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let call_site = Span::call_site();

        let mut world = None;
        let mut environment_type = None;
        // let mut module = None;
        let mut matrix_sdk_extensions_alias = None;

        {
            let sub_input;
            braced!(sub_input in input);
            let options = Punctuated::<Opt, Token![,]>::parse_terminated(&sub_input)?;

            for option in options.into_pairs() {
                match option.into_value() {
                    Opt::World(w) => {
                        world = Some(w.value());
                    }

                    // Opt::Module(m) => module = Some(m),
                    Opt::Environment(e) => environment_type = Some(e),

                    Opt::MatrixSdkExtensionsAlias(a) => matrix_sdk_extensions_alias = Some(a),
                }
            }
        }

        let world = world.expect("`world` is missing");
        // let module_type = module.expect("`module` is missing");
        let environment_type = environment_type.expect("`environment` is missing");

        let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
        let wit_directory = root.join("wit");
        let mut resolve = Resolve::default();

        let (pkg, sources) =
            resolve.push_dir(&wit_directory).map_err(|e| Error::new(call_site, e))?;

        let world = resolve
            .select_world(pkg, Some(&world))
            .map_err(|e| Error::new(call_site, format!("{e:?}")))?;

        Ok(Configuration {
            resolve,
            world,
            // module_type,
            environment_type,
            matrix_sdk_extensions_alias,
        })
    }
}

enum Opt {
    World(LitStr),
    // Module(TypePath),
    Environment(TypePath),
    MatrixSdkExtensionsAlias(TypePath),
}

impl Parse for Opt {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let look = input.lookahead1();

        if look.peek(keyword::world) {
            input.parse::<keyword::world>()?;
            input.parse::<Token![:]>()?;

            Ok(Opt::World(input.parse()?))
        // } else if look.peek(keyword::module) {
        //     input.parse::<keyword::module>()?;
        //     input.parse::<Token![:]>()?;

        //     Ok(Opt::Module(input.parse()?))
        } else if look.peek(keyword::environment) {
            input.parse::<keyword::environment>()?;
            input.parse::<Token![:]>()?;

            Ok(Opt::Environment(input.parse()?))
        } else if look.peek(keyword::matrix_sdk_extensions_alias) {
            input.parse::<keyword::matrix_sdk_extensions_alias>()?;
            input.parse::<Token![:]>()?;

            Ok(Opt::MatrixSdkExtensionsAlias(input.parse()?))
        } else {
            Err(look.error())
        }
    }
}

fn to_rust_upper_camel_case(name: &str) -> String {
    match name {
        "host" => "Host_".to_owned(),
        name => name.to_upper_camel_case(),
    }
}

fn to_rust_type(ty: &Type) -> String {
    match ty {
        Type::Bool => "bool".to_owned(),
        Type::U8 => "u8".to_owned(),
        Type::U16 => "u16".to_owned(),
        Type::U32 => "u32".to_owned(),
        Type::U64 => "u64".to_owned(),
        Type::S8 => "i8".to_owned(),
        Type::S16 => "i16".to_owned(),
        Type::S32 => "i32".to_owned(),
        Type::S64 => "i64".to_owned(),
        Type::Float32 => "f32".to_owned(),
        Type::Float64 => "f64".to_owned(),
        Type::Char => "char".to_owned(),
        Type::String => "String".to_owned(),
        Type::Id(_) => unimplemented!("{:?}", ty),
    }
}

#[cfg(feature = "wasmtime")]
#[proc_macro]
pub fn wasmtime_bindgen(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let call_site = Span::call_site();

    let configuration = parse_macro_input!(input as Configuration);
    let opts = wasmtime_wit_bindgen::Opts::default();
    let generated =
        opts.generate(&configuration.resolve, configuration.world).parse::<TokenStream>().unwrap();

    let resolve = &configuration.resolve;
    let world = &resolve.worlds[configuration.world];
    let world_type_name = to_rust_upper_camel_case(&world.name);
    let world_type = Ident::new(&world_type_name, call_site);
    let module_type = Ident::new(&format!("{}Module", world_type_name), call_site);
    let environment_type = &configuration.environment_type;

    let use_matrix_sdk_extensions = configuration
        .matrix_sdk_extensions_alias
        .map(|alias| quote! { use #alias as matrix_sdk_extensions; });

    let private_mod = Ident::new(&format!("__private_{}", world.name), call_site);

    let host_trait = {
        let package = world.package.and_then(|package| resolve.packages.get(package));

        let mut imports = Vec::new();

        for (_import_key, import) in world.imports.iter() {
            match import {
                WorldItem::Interface(interface_id) => {
                    let interface = resolve.interfaces.get(*interface_id).unwrap();

                    let mut functions = Vec::new();

                    for function in interface.functions.values() {
                        let function_name = Ident::new(&function.name.to_snake_case(), call_site);
                        let arguments = function.params.iter().map(|(name, ty)| {
                            let name = Ident::new(&name.to_snake_case(), call_site);
                            let ty = Ident::new(&to_rust_type(ty), call_site);

                            quote! {
                                #name: #ty
                            }
                        });
                        let results = match &function.results {
                            Results::Named(results) => match results.len() {
                                0 => "()".to_owned(),
                                1 => to_rust_type(&results[0].1),
                                _ => results.iter().fold(String::new(), |mut acc, (name, ty)| {
                                    acc.push_str(&format!(
                                        "{name}: {ty}, ",
                                        ty = to_rust_type(&ty)
                                    ));
                                    acc
                                }),
                            },
                            r => unimplemented!("{:?}", r),
                        }
                        .parse::<TokenStream>()
                        .unwrap();

                        functions.push(quote! {
                            fn #function_name( &mut self #( , #arguments )* ) -> matrix_sdk_extensions::Result< #results >;
                        });
                    }

                    let mut import = quote! {
                        pub trait Host {
                            #( #functions )*
                        }
                    };

                    if let Some(interface_name) = &interface.name {
                        let interface_name = Ident::new(&interface_name.to_snake_case(), call_site);

                        import = quote! {
                            pub mod #interface_name {
                                #use_matrix_sdk_extensions

                                #import
                            }
                        };
                    }

                    imports.push(import);
                }
                i => unimplemented!("{:?}", i),
            }
        }

        let mut output = quote! {
            #( #imports )*
        };

        if let Some(package) = package {
            let package_namespace = Ident::new(&package.name.namespace.to_snake_case(), call_site);
            let package_name = Ident::new(&package.name.name.to_snake_case(), call_site);

            output = quote! {
                pub mod #package_namespace {
                    pub mod #package_name {
                        #output
                    }
                }
            };
        }

        output
    };

    let impl_private_host_trait = {
        quote! {
            impl #private_mod::matrix::ui_timeline::std::Host for #environment_type {
                fn print(&mut self, msg: String) -> matrix_sdk_extensions::Result<()> {
                    matrix::ui_timeline::std::Host::print(self, msg)
                }
            }
        }
    };

    let impl_module = {
        quote! {
            struct #module_type;

            impl matrix_sdk_extensions::traits::Module for #module_type {
                type Environment = #environment_type;
                type Bindings = #private_mod::#world_type;

                fn new_environment() -> Self::Environment {
                    #environment_type::default()
                }
            }

            impl matrix_sdk_extensions::native::ModuleExt<#environment_type, #private_mod::#world_type> for #module_type {
                fn link(
                    linker: &mut wasmtime::component::Linker<#environment_type>,
                    get: impl Fn(&mut #environment_type) -> &mut #environment_type
                        + Send
                        + Sync
                        + Copy
                        + 'static,
                ) -> matrix_sdk_extensions::Result<()> {
                    #private_mod::#world_type::add_to_linker(linker, get)
                }

                fn instantiate_component(
                    store: &mut wasmtime::Store<#environment_type>,
                    component: &wasmtime::component::Component,
                    linker: &wasmtime::component::Linker<#environment_type>,
                ) -> matrix_sdk_extensions::Result<#private_mod::#world_type> {
                    let (bindings, _) = #private_mod::#world_type::instantiate(store, component, linker)?;

                    Ok(bindings)
                }
            }
        }
    };

    let instance_alias = {
        let ident = Ident::new(&format!("{}Instance", world_type_name), call_site);

        quote! {
            type #ident = matrix_sdk_extensions::native::NativeInstance<#module_type>;
        }
    };

    quote! {
        mod #private_mod {
            #generated
        };

        #use_matrix_sdk_extensions;

        #host_trait

        #impl_private_host_trait

        #impl_module

        #instance_alias
    }
    .into()
}

#[cfg(feature = "javascriptcore")]
#[proc_macro]
pub fn javascriptcore_bindgen(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let call_site = Span::call_site();

    let configuration = parse_macro_input!(input as Configuration);

    let resolve = &configuration.resolve;
    let world = &resolve.worlds[configuration.world];
    let world_type_name = to_rust_upper_camel_case(&world.name);
    let world_type = Ident::new(&world_type_name, call_site);
    let module_type = Ident::new(&format!("{}Module", world_type_name), call_site);
    let environment_type = &configuration.environment_type;

    let use_matrix_sdk_extensions = configuration
        .matrix_sdk_extensions_alias
        .map(|alias| quote! { use #alias as matrix_sdk_extensions; });

    let private_mod = Ident::new(&format!("__private_{}", world.name), call_site);

    let mut add_to_linkers = Vec::new();

    let host_trait = {
        let package = world.package.and_then(|package| resolve.packages.get(package));

        let mut imports = Vec::new();

        for (_import_key, import_item) in world.imports.iter() {
            match import_item {
                WorldItem::Interface(interface_id) => {
                    let interface = resolve.interfaces.get(*interface_id).unwrap();

                    let mut trait_functions = Vec::new();
                    let mut bindings = Vec::new();

                    for import in interface.functions.values() {
                        let function_name = Ident::new(&import.name.to_snake_case(), call_site);
                        let ((argument_names, argument_indexed_names), argument_types) = import
                            .params
                            .iter()
                            .enumerate()
                            .map(|(nth, (name, ty))| {
                                let name = Ident::new(&name.to_snake_case(), call_site);
                                let indexed_name = Ident::new(&format!("arg{nth}"), call_site);
                                let ty = Ident::new(&to_rust_type(ty), call_site);

                                ((name, indexed_name), ty)
                            })
                            .unzip::<_, _, (Vec<_>, Vec<_>), Vec<_>>();
                        let results = match &import.results {
                            Results::Named(results) => match results.len() {
                                0 => "()".to_owned(),
                                1 => to_rust_type(&results[0].1),
                                _ => results.iter().fold(String::new(), |mut acc, (name, ty)| {
                                    acc.push_str(&format!(
                                        "{name}: {ty}, ",
                                        ty = to_rust_type(&ty)
                                    ));
                                    acc
                                }),
                            },
                            r => unimplemented!("{:?}", r),
                        }
                        .parse::<TokenStream>()
                        .unwrap();

                        trait_functions.push(quote! {
                            fn #function_name( &mut self #( , #argument_names : #argument_types )* ) -> matrix_sdk_extensions::Result< #results >;
                        });

                        bindings.push(quote! {
                            {
                                #[function_callback]
                                fn js_function<Environment>(
                                    context: &JSContext,
                                    _function: Option<&JSObject>,
                                    this_object: Option<&JSObject>,
                                    arguments: &[JSValue],
                                ) -> core::result::Result<JSValue, JSException>
                                where
                                    Environment: Host,
                                {
                                    #use_matrix_sdk_extensions

                                    use matrix_sdk_extensions::javascriptcore::from_into::TryIntoRust;

                                    let this_object = this_object.expect("`this_object` is `None`");
                                    let env_ptr = this_object.as_number().expect("`this_object` must be a number") as usize;
                                    let env = unsafe { std::sync::Arc::from_raw(env_ptr as *const std::sync::Mutex<Environment>) };
                                    let mut env_lock = env.lock().unwrap();
                                    let env_mut: &mut Environment = &mut env_lock;

                                    let mut arguments = arguments.iter();

                                    #( let #argument_indexed_names: #argument_types = arguments
                                        .next()
                                        .expect(concat!("Argument `", stringify!( #argument_indexed_names ), "` is missing"))
                                        .try_into_rust()?; )*

                                    Host:: #function_name (env_mut #( , #argument_indexed_names )* ).unwrap();

                                    Ok(JSValue::new_undefined(context))
                                }

                                let function_name = stringify!( #function_name );
                                let function = JSValue::new_function(&context, function_name, Some(js_function::<Environment>))
                                    .as_object()
                                    .unwrap();

                                namespace
                                    .set_property(
                                        function_name,
                                        function
                                            .get_property("bind")
                                            .as_object()
                                            .unwrap()
                                            .call_as_function(
                                                Some(&function),
                                                &[JSValue::new_number(
                                                    context,
                                                    // FIXME: converting from u64 to f64 loss precision, and can be dramatic.
                                                    // Use the `conv` crate instead?
                                                    std::sync::Arc::into_raw(environment.clone()) as usize as u64 as f64,
                                                )],
                                            )
                                            .unwrap(),
                                    )
                                    .unwrap();
                            }
                        })
                    }

                    let import = quote! {
                        pub trait Host {
                            #( #trait_functions )*
                        }

                        pub fn add_to_linker<Environment>(
                            context: &matrix_sdk_extensions::javascriptcore::JSContext,
                            imports: &matrix_sdk_extensions::javascriptcore::JSObject,
                            environment: &std::sync::Arc<std::sync::Mutex<Environment>>,
                        ) -> matrix_sdk_extensions::Result<()>
                        where
                            Environment: Host,
                        {
                            use matrix_sdk_extensions::javascriptcore::{function_callback, JSContext, JSException, JSObject, JSValue};

                            imports.set_property("matrix:ui-timeline/std", JSValue::new_from_json(context, "{}").unwrap()).unwrap();
                            let namespace = imports.get_property("matrix:ui-timeline/std").as_object().unwrap();

                            #( #bindings )*

                            Ok(())
                        }
                    };

                    if let Some(interface_name) = &interface.name {
                        let interface_name = Ident::new(&interface_name.to_snake_case(), call_site);

                        imports.push(quote! {
                            pub mod #interface_name {
                                #use_matrix_sdk_extensions

                                #import
                            }
                        });
                        add_to_linkers.push(quote! { #interface_name::add_to_linker });
                    } else {
                        imports.push(import);
                        add_to_linkers.push(quote! { add_to_linker });
                    }
                }
                i => unimplemented!("{:?}", i),
            }
        }

        let mut output = quote! {
            #( #imports )*
        };

        if let Some(package) = package {
            let package_namespace = Ident::new(&package.name.namespace.to_snake_case(), call_site);
            let package_name = Ident::new(&package.name.name.to_snake_case(), call_site);

            output = quote! {
                pub mod #package_namespace {
                    pub mod #package_name {
                        #output
                    }
                }
            };

            add_to_linkers = add_to_linkers
                .iter()
                .map(|add_to_linker| {
                    quote! {
                        #package_namespace :: #package_name :: #add_to_linker
                    }
                })
                .collect();
        }

        output
    };

    let impl_module = {
        quote! {
            struct #module_type;

            impl matrix_sdk_extensions::traits::Module for #module_type {
                type Environment = #environment_type;
                type Bindings = #private_mod :: #world_type;

                fn new_environment() -> Self::Environment {
                    #environment_type ::default()
                }
            }

            impl matrix_sdk_extensions::javascriptcore::ModuleExt< #environment_type > for #module_type {
                fn link(
                    context: &matrix_sdk_extensions::javascriptcore::JSContext,
                    imports: &matrix_sdk_extensions::javascriptcore::JSObject,
                    environment: &std::sync::Arc<std::sync::Mutex< #environment_type >>,
                ) -> matrix_sdk_extensions::Result<()> {
                    #( #add_to_linkers ::< #environment_type >(context, imports, environment)?; )*
                    // matrix::ui_timeline::std::add_to_linker::< #environment_type >(context, imports, environment)?;

                    Ok(())
                }
            }
        }
    };

    let instance_alias = {
        let ident = Ident::new(&format!("{}Instance", world_type_name), call_site);

        quote! {
            type #ident = matrix_sdk_extensions::javascriptcore::JSInstance<#module_type>;
        }
    };

    quote! {
        mod #private_mod {
            #use_matrix_sdk_extensions

            pub struct #world_type;
        }

        #use_matrix_sdk_extensions

        #host_trait

        #impl_module

        #instance_alias
    }
    .into()
}
