const { DecryptedRoomEvent } = require('../');

describe(DecryptedRoomEvent.name, () => {
    test('cannot be instantiated', () => {
        expect(() => { new DecryptedRoomEvent() }).toThrow();
    });
});
