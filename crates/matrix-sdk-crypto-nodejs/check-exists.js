try {
    require('./lib/napi');
} catch (e) {
    if (e.message === 'Failed to load native binding' || e.message.startsWith("Cannot find module")) {
        let code;
        if (process.env.NODE_ENV === "production") {
            code = require('shelljs').exec('yarn build:release').code;
        } else {
            code = require('shelljs').exec('yarn build:debug').code;
        }
        if(code !== 0) {
            process.exit(code);
        }
    } else {
        throw e;
    }
}