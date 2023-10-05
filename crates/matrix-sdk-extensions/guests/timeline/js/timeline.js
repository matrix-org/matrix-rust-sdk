const utf8Decoder = new TextDecoder();

const utf8Encoder = new TextEncoder();

let utf8EncodedLen = 0;
function utf8Encode(s, realloc, memory) {
  if (typeof s !== 'string') throw new TypeError('expected a string');
  if (s.length === 0) {
    utf8EncodedLen = 0;
    return 1;
  }
  let allocLen = 0;
  let ptr = 0;
  let writtenTotal = 0;
  while (s.length > 0) {
    ptr = realloc(ptr, allocLen, 1, allocLen + s.length);
    allocLen += s.length;
    const { read, written } = utf8Encoder.encodeInto(
    s,
    new Uint8Array(memory.buffer, ptr + writtenTotal, allocLen - writtenTotal),
    );
    writtenTotal += written;
    s = s.slice(read);
  }
  if (allocLen > writtenTotal)
  ptr = realloc(ptr, allocLen, 1, writtenTotal);
  utf8EncodedLen = writtenTotal;
  return ptr;
}

export async function instantiate(compileCore, imports, instantiateCore = WebAssembly.instantiate) {
  const module0 = compileCore('timeline.core.wasm');
  const module1 = compileCore('timeline.core2.wasm');
  const module2 = compileCore('timeline.core3.wasm');
  
  const { print } = imports['matrix:ui-timeline/std'];
  let exports0;
  let exports1;
  
  function trampoline0(arg0, arg1) {
    const ptr0 = arg0;
    const len0 = arg1;
    const result0 = utf8Decoder.decode(new Uint8Array(memory0.buffer, ptr0, len0));
    print(result0);
  }
  let memory0;
  let exports2;
  let realloc0;
  const instanceFlags0 = new WebAssembly.Global({ value: "i32", mutable: true }, 3);
  Promise.all([module0, module1, module2]).catch(() => {});
  ({ exports: exports0 } = await instantiateCore(await module1));
  ({ exports: exports1 } = await instantiateCore(await module0, {
    'matrix:ui-timeline/std': {
      print: exports0['0'],
    },
  }));
  memory0 = exports1.memory;
  ({ exports: exports2 } = await instantiateCore(await module2, {
    '': {
      $imports: exports0.$imports,
      '0': trampoline0,
    },
  }));
  realloc0 = exports1.cabi_realloc;
  
  function greet(arg0) {
    const ptr0 = utf8Encode(arg0, realloc0, memory0);
    const len0 = utf8EncodedLen;
    exports1.greet(ptr0, len0);
  }
  
  return { greet };
}
