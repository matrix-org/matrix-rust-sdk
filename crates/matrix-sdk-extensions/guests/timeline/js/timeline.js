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
    ptr = realloc(ptr, allocLen, 1, allocLen += s.length * 2);
    const { read, written } = utf8Encoder.encodeInto(
    s,
    new Uint8Array(memory.buffer, ptr + writtenTotal, allocLen - writtenTotal),
    );
    writtenTotal += written;
    s = s.slice(read);
  }
  utf8EncodedLen = writtenTotal;
  return ptr;
}

function _instantiate(getCoreModule, imports, instantiateCore = WebAssembly.Instance) {
  const module0 = getCoreModule('timeline.core.wasm');
  const module1 = getCoreModule('timeline.core2.wasm');
  const module2 = getCoreModule('timeline.core3.wasm');
  
  const { print } = imports['matrix:ui-timeline/std'];
  let exports0;
  let exports1;
  
  function trampoline0(arg0, arg1) {
    var ptr0 = arg0;
    var len0 = arg1;
    var result0 = utf8Decoder.decode(new Uint8Array(memory0.buffer, ptr0, len0));
    print(result0);
  }
  let memory0;
  let exports2;
  let realloc0;
  const instanceFlags0 = new WebAssembly.Global({ value: "i32", mutable: true }, 3);
  ({ exports: exports0 } = instantiateCore(module1));
  ({ exports: exports1 } = instantiateCore(module0, {
    'matrix:ui-timeline/std': {
      print: exports0['$0'],
    },
  }));
  memory0 = exports1.memory;
  ({ exports: exports2 } = instantiateCore(module2, {
    '': {
      $imports: exports0.$imports,
      '0': trampoline0,
    },
  }));
  realloc0 = exports1.cabi_realloc;
  
  function greet(arg0) {
    var ptr0 = utf8Encode(arg0, realloc0, memory0);
    var len0 = utf8EncodedLen;
    exports1.greet(ptr0, len0);
  }
  
  return { greet,  };
}


const asmInit = [function asm0(imports) {
  var bufferView;
  var base64ReverseLookup = new Uint8Array(123/*'z'+1*/);
  for (var i = 25; i >= 0; --i) {
    base64ReverseLookup[48+i] = 52+i; // '0-9'
    base64ReverseLookup[65+i] = i; // 'A-Z'
    base64ReverseLookup[97+i] = 26+i; // 'a-z'
  }
  base64ReverseLookup[43] = 62; // '+'
  base64ReverseLookup[47] = 63; // '/'
  /** @noinline Inlining this function would mean expanding the base64 string 4x times in the source code, which Closure seems to be happy to do. */
  function base64DecodeToExistingUint8Array(uint8Array, offset, b64) {
    var b1, b2, i = 0, j = offset, bLength = b64.length, end = offset + (bLength*3>>2) - (b64[bLength-2] == '=') - (b64[bLength-1] == '=');
    for (; i < bLength; i += 4) {
      b1 = base64ReverseLookup[b64.charCodeAt(i+1)];
      b2 = base64ReverseLookup[b64.charCodeAt(i+2)];
      uint8Array[j++] = base64ReverseLookup[b64.charCodeAt(i)] << 2 | b1 >> 4;
      if (j < end) uint8Array[j++] = b1 << 4 | b2 >> 2;
      if (j < end) uint8Array[j++] = b2 << 6 | base64ReverseLookup[b64.charCodeAt(i+3)];
    }
    return uint8Array;
  }
function initActiveSegments(imports) {
  base64DecodeToExistingUint8Array(bufferView, 1048576, "SGVsbG8sICEAABAABwAAAAcAEAABAAAABAAAAAQAAAAEAAAABQAAAAYAAAAHAAAAY2FsbGVkIGBPcHRpb246OnVud3JhcCgpYCBvbiBhIGBOb25lYCB2YWx1ZW1lbW9yeSBhbGxvY2F0aW9uIG9mICBieXRlcyBmYWlsZWQAAABbABAAFQAAAHAAEAANAAAAbGlicmFyeS9zdGQvc3JjL2FsbG9jLnJzkAAQABgAAABiAQAACQAAAGxpYnJhcnkvc3RkL3NyYy9wYW5pY2tpbmcucnO4ABAAHAAAAGkCAAAfAAAAuAAQABwAAABqAgAAHgAAAAgAAAAMAAAABAAAAAkAAAAEAAAACAAAAAQAAAAKAAAABAAAAAgAAAAEAAAACwAAAAwAAAANAAAAEAAAAAQAAAAOAAAADwAAABAAAAAAAAAAAQAAABEAAAASAAAABAAAAAQAAAATAAAAFAAAABUAAABsaWJyYXJ5L2FsbG9jL3NyYy9yYXdfdmVjLnJzY2FwYWNpdHkgb3ZlcmZsb3cAAACAARAAEQAAAGQBEAAcAAAAFgIAAAUAAABhIGZvcm1hdHRpbmcgdHJhaXQgaW1wbGVtZW50YXRpb24gcmV0dXJuZWQgYW4gZXJyb3IAFgAAAAAAAAABAAAAFwAAAGxpYnJhcnkvYWxsb2Mvc3JjL2ZtdC5yc/ABEAAYAAAAYgIAACAAAAAbAAAAAAAAAAEAAAAcAAAAOiAAABgCEAAAAAAAKAIQAAIAAAAwMDAxMDIwMzA0MDUwNjA3MDgwOTEwMTExMjEzMTQxNTE2MTcxODE5MjAyMTIyMjMyNDI1MjYyNzI4MjkzMDMxMzIzMzM0MzUzNjM3MzgzOTQwNDE0MjQzNDQ0NTQ2NDc0ODQ5NTA1MTUyNTM1NDU1NTY1NzU4NTk2MDYxNjI2MzY0NjU2NjY3Njg2OTcwNzE3MjczNzQ3NTc2Nzc3ODc5ODA4MTgyODM4NDg1ODY4Nzg4ODk5MDkxOTI5Mzk0OTU5Njk3OTg5OUVycm9y");
}
function wasm2js_trap() { throw new Error('abort'); }


 var buffer = new ArrayBuffer(1114112);
 var HEAP8 = new Int8Array(buffer);
 var HEAP16 = new Int16Array(buffer);
 var HEAP32 = new Int32Array(buffer);
 var HEAPU8 = new Uint8Array(buffer);
 var HEAPU16 = new Uint16Array(buffer);
 var HEAPU32 = new Uint32Array(buffer);
 var HEAPF32 = new Float32Array(buffer);
 var HEAPF64 = new Float64Array(buffer);
 var Math_imul = Math.imul;
 var Math_fround = Math.fround;
 var Math_abs = Math.abs;
 var Math_clz32 = Math.clz32;
 var Math_min = Math.min;
 var Math_max = Math.max;
 var Math_floor = Math.floor;
 var Math_ceil = Math.ceil;
 var Math_trunc = Math.trunc;
 var Math_sqrt = Math.sqrt;
 var nan = NaN;
 var infinity = Infinity;
 var matrix_ui_timeline_std = imports["matrix:ui-timeline/std"];
 var fimport$0 = matrix_ui_timeline_std.print;
 var global$0 = 1048576;
 var global$1 = 1049841;
 var global$2 = 1049856;
 var i64toi32_i32$HIGH_BITS = 0;
 function $1($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  return $93($1_1, HEAP32[$0 >> 2], HEAP32[$0 + 8 >> 2]) | 0;
 }
 
 function $2($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  var $2_1 = 0, $3_1 = 0, $4 = 0, $5_1 = 0, $6 = 0, $7 = 0, $8_1 = 0, $9 = 0;
  $2_1 = global$0 - 80 | 0;
  global$0 = $2_1;
  if (!HEAPU8[1049358]) {
   HEAP8[1049358] = 1
  }
  HEAP32[$2_1 + 20 >> 2] = $1_1;
  HEAP32[$2_1 + 16 >> 2] = $1_1;
  HEAP32[$2_1 + 12 >> 2] = $0;
  $1_1 = $2_1 + 12 | 0;
  $3_1 = HEAP32[$1_1 + 4 >> 2];
  $0 = $2_1 + 24 | 0;
  HEAP32[$0 >> 2] = HEAP32[$1_1 >> 2];
  HEAP32[$0 + 4 >> 2] = $3_1;
  HEAP32[$0 + 8 >> 2] = HEAP32[$1_1 + 8 >> 2];
  HEAP32[$2_1 + 60 >> 2] = 1;
  HEAP32[$2_1 + 64 >> 2] = 0;
  HEAP32[$2_1 + 52 >> 2] = 2;
  HEAP32[$2_1 + 48 >> 2] = 1048584;
  HEAP32[$2_1 + 76 >> 2] = 1;
  HEAP32[$2_1 + 56 >> 2] = $2_1 + 72;
  HEAP32[$2_1 + 72 >> 2] = $0;
  $6 = $2_1 + 36 | 0;
  $3_1 = global$0 - 32 | 0;
  global$0 = $3_1;
  label$1 : {
   label$2 : {
    label$3 : {
     label$4 : {
      label$5 : {
       $4 = $2_1 + 48 | 0;
       $0 = HEAP32[$4 + 4 >> 2];
       label$6 : {
        if (!$0) {
         break label$6
        }
        $7 = HEAP32[$4 >> 2];
        $5_1 = $0 & 3;
        label$7 : {
         if ($0 >>> 0 < 4) {
          $0 = 0;
          break label$7;
         }
         $1_1 = $7 + 28 | 0;
         $9 = $0 & -4;
         $0 = 0;
         while (1) {
          $0 = HEAP32[$1_1 >> 2] + (HEAP32[$1_1 - 8 >> 2] + (HEAP32[$1_1 - 16 >> 2] + (HEAP32[$1_1 - 24 >> 2] + $0 | 0) | 0) | 0) | 0;
          $1_1 = $1_1 + 32 | 0;
          $8_1 = $8_1 + 4 | 0;
          if (($9 | 0) != ($8_1 | 0)) {
           continue
          }
          break;
         };
        }
        if ($5_1) {
         $1_1 = (($8_1 << 3) + $7 | 0) + 4 | 0;
         while (1) {
          $0 = HEAP32[$1_1 >> 2] + $0 | 0;
          $1_1 = $1_1 + 8 | 0;
          $5_1 = $5_1 - 1 | 0;
          if ($5_1) {
           continue
          }
          break;
         };
        }
        if (HEAP32[$4 + 12 >> 2]) {
         if (!HEAP32[$7 + 4 >> 2] & $0 >>> 0 < 16 | ($0 | 0) < 0) {
          break label$6
         }
         $0 = $0 << 1;
        }
        if ($0) {
         break label$5
        }
       }
       $1_1 = 1;
       $0 = 0;
       break label$4;
      }
      if (($0 | 0) < 0) {
       break label$3
      }
      $1_1 = $3($0, 1);
      if (!$1_1) {
       break label$2
      }
     }
     HEAP32[$3_1 + 20 >> 2] = 0;
     HEAP32[$3_1 + 16 >> 2] = $0;
     HEAP32[$3_1 + 12 >> 2] = $1_1;
     HEAP32[$3_1 + 24 >> 2] = $3_1 + 12;
     if (!$96($3_1 + 24 | 0, 1048908, $4)) {
      break label$1
     }
     $0 = global$0 + -64 | 0;
     global$0 = $0;
     HEAP32[$0 + 12 >> 2] = 51;
     HEAP32[$0 + 8 >> 2] = 1049004;
     HEAP32[$0 + 20 >> 2] = 1049056;
     HEAP32[$0 + 16 >> 2] = $3_1 + 31;
     HEAP32[$0 + 36 >> 2] = 2;
     HEAP32[$0 + 40 >> 2] = 0;
     HEAP32[$0 + 60 >> 2] = 25;
     HEAP32[$0 + 28 >> 2] = 2;
     HEAP32[$0 + 24 >> 2] = 1049132;
     HEAP32[$0 + 52 >> 2] = 26;
     HEAP32[$0 + 32 >> 2] = $0 + 48;
     HEAP32[$0 + 56 >> 2] = $0 + 16;
     HEAP32[$0 + 48 >> 2] = $0 + 8;
     $92($0 + 24 | 0, 1049096);
     wasm2js_trap();
    }
    $87();
    wasm2js_trap();
   }
   $86(1, $0);
   wasm2js_trap();
  }
  $0 = HEAP32[$3_1 + 16 >> 2];
  HEAP32[$6 >> 2] = HEAP32[$3_1 + 12 >> 2];
  HEAP32[$6 + 4 >> 2] = $0;
  HEAP32[$6 + 8 >> 2] = HEAP32[$3_1 + 20 >> 2];
  global$0 = $3_1 + 32 | 0;
  $0 = HEAP32[$2_1 + 40 >> 2];
  $1_1 = HEAP32[$2_1 + 36 >> 2];
  fimport$0($1_1 | 0, HEAP32[$2_1 + 44 >> 2]);
  if ($0) {
   $26($1_1)
  }
  if (HEAP32[$2_1 + 28 >> 2]) {
   $26(HEAP32[$2_1 + 24 >> 2])
  }
  global$0 = $2_1 + 80 | 0;
 }
 
 function $3($0, $1_1) {
  __inlined_func$33 : {
   if ($1_1 >>> 0 >= 9) {
    $0 = $29($1_1, $0);
    break __inlined_func$33;
   }
   $0 = $27($0);
  }
  return $0;
 }
 
 function $5($0, $1_1, $2_1, $3_1) {
  var $4 = 0, $5_1 = 0, $6 = 0, $7 = 0, $8_1 = 0, $9 = 0, wasm2js_i32$0 = 0, wasm2js_i32$1 = 0;
  __inlined_func$35 : {
   label$1 : {
    label$2 : {
     label$3 : {
      label$4 : {
       label$5 : {
        if ($2_1 >>> 0 >= 9) {
         $7 = $29($2_1, $3_1);
         if ($7) {
          break label$5
         }
         $1_1 = 0;
         break __inlined_func$35;
        }
        $1_1 = $45(8, 8);
        $2_1 = $45(20, 8);
        $4 = $45(16, 8);
        $5_1 = 0 - ($45(16, 8) << 2) | 0;
        $1_1 = (-65536 - ($4 + ($1_1 + $2_1 | 0) | 0) & -9) - 3 | 0;
        if (($1_1 >>> 0 > $5_1 >>> 0 ? $5_1 : $1_1) >>> 0 <= $3_1 >>> 0) {
         break label$2
        }
        $2_1 = $45($45(16, 8) - 5 >>> 0 > $3_1 >>> 0 ? 16 : $3_1 + 4 | 0, 8);
        $1_1 = $65($0);
        $5_1 = $50($1_1);
        $4 = $61($1_1, $5_1);
        label$7 : {
         label$8 : {
          label$9 : {
           label$10 : {
            label$11 : {
             label$12 : {
              if (!$55($1_1)) {
               if ($2_1 >>> 0 <= $5_1 >>> 0) {
                break label$9
               }
               if (($4 | 0) == HEAP32[262453]) {
                break label$7
               }
               if (($4 | 0) == HEAP32[262452]) {
                break label$10
               }
               if ($51($4)) {
                break label$3
               }
               $6 = $50($4);
               $8_1 = $6 + $5_1 | 0;
               if ($8_1 >>> 0 < $2_1 >>> 0) {
                break label$3
               }
               $5_1 = $8_1 - $2_1 | 0;
               if ($6 >>> 0 < 256) {
                break label$12
               }
               $23($4);
               break label$11;
              }
              $4 = $50($1_1);
              if ($2_1 >>> 0 < 256) {
               break label$3
              }
              if ($4 - $2_1 >>> 0 < 131073 & $4 >>> 0 >= $2_1 + 4 >>> 0) {
               break label$8
              }
              $5_1 = HEAP32[$1_1 >> 2];
              $45($2_1 + 31 | 0, 65536);
              break label$3;
             }
             $9 = HEAP32[$4 + 12 >> 2];
             $4 = HEAP32[$4 + 8 >> 2];
             if (($9 | 0) != ($4 | 0)) {
              HEAP32[$4 + 12 >> 2] = $9;
              HEAP32[$9 + 8 >> 2] = $4;
              break label$11;
             }
             (wasm2js_i32$0 = 1049792, wasm2js_i32$1 = HEAP32[262448] & __wasm_rotl_i32($6 >>> 3 | 0)), HEAP32[wasm2js_i32$0 >> 2] = wasm2js_i32$1;
            }
            if ($45(16, 8) >>> 0 <= $5_1 >>> 0) {
             $4 = $61($1_1, $2_1);
             $56($1_1, $2_1);
             $56($4, $5_1);
             $22($4, $5_1);
             if ($1_1) {
              break label$1
             }
             break label$3;
            }
            $56($1_1, $8_1);
            if ($1_1) {
             break label$1
            }
            break label$3;
           }
           $5_1 = $5_1 + HEAP32[262450] | 0;
           if ($5_1 >>> 0 < $2_1 >>> 0) {
            break label$3
           }
           $4 = $5_1 - $2_1 | 0;
           label$16 : {
            if ($45(16, 8) >>> 0 > $4 >>> 0) {
             $56($1_1, $5_1);
             $4 = 0;
             $5_1 = 0;
             break label$16;
            }
            $5_1 = $61($1_1, $2_1);
            $6 = $61($5_1, $4);
            $56($1_1, $2_1);
            $59($5_1, $4);
            HEAP32[$6 + 4 >> 2] = HEAP32[$6 + 4 >> 2] & -2;
           }
           HEAP32[262452] = $5_1;
           HEAP32[262450] = $4;
           if ($1_1) {
            break label$1
           }
           break label$3;
          }
          $4 = $5_1 - $2_1 | 0;
          if ($45(16, 8) >>> 0 > $4 >>> 0) {
           break label$8
          }
          $5_1 = $61($1_1, $2_1);
          $56($1_1, $2_1);
          $56($5_1, $4);
          $22($5_1, $4);
         }
         if ($1_1) {
          break label$1
         }
         break label$3;
        }
        $5_1 = $5_1 + HEAP32[262451] | 0;
        if ($5_1 >>> 0 > $2_1 >>> 0) {
         break label$4
        }
        break label$3;
       }
       $108($7, $0, $1_1 >>> 0 < $3_1 >>> 0 ? $1_1 : $3_1);
       $26($0);
       break label$2;
      }
      $4 = $61($1_1, $2_1);
      $56($1_1, $2_1);
      $2_1 = $5_1 - $2_1 | 0;
      HEAP32[$4 + 4 >> 2] = $2_1 | 1;
      HEAP32[262451] = $2_1;
      HEAP32[262453] = $4;
      if ($1_1) {
       break label$1
      }
     }
     $2_1 = $27($3_1);
     if (!$2_1) {
      break label$2
     }
     $1_1 = $50($1_1) + ($55($1_1) ? -8 : -4) | 0;
     $1_1 = $108($2_1, $0, $1_1 >>> 0 < $3_1 >>> 0 ? $1_1 : $3_1);
     $26($0);
     break __inlined_func$35;
    }
    $1_1 = $7;
    break __inlined_func$35;
   }
   $55($1_1);
   $1_1 = $63($1_1);
  }
  return $1_1;
 }
 
 function $8($0, $1_1, $2_1, $3_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  $2_1 = $2_1 | 0;
  $3_1 = $3_1 | 0;
  label$1 : {
   label$2 : {
    if (!$1_1) {
     if (!$3_1) {
      break label$1
     }
     $2_1 = $3($3_1, $2_1);
     break label$2;
    }
    $2_1 = $5($0, $1_1, $2_1, $3_1);
   }
   if ($2_1) {
    break label$1
   }
   wasm2js_trap();
  }
  return $2_1 | 0;
 }
 
 function $10($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  HEAP32[$0 + 8 >> 2] = 11661156;
  HEAP32[$0 + 12 >> 2] = -38005119;
  HEAP32[$0 >> 2] = -853640255;
  HEAP32[$0 + 4 >> 2] = -1046296420;
 }
 
 function $11($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  HEAP32[$0 + 8 >> 2] = 1091761936;
  HEAP32[$0 + 12 >> 2] = 2141169500;
  HEAP32[$0 >> 2] = -1841756907;
  HEAP32[$0 + 4 >> 2] = 655034299;
 }
 
 function $12($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  HEAP32[$0 + 8 >> 2] = -1168109704;
  HEAP32[$0 + 12 >> 2] = 327792390;
  HEAP32[$0 >> 2] = -1786456962;
  HEAP32[$0 + 4 >> 2] = 2127965620;
 }
 
 function $13($0, $1_1, $2_1) {
  var $3_1 = 0, $4 = 0;
  $3_1 = global$0 - 32 | 0;
  global$0 = $3_1;
  label$1 : {
   label$2 : {
    $4 = $1_1;
    $1_1 = $1_1 + $2_1 | 0;
    if ($4 >>> 0 > $1_1 >>> 0) {
     break label$2
    }
    $2_1 = HEAP32[$0 + 4 >> 2];
    $4 = $2_1 << 1;
    $1_1 = $1_1 >>> 0 < $4 >>> 0 ? $4 : $1_1;
    $4 = $1_1 >>> 0 <= 8 ? 8 : $1_1;
    $1_1 = ($4 ^ -1) >>> 31 | 0;
    label$3 : {
     if (!$2_1) {
      HEAP32[$3_1 + 24 >> 2] = 0;
      break label$3;
     }
     HEAP32[$3_1 + 28 >> 2] = $2_1;
     HEAP32[$3_1 + 24 >> 2] = 1;
     HEAP32[$3_1 + 20 >> 2] = HEAP32[$0 >> 2];
    }
    $21($3_1 + 8 | 0, $1_1, $4, $3_1 + 20 | 0);
    $1_1 = HEAP32[$3_1 + 12 >> 2];
    if (!HEAP32[$3_1 + 8 >> 2]) {
     HEAP32[$0 + 4 >> 2] = $4;
     HEAP32[$0 >> 2] = $1_1;
     break label$1;
    }
    if (($1_1 | 0) == -2147483647) {
     break label$1
    }
    if (!$1_1) {
     break label$2
    }
    $86($1_1, HEAP32[$3_1 + 16 >> 2]);
    wasm2js_trap();
   }
   $87();
   wasm2js_trap();
  }
  global$0 = $3_1 + 32 | 0;
 }
 
 function $14($0) {
  $0 = $0 | 0;
 }
 
 function $15($0) {
  $0 = $0 | 0;
  if (HEAP32[$0 + 4 >> 2]) {
   $26(HEAP32[$0 >> 2])
  }
 }
 
 function $16($0) {
  $0 = $0 | 0;
  var $1_1 = 0;
  $1_1 = HEAP32[$0 + 4 >> 2];
  if (!(!$1_1 | !HEAP32[$0 + 8 >> 2])) {
   $26($1_1)
  }
 }
 
 function $17($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  var $2_1 = 0, $3_1 = 0, $4 = 0, $5_1 = 0, $6 = 0;
  $3_1 = global$0 - 16 | 0;
  global$0 = $3_1;
  $0 = HEAP32[$0 >> 2];
  label$1 : {
   label$2 : {
    label$3 : {
     if ($1_1 >>> 0 >= 128) {
      HEAP32[$3_1 + 12 >> 2] = 0;
      if ($1_1 >>> 0 < 2048) {
       break label$3
      }
      if ($1_1 >>> 0 < 65536) {
       HEAP8[$3_1 + 14 | 0] = $1_1 & 63 | 128;
       HEAP8[$3_1 + 12 | 0] = $1_1 >>> 12 | 224;
       HEAP8[$3_1 + 13 | 0] = $1_1 >>> 6 & 63 | 128;
       $1_1 = 3;
       break label$2;
      }
      HEAP8[$3_1 + 15 | 0] = $1_1 & 63 | 128;
      HEAP8[$3_1 + 14 | 0] = $1_1 >>> 6 & 63 | 128;
      HEAP8[$3_1 + 13 | 0] = $1_1 >>> 12 & 63 | 128;
      HEAP8[$3_1 + 12 | 0] = $1_1 >>> 18 & 7 | 240;
      $1_1 = 4;
      break label$2;
     }
     $2_1 = HEAP32[$0 + 8 >> 2];
     if (($2_1 | 0) == HEAP32[$0 + 4 >> 2]) {
      $4 = global$0 - 32 | 0;
      global$0 = $4;
      label$10 : {
       label$21 : {
        $2_1 = $2_1 + 1 | 0;
        if (!$2_1) {
         break label$21
        }
        $6 = HEAP32[$0 + 4 >> 2];
        $5_1 = $6 << 1;
        $2_1 = $2_1 >>> 0 < $5_1 >>> 0 ? $5_1 : $2_1;
        $5_1 = $2_1 >>> 0 <= 8 ? 8 : $2_1;
        $2_1 = ($5_1 ^ -1) >>> 31 | 0;
        label$32 : {
         if (!$6) {
          HEAP32[$4 + 24 >> 2] = 0;
          break label$32;
         }
         HEAP32[$4 + 28 >> 2] = $6;
         HEAP32[$4 + 24 >> 2] = 1;
         HEAP32[$4 + 20 >> 2] = HEAP32[$0 >> 2];
        }
        $21($4 + 8 | 0, $2_1, $5_1, $4 + 20 | 0);
        $2_1 = HEAP32[$4 + 12 >> 2];
        if (!HEAP32[$4 + 8 >> 2]) {
         HEAP32[$0 + 4 >> 2] = $5_1;
         HEAP32[$0 >> 2] = $2_1;
         break label$10;
        }
        if (($2_1 | 0) == -2147483647) {
         break label$10
        }
        if (!$2_1) {
         break label$21
        }
        $86($2_1, HEAP32[$4 + 16 >> 2]);
        wasm2js_trap();
       }
       $87();
       wasm2js_trap();
      }
      global$0 = $4 + 32 | 0;
      $2_1 = HEAP32[$0 + 8 >> 2];
     }
     HEAP32[$0 + 8 >> 2] = $2_1 + 1;
     HEAP8[HEAP32[$0 >> 2] + $2_1 | 0] = $1_1;
     break label$1;
    }
    HEAP8[$3_1 + 13 | 0] = $1_1 & 63 | 128;
    HEAP8[$3_1 + 12 | 0] = $1_1 >>> 6 | 192;
    $1_1 = 2;
   }
   $2_1 = HEAP32[$0 + 8 >> 2];
   if ($1_1 >>> 0 > HEAP32[$0 + 4 >> 2] - $2_1 >>> 0) {
    $13($0, $2_1, $1_1);
    $2_1 = HEAP32[$0 + 8 >> 2];
   }
   $108(HEAP32[$0 >> 2] + $2_1 | 0, $3_1 + 12 | 0, $1_1);
   HEAP32[$0 + 8 >> 2] = $1_1 + $2_1;
  }
  global$0 = $3_1 + 16 | 0;
  return 0;
 }
 
 function $19($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  return byn$mgfn_shared$19($0, $1_1, 1048600) | 0;
 }
 
 function $20($0, $1_1, $2_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  $2_1 = $2_1 | 0;
  var $3_1 = 0;
  $0 = HEAP32[$0 >> 2];
  $3_1 = HEAP32[$0 + 8 >> 2];
  if (HEAP32[$0 + 4 >> 2] - $3_1 >>> 0 < $2_1 >>> 0) {
   $13($0, $3_1, $2_1);
   $3_1 = HEAP32[$0 + 8 >> 2];
  }
  $108(HEAP32[$0 >> 2] + $3_1 | 0, $1_1, $2_1);
  HEAP32[$0 + 8 >> 2] = $2_1 + $3_1;
  return 0;
 }
 
 function $21($0, $1_1, $2_1, $3_1) {
  var $4 = 0;
  label$1 : {
   label$2 : {
    if ($1_1) {
     if (($2_1 | 0) < 0) {
      break label$2
     }
     label$4 : {
      if (HEAP32[$3_1 + 4 >> 2]) {
       label$5 : {
        $4 = HEAP32[$3_1 + 8 >> 2];
        if (!$4) {
         break label$5
        }
        $3_1 = $5(HEAP32[$3_1 >> 2], $4, $1_1, $2_1);
        break label$4;
       }
      }
      $3_1 = $1_1;
      if (!$2_1) {
       break label$4
      }
      $3_1 = $3($2_1, $1_1);
     }
     if ($3_1) {
      HEAP32[$0 + 4 >> 2] = $3_1;
      HEAP32[$0 + 8 >> 2] = $2_1;
      HEAP32[$0 >> 2] = 0;
      return;
     }
     HEAP32[$0 + 4 >> 2] = $1_1;
     HEAP32[$0 + 8 >> 2] = $2_1;
     break label$1;
    }
    HEAP32[$0 + 4 >> 2] = 0;
    HEAP32[$0 + 8 >> 2] = $2_1;
    break label$1;
   }
   HEAP32[$0 + 4 >> 2] = 0;
  }
  HEAP32[$0 >> 2] = 1;
 }
 
 function $22($0, $1_1) {
  var $2_1 = 0, $3_1 = 0, $4 = 0, $5_1 = 0, wasm2js_i32$0 = 0, wasm2js_i32$1 = 0;
  $2_1 = $61($0, $1_1);
  label$1 : {
   label$2 : {
    label$3 : {
     label$4 : {
      label$5 : {
       label$6 : {
        label$7 : {
         if ($52($0)) {
          break label$7
         }
         $3_1 = HEAP32[$0 >> 2];
         if ($55($0)) {
          break label$6
         }
         $1_1 = $1_1 + $3_1 | 0;
         $0 = $62($0, $3_1);
         if (($0 | 0) == HEAP32[262452]) {
          if ((HEAP32[$2_1 + 4 >> 2] & 3) != 3) {
           break label$7
          }
          HEAP32[262450] = $1_1;
          $60($0, $1_1, $2_1);
          return;
         }
         if ($3_1 >>> 0 >= 256) {
          $23($0);
          break label$7;
         }
         $4 = HEAP32[$0 + 8 >> 2];
         $5_1 = HEAP32[$0 + 12 >> 2];
         if (($4 | 0) != ($5_1 | 0)) {
          HEAP32[$4 + 12 >> 2] = $5_1;
          HEAP32[$5_1 + 8 >> 2] = $4;
          break label$7;
         }
         (wasm2js_i32$0 = 1049792, wasm2js_i32$1 = HEAP32[262448] & __wasm_rotl_i32($3_1 >>> 3 | 0)), HEAP32[wasm2js_i32$0 >> 2] = wasm2js_i32$1;
        }
        if ($51($2_1)) {
         break label$4
        }
        if (($2_1 | 0) == HEAP32[262453]) {
         break label$2
        }
        if (($2_1 | 0) != HEAP32[262452]) {
         break label$5
        }
        HEAP32[262452] = $0;
        $1_1 = HEAP32[262450] + $1_1 | 0;
        HEAP32[262450] = $1_1;
        $59($0, $1_1);
        return;
       }
       break label$1;
      }
      $3_1 = $50($2_1);
      $1_1 = $3_1 + $1_1 | 0;
      label$11 : {
       if ($3_1 >>> 0 >= 256) {
        $23($2_1);
        break label$11;
       }
       $4 = HEAP32[$2_1 + 12 >> 2];
       $2_1 = HEAP32[$2_1 + 8 >> 2];
       if (($4 | 0) != ($2_1 | 0)) {
        HEAP32[$2_1 + 12 >> 2] = $4;
        HEAP32[$4 + 8 >> 2] = $2_1;
        break label$11;
       }
       (wasm2js_i32$0 = 1049792, wasm2js_i32$1 = HEAP32[262448] & __wasm_rotl_i32($3_1 >>> 3 | 0)), HEAP32[wasm2js_i32$0 >> 2] = wasm2js_i32$1;
      }
      $59($0, $1_1);
      if (HEAP32[262452] != ($0 | 0)) {
       break label$3
      }
      HEAP32[262450] = $1_1;
      return;
     }
     $60($0, $1_1, $2_1);
    }
    if ($1_1 >>> 0 >= 256) {
     $24($0, $1_1);
     return;
    }
    $2_1 = ($1_1 & -8) + 1049528 | 0;
    $3_1 = HEAP32[262448];
    $1_1 = 1 << ($1_1 >>> 3);
    label$15 : {
     if (!($3_1 & $1_1)) {
      HEAP32[262448] = $1_1 | $3_1;
      $1_1 = $2_1;
      break label$15;
     }
     $1_1 = HEAP32[$2_1 + 8 >> 2];
    }
    HEAP32[$2_1 + 8 >> 2] = $0;
    HEAP32[$1_1 + 12 >> 2] = $0;
    HEAP32[$0 + 12 >> 2] = $2_1;
    HEAP32[$0 + 8 >> 2] = $1_1;
    return;
   }
   HEAP32[262453] = $0;
   $1_1 = HEAP32[262451] + $1_1 | 0;
   HEAP32[262451] = $1_1;
   HEAP32[$0 + 4 >> 2] = $1_1 | 1;
   if (HEAP32[262452] != ($0 | 0)) {
    break label$1
   }
   HEAP32[262450] = 0;
   HEAP32[262452] = 0;
  }
 }
 
 function $23($0) {
  var $1_1 = 0, $2_1 = 0, $3_1 = 0, $4 = 0, $5_1 = 0, $6 = 0, wasm2js_i32$0 = 0, wasm2js_i32$1 = 0;
  $4 = HEAP32[$0 + 24 >> 2];
  label$1 : {
   label$2 : {
    if (($0 | 0) == HEAP32[$0 + 12 >> 2]) {
     $1_1 = $0 + 20 | 0;
     $3_1 = HEAP32[$1_1 >> 2];
     $2_1 = HEAP32[($3_1 ? 20 : 16) + $0 >> 2];
     if ($2_1) {
      break label$2
     }
     $1_1 = 0;
     break label$1;
    }
    $2_1 = HEAP32[$0 + 8 >> 2];
    $1_1 = HEAP32[$0 + 12 >> 2];
    HEAP32[$2_1 + 12 >> 2] = $1_1;
    HEAP32[$1_1 + 8 >> 2] = $2_1;
    break label$1;
   }
   $3_1 = $3_1 ? $1_1 : $0 + 16 | 0;
   while (1) {
    $6 = $3_1;
    $1_1 = $2_1 + 20 | 0;
    $5_1 = HEAP32[$1_1 >> 2];
    $3_1 = $1_1;
    $1_1 = $2_1;
    $3_1 = $5_1 ? $3_1 : $1_1 + 16 | 0;
    $2_1 = HEAP32[($5_1 ? 20 : 16) + $1_1 >> 2];
    if ($2_1) {
     continue
    }
    break;
   };
   HEAP32[$6 >> 2] = 0;
  }
  label$5 : {
   if (!$4) {
    break label$5
   }
   label$6 : {
    $2_1 = (HEAP32[$0 + 28 >> 2] << 2) + 1049384 | 0;
    if (HEAP32[$2_1 >> 2] != ($0 | 0)) {
     HEAP32[(HEAP32[$4 + 16 >> 2] == ($0 | 0) ? 16 : 20) + $4 >> 2] = $1_1;
     if ($1_1) {
      break label$6
     }
     break label$5;
    }
    HEAP32[$2_1 >> 2] = $1_1;
    if ($1_1) {
     break label$6
    }
    (wasm2js_i32$0 = 1049796, wasm2js_i32$1 = HEAP32[262449] & __wasm_rotl_i32(HEAP32[$0 + 28 >> 2])), HEAP32[wasm2js_i32$0 >> 2] = wasm2js_i32$1;
    return;
   }
   HEAP32[$1_1 + 24 >> 2] = $4;
   $2_1 = HEAP32[$0 + 16 >> 2];
   if ($2_1) {
    HEAP32[$1_1 + 16 >> 2] = $2_1;
    HEAP32[$2_1 + 24 >> 2] = $1_1;
   }
   $0 = HEAP32[$0 + 20 >> 2];
   if (!$0) {
    break label$5
   }
   HEAP32[$1_1 + 20 >> 2] = $0;
   HEAP32[$0 + 24 >> 2] = $1_1;
  }
 }
 
 function $24($0, $1_1) {
  var $2_1 = 0, $3_1 = 0, $4 = 0, $5_1 = 0;
  HEAP32[$0 + 16 >> 2] = 0;
  HEAP32[$0 + 20 >> 2] = 0;
  $2_1 = 0;
  label$1 : {
   if ($1_1 >>> 0 < 256) {
    break label$1
   }
   $2_1 = 31;
   if ($1_1 >>> 0 > 16777215) {
    break label$1
   }
   $4 = Math_clz32($1_1 >>> 8 | 0);
   $2_1 = (($1_1 >>> 6 - $4 & 1) - ($4 << 1) | 0) + 62 | 0;
  }
  HEAP32[$0 + 28 >> 2] = $2_1;
  $4 = ($2_1 << 2) + 1049384 | 0;
  $3_1 = $0;
  $0 = HEAP32[262449];
  $5_1 = 1 << $2_1;
  label$2 : {
   if (!($0 & $5_1)) {
    HEAP32[262449] = $0 | $5_1;
    HEAP32[$4 >> 2] = $3_1;
    break label$2;
   }
   $4 = HEAP32[$4 >> 2];
   $0 = $48($2_1);
   label$4 : {
    label$5 : {
     if (($50($4) | 0) == ($1_1 | 0)) {
      $0 = $4;
      break label$5;
     }
     $2_1 = $1_1 << $0;
     while (1) {
      $5_1 = (($2_1 >>> 29 & 4) + $4 | 0) + 16 | 0;
      $0 = HEAP32[$5_1 >> 2];
      if (!$0) {
       break label$4
      }
      $2_1 = $2_1 << 1;
      $4 = $0;
      if (($50($0) | 0) != ($1_1 | 0)) {
       continue
      }
      break;
     };
    }
    $1_1 = HEAP32[$0 + 8 >> 2];
    HEAP32[$1_1 + 12 >> 2] = $3_1;
    HEAP32[$0 + 8 >> 2] = $3_1;
    HEAP32[$3_1 + 12 >> 2] = $0;
    HEAP32[$3_1 + 8 >> 2] = $1_1;
    HEAP32[$3_1 + 24 >> 2] = 0;
    return;
   }
   HEAP32[$5_1 >> 2] = $3_1;
  }
  HEAP32[$3_1 + 24 >> 2] = $4;
  HEAP32[$3_1 + 8 >> 2] = $3_1;
  HEAP32[$3_1 + 12 >> 2] = $3_1;
 }
 
 function $25() {
  var $0 = 0, $1_1 = 0;
  $0 = HEAP32[262380];
  if ($0) {
   while (1) {
    $1_1 = $1_1 + 1 | 0;
    $0 = HEAP32[$0 + 8 >> 2];
    if ($0) {
     continue
    }
    break;
   }
  }
  HEAP32[262458] = $1_1 >>> 0 <= 4095 ? 4095 : $1_1;
  return 0;
 }
 
 function $26($0) {
  var $1_1 = 0, $2_1 = 0, $3_1 = 0, $4 = 0, $5_1 = 0, wasm2js_i32$0 = 0, wasm2js_i32$1 = 0;
  $0 = $65($0);
  $1_1 = $50($0);
  $2_1 = $61($0, $1_1);
  label$1 : {
   label$2 : {
    label$3 : {
     label$4 : {
      label$5 : {
       label$6 : {
        label$7 : {
         label$8 : {
          label$9 : {
           if ($52($0)) {
            break label$9
           }
           $3_1 = HEAP32[$0 >> 2];
           if ($55($0)) {
            break label$8
           }
           $1_1 = $1_1 + $3_1 | 0;
           $0 = $62($0, $3_1);
           if (($0 | 0) == HEAP32[262452]) {
            if ((HEAP32[$2_1 + 4 >> 2] & 3) != 3) {
             break label$9
            }
            HEAP32[262450] = $1_1;
            $60($0, $1_1, $2_1);
            break label$1;
           }
           if ($3_1 >>> 0 >= 256) {
            $23($0);
            break label$9;
           }
           $4 = HEAP32[$0 + 12 >> 2];
           $5_1 = HEAP32[$0 + 8 >> 2];
           if (($4 | 0) != ($5_1 | 0)) {
            HEAP32[$5_1 + 12 >> 2] = $4;
            HEAP32[$4 + 8 >> 2] = $5_1;
            break label$9;
           }
           (wasm2js_i32$0 = 1049792, wasm2js_i32$1 = HEAP32[262448] & __wasm_rotl_i32($3_1 >>> 3 | 0)), HEAP32[wasm2js_i32$0 >> 2] = wasm2js_i32$1;
          }
          if ($51($2_1)) {
           break label$6
          }
          if (($2_1 | 0) == HEAP32[262453]) {
           break label$4
          }
          if (($2_1 | 0) != HEAP32[262452]) {
           break label$7
          }
          HEAP32[262452] = $0;
          $1_1 = HEAP32[262450] + $1_1 | 0;
          HEAP32[262450] = $1_1;
          $59($0, $1_1);
          return;
         }
         break label$1;
        }
        $3_1 = $50($2_1);
        $1_1 = $3_1 + $1_1 | 0;
        label$13 : {
         if ($3_1 >>> 0 >= 256) {
          $23($2_1);
          break label$13;
         }
         $4 = HEAP32[$2_1 + 12 >> 2];
         $2_1 = HEAP32[$2_1 + 8 >> 2];
         if (($4 | 0) != ($2_1 | 0)) {
          HEAP32[$2_1 + 12 >> 2] = $4;
          HEAP32[$4 + 8 >> 2] = $2_1;
          break label$13;
         }
         (wasm2js_i32$0 = 1049792, wasm2js_i32$1 = HEAP32[262448] & __wasm_rotl_i32($3_1 >>> 3 | 0)), HEAP32[wasm2js_i32$0 >> 2] = wasm2js_i32$1;
        }
        $59($0, $1_1);
        if (HEAP32[262452] != ($0 | 0)) {
         break label$5
        }
        HEAP32[262450] = $1_1;
        return;
       }
       $60($0, $1_1, $2_1);
      }
      if ($1_1 >>> 0 < 256) {
       break label$3
      }
      $24($0, $1_1);
      $0 = HEAP32[262458] - 1 | 0;
      HEAP32[262458] = $0;
      if ($0) {
       break label$1
      }
      $25();
      return;
     }
     HEAP32[262453] = $0;
     $1_1 = HEAP32[262451] + $1_1 | 0;
     HEAP32[262451] = $1_1;
     HEAP32[$0 + 4 >> 2] = $1_1 | 1;
     if (HEAP32[262452] == ($0 | 0)) {
      HEAP32[262450] = 0;
      HEAP32[262452] = 0;
     }
     if ($1_1 >>> 0 <= HEAPU32[262456]) {
      break label$1
     }
     $0 = $45(8, 8);
     $1_1 = $45(20, 8);
     $2_1 = $45(16, 8);
     $3_1 = 0 - ($45(16, 8) << 2) | 0;
     $0 = (-65536 - ($2_1 + ($0 + $1_1 | 0) | 0) & -9) - 3 | 0;
     if (!($0 >>> 0 > $3_1 >>> 0 ? $3_1 : $0) | !HEAP32[262453]) {
      break label$1
     }
     $0 = $45(8, 8);
     $1_1 = $45(20, 8);
     $2_1 = $45(16, 8);
     if (HEAPU32[262451] <= $2_1 + ($1_1 + ($0 - 8 | 0) | 0) >>> 0) {
      break label$2
     }
     $1_1 = HEAP32[262453];
     $0 = 1049512;
     label$17 : {
      while (1) {
       if ($1_1 >>> 0 >= HEAPU32[$0 >> 2]) {
        if ($73($0) >>> 0 > $1_1 >>> 0) {
         break label$17
        }
       }
       $0 = HEAP32[$0 + 8 >> 2];
       if ($0) {
        continue
       }
       break;
      };
      $0 = 0;
     }
     $70($0);
     break label$2;
    }
    $2_1 = ($1_1 & -8) + 1049528 | 0;
    $3_1 = HEAP32[262448];
    $1_1 = 1 << ($1_1 >>> 3);
    label$21 : {
     if (!($3_1 & $1_1)) {
      HEAP32[262448] = $1_1 | $3_1;
      $1_1 = $2_1;
      break label$21;
     }
     $1_1 = HEAP32[$2_1 + 8 >> 2];
    }
    HEAP32[$2_1 + 8 >> 2] = $0;
    HEAP32[$1_1 + 12 >> 2] = $0;
    HEAP32[$0 + 12 >> 2] = $2_1;
    HEAP32[$0 + 8 >> 2] = $1_1;
    return;
   }
   if ($25() | HEAPU32[262451] <= HEAPU32[262456]) {
    break label$1
   }
   HEAP32[262456] = -1;
  }
 }
 
 function $27($0) {
  var $1_1 = 0, $2_1 = 0, $3_1 = 0, $4 = 0, $5_1 = 0, $6 = 0, $7 = 0, $8_1 = 0, $9 = 0, $10_1 = 0, $11_1 = 0, $12_1 = 0, $13_1 = 0, $14_1 = 0, $15_1 = 0, wasm2js_i32$0 = 0, wasm2js_i32$1 = 0;
  $11_1 = global$0 - 16 | 0;
  global$0 = $11_1;
  label$1 : {
   label$2 : {
    label$3 : {
     label$4 : {
      if ($0 >>> 0 >= 245) {
       $6 = $45(8, 8);
       $4 = $45(20, 8);
       $1_1 = $45(16, 8);
       $2_1 = 0 - ($45(16, 8) << 2) | 0;
       $1_1 = (-65536 - ($1_1 + ($4 + $6 | 0) | 0) & -9) - 3 | 0;
       if (($1_1 >>> 0 > $2_1 >>> 0 ? $2_1 : $1_1) >>> 0 <= $0 >>> 0) {
        break label$1
       }
       $5_1 = $45($0 + 4 | 0, 8);
       if (!HEAP32[262449]) {
        break label$2
       }
       $3_1 = 0 - $5_1 | 0;
       $6 = 0;
       label$6 : {
        if ($5_1 >>> 0 < 256) {
         break label$6
        }
        $6 = 31;
        if ($5_1 >>> 0 > 16777215) {
         break label$6
        }
        $0 = Math_clz32($5_1 >>> 8 | 0);
        $6 = (($5_1 >>> 6 - $0 & 1) - ($0 << 1) | 0) + 62 | 0;
       }
       $1_1 = HEAP32[($6 << 2) + 1049384 >> 2];
       if (!$1_1) {
        $0 = 0;
        $4 = 0;
        break label$4;
       }
       $7 = $5_1 << $48($6);
       $0 = 0;
       $4 = 0;
       while (1) {
        label$9 : {
         $2_1 = $50($1_1);
         if ($2_1 >>> 0 < $5_1 >>> 0) {
          break label$9
         }
         $2_1 = $2_1 - $5_1 | 0;
         if ($2_1 >>> 0 >= $3_1 >>> 0) {
          break label$9
         }
         $4 = $1_1;
         $3_1 = $2_1;
         if ($2_1) {
          break label$9
         }
         $3_1 = 0;
         $0 = $1_1;
         $1_1 = 0;
         break label$3;
        }
        $2_1 = HEAP32[$1_1 + 20 >> 2];
        $1_1 = HEAP32[(($7 >>> 29 & 4) + $1_1 | 0) + 16 >> 2];
        $0 = $2_1 ? (($2_1 | 0) != ($1_1 | 0) ? $2_1 : $0) : $0;
        $7 = $7 << 1;
        if ($1_1) {
         continue
        }
        break;
       };
       break label$4;
      }
      $5_1 = $45($45(16, 8) - 5 >>> 0 > $0 >>> 0 ? 16 : $0 + 4 | 0, 8);
      $1_1 = HEAP32[262448];
      $0 = $5_1 >>> 3 | 0;
      $2_1 = $1_1 >>> $0 | 0;
      if ($2_1 & 3) {
       $3_1 = $0 + (($2_1 ^ -1) & 1) | 0;
       $0 = $3_1 << 3;
       $4 = HEAP32[$0 + 1049536 >> 2];
       $2_1 = HEAP32[$4 + 8 >> 2];
       $0 = $0 + 1049528 | 0;
       label$11 : {
        if (($2_1 | 0) != ($0 | 0)) {
         HEAP32[$2_1 + 12 >> 2] = $0;
         HEAP32[$0 + 8 >> 2] = $2_1;
         break label$11;
        }
        (wasm2js_i32$0 = 1049792, wasm2js_i32$1 = __wasm_rotl_i32($3_1) & $1_1), HEAP32[wasm2js_i32$0 >> 2] = wasm2js_i32$1;
       }
       $57($4, $3_1 << 3);
       $3_1 = $63($4);
       break label$1;
      }
      if (HEAPU32[262450] >= $5_1 >>> 0) {
       break label$2
      }
      label$13 : {
       label$14 : {
        label$15 : {
         label$16 : {
          label$17 : {
           label$18 : {
            if (!$2_1) {
             $0 = HEAP32[262449];
             if (!$0) {
              break label$2
             }
             $1_1 = HEAP32[(__wasm_ctz_i32($47($0)) << 2) + 1049384 >> 2];
             $3_1 = $50($1_1) - $5_1 | 0;
             $0 = $66($1_1);
             if ($0) {
              while (1) {
               $2_1 = $50($0) - $5_1 | 0;
               $4 = $2_1 >>> 0 < $3_1 >>> 0;
               $3_1 = $4 ? $2_1 : $3_1;
               $1_1 = $4 ? $0 : $1_1;
               $0 = $66($0);
               if ($0) {
                continue
               }
               break;
              }
             }
             $4 = $61($1_1, $5_1);
             $23($1_1);
             if ($45(16, 8) >>> 0 > $3_1 >>> 0) {
              break label$17
             }
             $58($1_1, $5_1);
             $59($4, $3_1);
             $0 = HEAP32[262450];
             if ($0) {
              break label$18
             }
             break label$14;
            }
            $0 = $0 & 31;
            $2_1 = __wasm_ctz_i32($47($46(1 << $0) & $2_1 << $0));
            $0 = $2_1 << 3;
            $3_1 = HEAP32[$0 + 1049536 >> 2];
            $1_1 = HEAP32[$3_1 + 8 >> 2];
            $0 = $0 + 1049528 | 0;
            label$22 : {
             if (($1_1 | 0) != ($0 | 0)) {
              HEAP32[$1_1 + 12 >> 2] = $0;
              HEAP32[$0 + 8 >> 2] = $1_1;
              break label$22;
             }
             (wasm2js_i32$0 = 1049792, wasm2js_i32$1 = HEAP32[262448] & __wasm_rotl_i32($2_1)), HEAP32[wasm2js_i32$0 >> 2] = wasm2js_i32$1;
            }
            $58($3_1, $5_1);
            $4 = $61($3_1, $5_1);
            $2_1 = ($2_1 << 3) - $5_1 | 0;
            $59($4, $2_1);
            $0 = HEAP32[262450];
            if ($0) {
             break label$16
            }
            break label$15;
           }
           $7 = ($0 & -8) + 1049528 | 0;
           $6 = HEAP32[262452];
           $2_1 = HEAP32[262448];
           $0 = 1 << ($0 >>> 3);
           label$24 : {
            if (!($2_1 & $0)) {
             HEAP32[262448] = $0 | $2_1;
             $0 = $7;
             break label$24;
            }
            $0 = HEAP32[$7 + 8 >> 2];
           }
           HEAP32[$7 + 8 >> 2] = $6;
           HEAP32[$0 + 12 >> 2] = $6;
           HEAP32[$6 + 12 >> 2] = $7;
           HEAP32[$6 + 8 >> 2] = $0;
           break label$14;
          }
          $57($1_1, $3_1 + $5_1 | 0);
          break label$13;
         }
         $7 = ($0 & -8) + 1049528 | 0;
         $6 = HEAP32[262452];
         $1_1 = HEAP32[262448];
         $0 = 1 << ($0 >>> 3);
         label$26 : {
          if (!($1_1 & $0)) {
           HEAP32[262448] = $0 | $1_1;
           $0 = $7;
           break label$26;
          }
          $0 = HEAP32[$7 + 8 >> 2];
         }
         HEAP32[$7 + 8 >> 2] = $6;
         HEAP32[$0 + 12 >> 2] = $6;
         HEAP32[$6 + 12 >> 2] = $7;
         HEAP32[$6 + 8 >> 2] = $0;
        }
        HEAP32[262452] = $4;
        HEAP32[262450] = $2_1;
        $3_1 = $63($3_1);
        break label$1;
       }
       HEAP32[262452] = $4;
       HEAP32[262450] = $3_1;
      }
      $3_1 = $63($1_1);
      if (!$3_1) {
       break label$2
      }
      break label$1;
     }
     if (!($0 | $4)) {
      $4 = 0;
      $0 = $46(1 << $6) & HEAP32[262449];
      if (!$0) {
       break label$2
      }
      $0 = HEAP32[(__wasm_ctz_i32($47($0)) << 2) + 1049384 >> 2];
     }
     $1_1 = 1;
    }
    while (1) {
     if (!$1_1) {
      $1_1 = $50($0);
      $6 = $1_1 - $5_1 | 0;
      $2_1 = $6 >>> 0 < $3_1 >>> 0;
      $1_1 = $1_1 >>> 0 < $5_1 >>> 0;
      $4 = $1_1 ? $4 : $2_1 ? $0 : $4;
      $3_1 = $1_1 ? $3_1 : $2_1 ? $6 : $3_1;
      $0 = $66($0);
      $1_1 = 1;
      continue;
     }
     if ($0) {
      $1_1 = 0;
      continue;
     } else {
      if (!$4) {
       break label$2
      }
      $0 = HEAP32[262450];
      if ($0 >>> 0 >= $5_1 >>> 0 & $0 - $5_1 >>> 0 <= $3_1 >>> 0) {
       break label$2
      }
      $6 = $61($4, $5_1);
      $23($4);
      label$33 : {
       if ($45(16, 8) >>> 0 <= $3_1 >>> 0) {
        $58($4, $5_1);
        $59($6, $3_1);
        if ($3_1 >>> 0 >= 256) {
         $24($6, $3_1);
         break label$33;
        }
        $2_1 = ($3_1 & -8) + 1049528 | 0;
        $1_1 = HEAP32[262448];
        $0 = 1 << ($3_1 >>> 3);
        label$36 : {
         if (!($1_1 & $0)) {
          HEAP32[262448] = $0 | $1_1;
          $0 = $2_1;
          break label$36;
         }
         $0 = HEAP32[$2_1 + 8 >> 2];
        }
        HEAP32[$2_1 + 8 >> 2] = $6;
        HEAP32[$0 + 12 >> 2] = $6;
        HEAP32[$6 + 12 >> 2] = $2_1;
        HEAP32[$6 + 8 >> 2] = $0;
        break label$33;
       }
       $57($4, $3_1 + $5_1 | 0);
      }
      $3_1 = $63($4);
      if ($3_1) {
       break label$1
      }
     }
     break;
    };
   }
   label$38 : {
    label$39 : {
     $0 = HEAP32[262450];
     if ($0 >>> 0 < $5_1 >>> 0) {
      $0 = HEAP32[262451];
      if ($0 >>> 0 <= $5_1 >>> 0) {
       $0 = $45((($45(8, 8) + $5_1 | 0) + $45(20, 8) | 0) + $45(16, 8) | 0, 65536);
       $2_1 = __wasm_memory_grow($0 >>> 16 | 0);
       $1_1 = $11_1 + 4 | 0;
       HEAP32[$1_1 + 8 >> 2] = 0;
       $7 = $0 & -65536;
       $0 = ($2_1 | 0) == -1;
       HEAP32[$1_1 + 4 >> 2] = $0 ? 0 : $7;
       HEAP32[$1_1 >> 2] = $0 ? 0 : $2_1 << 16;
       $8_1 = HEAP32[$11_1 + 4 >> 2];
       if (!$8_1) {
        $3_1 = 0;
        break label$1;
       }
       $14_1 = HEAP32[$11_1 + 12 >> 2];
       $10_1 = HEAP32[$11_1 + 8 >> 2];
       $1_1 = $10_1 + HEAP32[262454] | 0;
       HEAP32[262454] = $1_1;
       $0 = HEAP32[262455];
       HEAP32[262455] = $0 >>> 0 > $1_1 >>> 0 ? $0 : $1_1;
       label$43 : {
        if (HEAP32[262453]) {
         $0 = 1049512;
         while (1) {
          if (($73($0) | 0) == ($8_1 | 0)) {
           break label$43
          }
          $0 = HEAP32[$0 + 8 >> 2];
          if ($0) {
           continue
          }
          break;
         };
         break label$39;
        }
        $0 = HEAP32[262457];
        if (!($0 >>> 0 <= $8_1 >>> 0 ? $0 : 0)) {
         HEAP32[262457] = $8_1
        }
        HEAP32[262458] = 4095;
        HEAP32[262381] = $14_1;
        HEAP32[262379] = $10_1;
        HEAP32[262378] = $8_1;
        HEAP32[262385] = 1049528;
        HEAP32[262387] = 1049536;
        HEAP32[262384] = 1049528;
        HEAP32[262389] = 1049544;
        HEAP32[262386] = 1049536;
        HEAP32[262391] = 1049552;
        HEAP32[262388] = 1049544;
        HEAP32[262393] = 1049560;
        HEAP32[262390] = 1049552;
        HEAP32[262395] = 1049568;
        HEAP32[262392] = 1049560;
        HEAP32[262397] = 1049576;
        HEAP32[262394] = 1049568;
        HEAP32[262399] = 1049584;
        HEAP32[262396] = 1049576;
        HEAP32[262401] = 1049592;
        HEAP32[262398] = 1049584;
        HEAP32[262400] = 1049592;
        HEAP32[262403] = 1049600;
        HEAP32[262402] = 1049600;
        HEAP32[262405] = 1049608;
        HEAP32[262404] = 1049608;
        HEAP32[262407] = 1049616;
        HEAP32[262406] = 1049616;
        HEAP32[262409] = 1049624;
        HEAP32[262408] = 1049624;
        HEAP32[262411] = 1049632;
        HEAP32[262410] = 1049632;
        HEAP32[262413] = 1049640;
        HEAP32[262412] = 1049640;
        HEAP32[262415] = 1049648;
        HEAP32[262414] = 1049648;
        HEAP32[262417] = 1049656;
        HEAP32[262419] = 1049664;
        HEAP32[262416] = 1049656;
        HEAP32[262421] = 1049672;
        HEAP32[262418] = 1049664;
        HEAP32[262423] = 1049680;
        HEAP32[262420] = 1049672;
        HEAP32[262425] = 1049688;
        HEAP32[262422] = 1049680;
        HEAP32[262427] = 1049696;
        HEAP32[262424] = 1049688;
        HEAP32[262429] = 1049704;
        HEAP32[262426] = 1049696;
        HEAP32[262431] = 1049712;
        HEAP32[262428] = 1049704;
        HEAP32[262433] = 1049720;
        HEAP32[262430] = 1049712;
        HEAP32[262435] = 1049728;
        HEAP32[262432] = 1049720;
        HEAP32[262437] = 1049736;
        HEAP32[262434] = 1049728;
        HEAP32[262439] = 1049744;
        HEAP32[262436] = 1049736;
        HEAP32[262441] = 1049752;
        HEAP32[262438] = 1049744;
        HEAP32[262443] = 1049760;
        HEAP32[262440] = 1049752;
        HEAP32[262445] = 1049768;
        HEAP32[262442] = 1049760;
        HEAP32[262447] = 1049776;
        HEAP32[262444] = 1049768;
        HEAP32[262446] = 1049776;
        $4 = $45(8, 8);
        $2_1 = $45(20, 8);
        $1_1 = $45(16, 8);
        $0 = $63($8_1);
        $0 = $45($0, 8) - $0 | 0;
        $3_1 = $61($8_1, $0);
        HEAP32[262453] = $3_1;
        $4 = $10_1 + 8 - ($0 + ($1_1 + ($2_1 + $4 | 0) | 0)) | 0;
        HEAP32[262451] = $4;
        HEAP32[$3_1 + 4 >> 2] = $4 | 1;
        $2_1 = $45(8, 8);
        $1_1 = $45(20, 8);
        $0 = $45(16, 8);
        (wasm2js_i32$0 = $61($3_1, $4), wasm2js_i32$1 = $0 + ($1_1 + ($2_1 - 8 | 0) | 0) | 0), HEAP32[wasm2js_i32$0 + 4 >> 2] = wasm2js_i32$1;
        HEAP32[262456] = 2097152;
        break label$38;
       }
       if ($70($0)) {
        break label$39
       }
       if (($71($0) | 0) != ($14_1 | 0)) {
        break label$39
       }
       $2_1 = HEAP32[262453];
       $1_1 = HEAP32[$0 >> 2];
       if ($2_1 >>> 0 >= $1_1 >>> 0) {
        $1_1 = HEAP32[$0 + 4 >> 2] + $1_1 >>> 0 > $2_1 >>> 0
       } else {
        $1_1 = 0
       }
       if (!$1_1) {
        break label$39
       }
       HEAP32[$0 + 4 >> 2] = $10_1 + HEAP32[$0 + 4 >> 2];
       $1_1 = $10_1 + HEAP32[262451] | 0;
       $0 = HEAP32[262453];
       $2_1 = $0;
       $0 = $63($0);
       $0 = $45($0, 8) - $0 | 0;
       $3_1 = $61($2_1, $0);
       $4 = $1_1 - $0 | 0;
       HEAP32[262451] = $4;
       HEAP32[262453] = $3_1;
       HEAP32[$3_1 + 4 >> 2] = $4 | 1;
       $2_1 = $45(8, 8);
       $1_1 = $45(20, 8);
       $0 = $45(16, 8);
       (wasm2js_i32$0 = $61($3_1, $4), wasm2js_i32$1 = (($2_1 - 8 | 0) + $1_1 | 0) + $0 | 0), HEAP32[wasm2js_i32$0 + 4 >> 2] = wasm2js_i32$1;
       HEAP32[262456] = 2097152;
       break label$38;
      }
      $1_1 = $0 - $5_1 | 0;
      HEAP32[262451] = $1_1;
      $2_1 = HEAP32[262453];
      $0 = $61($2_1, $5_1);
      HEAP32[262453] = $0;
      HEAP32[$0 + 4 >> 2] = $1_1 | 1;
      $58($2_1, $5_1);
      $3_1 = $63($2_1);
      break label$1;
     }
     $2_1 = HEAP32[262452];
     $1_1 = $0 - $5_1 | 0;
     if ($45(16, 8) >>> 0 <= $1_1 >>> 0) {
      $0 = $61($2_1, $5_1);
      HEAP32[262450] = $1_1;
      HEAP32[262452] = $0;
      $59($0, $1_1);
      $58($2_1, $5_1);
      $3_1 = $63($2_1);
      break label$1;
     }
     HEAP32[262452] = 0;
     $0 = HEAP32[262450];
     HEAP32[262450] = 0;
     $57($2_1, $0);
     $3_1 = $63($2_1);
     break label$1;
    }
    $0 = HEAP32[262457];
    HEAP32[262457] = $0 >>> 0 < $8_1 >>> 0 ? $0 : $8_1;
    $1_1 = $8_1 + $10_1 | 0;
    $0 = 1049512;
    label$48 : {
     while (1) {
      if (($1_1 | 0) != HEAP32[$0 >> 2]) {
       $0 = HEAP32[$0 + 8 >> 2];
       if ($0) {
        continue
       }
       break label$48;
      }
      break;
     };
     if ($70($0)) {
      break label$48
     }
     if (($71($0) | 0) != ($14_1 | 0)) {
      break label$48
     }
     $3_1 = HEAP32[$0 >> 2];
     HEAP32[$0 >> 2] = $8_1;
     HEAP32[$0 + 4 >> 2] = $10_1 + HEAP32[$0 + 4 >> 2];
     $4 = $63($8_1);
     $2_1 = $45($4, 8);
     $1_1 = $63($3_1);
     $0 = $45($1_1, 8);
     $6 = $8_1 + ($2_1 - $4 | 0) | 0;
     $7 = $61($6, $5_1);
     $58($6, $5_1);
     $0 = $3_1 + ($0 - $1_1 | 0) | 0;
     $5_1 = $0 - ($5_1 + $6 | 0) | 0;
     label$51 : {
      if (HEAP32[262453] != ($0 | 0)) {
       if (HEAP32[262452] == ($0 | 0)) {
        break label$51
       }
       if ((HEAP32[$0 + 4 >> 2] & 3) == 1) {
        $4 = $50($0);
        label$54 : {
         if ($4 >>> 0 >= 256) {
          $23($0);
          break label$54;
         }
         $2_1 = HEAP32[$0 + 12 >> 2];
         $1_1 = HEAP32[$0 + 8 >> 2];
         if (($2_1 | 0) != ($1_1 | 0)) {
          HEAP32[$1_1 + 12 >> 2] = $2_1;
          HEAP32[$2_1 + 8 >> 2] = $1_1;
          break label$54;
         }
         (wasm2js_i32$0 = 1049792, wasm2js_i32$1 = HEAP32[262448] & __wasm_rotl_i32($4 >>> 3 | 0)), HEAP32[wasm2js_i32$0 >> 2] = wasm2js_i32$1;
        }
        $5_1 = $5_1 + $4 | 0;
        $0 = $61($0, $4);
       }
       $60($7, $5_1, $0);
       if ($5_1 >>> 0 >= 256) {
        $24($7, $5_1);
        $3_1 = $63($6);
        break label$1;
       }
       $2_1 = ($5_1 & -8) + 1049528 | 0;
       $1_1 = HEAP32[262448];
       $0 = 1 << ($5_1 >>> 3);
       label$58 : {
        if (!($1_1 & $0)) {
         HEAP32[262448] = $0 | $1_1;
         $0 = $2_1;
         break label$58;
        }
        $0 = HEAP32[$2_1 + 8 >> 2];
       }
       HEAP32[$2_1 + 8 >> 2] = $7;
       HEAP32[$0 + 12 >> 2] = $7;
       HEAP32[$7 + 12 >> 2] = $2_1;
       HEAP32[$7 + 8 >> 2] = $0;
       $3_1 = $63($6);
       break label$1;
      }
      HEAP32[262453] = $7;
      $0 = HEAP32[262451] + $5_1 | 0;
      HEAP32[262451] = $0;
      HEAP32[$7 + 4 >> 2] = $0 | 1;
      $3_1 = $63($6);
      break label$1;
     }
     HEAP32[262452] = $7;
     $0 = HEAP32[262450] + $5_1 | 0;
     HEAP32[262450] = $0;
     $59($7, $0);
     $3_1 = $63($6);
     break label$1;
    }
    $9 = HEAP32[262453];
    $0 = 1049512;
    label$60 : {
     while (1) {
      if ($9 >>> 0 >= HEAPU32[$0 >> 2]) {
       if ($73($0) >>> 0 > $9 >>> 0) {
        break label$60
       }
      }
      $0 = HEAP32[$0 + 8 >> 2];
      if ($0) {
       continue
      }
      break;
     };
     $0 = 0;
    }
    $6 = $73($0);
    $15_1 = $45(20, 8);
    $1_1 = ($6 - $15_1 | 0) - 23 | 0;
    $0 = $63($1_1);
    $0 = ($45($0, 8) - $0 | 0) + $1_1 | 0;
    $12_1 = $0 >>> 0 < $45(16, 8) + $9 >>> 0 ? $9 : $0;
    $13_1 = $63($12_1);
    $0 = $61($12_1, $15_1);
    $3_1 = $45(8, 8);
    $4 = $45(20, 8);
    $2_1 = $45(16, 8);
    $1_1 = $63($8_1);
    $1_1 = $45($1_1, 8) - $1_1 | 0;
    $7 = $61($8_1, $1_1);
    HEAP32[262453] = $7;
    $3_1 = $10_1 + 8 - ($1_1 + ($2_1 + ($3_1 + $4 | 0) | 0)) | 0;
    HEAP32[262451] = $3_1;
    HEAP32[$7 + 4 >> 2] = $3_1 | 1;
    $4 = $45(8, 8);
    $2_1 = $45(20, 8);
    $1_1 = $45(16, 8);
    (wasm2js_i32$0 = $61($7, $3_1), wasm2js_i32$1 = $1_1 + ($2_1 + ($4 - 8 | 0) | 0) | 0), HEAP32[wasm2js_i32$0 + 4 >> 2] = wasm2js_i32$1;
    HEAP32[262456] = 2097152;
    $58($12_1, $15_1);
    $4 = HEAP32[262378];
    $2_1 = HEAP32[262379];
    $1_1 = HEAP32[262381];
    HEAP32[$13_1 + 8 >> 2] = HEAP32[262380];
    HEAP32[$13_1 + 12 >> 2] = $1_1;
    HEAP32[$13_1 >> 2] = $4;
    HEAP32[$13_1 + 4 >> 2] = $2_1;
    HEAP32[262381] = $14_1;
    HEAP32[262379] = $10_1;
    HEAP32[262378] = $8_1;
    HEAP32[262380] = $13_1;
    while (1) {
     $1_1 = $61($0, 4);
     HEAP32[$0 + 4 >> 2] = 7;
     $0 = $1_1;
     if ($6 >>> 0 > $0 + 4 >>> 0) {
      continue
     }
     break;
    };
    if (($9 | 0) == ($12_1 | 0)) {
     break label$38
    }
    $0 = $12_1 - $9 | 0;
    $60($9, $0, $61($9, $0));
    if ($0 >>> 0 >= 256) {
     $24($9, $0);
     break label$38;
    }
    $2_1 = ($0 & -8) + 1049528 | 0;
    $1_1 = HEAP32[262448];
    $0 = 1 << ($0 >>> 3);
    label$65 : {
     if (!($1_1 & $0)) {
      HEAP32[262448] = $0 | $1_1;
      $0 = $2_1;
      break label$65;
     }
     $0 = HEAP32[$2_1 + 8 >> 2];
    }
    HEAP32[$2_1 + 8 >> 2] = $9;
    HEAP32[$0 + 12 >> 2] = $9;
    HEAP32[$9 + 12 >> 2] = $2_1;
    HEAP32[$9 + 8 >> 2] = $0;
   }
   $3_1 = 0;
   $0 = HEAP32[262451];
   if ($0 >>> 0 <= $5_1 >>> 0) {
    break label$1
   }
   $1_1 = $0 - $5_1 | 0;
   HEAP32[262451] = $1_1;
   $2_1 = HEAP32[262453];
   $0 = $61($2_1, $5_1);
   HEAP32[262453] = $0;
   HEAP32[$0 + 4 >> 2] = $1_1 | 1;
   $58($2_1, $5_1);
   $3_1 = $63($2_1);
  }
  global$0 = $11_1 + 16 | 0;
  return $3_1;
 }
 
 function $29($0, $1_1) {
  var $2_1 = 0, $3_1 = 0, $4 = 0, $5_1 = 0, $6 = 0;
  if ($45(16, 8) >>> 0 > $0 >>> 0) {
   $0 = $45(16, 8)
  }
  $3_1 = $45(8, 8);
  $2_1 = $45(20, 8);
  $4 = $45(16, 8);
  $5_1 = 0 - ($45(16, 8) << 2) | 0;
  $3_1 = (-65536 - (($2_1 + $3_1 | 0) + $4 | 0) & -9) - 3 | 0;
  label$2 : {
   if (($3_1 >>> 0 > $5_1 >>> 0 ? $5_1 : $3_1) - $0 >>> 0 <= $1_1 >>> 0) {
    break label$2
   }
   $3_1 = $45($45(16, 8) - 5 >>> 0 > $1_1 >>> 0 ? 16 : $1_1 + 4 | 0, 8);
   $2_1 = $27((($3_1 + $0 | 0) + $45(16, 8) | 0) - 4 | 0);
   if (!$2_1) {
    break label$2
   }
   $1_1 = $65($2_1);
   $4 = $0 - 1 | 0;
   label$3 : {
    if (!($4 & $2_1)) {
     $0 = $1_1;
     break label$3;
    }
    $5_1 = $0;
    $0 = $65($2_1 + $4 & 0 - $0);
    $0 = ($0 - $1_1 >>> 0 <= $45(16, 8) >>> 0 ? $5_1 : 0) + $0 | 0;
    $2_1 = $0 - $1_1 | 0;
    $4 = $50($1_1) - $2_1 | 0;
    if (!$55($1_1)) {
     $56($0, $4);
     $56($1_1, $2_1);
     $22($1_1, $2_1);
     break label$3;
    }
    $1_1 = HEAP32[$1_1 >> 2];
    HEAP32[$0 + 4 >> 2] = $4;
    HEAP32[$0 >> 2] = $1_1 + $2_1;
   }
   label$6 : {
    if ($55($0)) {
     break label$6
    }
    $1_1 = $50($0);
    if ($1_1 >>> 0 <= $45(16, 8) + $3_1 >>> 0) {
     break label$6
    }
    $2_1 = $61($0, $3_1);
    $56($0, $3_1);
    $1_1 = $1_1 - $3_1 | 0;
    $56($2_1, $1_1);
    $22($2_1, $1_1);
   }
   $6 = $63($0);
   $55($0);
  }
  return $6;
 }
 
 function $32($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  $0 = global$0 - 48 | 0;
  global$0 = $0;
  if (HEAPU8[1049356]) {
   HEAP32[$0 + 24 >> 2] = 1;
   HEAP32[$0 + 28 >> 2] = 0;
   HEAP32[$0 + 16 >> 2] = 2;
   HEAP32[$0 + 12 >> 2] = 1048704;
   HEAP32[$0 + 40 >> 2] = 2;
   HEAP32[$0 + 44 >> 2] = $1_1;
   HEAP32[$0 + 20 >> 2] = $0 + 36;
   HEAP32[$0 + 36 >> 2] = $0 + 44;
   $92($0 + 12 | 0, 1048744);
   wasm2js_trap();
  }
  global$0 = $0 + 48 | 0;
 }
 
 function $37($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  var $2_1 = 0, $3_1 = 0, $4 = 0, $5_1 = 0, $6 = 0;
  $2_1 = global$0 - 48 | 0;
  global$0 = $2_1;
  $3_1 = $1_1 + 4 | 0;
  if (!HEAP32[$1_1 + 4 >> 2]) {
   $4 = HEAP32[$1_1 >> 2];
   $5_1 = $2_1 + 40 | 0;
   HEAP32[$5_1 >> 2] = 0;
   HEAP32[$2_1 + 32 >> 2] = 1;
   HEAP32[$2_1 + 36 >> 2] = 0;
   HEAP32[$2_1 + 44 >> 2] = $2_1 + 32;
   $96($2_1 + 44 | 0, 1048600, $4);
   $6 = HEAP32[$5_1 >> 2];
   HEAP32[$2_1 + 24 >> 2] = $6;
   $5_1 = HEAP32[$2_1 + 36 >> 2];
   $4 = HEAP32[$2_1 + 32 >> 2];
   HEAP32[$2_1 + 16 >> 2] = $4;
   HEAP32[$2_1 + 20 >> 2] = $5_1;
   HEAP32[$3_1 + 8 >> 2] = $6;
   HEAP32[$3_1 >> 2] = $4;
   HEAP32[$3_1 + 4 >> 2] = $5_1;
  }
  $4 = $2_1 + 8 | 0;
  HEAP32[$4 >> 2] = HEAP32[$3_1 + 8 >> 2];
  HEAP32[$1_1 + 12 >> 2] = 0;
  $5_1 = HEAP32[$3_1 >> 2];
  $3_1 = HEAP32[$3_1 + 4 >> 2];
  HEAP32[$1_1 + 4 >> 2] = 1;
  HEAP32[$1_1 + 8 >> 2] = 0;
  HEAP32[$2_1 >> 2] = $5_1;
  HEAP32[$2_1 + 4 >> 2] = $3_1;
  $1_1 = $3(12, 4);
  if (!$1_1) {
   $86(4, 12);
   wasm2js_trap();
  }
  $3_1 = HEAP32[$2_1 + 4 >> 2];
  HEAP32[$1_1 >> 2] = HEAP32[$2_1 >> 2];
  HEAP32[$1_1 + 4 >> 2] = $3_1;
  HEAP32[$1_1 + 8 >> 2] = HEAP32[$4 >> 2];
  HEAP32[$0 + 4 >> 2] = 1048820;
  HEAP32[$0 >> 2] = $1_1;
  global$0 = $2_1 + 48 | 0;
 }
 
 function $38($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  var $2_1 = 0, $3_1 = 0, $4 = 0, $5_1 = 0;
  $2_1 = global$0 - 32 | 0;
  global$0 = $2_1;
  $4 = $1_1 + 4 | 0;
  if (!HEAP32[$1_1 + 4 >> 2]) {
   $1_1 = HEAP32[$1_1 >> 2];
   $3_1 = $2_1 + 24 | 0;
   HEAP32[$3_1 >> 2] = 0;
   HEAP32[$2_1 + 16 >> 2] = 1;
   HEAP32[$2_1 + 20 >> 2] = 0;
   HEAP32[$2_1 + 28 >> 2] = $2_1 + 16;
   $96($2_1 + 28 | 0, 1048600, $1_1);
   $5_1 = HEAP32[$3_1 >> 2];
   HEAP32[$2_1 + 8 >> 2] = $5_1;
   $1_1 = HEAP32[$2_1 + 20 >> 2];
   $3_1 = HEAP32[$2_1 + 16 >> 2];
   HEAP32[$2_1 >> 2] = $3_1;
   HEAP32[$2_1 + 4 >> 2] = $1_1;
   HEAP32[$4 + 8 >> 2] = $5_1;
   HEAP32[$4 >> 2] = $3_1;
   HEAP32[$4 + 4 >> 2] = $1_1;
  }
  HEAP32[$0 + 4 >> 2] = 1048820;
  HEAP32[$0 >> 2] = $4;
  global$0 = $2_1 + 32 | 0;
 }
 
 function $39($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  var $2_1 = 0, $3_1 = 0;
  $2_1 = HEAP32[$1_1 + 4 >> 2];
  $3_1 = HEAP32[$1_1 >> 2];
  $1_1 = $3(8, 4);
  if (!$1_1) {
   $86(4, 8);
   wasm2js_trap();
  }
  HEAP32[$1_1 + 4 >> 2] = $2_1;
  HEAP32[$1_1 >> 2] = $3_1;
  HEAP32[$0 + 4 >> 2] = 1048836;
  HEAP32[$0 >> 2] = $1_1;
 }
 
 function $40($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  HEAP32[$0 + 4 >> 2] = 1048836;
  HEAP32[$0 >> 2] = $1_1;
 }
 
 function $41($0, $1_1, $2_1, $3_1, $4, $5_1) {
  var $6 = 0, $7 = 0;
  $6 = global$0 - 32 | 0;
  global$0 = $6;
  $7 = HEAP32[262345];
  HEAP32[262345] = $7 + 1;
  label$1 : {
   label$2 : {
    if (HEAPU8[1049840] | ($7 | 0) < 0) {
     break label$2
    }
    HEAP8[1049840] = 1;
    HEAP32[262459] = HEAP32[262459] + 1;
    HEAP8[$6 + 29 | 0] = $5_1;
    HEAP8[$6 + 28 | 0] = $4;
    HEAP32[$6 + 24 >> 2] = $3_1;
    HEAP32[$6 + 20 >> 2] = $2_1;
    HEAP32[$6 + 16 >> 2] = 1048892;
    HEAP32[$6 + 12 >> 2] = 1048624;
    $2_1 = HEAP32[262341];
    if (($2_1 | 0) < 0) {
     break label$2
    }
    HEAP32[262341] = $2_1 + 1;
    if (HEAP32[262343]) {
     FUNCTION_TABLE[HEAP32[$1_1 + 16 >> 2]]($6, $0);
     $0 = HEAP32[$6 + 4 >> 2];
     HEAP32[$6 + 12 >> 2] = HEAP32[$6 >> 2];
     HEAP32[$6 + 16 >> 2] = $0;
     FUNCTION_TABLE[HEAP32[HEAP32[262344] + 20 >> 2]](HEAP32[262343], $6 + 12 | 0);
     $2_1 = HEAP32[262341] - 1 | 0;
    }
    HEAP32[262341] = $2_1;
    HEAP8[1049840] = 0;
    if ($4) {
     break label$1
    }
   }
   wasm2js_trap();
  }
  wasm2js_trap();
 }
 
 function $45($0, $1_1) {
  return ($0 + $1_1 | 0) - 1 & 0 - $1_1;
 }
 
 function $46($0) {
  $0 = $0 << 1;
  return 0 - $0 | $0;
 }
 
 function $47($0) {
  return 0 - $0 & $0;
 }
 
 function $48($0) {
  return ($0 | 0) != 31 ? 25 - ($0 >>> 1 | 0) | 0 : 0;
 }
 
 function $50($0) {
  return HEAP32[$0 + 4 >> 2] & -8;
 }
 
 function $51($0) {
  return (HEAPU8[$0 + 4 | 0] & 2) >>> 1 | 0;
 }
 
 function $52($0) {
  return HEAP32[$0 + 4 >> 2] & 1;
 }
 
 function $55($0) {
  return !(HEAPU8[$0 + 4 | 0] & 3);
 }
 
 function $56($0, $1_1) {
  HEAP32[$0 + 4 >> 2] = HEAP32[$0 + 4 >> 2] & 1 | $1_1 | 2;
  $0 = $0 + $1_1 | 0;
  HEAP32[$0 + 4 >> 2] = HEAP32[$0 + 4 >> 2] | 1;
 }
 
 function $57($0, $1_1) {
  HEAP32[$0 + 4 >> 2] = $1_1 | 3;
  $0 = $0 + $1_1 | 0;
  HEAP32[$0 + 4 >> 2] = HEAP32[$0 + 4 >> 2] | 1;
 }
 
 function $58($0, $1_1) {
  HEAP32[$0 + 4 >> 2] = $1_1 | 3;
 }
 
 function $59($0, $1_1) {
  HEAP32[$0 + 4 >> 2] = $1_1 | 1;
  HEAP32[$0 + $1_1 >> 2] = $1_1;
 }
 
 function $60($0, $1_1, $2_1) {
  HEAP32[$2_1 + 4 >> 2] = HEAP32[$2_1 + 4 >> 2] & -2;
  HEAP32[$0 + 4 >> 2] = $1_1 | 1;
  HEAP32[$0 + $1_1 >> 2] = $1_1;
 }
 
 function $61($0, $1_1) {
  return $0 + $1_1 | 0;
 }
 
 function $62($0, $1_1) {
  return $0 - $1_1 | 0;
 }
 
 function $63($0) {
  return $0 + 8 | 0;
 }
 
 function $65($0) {
  return $0 - 8 | 0;
 }
 
 function $66($0) {
  var $1_1 = 0;
  $1_1 = HEAP32[$0 + 16 >> 2];
  if (!$1_1) {
   $1_1 = HEAP32[$0 + 20 >> 2]
  }
  return $1_1;
 }
 
 function $70($0) {
  return HEAP32[$0 + 12 >> 2] & 1;
 }
 
 function $71($0) {
  return HEAP32[$0 + 12 >> 2] >>> 1 | 0;
 }
 
 function $73($0) {
  return HEAP32[$0 >> 2] + HEAP32[$0 + 4 >> 2] | 0;
 }
 
 function $80($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  var $2_1 = 0, $3_1 = 0, $4 = 0, $5_1 = 0, $6 = 0;
  $0 = HEAP32[$0 >> 2];
  $3_1 = global$0 - 16 | 0;
  global$0 = $3_1;
  label$1 : {
   label$2 : {
    label$3 : {
     if ($1_1 >>> 0 >= 128) {
      HEAP32[$3_1 + 12 >> 2] = 0;
      if ($1_1 >>> 0 < 2048) {
       break label$3
      }
      if ($1_1 >>> 0 < 65536) {
       HEAP8[$3_1 + 14 | 0] = $1_1 & 63 | 128;
       HEAP8[$3_1 + 12 | 0] = $1_1 >>> 12 | 224;
       HEAP8[$3_1 + 13 | 0] = $1_1 >>> 6 & 63 | 128;
       $1_1 = 3;
       break label$2;
      }
      HEAP8[$3_1 + 15 | 0] = $1_1 & 63 | 128;
      HEAP8[$3_1 + 14 | 0] = $1_1 >>> 6 & 63 | 128;
      HEAP8[$3_1 + 13 | 0] = $1_1 >>> 12 & 63 | 128;
      HEAP8[$3_1 + 12 | 0] = $1_1 >>> 18 & 7 | 240;
      $1_1 = 4;
      break label$2;
     }
     $2_1 = HEAP32[$0 + 8 >> 2];
     if (($2_1 | 0) == HEAP32[$0 + 4 >> 2]) {
      $4 = global$0 - 32 | 0;
      global$0 = $4;
      label$10 : {
       label$21 : {
        $2_1 = $2_1 + 1 | 0;
        if (!$2_1) {
         break label$21
        }
        $6 = HEAP32[$0 + 4 >> 2];
        $5_1 = $6 << 1;
        $2_1 = $2_1 >>> 0 < $5_1 >>> 0 ? $5_1 : $2_1;
        $5_1 = $2_1 >>> 0 <= 8 ? 8 : $2_1;
        $2_1 = ($5_1 ^ -1) >>> 31 | 0;
        label$32 : {
         if (!$6) {
          HEAP32[$4 + 24 >> 2] = 0;
          break label$32;
         }
         HEAP32[$4 + 28 >> 2] = $6;
         HEAP32[$4 + 24 >> 2] = 1;
         HEAP32[$4 + 20 >> 2] = HEAP32[$0 >> 2];
        }
        $85($4 + 8 | 0, $2_1, $5_1, $4 + 20 | 0);
        $2_1 = HEAP32[$4 + 12 >> 2];
        if (!HEAP32[$4 + 8 >> 2]) {
         HEAP32[$0 + 4 >> 2] = $5_1;
         HEAP32[$0 >> 2] = $2_1;
         break label$10;
        }
        if (($2_1 | 0) == -2147483647) {
         break label$10
        }
        if (!$2_1) {
         break label$21
        }
        $86($2_1, HEAP32[$4 + 16 >> 2]);
        wasm2js_trap();
       }
       $87();
       wasm2js_trap();
      }
      global$0 = $4 + 32 | 0;
      $2_1 = HEAP32[$0 + 8 >> 2];
     }
     HEAP32[$0 + 8 >> 2] = $2_1 + 1;
     HEAP8[HEAP32[$0 >> 2] + $2_1 | 0] = $1_1;
     break label$1;
    }
    HEAP8[$3_1 + 13 | 0] = $1_1 & 63 | 128;
    HEAP8[$3_1 + 12 | 0] = $1_1 >>> 6 | 192;
    $1_1 = 2;
   }
   $2_1 = HEAP32[$0 + 8 >> 2];
   if ($1_1 >>> 0 > HEAP32[$0 + 4 >> 2] - $2_1 >>> 0) {
    $84($0, $2_1, $1_1);
    $2_1 = HEAP32[$0 + 8 >> 2];
   }
   $108(HEAP32[$0 >> 2] + $2_1 | 0, $3_1 + 12 | 0, $1_1);
   HEAP32[$0 + 8 >> 2] = $1_1 + $2_1;
  }
  global$0 = $3_1 + 16 | 0;
  return 0;
 }
 
 function $82($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  return byn$mgfn_shared$19($0, $1_1, 1048908) | 0;
 }
 
 function $83($0, $1_1, $2_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  $2_1 = $2_1 | 0;
  var $3_1 = 0;
  $0 = HEAP32[$0 >> 2];
  $3_1 = HEAP32[$0 + 8 >> 2];
  if (HEAP32[$0 + 4 >> 2] - $3_1 >>> 0 < $2_1 >>> 0) {
   $84($0, $3_1, $2_1);
   $3_1 = HEAP32[$0 + 8 >> 2];
  }
  $108(HEAP32[$0 >> 2] + $3_1 | 0, $1_1, $2_1);
  HEAP32[$0 + 8 >> 2] = $2_1 + $3_1;
  return 0;
 }
 
 function $84($0, $1_1, $2_1) {
  var $3_1 = 0, $4 = 0;
  $3_1 = global$0 - 32 | 0;
  global$0 = $3_1;
  label$1 : {
   label$2 : {
    $4 = $1_1;
    $1_1 = $1_1 + $2_1 | 0;
    if ($4 >>> 0 > $1_1 >>> 0) {
     break label$2
    }
    $2_1 = HEAP32[$0 + 4 >> 2];
    $4 = $2_1 << 1;
    $1_1 = $1_1 >>> 0 < $4 >>> 0 ? $4 : $1_1;
    $4 = $1_1 >>> 0 <= 8 ? 8 : $1_1;
    $1_1 = ($4 ^ -1) >>> 31 | 0;
    label$3 : {
     if (!$2_1) {
      HEAP32[$3_1 + 24 >> 2] = 0;
      break label$3;
     }
     HEAP32[$3_1 + 28 >> 2] = $2_1;
     HEAP32[$3_1 + 24 >> 2] = 1;
     HEAP32[$3_1 + 20 >> 2] = HEAP32[$0 >> 2];
    }
    $85($3_1 + 8 | 0, $1_1, $4, $3_1 + 20 | 0);
    $1_1 = HEAP32[$3_1 + 12 >> 2];
    if (!HEAP32[$3_1 + 8 >> 2]) {
     HEAP32[$0 + 4 >> 2] = $4;
     HEAP32[$0 >> 2] = $1_1;
     break label$1;
    }
    if (($1_1 | 0) == -2147483647) {
     break label$1
    }
    if (!$1_1) {
     break label$2
    }
    $86($1_1, HEAP32[$3_1 + 16 >> 2]);
    wasm2js_trap();
   }
   $87();
   wasm2js_trap();
  }
  global$0 = $3_1 + 32 | 0;
 }
 
 function $85($0, $1_1, $2_1, $3_1) {
  folding_inner0 : {
   label$1 : {
    if ($1_1) {
     if (($2_1 | 0) < 0) {
      break label$1
     }
     label$3 : {
      label$4 : {
       label$5 : {
        if (HEAP32[$3_1 + 4 >> 2]) {
         $1_1 = HEAP32[$3_1 + 8 >> 2];
         if (!$1_1) {
          if (!$2_1) {
           $1_1 = 1;
           break label$4;
          }
          $1_1 = $3($2_1, 1);
          break label$5;
         }
         $1_1 = $5(HEAP32[$3_1 >> 2], $1_1, 1, $2_1);
         break label$5;
        }
        if (!$2_1) {
         $1_1 = 1;
         break label$4;
        }
        $1_1 = $3($2_1, 1);
       }
       if (!$1_1) {
        break label$3
       }
      }
      HEAP32[$0 + 4 >> 2] = $1_1;
      HEAP32[$0 + 8 >> 2] = $2_1;
      HEAP32[$0 >> 2] = 0;
      return;
     }
     HEAP32[$0 + 4 >> 2] = 1;
     break folding_inner0;
    }
    HEAP32[$0 + 4 >> 2] = 0;
    break folding_inner0;
   }
   HEAP32[$0 + 4 >> 2] = 0;
   HEAP32[$0 >> 2] = 1;
   return;
  }
  HEAP32[$0 + 8 >> 2] = $2_1;
  HEAP32[$0 >> 2] = 1;
 }
 
 function $86($0, $1_1) {
  var $2_1 = 0;
  $2_1 = $0;
  $0 = HEAP32[262340];
  FUNCTION_TABLE[($0 ? $0 : 3) | 0]($2_1, $1_1);
  wasm2js_trap();
 }
 
 function $87() {
  var $0 = 0;
  $0 = global$0 - 32 | 0;
  global$0 = $0;
  HEAP32[$0 + 20 >> 2] = 0;
  HEAP32[$0 + 24 >> 2] = 0;
  HEAP32[$0 + 12 >> 2] = 1;
  HEAP32[$0 + 8 >> 2] = 1048980;
  HEAP32[$0 + 16 >> 2] = 1048932;
  $92($0 + 8 | 0, 1048988);
  wasm2js_trap();
 }
 
 function $91($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  wasm2js_trap();
 }
 
 function $92($0, $1_1) {
  var $2_1 = 0, $3_1 = 0;
  $2_1 = global$0 - 32 | 0;
  global$0 = $2_1;
  HEAP16[$2_1 + 28 >> 1] = 1;
  HEAP32[$2_1 + 24 >> 2] = $1_1;
  HEAP32[$2_1 + 20 >> 2] = $0;
  HEAP32[$2_1 + 16 >> 2] = 1049112;
  HEAP32[$2_1 + 12 >> 2] = 1049112;
  $1_1 = global$0 - 16 | 0;
  global$0 = $1_1;
  label$1 : {
   $0 = $2_1 + 12 | 0;
   $2_1 = HEAP32[$0 + 12 >> 2];
   if ($2_1) {
    $3_1 = HEAP32[$0 + 8 >> 2];
    if (!$3_1) {
     break label$1
    }
    HEAP32[$1_1 + 12 >> 2] = $2_1;
    HEAP32[$1_1 + 8 >> 2] = $0;
    HEAP32[$1_1 + 4 >> 2] = $3_1;
    $0 = global$0 - 16 | 0;
    global$0 = $0;
    $1_1 = $1_1 + 4 | 0;
    $2_1 = HEAP32[$1_1 >> 2];
    $3_1 = HEAP32[$2_1 + 12 >> 2];
    label$10 : {
     label$2 : {
      label$3 : {
       switch (HEAP32[$2_1 + 4 >> 2]) {
       case 0:
        if ($3_1) {
         break label$10
        }
        $2_1 = 0;
        $3_1 = 1048624;
        break label$2;
       case 1:
        break label$3;
       default:
        break label$10;
       };
      }
      if ($3_1) {
       break label$10
      }
      $3_1 = HEAP32[$2_1 >> 2];
      $2_1 = HEAP32[$3_1 + 4 >> 2];
      $3_1 = HEAP32[$3_1 >> 2];
     }
     HEAP32[$0 + 4 >> 2] = $2_1;
     HEAP32[$0 >> 2] = $3_1;
     $3_1 = $0;
     $0 = HEAP32[$1_1 + 4 >> 2];
     $41($3_1, 1048852, HEAP32[$0 + 8 >> 2], HEAP32[$1_1 + 8 >> 2], HEAPU8[$0 + 16 | 0], HEAPU8[$0 + 17 | 0]);
     wasm2js_trap();
    }
    HEAP32[$0 + 4 >> 2] = 0;
    HEAP32[$0 >> 2] = $2_1;
    $3_1 = $0;
    $0 = HEAP32[$1_1 + 4 >> 2];
    $41($3_1, 1048872, HEAP32[$0 + 8 >> 2], HEAP32[$1_1 + 8 >> 2], HEAPU8[$0 + 16 | 0], HEAPU8[$0 + 17 | 0]);
    wasm2js_trap();
   }
   $94(1048788);
   wasm2js_trap();
  }
  $94(1048804);
  wasm2js_trap();
 }
 
 function $93($0, $1_1, $2_1) {
  var $3_1 = 0, $4 = 0, $5_1 = 0, $6 = 0, $7 = 0, $8_1 = 0, $9 = 0, $10_1 = 0, $11_1 = 0, $12_1 = 0, $13_1 = 0;
  folding_inner0 : {
   $10_1 = HEAP32[$0 >> 2];
   $3_1 = HEAP32[$0 + 8 >> 2];
   if ($10_1 | $3_1) {
    label$2 : {
     if (!$3_1) {
      break label$2
     }
     $9 = $1_1 + $2_1 | 0;
     $7 = HEAP32[$0 + 12 >> 2] + 1 | 0;
     $4 = $1_1;
     while (1) {
      label$4 : {
       $3_1 = $4;
       $7 = $7 - 1 | 0;
       if (!$7) {
        break label$4
       }
       if (($3_1 | 0) == ($9 | 0)) {
        break label$2
       }
       $4 = HEAP8[$3_1 | 0];
       label$5 : {
        if (($4 | 0) >= 0) {
         $5_1 = $4 & 255;
         $4 = $3_1 + 1 | 0;
         break label$5;
        }
        $8_1 = HEAPU8[$3_1 + 1 | 0] & 63;
        $5_1 = $4 & 31;
        if ($4 >>> 0 <= 4294967263) {
         $5_1 = $8_1 | $5_1 << 6;
         $4 = $3_1 + 2 | 0;
         break label$5;
        }
        $8_1 = HEAPU8[$3_1 + 2 | 0] & 63 | $8_1 << 6;
        if ($4 >>> 0 < 4294967280) {
         $5_1 = $8_1 | $5_1 << 12;
         $4 = $3_1 + 3 | 0;
         break label$5;
        }
        $5_1 = $5_1 << 18 & 1835008 | (HEAPU8[$3_1 + 3 | 0] & 63 | $8_1 << 6);
        if (($5_1 | 0) == 1114112) {
         break label$2
        }
        $4 = $3_1 + 4 | 0;
       }
       $6 = $4 + ($6 - $3_1 | 0) | 0;
       if (($5_1 | 0) != 1114112) {
        continue
       }
       break label$2;
      }
      break;
     };
     if (($3_1 | 0) == ($9 | 0)) {
      break label$2
     }
     $4 = HEAP8[$3_1 | 0];
     if (!(($4 | 0) >= 0 | $4 >>> 0 < 4294967264 | $4 >>> 0 < 4294967280) & (($4 & 255) << 18 & 1835008 | (HEAPU8[$3_1 + 3 | 0] & 63 | ((HEAPU8[$3_1 + 2 | 0] & 63) << 6 | (HEAPU8[$3_1 + 1 | 0] & 63) << 12))) == 1114112) {
      break label$2
     }
     label$10 : {
      label$11 : {
       if (!$6) {
        break label$11
       }
       if ($2_1 >>> 0 <= $6 >>> 0) {
        $3_1 = 0;
        if (($2_1 | 0) == ($6 | 0)) {
         break label$11
        }
        break label$10;
       }
       $3_1 = 0;
       if (HEAP8[$1_1 + $6 | 0] < -64) {
        break label$10
       }
      }
      $3_1 = $1_1;
     }
     $2_1 = $3_1 ? $6 : $2_1;
     $1_1 = $3_1 ? $3_1 : $1_1;
    }
    if (!$10_1) {
     break folding_inner0
    }
    $13_1 = HEAP32[$0 + 4 >> 2];
    label$14 : {
     if ($2_1 >>> 0 >= 16) {
      $6 = 0;
      $7 = 0;
      $3_1 = 0;
      __inlined_func$102 : {
       label$1 : {
        label$20 : {
         $5_1 = $1_1 + 3 & -4;
         $10_1 = $5_1 - $1_1 | 0;
         if ($10_1 >>> 0 > $2_1 >>> 0) {
          break label$20
         }
         $9 = $2_1 - $10_1 | 0;
         if ($9 >>> 0 < 4) {
          break label$20
         }
         $8_1 = $9 & 3;
         $4 = 0;
         $12_1 = ($1_1 | 0) == ($5_1 | 0);
         label$31 : {
          if ($12_1) {
           break label$31
          }
          if ($5_1 + ($1_1 ^ -1) >>> 0 >= 3) {
           while (1) {
            $7 = $1_1 + $6 | 0;
            $4 = ((($4 + (HEAP8[$7 | 0] > -65) | 0) + (HEAP8[$7 + 1 | 0] > -65) | 0) + (HEAP8[$7 + 2 | 0] > -65) | 0) + (HEAP8[$7 + 3 | 0] > -65) | 0;
            $6 = $6 + 4 | 0;
            if ($6) {
             continue
            }
            break;
           }
          }
          if ($12_1) {
           break label$31
          }
          $7 = $1_1 - $5_1 | 0;
          $5_1 = $1_1 + $6 | 0;
          while (1) {
           $4 = (HEAP8[$5_1 | 0] > -65) + $4 | 0;
           $5_1 = $5_1 + 1 | 0;
           $7 = $7 + 1 | 0;
           if ($7) {
            continue
           }
           break;
          };
         }
         $6 = $1_1 + $10_1 | 0;
         label$8 : {
          if (!$8_1) {
           break label$8
          }
          $5_1 = ($9 & -4) + $6 | 0;
          $3_1 = HEAP8[$5_1 | 0] > -65;
          if (($8_1 | 0) == 1) {
           break label$8
          }
          $3_1 = (HEAP8[$5_1 + 1 | 0] > -65) + $3_1 | 0;
          if (($8_1 | 0) == 2) {
           break label$8
          }
          $3_1 = (HEAP8[$5_1 + 2 | 0] > -65) + $3_1 | 0;
         }
         $9 = $9 >>> 2 | 0;
         $7 = $3_1 + $4 | 0;
         while (1) {
          $3_1 = $6;
          if (!$9) {
           break label$1
          }
          $8_1 = $9 >>> 0 >= 192 ? 192 : $9;
          $10_1 = $8_1 & 3;
          $6 = $8_1 << 2;
          $5_1 = 0;
          if ($8_1 >>> 0 >= 4) {
           $12_1 = $3_1 + ($6 & 1008) | 0;
           $4 = $3_1;
           while (1) {
            $11_1 = HEAP32[$4 >> 2];
            $11_1 = $5_1 + ((($11_1 ^ -1) >>> 7 | $11_1 >>> 6) & 16843009) | 0;
            $5_1 = HEAP32[$4 + 4 >> 2];
            $11_1 = $11_1 + ((($5_1 ^ -1) >>> 7 | $5_1 >>> 6) & 16843009) | 0;
            $5_1 = HEAP32[$4 + 8 >> 2];
            $11_1 = $11_1 + ((($5_1 ^ -1) >>> 7 | $5_1 >>> 6) & 16843009) | 0;
            $5_1 = HEAP32[$4 + 12 >> 2];
            $5_1 = $11_1 + ((($5_1 ^ -1) >>> 7 | $5_1 >>> 6) & 16843009) | 0;
            $4 = $4 + 16 | 0;
            if (($12_1 | 0) != ($4 | 0)) {
             continue
            }
            break;
           };
          }
          $9 = $9 - $8_1 | 0;
          $6 = $3_1 + $6 | 0;
          $7 = (Math_imul(($5_1 >>> 8 & 16711935) + ($5_1 & 16711935) | 0, 65537) >>> 16 | 0) + $7 | 0;
          if (!$10_1) {
           continue
          }
          break;
         };
         $3_1 = $3_1 + (($8_1 & 252) << 2) | 0;
         $4 = HEAP32[$3_1 >> 2];
         $4 = (($4 ^ -1) >>> 7 | $4 >>> 6) & 16843009;
         label$12 : {
          if (($10_1 | 0) == 1) {
           break label$12
          }
          $6 = HEAP32[$3_1 + 4 >> 2];
          $4 = $4 + ((($6 ^ -1) >>> 7 | $6 >>> 6) & 16843009) | 0;
          if (($10_1 | 0) == 2) {
           break label$12
          }
          $3_1 = HEAP32[$3_1 + 8 >> 2];
          $4 = $4 + ((($3_1 ^ -1) >>> 7 | $3_1 >>> 6) & 16843009) | 0;
         }
         $3_1 = $4;
         $3_1 = (Math_imul(($3_1 >>> 8 & 459007) + ($3_1 & 16711935) | 0, 65537) >>> 16 | 0) + $7 | 0;
         break __inlined_func$102;
        }
        $3_1 = 0;
        if (!$2_1) {
         break __inlined_func$102
        }
        $6 = $2_1 & 3;
        label$144 : {
         if ($2_1 >>> 0 < 4) {
          $5_1 = 0;
          break label$144;
         }
         $4 = $2_1 & -4;
         $5_1 = 0;
         while (1) {
          $3_1 = $1_1 + $5_1 | 0;
          $7 = ((((HEAP8[$3_1 | 0] > -65) + $7 | 0) + (HEAP8[$3_1 + 1 | 0] > -65) | 0) + (HEAP8[$3_1 + 2 | 0] > -65) | 0) + (HEAP8[$3_1 + 3 | 0] > -65) | 0;
          $5_1 = $5_1 + 4 | 0;
          if (($4 | 0) != ($5_1 | 0)) {
           continue
          }
          break;
         };
        }
        if (!$6) {
         break label$1
        }
        $4 = $1_1 + $5_1 | 0;
        while (1) {
         $7 = (HEAP8[$4 | 0] > -65) + $7 | 0;
         $4 = $4 + 1 | 0;
         $6 = $6 - 1 | 0;
         if ($6) {
          continue
         }
         break;
        };
       }
       $3_1 = $7;
      }
      break label$14;
     }
     if (!$2_1) {
      $3_1 = 0;
      break label$14;
     }
     $7 = $2_1 & 3;
     label$175 : {
      if ($2_1 >>> 0 < 4) {
       $3_1 = 0;
       $5_1 = 0;
       break label$175;
      }
      $4 = $2_1 & -4;
      $3_1 = 0;
      $5_1 = 0;
      while (1) {
       $6 = $3_1;
       $3_1 = $1_1 + $5_1 | 0;
       $3_1 = ((($6 + (HEAP8[$3_1 | 0] > -65) | 0) + (HEAP8[$3_1 + 1 | 0] > -65) | 0) + (HEAP8[$3_1 + 2 | 0] > -65) | 0) + (HEAP8[$3_1 + 3 | 0] > -65) | 0;
       $5_1 = $5_1 + 4 | 0;
       if (($4 | 0) != ($5_1 | 0)) {
        continue
       }
       break;
      };
     }
     if (!$7) {
      break label$14
     }
     $4 = $1_1 + $5_1 | 0;
     while (1) {
      $3_1 = (HEAP8[$4 | 0] > -65) + $3_1 | 0;
      $4 = $4 + 1 | 0;
      $7 = $7 - 1 | 0;
      if ($7) {
       continue
      }
      break;
     };
    }
    label$21 : {
     if ($3_1 >>> 0 < $13_1 >>> 0) {
      $6 = $13_1 - $3_1 | 0;
      $3_1 = 0;
      label$23 : {
       label$24 : {
        switch (HEAPU8[$0 + 32 | 0] - 1 | 0) {
        case 0:
         $3_1 = $6;
         $6 = 0;
         break label$23;
        case 1:
         break label$24;
        default:
         break label$23;
        };
       }
       $3_1 = $6 >>> 1 | 0;
       $6 = $6 + 1 >>> 1 | 0;
      }
      $3_1 = $3_1 + 1 | 0;
      $4 = HEAP32[$0 + 24 >> 2];
      $5_1 = HEAP32[$0 + 16 >> 2];
      $0 = HEAP32[$0 + 20 >> 2];
      while (1) {
       $3_1 = $3_1 - 1 | 0;
       if (!$3_1) {
        break label$21
       }
       if (!(FUNCTION_TABLE[HEAP32[$4 + 16 >> 2]]($0, $5_1) | 0)) {
        continue
       }
       break;
      };
      return 1;
     }
     break folding_inner0;
    }
    if (FUNCTION_TABLE[HEAP32[$4 + 12 >> 2]]($0, $1_1, $2_1) | 0) {
     $0 = 1
    } else {
     $3_1 = 0;
     label$29 : {
      while (1) {
       $1_1 = $6;
       if (($3_1 | 0) == ($6 | 0)) {
        break label$29
       }
       $3_1 = $3_1 + 1 | 0;
       if (!(FUNCTION_TABLE[HEAP32[$4 + 16 >> 2]]($0, $5_1) | 0)) {
        continue
       }
       break;
      };
      $1_1 = $3_1 - 1 | 0;
     }
     $0 = $1_1 >>> 0 < $6 >>> 0;
    }
    return $0;
   }
   return FUNCTION_TABLE[HEAP32[HEAP32[$0 + 24 >> 2] + 12 >> 2]](HEAP32[$0 + 20 >> 2], $1_1, $2_1) | 0;
  }
  return FUNCTION_TABLE[HEAP32[HEAP32[$0 + 24 >> 2] + 12 >> 2]](HEAP32[$0 + 20 >> 2], $1_1, $2_1) | 0;
 }
 
 function $94($0) {
  var $1_1 = 0;
  $1_1 = global$0 - 32 | 0;
  global$0 = $1_1;
  HEAP32[$1_1 + 12 >> 2] = 0;
  HEAP32[$1_1 + 16 >> 2] = 0;
  HEAP32[$1_1 + 4 >> 2] = 1;
  HEAP32[$1_1 + 8 >> 2] = 1049112;
  HEAP32[$1_1 + 28 >> 2] = 43;
  HEAP32[$1_1 + 24 >> 2] = 1048624;
  HEAP32[$1_1 >> 2] = $1_1 + 24;
  $92($1_1, $0);
  wasm2js_trap();
 }
 
 function $95($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  var $2_1 = 0, $3_1 = 0, $4 = 0, $5_1 = 0, $6 = 0, $7 = 0, $8_1 = 0, $9 = 0, $10_1 = 0, $11_1 = 0, $12_1 = 0, $13_1 = 0;
  $3_1 = $1_1;
  $1_1 = 0;
  $10_1 = global$0 - 48 | 0;
  global$0 = $10_1;
  $5_1 = 39;
  $0 = HEAP32[$0 >> 2];
  if ($0 >>> 0 >= 1e4) {
   while (1) {
    $9 = 0;
    __inlined_func$_ZN17compiler_builtins3int4udiv10divmod_u6417h6026910b5ed08e40E : {
     if (!$1_1) {
      i64toi32_i32$HIGH_BITS = 0;
      $2_1 = ($0 >>> 0) / 1e4 | 0;
      break __inlined_func$_ZN17compiler_builtins3int4udiv10divmod_u6417h6026910b5ed08e40E;
     }
     $4 = 51 - Math_clz32($1_1) | 0;
     $7 = 0 - $4 | 0;
     $8_1 = $4 & 63;
     $2_1 = $8_1 & 31;
     if ($8_1 >>> 0 >= 32) {
      $8_1 = 0;
      $6 = $1_1 >>> $2_1 | 0;
     } else {
      $8_1 = $1_1 >>> $2_1 | 0;
      $6 = ((1 << $2_1) - 1 & $1_1) << 32 - $2_1 | $0 >>> $2_1;
     }
     $7 = $7 & 63;
     $2_1 = $7 & 31;
     if ($7 >>> 0 >= 32) {
      $7 = $0 << $2_1;
      $2_1 = 0;
     } else {
      $7 = (1 << $2_1) - 1 & $0 >>> 32 - $2_1 | $1_1 << $2_1;
      $2_1 = $0 << $2_1;
     }
     if ($4) {
      while (1) {
       $11_1 = $8_1 << 1 | $6 >>> 31;
       $8_1 = $6 << 1 | $7 >>> 31;
       $12_1 = 0 - ($11_1 + ($8_1 >>> 0 > 9999) | 0) >> 31;
       $13_1 = $12_1 & 1e4;
       $6 = $8_1 - $13_1 | 0;
       $8_1 = $11_1 - ($8_1 >>> 0 < $13_1 >>> 0) | 0;
       $7 = $7 << 1 | $2_1 >>> 31;
       $2_1 = $9 | $2_1 << 1;
       $9 = $12_1 & 1;
       $4 = $4 - 1 | 0;
       if ($4) {
        continue
       }
       break;
      }
     }
     i64toi32_i32$HIGH_BITS = $7 << 1 | $2_1 >>> 31;
     $2_1 = $9 | $2_1 << 1;
    }
    $7 = $2_1 >>> 16 | 0;
    $8_1 = Math_imul($7, 0);
    $6 = Math_imul($2_1 & 65535, 1e4);
    $9 = Math_imul($7, 1e4) + ($6 >>> 16 | 0) | 0;
    $4 = $9 & 65535;
    $7 = i64toi32_i32$HIGH_BITS;
    i64toi32_i32$HIGH_BITS = $8_1 + Math_imul($7, 1e4) + ($9 >>> 16) + ($4 >>> 16) | 0;
    $11_1 = ($10_1 + 9 | 0) + $5_1 | 0;
    $9 = $11_1 - 4 | 0;
    $4 = $0 - ($6 & 65535 | $4 << 16) | 0;
    $8_1 = (($4 & 65535) >>> 0) / 100 | 0;
    $6 = ($8_1 << 1) + 1049148 | 0;
    $6 = HEAPU8[$6 | 0] | HEAPU8[$6 + 1 | 0] << 8;
    HEAP8[$9 | 0] = $6;
    HEAP8[$9 + 1 | 0] = $6 >>> 8;
    $6 = $11_1 - 2 | 0;
    $4 = (($4 - Math_imul($8_1, 100) & 65535) << 1) + 1049148 | 0;
    $4 = HEAPU8[$4 | 0] | HEAPU8[$4 + 1 | 0] << 8;
    HEAP8[$6 | 0] = $4;
    HEAP8[$6 + 1 | 0] = $4 >>> 8;
    $5_1 = $5_1 - 4 | 0;
    $4 = !$1_1 & $0 >>> 0 > 99999999 | ($1_1 | 0) != 0;
    $0 = $2_1;
    $1_1 = $7;
    if ($4) {
     continue
    }
    break;
   }
  }
  if ($0 >>> 0 > 99) {
   $5_1 = $5_1 - 2 | 0;
   $1_1 = $5_1 + ($10_1 + 9 | 0) | 0;
   $2_1 = $0;
   $0 = (($0 & 65535) >>> 0) / 100 | 0;
   $2_1 = (($2_1 - Math_imul($0, 100) & 65535) << 1) + 1049148 | 0;
   $2_1 = HEAPU8[$2_1 | 0] | HEAPU8[$2_1 + 1 | 0] << 8;
   HEAP8[$1_1 | 0] = $2_1;
   HEAP8[$1_1 + 1 | 0] = $2_1 >>> 8;
  }
  label$5 : {
   if ($0 >>> 0 >= 10) {
    $5_1 = $5_1 - 2 | 0;
    $1_1 = $5_1 + ($10_1 + 9 | 0) | 0;
    $0 = ($0 << 1) + 1049148 | 0;
    $0 = HEAPU8[$0 | 0] | HEAPU8[$0 + 1 | 0] << 8;
    HEAP8[$1_1 | 0] = $0;
    HEAP8[$1_1 + 1 | 0] = $0 >>> 8;
    break label$5;
   }
   $5_1 = $5_1 - 1 | 0;
   HEAP8[$5_1 + ($10_1 + 9 | 0) | 0] = $0 + 48;
  }
  $4 = ($10_1 + 9 | 0) + $5_1 | 0;
  $0 = HEAP32[$3_1 + 28 >> 2];
  $1_1 = $0 & 1;
  $2_1 = $1_1 ? 43 : 1114112;
  $8_1 = 39 - $5_1 | 0;
  $1_1 = $1_1 + $8_1 | 0;
  $7 = $0 & 4 ? 1049112 : 0;
  __inlined_func$97 : {
   folding_inner0 : {
    label$12 : {
     if (!HEAP32[$3_1 >> 2]) {
      $0 = 1;
      $1_1 = HEAP32[$3_1 + 20 >> 2];
      $3_1 = HEAP32[$3_1 + 24 >> 2];
      if ($103($1_1, $3_1, $2_1, $7)) {
       break label$12
      }
      break folding_inner0;
     }
     $5_1 = HEAP32[$3_1 + 4 >> 2];
     if ($5_1 >>> 0 <= $1_1 >>> 0) {
      $0 = 1;
      $1_1 = HEAP32[$3_1 + 20 >> 2];
      $3_1 = HEAP32[$3_1 + 24 >> 2];
      if ($103($1_1, $3_1, $2_1, $7)) {
       break label$12
      }
      break folding_inner0;
     }
     if ($0 & 8) {
      $12_1 = HEAP32[$3_1 + 16 >> 2];
      HEAP32[$3_1 + 16 >> 2] = 48;
      $11_1 = HEAPU8[$3_1 + 32 | 0];
      $0 = 1;
      HEAP8[$3_1 + 32 | 0] = 1;
      $6 = HEAP32[$3_1 + 20 >> 2];
      $9 = HEAP32[$3_1 + 24 >> 2];
      if ($103($6, $9, $2_1, $7)) {
       break label$12
      }
      $0 = ($5_1 - $1_1 | 0) + 1 | 0;
      label$16 : {
       while (1) {
        $0 = $0 - 1 | 0;
        if (!$0) {
         break label$16
        }
        if (!(FUNCTION_TABLE[HEAP32[$9 + 16 >> 2]]($6, 48) | 0)) {
         continue
        }
        break;
       };
       $2_1 = 1;
       break __inlined_func$97;
      }
      $0 = 1;
      if (FUNCTION_TABLE[HEAP32[$9 + 12 >> 2]]($6, $4, $8_1) | 0) {
       break label$12
      }
      HEAP8[$3_1 + 32 | 0] = $11_1;
      HEAP32[$3_1 + 16 >> 2] = $12_1;
      $0 = 0;
      break label$12;
     }
     $1_1 = $5_1 - $1_1 | 0;
     label$18 : {
      label$19 : {
       label$20 : {
        $0 = HEAPU8[$3_1 + 32 | 0];
        switch ($0 - 1 | 0) {
        case 1:
         break label$19;
        case 0:
        case 2:
         break label$20;
        default:
         break label$18;
        };
       }
       $0 = $1_1;
       $1_1 = 0;
       break label$18;
      }
      $0 = $1_1 >>> 1 | 0;
      $1_1 = $1_1 + 1 >>> 1 | 0;
     }
     $0 = $0 + 1 | 0;
     $5_1 = HEAP32[$3_1 + 24 >> 2];
     $6 = HEAP32[$3_1 + 16 >> 2];
     $3_1 = HEAP32[$3_1 + 20 >> 2];
     label$21 : {
      while (1) {
       $0 = $0 - 1 | 0;
       if (!$0) {
        break label$21
       }
       if (!(FUNCTION_TABLE[HEAP32[$5_1 + 16 >> 2]]($3_1, $6) | 0)) {
        continue
       }
       break;
      };
      $2_1 = 1;
      break __inlined_func$97;
     }
     $0 = 1;
     if ($103($3_1, $5_1, $2_1, $7)) {
      break label$12
     }
     if (FUNCTION_TABLE[HEAP32[$5_1 + 12 >> 2]]($3_1, $4, $8_1) | 0) {
      break label$12
     }
     $0 = 0;
     while (1) {
      $2_1 = 0;
      if (($0 | 0) == ($1_1 | 0)) {
       break __inlined_func$97
      }
      $0 = $0 + 1 | 0;
      if (!(FUNCTION_TABLE[HEAP32[$5_1 + 16 >> 2]]($3_1, $6) | 0)) {
       continue
      }
      break;
     };
     $2_1 = $0 - 1 >>> 0 < $1_1 >>> 0;
     break __inlined_func$97;
    }
    $2_1 = $0;
    break __inlined_func$97;
   }
   $2_1 = FUNCTION_TABLE[HEAP32[$3_1 + 12 >> 2]]($1_1, $4, $8_1) | 0;
  }
  $0 = $2_1;
  global$0 = $10_1 + 48 | 0;
  return $0 | 0;
 }
 
 function $96($0, $1_1, $2_1) {
  var $3_1 = 0, $4 = 0, $5_1 = 0, $6 = 0, $7 = 0, $8_1 = 0, $9 = 0, $10_1 = 0, $11_1 = 0, $12_1 = 0;
  $3_1 = global$0 - 48 | 0;
  global$0 = $3_1;
  HEAP32[$3_1 + 36 >> 2] = $1_1;
  HEAP8[$3_1 + 44 | 0] = 3;
  HEAP32[$3_1 + 28 >> 2] = 32;
  HEAP32[$3_1 + 40 >> 2] = 0;
  HEAP32[$3_1 + 32 >> 2] = $0;
  HEAP32[$3_1 + 20 >> 2] = 0;
  HEAP32[$3_1 + 12 >> 2] = 0;
  label$1 : {
   label$2 : {
    $9 = HEAP32[$2_1 + 16 >> 2];
    label$3 : {
     label$4 : {
      if (!$9) {
       $0 = HEAP32[$2_1 + 12 >> 2];
       if (!$0) {
        break label$4
       }
       $1_1 = HEAP32[$2_1 + 8 >> 2];
       $5_1 = $0 << 3;
       $7 = ($0 - 1 & 536870911) + 1 | 0;
       $0 = HEAP32[$2_1 >> 2];
       while (1) {
        $4 = HEAP32[$0 + 4 >> 2];
        if ($4) {
         if (FUNCTION_TABLE[HEAP32[HEAP32[$3_1 + 36 >> 2] + 12 >> 2]](HEAP32[$3_1 + 32 >> 2], HEAP32[$0 >> 2], $4) | 0) {
          break label$3
         }
        }
        if (FUNCTION_TABLE[HEAP32[$1_1 + 4 >> 2]](HEAP32[$1_1 >> 2], $3_1 + 12 | 0) | 0) {
         break label$3
        }
        $1_1 = $1_1 + 8 | 0;
        $0 = $0 + 8 | 0;
        $5_1 = $5_1 - 8 | 0;
        if ($5_1) {
         continue
        }
        break;
       };
       break label$4;
      }
      $0 = HEAP32[$2_1 + 20 >> 2];
      if (!$0) {
       break label$4
      }
      $12_1 = $0 << 5;
      $7 = ($0 - 1 & 134217727) + 1 | 0;
      $8_1 = HEAP32[$2_1 + 8 >> 2];
      $0 = HEAP32[$2_1 >> 2];
      while (1) {
       $1_1 = HEAP32[$0 + 4 >> 2];
       if ($1_1) {
        if (FUNCTION_TABLE[HEAP32[HEAP32[$3_1 + 36 >> 2] + 12 >> 2]](HEAP32[$3_1 + 32 >> 2], HEAP32[$0 >> 2], $1_1) | 0) {
         break label$3
        }
       }
       $1_1 = $5_1 + $9 | 0;
       HEAP32[$3_1 + 28 >> 2] = HEAP32[$1_1 + 16 >> 2];
       HEAP8[$3_1 + 44 | 0] = HEAPU8[$1_1 + 28 | 0];
       HEAP32[$3_1 + 40 >> 2] = HEAP32[$1_1 + 24 >> 2];
       $6 = HEAP32[$1_1 + 12 >> 2];
       $10_1 = 0;
       $4 = 0;
       label$10 : {
        label$11 : {
         switch (HEAP32[$1_1 + 8 >> 2] - 1 | 0) {
         case 0:
          $11_1 = ($6 << 3) + $8_1 | 0;
          if (HEAP32[$11_1 + 4 >> 2] != 24) {
           break label$10
          }
          $6 = HEAP32[HEAP32[$11_1 >> 2] >> 2];
          break;
         case 1:
          break label$10;
         default:
          break label$11;
         };
        }
        $4 = 1;
       }
       HEAP32[$3_1 + 16 >> 2] = $6;
       HEAP32[$3_1 + 12 >> 2] = $4;
       $4 = HEAP32[$1_1 + 4 >> 2];
       label$13 : {
        label$14 : {
         switch (HEAP32[$1_1 >> 2] - 1 | 0) {
         case 0:
          $6 = ($4 << 3) + $8_1 | 0;
          if (HEAP32[$6 + 4 >> 2] != 24) {
           break label$13
          }
          $4 = HEAP32[HEAP32[$6 >> 2] >> 2];
          break;
         case 1:
          break label$13;
         default:
          break label$14;
         };
        }
        $10_1 = 1;
       }
       HEAP32[$3_1 + 24 >> 2] = $4;
       HEAP32[$3_1 + 20 >> 2] = $10_1;
       $1_1 = (HEAP32[$1_1 + 20 >> 2] << 3) + $8_1 | 0;
       if (FUNCTION_TABLE[HEAP32[$1_1 + 4 >> 2]](HEAP32[$1_1 >> 2], $3_1 + 12 | 0) | 0) {
        break label$3
       }
       $0 = $0 + 8 | 0;
       $5_1 = $5_1 + 32 | 0;
       if (($12_1 | 0) != ($5_1 | 0)) {
        continue
       }
       break;
      };
     }
     if (HEAPU32[$2_1 + 4 >> 2] <= $7 >>> 0) {
      break label$2
     }
     $0 = HEAP32[$2_1 >> 2] + ($7 << 3) | 0;
     if (!(FUNCTION_TABLE[HEAP32[HEAP32[$3_1 + 36 >> 2] + 12 >> 2]](HEAP32[$3_1 + 32 >> 2], HEAP32[$0 >> 2], HEAP32[$0 + 4 >> 2]) | 0)) {
      break label$2
     }
    }
    $0 = 1;
    break label$1;
   }
   $0 = 0;
  }
  global$0 = $3_1 + 48 | 0;
  return $0;
 }
 
 function $99($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  return $93($1_1, HEAP32[$0 >> 2], HEAP32[$0 + 4 >> 2]) | 0;
 }
 
 function $101($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  return FUNCTION_TABLE[HEAP32[HEAP32[$0 + 4 >> 2] + 12 >> 2]](HEAP32[$0 >> 2], $1_1) | 0;
 }
 
 function $103($0, $1_1, $2_1, $3_1) {
  var $4 = 0;
  label$1 : {
   label$2 : {
    if (($2_1 | 0) != 1114112) {
     $4 = 1;
     if (FUNCTION_TABLE[HEAP32[$1_1 + 16 >> 2]]($0, $2_1) | 0) {
      break label$2
     }
    }
    if ($3_1) {
     break label$1
    }
    $4 = 0;
   }
   return $4;
  }
  return FUNCTION_TABLE[HEAP32[$1_1 + 12 >> 2]]($0, $3_1, 0) | 0;
 }
 
 function $106($0, $1_1) {
  $0 = $0 | 0;
  $1_1 = $1_1 | 0;
  return FUNCTION_TABLE[HEAP32[HEAP32[$1_1 + 24 >> 2] + 12 >> 2]](HEAP32[$1_1 + 20 >> 2], 1049348, 5) | 0;
 }
 
 function $108($0, $1_1, $2_1) {
  var $3_1 = 0, $4 = 0, $5_1 = 0, $6 = 0, $7 = 0, $8_1 = 0, $9 = 0, $10_1 = 0;
  $6 = $2_1;
  label$1 : {
   if ($2_1 >>> 0 < 16) {
    $2_1 = $0;
    break label$1;
   }
   $3_1 = 0 - $0 & 3;
   $4 = $3_1 + $0 | 0;
   if ($3_1) {
    $2_1 = $0;
    $5_1 = $1_1;
    while (1) {
     HEAP8[$2_1 | 0] = HEAPU8[$5_1 | 0];
     $5_1 = $5_1 + 1 | 0;
     $2_1 = $2_1 + 1 | 0;
     if ($4 >>> 0 > $2_1 >>> 0) {
      continue
     }
     break;
    };
   }
   $8_1 = $6 - $3_1 | 0;
   $7 = $8_1 & -4;
   $2_1 = $7 + $4 | 0;
   $3_1 = $1_1 + $3_1 | 0;
   label$5 : {
    if ($3_1 & 3) {
     if (($7 | 0) <= 0) {
      break label$5
     }
     $6 = $3_1 << 3;
     $9 = $6 & 24;
     $5_1 = $3_1 & -4;
     $1_1 = $5_1 + 4 | 0;
     $6 = 0 - $6 & 24;
     $5_1 = HEAP32[$5_1 >> 2];
     while (1) {
      $10_1 = $5_1 >>> $9 | 0;
      $5_1 = HEAP32[$1_1 >> 2];
      HEAP32[$4 >> 2] = $10_1 | $5_1 << $6;
      $1_1 = $1_1 + 4 | 0;
      $4 = $4 + 4 | 0;
      if ($4 >>> 0 < $2_1 >>> 0) {
       continue
      }
      break;
     };
     break label$5;
    }
    if (($7 | 0) <= 0) {
     break label$5
    }
    $1_1 = $3_1;
    while (1) {
     HEAP32[$4 >> 2] = HEAP32[$1_1 >> 2];
     $1_1 = $1_1 + 4 | 0;
     $4 = $4 + 4 | 0;
     if ($4 >>> 0 < $2_1 >>> 0) {
      continue
     }
     break;
    };
   }
   $6 = $8_1 & 3;
   $1_1 = $3_1 + $7 | 0;
  }
  if ($6) {
   $3_1 = $2_1 + $6 | 0;
   while (1) {
    HEAP8[$2_1 | 0] = HEAPU8[$1_1 | 0];
    $1_1 = $1_1 + 1 | 0;
    $2_1 = $2_1 + 1 | 0;
    if ($3_1 >>> 0 > $2_1 >>> 0) {
     continue
    }
    break;
   };
  }
  return $0;
 }
 
 function __wasm_ctz_i32($0) {
  if ($0) {
   return 31 - Math_clz32($0 - 1 ^ $0) | 0
  }
  return 32;
 }
 
 function __wasm_rotl_i32($0) {
  var $1_1 = 0;
  $1_1 = $0 & 31;
  $0 = 0 - $0 & 31;
  return (-1 >>> $1_1 & -2) << $1_1 | (-1 << $0 & -2) >>> $0;
 }
 
 function byn$mgfn_shared$19($0, $1_1, $2_1) {
  var $3_1 = 0;
  $3_1 = global$0 - 16 | 0;
  global$0 = $3_1;
  HEAP32[$3_1 + 12 >> 2] = HEAP32[$0 >> 2];
  $0 = $96($3_1 + 12 | 0, $2_1, $1_1);
  global$0 = $3_1 + 16 | 0;
  return $0;
 }
 
 bufferView = HEAPU8;
 initActiveSegments(imports);
 var FUNCTION_TABLE = [null, $1, $95, $32, $14, $20, $17, $19, $15, $11, $10, $39, $40, $16, $37, $38, $14, $12, $14, $83, $80, $82, $14, $106, $91, $101, $99, $14, $12];
 function __wasm_memory_size() {
  return buffer.byteLength / 65536 | 0;
 }
 
 function __wasm_memory_grow(pagesToAdd) {
  pagesToAdd = pagesToAdd | 0;
  var oldPages = __wasm_memory_size() | 0;
  var newPages = oldPages + pagesToAdd | 0;
  if ((oldPages < newPages) && (newPages < 65536)) {
   var newBuffer = new ArrayBuffer(Math_imul(newPages, 65536));
   var newHEAP8 = new Int8Array(newBuffer);
   newHEAP8.set(HEAP8);
   HEAP8 = new Int8Array(newBuffer);
   HEAP16 = new Int16Array(newBuffer);
   HEAP32 = new Int32Array(newBuffer);
   HEAPU8 = new Uint8Array(newBuffer);
   HEAPU16 = new Uint16Array(newBuffer);
   HEAPU32 = new Uint32Array(newBuffer);
   HEAPF32 = new Float32Array(newBuffer);
   HEAPF64 = new Float64Array(newBuffer);
   buffer = newBuffer;
   bufferView = HEAPU8;
  }
  return oldPages;
 }
 
 return {
  "memory": Object.create(Object.prototype, {
   "grow": {
    "value": __wasm_memory_grow
   }, 
   "buffer": {
    "get": function () {
     return buffer;
    }
    
   }
  }), 
  "greet": $2, 
  "cabi_realloc": $8, 
  "__data_end": {
   get value() {
    return global$1;
   }, 
   set value(_global$1) {
    global$1 = _global$1;
   }
  }, 
  "__heap_base": {
   get value() {
    return global$2;
   }, 
   set value(_global$2) {
    global$2 = _global$2;
   }
  }
 };
},
function asm1(imports) {
  function Table(ret) {
  // grow method not included; table is not growable
  ret.set = function(i, func) {
    this[i] = func;
  };
  ret.get = function(i) {
    return this[i];
  };
  return ret;
}


 var Math_imul = Math.imul;
 var Math_fround = Math.fround;
 var Math_abs = Math.abs;
 var Math_clz32 = Math.clz32;
 var Math_min = Math.min;
 var Math_max = Math.max;
 var Math_floor = Math.floor;
 var Math_ceil = Math.ceil;
 var Math_trunc = Math.trunc;
 var Math_sqrt = Math.sqrt;
 var nan = NaN;
 var infinity = Infinity;
 function $0($0_1, $1) {
  $0_1 = $0_1 | 0;
  $1 = $1 | 0;
  FUNCTION_TABLE[0]($0_1, $1);
 }
 
 var FUNCTION_TABLE = Table([]);
 return {
  "$0": $0, 
  "$imports": FUNCTION_TABLE
 };
},
function asm2(imports) {
  var $ = imports[""];
 var FUNCTION_TABLE = $.$imports;
 var Math_imul = Math.imul;
 var Math_fround = Math.fround;
 var Math_abs = Math.abs;
 var Math_clz32 = Math.clz32;
 var Math_min = Math.min;
 var Math_max = Math.max;
 var Math_floor = Math.floor;
 var Math_ceil = Math.ceil;
 var Math_trunc = Math.trunc;
 var Math_sqrt = Math.sqrt;
 var nan = NaN;
 var infinity = Infinity;
 var fimport$0 = $["0"];
 FUNCTION_TABLE[0] = fimport$0;
 return {
  
 };
}];

function instantiate(imports) {
  const wasm_file_to_asm_index = {
    'timeline.core.wasm': 0,
    'timeline.core2.wasm': 1,
    'timeline.core3.wasm': 2
  };

  return _instantiate(
     module_name => wasm_file_to_asm_index[module_name],
      imports,
      (module_index, imports) => ({ exports: asmInit[module_index](imports) })
  );
}
