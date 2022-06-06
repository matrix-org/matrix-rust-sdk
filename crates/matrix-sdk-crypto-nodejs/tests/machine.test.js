const { initTracing, OlmMachine, UserId, DeviceId, RoomId, DeviceLists, RequestType, KeysUploadRequest, KeysQueryRequest, EncryptionSettings } = require('../');

//initTracing();

describe(OlmMachine.name, () => {
    test('cannot be instantiated with the constructor', () => {
        expect(() => { new OlmMachine() }).toThrow();
    });

    test('can be instantiated with the async initializer', async () => {
        expect(await OlmMachine.initialize(new UserId('@foo:bar.org'), new DeviceId('baz'))).toBeInstanceOf(OlmMachine);
    });

    const user = new UserId('@alice:example.org');
    const device = new DeviceId('foobar');
    const room = new RoomId('!baz:matrix.org');

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

    test('can receive sync changes', async () => {
        const m = await machine();
        const toDeviceEvents = JSON.stringify({});
        const changedDevices = new DeviceLists();
        const oneTimeKeyCounts = {};
        const unusedFallbackKeys = [];

        const receiveSyncChanges = JSON.parse(await m.receiveSyncChanges(toDeviceEvents, changedDevices, oneTimeKeyCounts, unusedFallbackKeys));

        expect(receiveSyncChanges).toEqual({});
    });

    test('can get the outgoing requests that need to be send out', async () => {
        const m = await machine();
        const toDeviceEvents = JSON.stringify({});
        const changedDevices = new DeviceLists();
        const oneTimeKeyCounts = {};
        const unusedFallbackKeys = [];

        const receiveSyncChanges = JSON.parse(await m.receiveSyncChanges(toDeviceEvents, changedDevices, oneTimeKeyCounts, unusedFallbackKeys));

        expect(receiveSyncChanges).toEqual({});

        const outgoingRequests = await m.outgoingRequests();

        expect(outgoingRequests).toHaveLength(2);

        expect(outgoingRequests[0]).toBeInstanceOf(KeysUploadRequest);
        expect(outgoingRequests[0].id).toBeDefined();
        expect(outgoingRequests[0].type).toStrictEqual(RequestType.KeysUpload);
        {
            const body = JSON.parse(outgoingRequests[0].body);
            expect(body.device_keys).toBeDefined();
            expect(body.one_time_keys).toBeDefined();
        }

        expect(outgoingRequests[1]).toBeInstanceOf(KeysQueryRequest);
        expect(outgoingRequests[1].id).toBeDefined();
        expect(outgoingRequests[1].type).toStrictEqual(RequestType.KeysQuery);
        {
            const body = JSON.parse(outgoingRequests[1].body);
            expect(body.timeout).toBeDefined();
            expect(body.device_keys).toBeDefined();
            expect(body.token).toBeDefined();
        }
    });

    test('can mark requests as sent', async () => {
        const m = await machine(new UserId('@alice:example.org'), new DeviceId('DEVICEID'));

        const toDeviceEvents = JSON.stringify({});
        const changedDevices = new DeviceLists();
        const oneTimeKeyCounts = {};
        const unusedFallbackKeys = [];

        const receiveSyncChanges = await m.receiveSyncChanges(toDeviceEvents, changedDevices, oneTimeKeyCounts, unusedFallbackKeys);
        const outgoingRequests = await m.outgoingRequests();

        expect(outgoingRequests).toHaveLength(2);

        {
            const request = outgoingRequests[0];
            expect(request).toBeInstanceOf(KeysUploadRequest);

            // https://spec.matrix.org/v1.2/client-server-api/#post_matrixclientv3keysupload
            const hypothetical_response = JSON.stringify({
                "one_time_key_counts": {
                    "curve25519": 10,
                    "signed_curve25519": 20
                }
            });
            const marked = await m.markRequestAsSent(request.id, request.type, hypothetical_response);
            expect(marked).toStrictEqual(true);
        }

        {
            const request = outgoingRequests[1];
            expect(request).toBeInstanceOf(KeysQueryRequest);

            // https://spec.matrix.org/v1.2/client-server-api/#post_matrixclientv3keysquery
            const hypothetical_response = JSON.stringify({
                "device_keys": {
                    "@alice:example.org": {
                        "JLAFKJWSCS": {
                            "algorithms": [
                                "m.olm.v1.curve25519-aes-sha2",
                                "m.megolm.v1.aes-sha2"
                            ],
                            "device_id": "JLAFKJWSCS",
                            "keys": {
                                "curve25519:JLAFKJWSCS": "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4",
                                "ed25519:JLAFKJWSCS": "nE6W2fCblxDcOFmeEtCHNl8/l8bXcu7GKyAswA4r3mM"
                            },
                            "signatures": {
                                "@alice:example.org": {
                                    "ed25519:JLAFKJWSCS": "m53Wkbh2HXkc3vFApZvCrfXcX3AI51GsDHustMhKwlv3TuOJMj4wistcOTM8q2+e/Ro7rWFUb9ZfnNbwptSUBA"
                                }
                            },
                            "unsigned": {
                                "device_display_name": "Alice's mobile phone"
                            },
                            "user_id": "@alice:example.org"
                        }
                    }
                },
                "failures": {}
            });
            const marked = await m.markRequestAsSent(request.id, request.type, hypothetical_response);
            expect(marked).toStrictEqual(true);

            console.log(await m.getMissingSessions([user]));
        }
    });

    /*
    test('can get missing sessions', async () => {
        const m = await machine();

        expect(await m.getMissingSessions([user])).toStrictEqual(null);
    });

    test('can update tracked users', async () => {
        const m = await machine();

        expect(await m.updateTrackedUsers([user])).toStrictEqual(undefined);
    });

    test('can share a room key', async () => {
        const m = await machine();

        console.log(await m.shareRoomKey(room, [new UserId('@bob:example.org')], new EncryptionSettings()));
    });
    */
});
