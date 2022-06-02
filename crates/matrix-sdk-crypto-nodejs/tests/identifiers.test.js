const { UserId, DeviceId, RoomId, ServerName } = require('../');

describe('UserId', () => {
    test('cannot be invalid', () => {
        expect(() => { new UserId('@foobar') }).toThrow();
    });

    const user = new UserId('@foo:bar.org');

    test('localpart is present', () => {
        expect(user.localpart).toStrictEqual('foo');
    });

    test('server name is present', () => {
        expect(user.serverName).toBeInstanceOf(ServerName);
    });

    test('user ID is not historical', () => {
        expect(user.isHistorical()).toStrictEqual(false);
    });

    test('can read the user ID as a string', () => {
        expect(user.toString()).toStrictEqual('@foo:bar.org');
    })
});

describe('DeviceId', () => {
    const device = new DeviceId('foo');

    test('can read the device ID as a string', () => {
        expect(device.toString()).toStrictEqual('foo');
    })
});

describe('RoomId', () => {
    test('cannot be invalid', () => {
        expect(() => { new RoomId('!foo') }).toThrow();
    });

    const room = new RoomId('!foo:bar.org');

    test('localpart is present', () => {
        expect(room.localpart).toStrictEqual('foo');
    });

    test('server name is present', () => {
        expect(room.serverName).toBeInstanceOf(ServerName);
    });

    test('can read the room ID as string', () => {
        expect(room.toString()).toStrictEqual('!foo:bar.org');
    });
});

describe('ServerName', () => {
    test('cannot be invalid', () => {
        expect(() => { new ServerName('@foobar') }).toThrow()
    });

    test('host is present', () => {
        expect(new ServerName('foo.org').host).toStrictEqual('foo.org');
    });

    test('port can be optional', () => {
        expect(new ServerName('foo.org').port).toStrictEqual(null);
        expect(new ServerName('foo.org:1234').port).toStrictEqual(1234);
    });

    test('server is not an IP literal', () => {
        expect(new ServerName('foo.org').isIpLiteral()).toStrictEqual(false);
    });
});
