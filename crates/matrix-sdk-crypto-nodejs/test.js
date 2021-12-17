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

class ProxyMachine {
    #inner;

    constructor(store) {
        this.#inner = new mod.OlmMachine({
            doCall: (s) => store.doCall(s),
        });
    }

    test() {
        this.#inner.test();
    }
}

const store = new TestStore();
const machine = new ProxyMachine(store);
machine.test();
