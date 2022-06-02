const { OlmMachine, UserId, DeviceId, RoomId } = require('../');

describe(OlmMachine.name, () => {
    test('cannot be instantiated with the constructor', () => {
        expect(() => { new OlmMachine() }).toThrow();
    });

    test('can be instantiated with the async initializer', async () => {
        expect(await OlmMachine.initialize(new UserId('@foo:bar.org'), new DeviceId('baz'))).toBeInstanceOf(OlmMachine);
    });

    const user = new UserId('@foo:bar.org');
    const device = new DeviceId('baz');
    const room = new RoomId('!qux:matrix.org');

    function machine(new_user, new_device) {
        return OlmMachine.initialize(new_user || user, new_device || device);
    }

    test('can read user ID', async () => {
        expect((await machine()).userId.toString()).toStrictEqual(user.toString());
    });

    test('can read device ID', async () => {
        expect((await machine()).deviceId.toString()).toStrictEqual(device.toString());
    });

    test('can read identity keys', async () => {
        const identityKeys = (await machine()).identityKeys;

        expect(identityKeys.ed25519.toBase64()).toMatch(/^[A-Za-z0-9+/]+$/);
        expect(identityKeys.curve25519.toBase64()).toMatch(/^[A-Za-z0-9+/]+$/);
    });
});
