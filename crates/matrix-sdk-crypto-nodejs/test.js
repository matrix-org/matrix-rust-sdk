const mod = require('./index');
const machine = new mod.OlmMachine("@travis:localhost", "DEVICE", {
    doCall: (s) => {
        console.log('Called: ', s);
        //throw new Error("oops");
        return 2;
    },
});
console.log("User ID: ", machine.userId);
console.log("Device ID: ", machine.deviceId);
console.log("Fn1: ", machine.test());
console.log("Fn2: ", machine.test());
