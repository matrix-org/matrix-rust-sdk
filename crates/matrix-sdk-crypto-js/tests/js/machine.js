const { EncryptionAlgorithm, EncryptionSettings, HistoryVisibility, UserId, DeviceId, OlmMachine, DeviceLists, KeysUploadRequest, KeysQueryRequest } = require('../../js/pkg/matrix_sdk_crypto');
const test = require('node:test');
const assert = require('node:assert/strict');

test('EncryptionAlgorithm', (t) => {
    assert.equal(EncryptionAlgorithm.OlmV1Curve25519AesSha2, 0);
    assert.equal(EncryptionAlgorithm.MegolmV1AesSha2, 1);
});

test('EncryptionSettings', (t) => {
    let es = new EncryptionSettings();

    assert.equal(es.algorithm, EncryptionAlgorithm.MegolmV1AesSha2, 'It has a default algorithm');
    assert.equal(es.rotationPeriod, 604800000000n, 'It has a default rotation period');
    assert.equal(es.rotationPeriodMessages, 100n, 'It has a default message rotation period');
    assert.equal(es.historyVisibility, HistoryVisibility.Shared, 'It has a default history visibility');

    es.algorithm = EncryptionSettings.OlmV1Curve25519AesSha2;
    assert.equal(es.algorithm, EncryptionAlgorithm.OlmV1Curve25519AesSha2, 'It has a new algorithm');
    assert.throws(() => { es.algorithm = 42 }, Error, 'Enum values are validated');

    es.rotationPeriod = 42n;
    assert.equal(es.rotationPeriod, 42n, 'It has a new rotation period');

    es.rotationPeriodMessages = 153n;
    assert.equal(es.rotationPeriodMessages, 153n, 'It has a new message rotation period');

    es.historyVisibility = HistoryVisibility.WorldReadable;
    assert.equal(es.historyVisibility, HistoryVisibility.WorldReadable, 'It has a new history visibility');
    assert.throws(() => { es.historyVisibility = 42 }, Error, 'Enum values are validated');
});

test('OlmMachine', async (t) => {
    const user_id = new UserId('@foo:bar.org');
    const device_id = new DeviceId('baz');

    await t.test('Construct', async (t) => {
        const machine = await new OlmMachine(user_id, device_id);

        assert.ok(machine instanceof OlmMachine);
        assert.equal(machine.userId().toString(), '@foo:bar.org', 'User ID is present');
        assert.equal(machine.deviceId().toString(), 'baz', 'Device ID is present');
    });

    await t.test('Identity keys', async (t) => {
        const machine = await new OlmMachine(user_id, device_id);
        const identity_keys = machine.identityKeys();
        
        assert.match(identity_keys.ed25519.toBase64(), /^[A-Za-z0-9+/]+$/, 'Ed25519 can be base64-encoded');
        assert.match(identity_keys.curve25519.toBase64(), /^[A-Za-z0-9+/]+$/, 'Curve25519 can be base64-encoded');
        assert.ok(identity_keys.curve25519.length > 0, 'Curve25519\'s length is greater than zero');
    });

    await t.test('Display name', async (t) => {
        const machine = await new OlmMachine(user_id, device_id);

        assert.equal(await machine.displayName(), undefined, 'Display name is absent by default');
    });

    await t.test('Tracked users', async (t) => {
        const machine = await new OlmMachine(user_id, device_id);
        const tracked_users = machine.trackedUsers();

        assert.ok(tracked_users instanceof Set, 'Tracket users are stored in a `Set`');
        assert.equal(tracked_users.size, 0, 'No tracked users by default');
    });

    await t.test('Update tracked users', async (t) => {
        const machine = await new OlmMachine(user_id, device_id);
        const update_tracked_users = await machine.updateTrackedUsers([new UserId('@foo:matrix.org'), new UserId('@bar:matrix.org')]);

        assert.equal(update_tracked_users, undefined, 'Updating tracked users returns nothing');
    });

    await t.test('Receive sync changes', async (t) => {
        const machine = await new OlmMachine(user_id, device_id);
        const to_device_events = JSON.stringify({});
        const changed_devices = new DeviceLists(
            [new UserId('@foo:matrix.org'), new UserId('@bar:matrix.org')],
            [new UserId('@baz:matrix.org'), new UserId('@qux:matrix.org')],
        );
        const one_time_key_counts = new Map();
        one_time_key_counts.set('foo', 42);
        one_time_key_counts.set('bar', 153);
        const unused_fallback_keys = new Set();
        unused_fallback_keys.add('baz');
        unused_fallback_keys.add('qux');

        const decrypted_to_device = JSON.parse(
            await machine.receiveSyncChanges(
                to_device_events,
                changed_devices,
                one_time_key_counts,
                unused_fallback_keys,
            )
        );

        assert.deepEqual(decrypted_to_device, {}, 'Nothing to do by default');
    });

    await t.test('Outgoing requests', async (t) => {
        const machine = await new OlmMachine(user_id, device_id);
        const outgoing_requests = await machine.outgoingRequests();

        assert.ok(outgoing_requests instanceof Array, 'Outgoing requests are stored in an `Array`');
        assert.equal(outgoing_requests.length, 2, 'There is 2 outgoing requests');

        const request1 = outgoing_requests[0];
        const request2 = outgoing_requests[1];

        assert.ok(request1 instanceof KeysUploadRequest, 'First request is `KeysUploadRequest');
        assert.ok(request1.request_id.length > 0, 'First request has an ID');
        assert.ok(JSON.parse(request1.body) instanceof Object, 'First request has a valid body');

        assert.ok(request2 instanceof KeysQueryRequest, 'Second request is `KeysQueryRequest`');
        assert.ok(request2.request_id.length > 0, 'Second request has an ID');
        assert.ok(JSON.parse(request2.body) instanceof Object, 'Second request has a valid body');
    });
});
