const { OlmMachine, UserId, DeviceId, DeviceKeyId, RoomId, DeviceLists, RequestType, KeysUploadRequest, KeysQueryRequest, KeysClaimRequest, EncryptionSettings, DecryptedRoomEvent, VerificationState, CrossSigningStatus, MaybeSignature } = require('../pkg/matrix_sdk_crypto_js');
require('fake-indexeddb/auto');

describe(OlmMachine.name, () => {
    test('can be instantiated with the async initializer', async () => {
        expect(await new OlmMachine(new UserId('@foo:bar.org'), new DeviceId('baz'))).toBeInstanceOf(OlmMachine);
    });

    test('can be instantiated with a store', async () => {
        // No databases.
        expect(await indexedDB.databases()).toHaveLength(0);

        let store_name = 'hello';
        let store_passphrase = 'world';

        // Creating a new Olm machine.
        expect(await new OlmMachine(new UserId('@foo:bar.org'), new DeviceId('baz'), store_name, store_passphrase)).toBeInstanceOf(OlmMachine);

        // Oh, there is 2 databases now, prefixed by `store_name`.
        let databases = await indexedDB.databases();

        expect(databases).toHaveLength(2);
        expect(databases).toStrictEqual([
            { name: `${store_name}::matrix-sdk-crypto-meta`, version: 1 },
            { name: `${store_name}::matrix-sdk-crypto`, version: 1 },
        ]);

        // Creating a new Olm machine, with the stored state.
        expect(await new OlmMachine(new UserId('@foo:bar.org'), new DeviceId('baz'), store_name, store_passphrase)).toBeInstanceOf(OlmMachine);

        // Same number of databases.
        expect(await indexedDB.databases()).toHaveLength(2);
    });

    describe('cannot be instantiated with a store', () => {
        test('store name is missing', async () => {
            let store_name = null;
            let store_passphrase = 'world';

            let err = null;

            try {
                await new OlmMachine(new UserId('@foo:bar.org'), new DeviceId('baz'), store_name, store_passphrase);
            } catch (error) {
                err = error;
            }

            expect(err).toBeDefined();
        });

        test('store passphrase is missing', async () => {
            let store_name = 'hello';
            let store_passphrase = null;

            let err = null;

            try {
                await new OlmMachine(new UserId('@foo:bar.org'), new DeviceId('baz'), store_name, store_passphrase);
            } catch (error) {
                err = error;
            }

            expect(err).toBeDefined();
        });
    });

    const user = new UserId('@alice:example.org');
    const device = new DeviceId('foobar');
    const room = new RoomId('!baz:matrix.org');

    function machine(new_user, new_device) {
        return new OlmMachine(new_user || user, new_device || device);
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

    test('can read display name', async () => {
        expect(await machine().displayName).toBeUndefined();
    });

    test('can read tracked users', async () => {
        const trackedUsers = (await machine()).trackedUsers();

        expect(trackedUsers).toBeInstanceOf(Set);
        expect(trackedUsers.size).toStrictEqual(0);
    });

    test('can update tracked users', async () => {
        const m = await machine();

        expect(await m.updateTrackedUsers([user])).toStrictEqual(undefined);
    });

    test('can receive sync changes', async () => {
        const m = await machine();
        const toDeviceEvents = JSON.stringify({});
        const changedDevices = new DeviceLists();
        const oneTimeKeyCounts = new Map();
        const unusedFallbackKeys = new Set();

        const receiveSyncChanges = JSON.parse(await m.receiveSyncChanges(toDeviceEvents, changedDevices, oneTimeKeyCounts, unusedFallbackKeys));

        expect(receiveSyncChanges).toEqual({});
    });

    test('can get the outgoing requests that need to be send out', async () => {
        const m = await machine();
        const toDeviceEvents = JSON.stringify({});
        const changedDevices = new DeviceLists();
        const oneTimeKeyCounts = new Map();
        const unusedFallbackKeys = new Set();

        const receiveSyncChanges = JSON.parse(await m.receiveSyncChanges(toDeviceEvents, changedDevices, oneTimeKeyCounts, unusedFallbackKeys));

        expect(receiveSyncChanges).toEqual({});

        const outgoingRequests = await m.outgoingRequests();

        expect(outgoingRequests).toHaveLength(2);

        {
            expect(outgoingRequests[0]).toBeInstanceOf(KeysUploadRequest);
            expect(outgoingRequests[0].id).toBeDefined();
            expect(outgoingRequests[0].type).toStrictEqual(RequestType.KeysUpload);

            const body = JSON.parse(outgoingRequests[0].body);
            expect(body.device_keys).toBeDefined();
            expect(body.one_time_keys).toBeDefined();
        }

        {
            expect(outgoingRequests[1]).toBeInstanceOf(KeysQueryRequest);
            expect(outgoingRequests[1].id).toBeDefined();
            expect(outgoingRequests[1].type).toStrictEqual(RequestType.KeysQuery);

            const body = JSON.parse(outgoingRequests[1].body);
            expect(body.timeout).toBeDefined();
            expect(body.device_keys).toBeDefined();
            expect(body.token).toBeDefined();
        }
    });

    describe('setup workflow to mark requests as sent', () => {
        let m;
        let ougoingRequests;

        beforeAll(async () => {
            m = await machine(new UserId('@alice:example.org'), new DeviceId('DEVICEID'));

            const toDeviceEvents = JSON.stringify({});
            const changedDevices = new DeviceLists();
            const oneTimeKeyCounts = new Map();
            const unusedFallbackKeys = new Set();

            const receiveSyncChanges = await m.receiveSyncChanges(toDeviceEvents, changedDevices, oneTimeKeyCounts, unusedFallbackKeys);
            outgoingRequests = await m.outgoingRequests();

            expect(outgoingRequests).toHaveLength(2);
        });

        test('can mark requests as sent', async () => {
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
            }
        });
    });

    describe('setup workflow to encrypt/decrypt events', () => {
        let m;
        const user = new UserId('@alice:example.org');
        const device = new DeviceId('JLAFKJWSCS');
        const room = new RoomId('!test:localhost');

        beforeAll(async () => {
            m = await machine(user, device);
        });
        
        test('can pass keysquery and keysclaim requests directly', async () => {
            {
                // derived from https://github.com/matrix-org/matrix-rust-sdk/blob/7f49618d350fab66b7e1dc4eaf64ec25ceafd658/benchmarks/benches/crypto_bench/keys_query.json
                const hypothetical_response = JSON.stringify({
                    "device_keys": {
                        "@example:localhost": {
                            "AFGUOBTZWM": {
                                "algorithms": [
                                    "m.olm.v1.curve25519-aes-sha2",
                                    "m.megolm.v1.aes-sha2"
                                ],
                                "device_id": "AFGUOBTZWM",
                                "keys": {
                                    "curve25519:AFGUOBTZWM": "boYjDpaC+7NkECQEeMh5dC+I1+AfriX0VXG2UV7EUQo",
                                    "ed25519:AFGUOBTZWM": "NayrMQ33ObqMRqz6R9GosmHdT6HQ6b/RX/3QlZ2yiec"
                                },
                                "signatures": {
                                    "@example:localhost": {
                                        "ed25519:AFGUOBTZWM": "RoSWvru1jj6fs2arnTedWsyIyBmKHMdOu7r9gDi0BZ61h9SbCK2zLXzuJ9ZFLao2VvA0yEd7CASCmDHDLYpXCA"
                                    }
                                },
                                "user_id": "@example:localhost",
                                "unsigned": {
                                    "device_display_name": "rust-sdk"
                                }
                            },
                        }
                    },
                    "failures": {},
                    "master_keys": {
                        "@example:localhost": {
                            "user_id": "@example:localhost",
                            "usage": [
                                "master"
                            ],
                            "keys": {
                                "ed25519:n2lpJGx0LiKnuNE1IucZP3QExrD4SeRP0veBHPe3XUU": "n2lpJGx0LiKnuNE1IucZP3QExrD4SeRP0veBHPe3XUU"
                            },
                            "signatures": {
                                "@example:localhost": {
                                    "ed25519:TCSJXPWGVS": "+j9G3L41I1fe0++wwusTTQvbboYW0yDtRWUEujhwZz4MAltjLSfJvY0hxhnz+wHHmuEXvQDen39XOpr1p29sAg"
                                }
                            }
                        }
                    },
                    "self_signing_keys": {
                        "@example:localhost": {
                            "user_id": "@example:localhost",
                            "usage": [
                                "self_signing"
                            ],
                            "keys": {
                                "ed25519:kQXOuy639Yt47mvNTdrIluoC6DMvfbZLYbxAmwiDyhI": "kQXOuy639Yt47mvNTdrIluoC6DMvfbZLYbxAmwiDyhI"
                            },
                            "signatures": {
                                "@example:localhost": {
                                    "ed25519:n2lpJGx0LiKnuNE1IucZP3QExrD4SeRP0veBHPe3XUU": "q32ifix/qyRpvmegw2BEJklwoBCAJldDNkcX+fp+lBA4Rpyqtycxge6BA4hcJdxYsy3oV0IHRuugS8rJMMFyAA"
                                }
                            }
                        }
                    },
                    "user_signing_keys": {
                        "@example:localhost": {
                            "user_id": "@example:localhost",
                            "usage": [
                                "user_signing"
                            ],
                            "keys": {
                                "ed25519:g4ED07Fnqf3GzVWNN1pZ0IFrPQVdqQf+PYoJNH4eE0s": "g4ED07Fnqf3GzVWNN1pZ0IFrPQVdqQf+PYoJNH4eE0s"
                            },
                            "signatures": {
                                "@example:localhost": {
                                    "ed25519:n2lpJGx0LiKnuNE1IucZP3QExrD4SeRP0veBHPe3XUU": "nKQu8alQKDefNbZz9luYPcNj+Z+ouQSot4fU/A23ELl1xrI06QVBku/SmDx0sIW1ytso0Cqwy1a+3PzCa1XABg"
                                }
                            }
                        }
                    }
                });
                const marked = await m.markRequestAsSent('foo', RequestType.KeysQuery, hypothetical_response);
            }

            {
                // derived from https://github.com/matrix-org/matrix-rust-sdk/blob/7f49618d350fab66b7e1dc4eaf64ec25ceafd658/benchmarks/benches/crypto_bench/keys_claim.json
                const hypothetical_response = JSON.stringify({
                    "one_time_keys": {
                        "@example:localhost": {
                            "AFGUOBTZWM": {
                                "signed_curve25519:AAAABQ": {
                                    "key": "9IGouMnkB6c6HOd4xUsNv4i3Dulb4IS96TzDordzOws",
                                    "signatures": {
                                        "@example:localhost": {
                                            "ed25519:AFGUOBTZWM": "2bvUbbmJegrV0eVP/vcJKuIWC3kud+V8+C0dZtg4dVovOSJdTP/iF36tQn2bh5+rb9xLlSeztXBdhy4c+LiOAg"
                                        }
                                    }
                                }
                            },
                        }
                    },
                    "failures": {}
                });
                const marked = await m.markRequestAsSent('bar', RequestType.KeysClaim, hypothetical_response);
            }
        });

        test('can share a room key', async () => {
            const other_users = [new UserId('@example:localhost')];

            const requests = JSON.parse(await m.shareRoomKey(room, other_users, new EncryptionSettings()));

            expect(requests).toHaveLength(1);
            expect(requests[0].event_type).toBeDefined();
            expect(requests[0].txn_id).toBeDefined();
            expect(requests[0].messages).toBeDefined();
            expect(requests[0].messages['@example:localhost']).toBeDefined();
        });

        let encrypted;

        test('can encrypt an event', async () => {
            encrypted = JSON.parse(await m.encryptRoomEvent(
                room,
                'm.room.message',
                JSON.stringify({
                    "msgtype": "m.text",
                    "body": "Hello, World!"
                }),
            ));

            expect(encrypted.algorithm).toBeDefined();
            expect(encrypted.ciphertext).toBeDefined();
            expect(encrypted.sender_key).toBeDefined();
            expect(encrypted.device_id).toStrictEqual(device.toString());
            expect(encrypted.session_id).toBeDefined();
        });

        test('can decrypt an event', async () => {
            const decrypted = await m.decryptRoomEvent(
                JSON.stringify({
                    "type": "m.room.encrypted",
                    "event_id": "$xxxxx:example.org",
                    "origin_server_ts": Date.now(),
                    "sender": user.toString(),
                    content: encrypted,
                    unsigned: {
                        "age": 1234
                    }
                }),
                room,
            );

            expect(decrypted).toBeInstanceOf(DecryptedRoomEvent);

            const event = JSON.parse(decrypted.event);
            expect(event.content.msgtype).toStrictEqual("m.text");
            expect(event.content.body).toStrictEqual("Hello, World!");

            expect(decrypted.sender.toString()).toStrictEqual(user.toString());
            expect(decrypted.senderDevice.toString()).toStrictEqual(device.toString());
            expect(decrypted.senderCurve25519Key).toBeDefined();
            expect(decrypted.senderClaimedEd25519Key).toBeDefined();
            expect(decrypted.forwardingCurve25519KeyChain).toHaveLength(0);
            expect(decrypted.verificationState).toStrictEqual(VerificationState.Trusted);
        });
    });

    test('can read cross-signing status', async () => {
        const m = await machine();
        const crossSigningStatus = await m.crossSigningStatus();

        expect(crossSigningStatus).toBeInstanceOf(CrossSigningStatus);
        expect(crossSigningStatus.hasMaster).toStrictEqual(false);
        expect(crossSigningStatus.hasSelfSigning).toStrictEqual(false);
        expect(crossSigningStatus.hasUserSigning).toStrictEqual(false);
    });

    test('can sign a message', async () => {
        const m = await machine();
        const signatures = await m.sign('foo');

        expect(signatures.isEmpty()).toStrictEqual(false);
        expect(signatures.count).toStrictEqual(1);

        let base64;

        // `get`
        {
            const signature = signatures.get(user);

            expect(signature.has('ed25519:foobar')).toStrictEqual(true);

            const s = signature.get('ed25519:foobar');

            expect(s).toBeInstanceOf(MaybeSignature);

            expect(s.isValid()).toStrictEqual(true);
            expect(s.isInvalid()).toStrictEqual(false);
            expect(s.invalidSignatureSource).toBeUndefined();

            base64 = s.signature.toBase64();

            expect(base64).toMatch(/^[A-Za-z0-9\+/]+$/);
            expect(s.signature.ed25519.toBase64()).toStrictEqual(base64);
        }

        // `getSignature`
        {
            const signature = signatures.getSignature(user, new DeviceKeyId('ed25519:foobar'));
            expect(signature.toBase64()).toStrictEqual(base64);
        }

        // Unknown signatures.
        {
            expect(signatures.get(new UserId('@hello:example.org'))).toBeUndefined();
            expect(signatures.getSignature(user, new DeviceKeyId('world:foobar'))).toBeUndefined();
        }
    });
});
