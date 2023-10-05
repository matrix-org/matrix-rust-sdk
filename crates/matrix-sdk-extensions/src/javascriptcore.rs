pub struct NativeInstance;

#[cfg(test)]
mod tests {
    use javascriptcore::*;

    #[test]
    fn test_basics() {
        let context = JSContext::new();
        let global = context.global_object().unwrap();
        let wasm = global.get_property("WebAssembly").as_object().unwrap();
        let wasm_module = wasm.get_property("Module").as_object().unwrap();
    }
}
