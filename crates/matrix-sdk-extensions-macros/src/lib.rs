use std::{env, path::PathBuf};

use heck::{ToSnakeCase as _, ToUpperCamelCase as _};
use proc_macro2::Span;
use quote::quote;
use syn::{
    braced,
    parse::{Error, Parse, ParseStream, Result},
    punctuated::Punctuated,
    LitStr, Token, TypePath,
};
use wit_parser::{Resolve, Type, WorldId};

struct Configuration {
    resolve: Resolve,
    world: WorldId,
    module_type: TypePath,
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
        let mut module = None;
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

                    Opt::Module(m) => module = Some(m),

                    Opt::Environment(e) => environment_type = Some(e),

                    Opt::MatrixSdkExtensionsAlias(a) => matrix_sdk_extensions_alias = Some(a),
                }
            }
        }

        let world = world.expect("`world` is missing");
        let module_type = module.expect("`module` is missing");
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
            module_type,
            environment_type,
            matrix_sdk_extensions_alias,
        })
    }
}

#[derive(Debug)]
enum Opt {
    World(LitStr),
    Module(TypePath),
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
        } else if look.peek(keyword::module) {
            input.parse::<keyword::module>()?;
            input.parse::<Token![:]>()?;

            Ok(Opt::Module(input.parse()?))
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
    use proc_macro2::{Ident, TokenStream};
    use syn::parse_macro_input;
    use wit_parser::{Results, WorldItem};

    let call_site = Span::call_site();

    let configuration = parse_macro_input!(input as Configuration);
    let opts = wasmtime_wit_bindgen::Opts::default();
    let generated =
        opts.generate(&configuration.resolve, configuration.world).parse::<TokenStream>().unwrap();

    let resolve = &configuration.resolve;
    let world = &resolve.worlds[configuration.world];
    let world_type_name = to_rust_upper_camel_case(&world.name);
    let world_type = Ident::new(&world_type_name, call_site);
    let module_type = &configuration.module_type;
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
            impl matrix_sdk_extensions::traits::Module for #module_type {
                type Environment = #environment_type;
                type Bindings = #world_type;

                fn new_environment() -> Self::Environment {
                    #environment_type::default()
                }
            }

            impl matrix_sdk_extensions::native::ModuleNativeExt<#environment_type, #world_type> for #module_type {
                fn link(
                    linker: &mut wasmtime::component::Linker<#environment_type>,
                    get: impl Fn(&mut #environment_type) -> &mut #environment_type
                        + Send
                        + Sync
                        + Copy
                        + 'static,
                ) -> matrix_sdk_extensions::Result<()> {
                    #world_type::add_to_linker(linker, get)
                }

                fn instantiate_component(
                    store: &mut wasmtime::Store<#environment_type>,
                    component: &wasmtime::component::Component,
                    linker: &wasmtime::component::Linker<#environment_type>,
                ) -> matrix_sdk_extensions::Result<#world_type> {
                    let (bindings, _) = #world_type::instantiate(store, component, linker)?;

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

        pub use #private_mod::#world_type;
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
pub fn javascriptcore_bindgen(_input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    todo!()
}
