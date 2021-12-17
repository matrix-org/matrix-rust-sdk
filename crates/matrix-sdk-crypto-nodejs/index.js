const { existsSync } = require('fs');
const { join } = require('path');

let mod;
const independentBuildExists = existsSync(join(__dirname, 'index.node'));
if (independentBuildExists) {
    mod = require('./index.node');
} else {
    mod = require('./napi-module');
}

module.exports = mod;
