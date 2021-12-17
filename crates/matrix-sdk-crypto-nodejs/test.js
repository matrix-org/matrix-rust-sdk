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
        this.#inner = new mod.OlmMachine([
            (s) => store.doCall(s),
        ]);
    }

    test1 = () => {
        this.#inner.test1();
    };
}

const store = new TestStore();
const machine = new ProxyMachine(store);
// const machine = new mod.OlmMachine([
//     (s) => store.doCall(s),
// ]);
machine.test1();
