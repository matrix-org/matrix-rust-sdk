import { instantiate } from './guests/timeline/js/timeline.js';
import fs from 'fs/promises';

async function compile(module_path) {
  const path = `./guests/timeline/js/${module_path}`;
  console.log(`[log] Loading \`${path}\``);
  const buffer = await fs.readFile(path);
  return WebAssembly.compile(buffer);
}

const exports = await instantiate(
  compile,
  {
    'matrix:ui-timeline/std': {
      print: function (msg) {
        console.log(msg);
      }
    }
  }
);

exports.greet('Gordon');
