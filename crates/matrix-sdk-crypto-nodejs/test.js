const mod = require('./index');

class TestStore {
    #ts;

    constructor() {
        this.#ts = Date.now();
        console.log("TS: ", this.#ts);
    }

    doCall(s) {
        console.log('Call: ', s, this.#ts);
    }

    writeValue(k, v) {
        console.log('Write: ', k, v, this.#ts);
    }
}

const store = new TestStore();

const machine = new mod.OlmMachine(store);
// console.log("User ID: ", machine.userId);
// console.log("Device ID: ", machine.deviceId);
machine.test();

// const mod = require("./index");
const machine2 = new mod.DemoMachine(store);
machine2.doWork("keyHere", "valueHere");