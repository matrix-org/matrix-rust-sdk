const { UserId, DeviceId, RoomId, ServerName } = require('../../pkg/matrix_sdk_crypto');
const test = require('node:test');
const assert = require('node:assert/strict');

test('UserId', (t) => {
    assert.throws(() => { new UserId('@foobar') }, Error, 'An invalid user ID must throw an error');

    const user = new UserId('@foo:bar.org');

    assert.equal(user.localpart(), 'foo', 'Localpart is present');
    assert.ok(user.serverName() instanceof ServerName, 'Server name is present');
    assert.equal(user.isHistorical, false, 'User ID is not historical');
    assert.equal(user.toString(), '@foo:bar.org', 'Can read the user ID as a string');
});

test('DeviceId', (t) => {
    assert.equal(new DeviceId('foo').toString(), 'foo', 'Can read the device ID as a string');
});

test('RoomId', (t) => {
    assert.throws(() => { new UserId('!foo') }, Error, 'An invalid room ID must throw an error');

    const room = new RoomId('!foo:bar.org');

    assert.equal(room.localpart(), 'foo', 'Localpart is present');
    assert.ok(room.serverName() instanceof ServerName, 'Server name is present');
    assert.equal(room.toString(), '!foo:bar.org', 'Can read the room ID as a string');
});

test('ServerName', (t) => {
    assert.throws(() => { new ServerName('@foobar') }, Error, 'An invalid server name must throw an error');

    assert.equal(new ServerName('foo.org').host(), 'foo.org', 'Host is present');
    assert.equal(new ServerName('foo.org').port(), undefined, 'Port is absent');
    assert.equal(new ServerName('foo.org:1234').port(), 1234, 'Port is present');
    assert.equal(new ServerName('foo.org').isIpLiteral(), false, 'Server name is not an IP literal');
});
