const { OlmMachine } = require('../');

describe(OlmMachine.name, () => {
    test('cannot be instantiated', () => {
        expect(() => { new OlmMachine() }).toThrow();
    });
});
