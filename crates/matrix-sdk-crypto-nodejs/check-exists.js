try {
    require('./index');
} catch (e) {
    if (e.message === 'Failed to load native binding' || e.message.startsWith("Cannot find module")) {
        if (process.env.NODE_ENV === "production") {
            require('shelljs').exec('yarn build:release');
        } else {
            require('shelljs').exec('yarn build:debug');
        }
    } else {
        throw e;
    }
}