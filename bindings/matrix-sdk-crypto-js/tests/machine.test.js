const {
    CrossSigningStatus,
    DecryptedRoomEvent,
    DeviceId,
    DeviceKeyId,
    DeviceLists,
    EncryptionSettings,
    EventId,
    InboundGroupSession,
    KeysQueryRequest,
    KeysUploadRequest,
    MaybeSignature,
    OlmMachine,
    OwnUserIdentity,
    RequestType,
    RoomId,
    RoomMessageRequest,
    ShieldColor,
    SignatureUploadRequest,
    ToDeviceRequest,
    UserId,
    UserIdentity,
    VerificationRequest,
    VerificationState,
    Versions,
    getVersions,
} = require("../pkg/matrix_sdk_crypto_js");
const { addMachineToMachine } = require("./helper");
require("fake-indexeddb/auto");

describe("Versions", () => {
    test("can find out the crate versions", async () => {
        const versions = getVersions();

        expect(versions).toBeInstanceOf(Versions);
        expect(versions.vodozemac).toBeDefined();
        expect(versions.matrix_sdk_crypto).toBeDefined();
    });
});

describe(OlmMachine.name, () => {
    test("can be instantiated with the async initializer", async () => {
        expect(await OlmMachine.initialize(new UserId("@foo:bar.org"), new DeviceId("baz"))).toBeInstanceOf(OlmMachine);
    });

    test("can be instantiated with a store", async () => {
        let store_name = "hello";
        let store_passphrase = "world";

        const by_store_name = (db) => db.name.startsWith(store_name);

        // No databases.
        expect((await indexedDB.databases()).filter(by_store_name)).toHaveLength(0);

        // Creating a new Olm machine.
        expect(
            await OlmMachine.initialize(new UserId("@foo:bar.org"), new DeviceId("baz"), store_name, store_passphrase),
        ).toBeInstanceOf(OlmMachine);

        // Oh, there is 2 databases now, prefixed by `store_name`.
        let databases = (await indexedDB.databases()).filter(by_store_name);

        expect(databases).toHaveLength(2);
        expect(databases).toStrictEqual([
            { name: `${store_name}::matrix-sdk-crypto-meta`, version: 1 },
            { name: `${store_name}::matrix-sdk-crypto`, version: 2 },
        ]);

        // Creating a new Olm machine, with the stored state.
        expect(
            await OlmMachine.initialize(new UserId("@foo:bar.org"), new DeviceId("baz"), store_name, store_passphrase),
        ).toBeInstanceOf(OlmMachine);

        // Same number of databases.
        expect((await indexedDB.databases()).filter(by_store_name)).toHaveLength(2);
    });

    describe("cannot be instantiated with a store", () => {
        test("store name is missing", async () => {
            let store_name = null;
            let store_passphrase = "world";

            let err = null;

            try {
                await OlmMachine.initialize(
                    new UserId("@foo:bar.org"),
                    new DeviceId("baz"),
                    store_name,
                    store_passphrase,
                );
            } catch (error) {
                err = error;
            }

            expect(err).toBeDefined();
        });

        test("store passphrase is missing", async () => {
            let store_name = "hello";
            let store_passphrase = null;

            let err = null;

            try {
                await OlmMachine.initialize(
                    new UserId("@foo:bar.org"),
                    new DeviceId("baz"),
                    store_name,
                    store_passphrase,
                );
            } catch (error) {
                err = error;
            }

            expect(err).toBeDefined();
        });
    });

    const user = new UserId("@alice:example.org");
    const device = new DeviceId("foobar");
    const room = new RoomId("!baz:matrix.org");

    function machine(new_user, new_device) {
        return OlmMachine.initialize(new_user || user, new_device || device);
    }

    test("can drop/close", async () => {
        m = await machine();
        m.close();
    });

    test("can drop/close with a store", async () => {
        let store_name = "temporary";
        let store_passphrase = "temporary";

        const by_store_name = (db) => db.name.startsWith(store_name);

        // No databases.
        expect((await indexedDB.databases()).filter(by_store_name)).toHaveLength(0);

        // Creating a new Olm machine.
        const m = await OlmMachine.initialize(
            new UserId("@foo:bar.org"),
            new DeviceId("baz"),
            store_name,
            store_passphrase,
        );
        expect(m).toBeInstanceOf(OlmMachine);

        // Oh, there is 2 databases now, prefixed by `store_name`.
        let databases = (await indexedDB.databases()).filter(by_store_name);

        expect(databases).toHaveLength(2);
        expect(databases).toStrictEqual([
            { name: `${store_name}::matrix-sdk-crypto-meta`, version: 1 },
            { name: `${store_name}::matrix-sdk-crypto`, version: 2 },
        ]);

        // Let's force to close the `OlmMachine`.
        m.close();

        // Now we can delete the databases!
        for (const database_name of [`${store_name}::matrix-sdk-crypto`, `${store_name}::matrix-sdk-crypto-meta`]) {
            const deleting = indexedDB.deleteDatabase(database_name);
            deleting.onsuccess = () => {};
            deleting.onerror = () => {
                throw new Error("failed to remove the database (error)");
            };
            deleting.onblocked = () => {
                throw new Error("failed to remove the database (blocked)");
            };
        }
    });

    test("can read user ID", async () => {
        expect((await machine()).userId.toString()).toStrictEqual(user.toString());
    });

    test("can read device ID", async () => {
        expect((await machine()).deviceId.toString()).toStrictEqual(device.toString());
    });

    test("can read identity keys", async () => {
        const identityKeys = (await machine()).identityKeys;

        expect(identityKeys.ed25519.toBase64()).toMatch(/^[A-Za-z0-9+/]+$/);
        expect(identityKeys.curve25519.toBase64()).toMatch(/^[A-Za-z0-9+/]+$/);
    });

    test("can read display name", async () => {
        expect(await machine().displayName).toBeUndefined();
    });

    test("can read tracked users", async () => {
        const m = await machine();
        const trackedUsers = await m.trackedUsers();

        expect(trackedUsers).toBeInstanceOf(Set);
        expect(trackedUsers.size).toStrictEqual(0);
    });

    test("can update tracked users", async () => {
        const m = await machine();

        expect(await m.updateTrackedUsers([user])).toStrictEqual(undefined);
    });

    test("can receive sync changes", async () => {
        const m = await machine();
        const toDeviceEvents = JSON.stringify([]);
        const changedDevices = new DeviceLists();
        const oneTimeKeyCounts = new Map();
        const unusedFallbackKeys = new Set();

        const receiveSyncChanges = JSON.parse(
            await m.receiveSyncChanges(toDeviceEvents, changedDevices, oneTimeKeyCounts, unusedFallbackKeys),
        );

        expect(receiveSyncChanges).toEqual([]);
    });

    test("can get the outgoing requests that need to be send out", async () => {
        const m = await machine();
        const toDeviceEvents = JSON.stringify([]);
        const changedDevices = new DeviceLists();
        const oneTimeKeyCounts = new Map();
        const unusedFallbackKeys = new Set();

        const receiveSyncChanges = JSON.parse(
            await m.receiveSyncChanges(toDeviceEvents, changedDevices, oneTimeKeyCounts, unusedFallbackKeys),
        );

        expect(receiveSyncChanges).toEqual([]);

        const outgoingRequests = await m.outgoingRequests();

        expect(outgoingRequests).toHaveLength(2);

        {
            expect(outgoingRequests[0]).toBeInstanceOf(KeysUploadRequest);
            expect(outgoingRequests[0].id).toBeDefined();
            expect(outgoingRequests[0].type).toStrictEqual(RequestType.KeysUpload);
            expect(outgoingRequests[0].body).toBeDefined();

            const body = JSON.parse(outgoingRequests[0].body);
            expect(body.device_keys).toBeDefined();
            expect(body.one_time_keys).toBeDefined();
        }

        {
            expect(outgoingRequests[1]).toBeInstanceOf(KeysQueryRequest);
            expect(outgoingRequests[1].id).toBeDefined();
            expect(outgoingRequests[1].type).toStrictEqual(RequestType.KeysQuery);
            expect(outgoingRequests[1].body).toBeDefined();

            const body = JSON.parse(outgoingRequests[1].body);
            expect(body.timeout).toBeDefined();
            expect(body.device_keys).toBeDefined();
            expect(body.token).toBeDefined();
        }
    });

    describe("setup workflow to mark requests as sent", () => {
        let m;
        let ougoingRequests;

        beforeAll(async () => {
            m = await machine(new UserId("@alice:example.org"), new DeviceId("DEVICEID"));

            const toDeviceEvents = JSON.stringify([]);
            const changedDevices = new DeviceLists();
            const oneTimeKeyCounts = new Map();
            const unusedFallbackKeys = new Set();

            const receiveSyncChanges = await m.receiveSyncChanges(
                toDeviceEvents,
                changedDevices,
                oneTimeKeyCounts,
                unusedFallbackKeys,
            );
            outgoingRequests = await m.outgoingRequests();

            expect(outgoingRequests).toHaveLength(2);
        });

        test("can mark requests as sent", async () => {
            {
                const request = outgoingRequests[0];
                expect(request).toBeInstanceOf(KeysUploadRequest);

                // https://spec.matrix.org/v1.2/client-server-api/#post_matrixclientv3keysupload
                const hypothetical_response = JSON.stringify({
                    one_time_key_counts: {
                        curve25519: 10,
                        signed_curve25519: 20,
                    },
                });
                const marked = await m.markRequestAsSent(request.id, request.type, hypothetical_response);
                expect(marked).toStrictEqual(true);
            }

            {
                const request = outgoingRequests[1];
                expect(request).toBeInstanceOf(KeysQueryRequest);

                // https://spec.matrix.org/v1.2/client-server-api/#post_matrixclientv3keysquery
                const hypothetical_response = JSON.stringify({
                    device_keys: {
                        "@alice:example.org": {
                            JLAFKJWSCS: {
                                algorithms: ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
                                device_id: "JLAFKJWSCS",
                                keys: {
                                    "curve25519:JLAFKJWSCS": "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4",
                                    "ed25519:JLAFKJWSCS": "nE6W2fCblxDcOFmeEtCHNl8/l8bXcu7GKyAswA4r3mM",
                                },
                                signatures: {
                                    "@alice:example.org": {
                                        "ed25519:JLAFKJWSCS":
                                            "m53Wkbh2HXkc3vFApZvCrfXcX3AI51GsDHustMhKwlv3TuOJMj4wistcOTM8q2+e/Ro7rWFUb9ZfnNbwptSUBA",
                                    },
                                },
                                unsigned: {
                                    device_display_name: "Alice's mobile phone",
                                },
                                user_id: "@alice:example.org",
                            },
                        },
                    },
                    failures: {},
                });
                const marked = await m.markRequestAsSent(request.id, request.type, hypothetical_response);
                expect(marked).toStrictEqual(true);
            }
        });
    });

    describe("setup workflow to encrypt/decrypt events", () => {
        let m;
        const user = new UserId("@alice:example.org");
        const device = new DeviceId("JLAFKJWSCS");
        const room = new RoomId("!test:localhost");

        beforeAll(async () => {
            m = await machine(user, device);
        });

        test("can pass keysquery and keysclaim requests directly", async () => {
            {
                // derived from https://github.com/matrix-org/matrix-rust-sdk/blob/7f49618d350fab66b7e1dc4eaf64ec25ceafd658/benchmarks/benches/crypto_bench/keys_query.json
                const hypothetical_response = JSON.stringify({
                    device_keys: {
                        "@example:localhost": {
                            AFGUOBTZWM: {
                                algorithms: ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
                                device_id: "AFGUOBTZWM",
                                keys: {
                                    "curve25519:AFGUOBTZWM": "boYjDpaC+7NkECQEeMh5dC+I1+AfriX0VXG2UV7EUQo",
                                    "ed25519:AFGUOBTZWM": "NayrMQ33ObqMRqz6R9GosmHdT6HQ6b/RX/3QlZ2yiec",
                                },
                                signatures: {
                                    "@example:localhost": {
                                        "ed25519:AFGUOBTZWM":
                                            "RoSWvru1jj6fs2arnTedWsyIyBmKHMdOu7r9gDi0BZ61h9SbCK2zLXzuJ9ZFLao2VvA0yEd7CASCmDHDLYpXCA",
                                    },
                                },
                                user_id: "@example:localhost",
                                unsigned: {
                                    device_display_name: "rust-sdk",
                                },
                            },
                        },
                    },
                    failures: {},
                    master_keys: {
                        "@example:localhost": {
                            user_id: "@example:localhost",
                            usage: ["master"],
                            keys: {
                                "ed25519:n2lpJGx0LiKnuNE1IucZP3QExrD4SeRP0veBHPe3XUU":
                                    "n2lpJGx0LiKnuNE1IucZP3QExrD4SeRP0veBHPe3XUU",
                            },
                            signatures: {
                                "@example:localhost": {
                                    "ed25519:TCSJXPWGVS":
                                        "+j9G3L41I1fe0++wwusTTQvbboYW0yDtRWUEujhwZz4MAltjLSfJvY0hxhnz+wHHmuEXvQDen39XOpr1p29sAg",
                                },
                            },
                        },
                    },
                    self_signing_keys: {
                        "@example:localhost": {
                            user_id: "@example:localhost",
                            usage: ["self_signing"],
                            keys: {
                                "ed25519:kQXOuy639Yt47mvNTdrIluoC6DMvfbZLYbxAmwiDyhI":
                                    "kQXOuy639Yt47mvNTdrIluoC6DMvfbZLYbxAmwiDyhI",
                            },
                            signatures: {
                                "@example:localhost": {
                                    "ed25519:n2lpJGx0LiKnuNE1IucZP3QExrD4SeRP0veBHPe3XUU":
                                        "q32ifix/qyRpvmegw2BEJklwoBCAJldDNkcX+fp+lBA4Rpyqtycxge6BA4hcJdxYsy3oV0IHRuugS8rJMMFyAA",
                                },
                            },
                        },
                    },
                    user_signing_keys: {
                        "@example:localhost": {
                            user_id: "@example:localhost",
                            usage: ["user_signing"],
                            keys: {
                                "ed25519:g4ED07Fnqf3GzVWNN1pZ0IFrPQVdqQf+PYoJNH4eE0s":
                                    "g4ED07Fnqf3GzVWNN1pZ0IFrPQVdqQf+PYoJNH4eE0s",
                            },
                            signatures: {
                                "@example:localhost": {
                                    "ed25519:n2lpJGx0LiKnuNE1IucZP3QExrD4SeRP0veBHPe3XUU":
                                        "nKQu8alQKDefNbZz9luYPcNj+Z+ouQSot4fU/A23ELl1xrI06QVBku/SmDx0sIW1ytso0Cqwy1a+3PzCa1XABg",
                                },
                            },
                        },
                    },
                });
                const marked = await m.markRequestAsSent("foo", RequestType.KeysQuery, hypothetical_response);
            }

            {
                // derived from https://github.com/matrix-org/matrix-rust-sdk/blob/7f49618d350fab66b7e1dc4eaf64ec25ceafd658/benchmarks/benches/crypto_bench/keys_claim.json
                const hypothetical_response = JSON.stringify({
                    one_time_keys: {
                        "@example:localhost": {
                            AFGUOBTZWM: {
                                "signed_curve25519:AAAABQ": {
                                    key: "9IGouMnkB6c6HOd4xUsNv4i3Dulb4IS96TzDordzOws",
                                    signatures: {
                                        "@example:localhost": {
                                            "ed25519:AFGUOBTZWM":
                                                "2bvUbbmJegrV0eVP/vcJKuIWC3kud+V8+C0dZtg4dVovOSJdTP/iF36tQn2bh5+rb9xLlSeztXBdhy4c+LiOAg",
                                        },
                                    },
                                },
                            },
                        },
                    },
                    failures: {},
                });
                const marked = await m.markRequestAsSent("bar", RequestType.KeysClaim, hypothetical_response);
            }
        });

        test("can share a room key", async () => {
            const other_users = [new UserId("@example:localhost")];

            const requests = await m.shareRoomKey(room, other_users, new EncryptionSettings());

            expect(requests).toHaveLength(1);
            expect(requests[0]).toBeInstanceOf(ToDeviceRequest);
            expect(requests[0].event_type).toEqual("m.room.encrypted");
            expect(requests[0].txn_id).toBeDefined();
            const content = JSON.parse(requests[0].body);
            expect(Object.keys(content.messages)).toEqual(["@example:localhost"]);
        });

        let encrypted;

        test("can encrypt an event", async () => {
            encrypted = JSON.parse(
                await m.encryptRoomEvent(
                    room,
                    "m.room.message",
                    JSON.stringify({
                        msgtype: "m.text",
                        body: "Hello, World!",
                    }),
                ),
            );

            expect(encrypted.algorithm).toBeDefined();
            expect(encrypted.ciphertext).toBeDefined();
            expect(encrypted.sender_key).toBeDefined();
            expect(encrypted.device_id).toStrictEqual(device.toString());
            expect(encrypted.session_id).toBeDefined();
        });

        test("can decrypt an event", async () => {
            const decrypted = await m.decryptRoomEvent(
                JSON.stringify({
                    type: "m.room.encrypted",
                    event_id: "$xxxxx:example.org",
                    origin_server_ts: Date.now(),
                    sender: user.toString(),
                    content: encrypted,
                    unsigned: {
                        age: 1234,
                    },
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
            expect(decrypted.shieldState(true).color).toStrictEqual(ShieldColor.Red);
            expect(decrypted.shieldState(false).color).toStrictEqual(ShieldColor.Red);
        });
    });

    test("failure to decrypt returns a valid error", async () => {
        const m = await machine();
        const evt = {
            type: "m.room.encrypted",
            event_id: "$xxxxx:example.org",
            origin_server_ts: Date.now(),
            sender: user.toString(),
            content: {
                algorithm: "m.megolm.v1.aes-sha2",
                ciphertext: "blah",
            },
        };
        await expect(() => m.decryptRoomEvent(JSON.stringify(evt), room)).rejects.toThrowError();
    });

    test("can read cross-signing status", async () => {
        const m = await machine();
        const crossSigningStatus = await m.crossSigningStatus();

        expect(crossSigningStatus).toBeInstanceOf(CrossSigningStatus);
        expect(crossSigningStatus.hasMaster).toStrictEqual(false);
        expect(crossSigningStatus.hasSelfSigning).toStrictEqual(false);
        expect(crossSigningStatus.hasUserSigning).toStrictEqual(false);
    });

    test("can sign a message", async () => {
        const m = await machine();
        const signatures = await m.sign("foo");

        expect(signatures.isEmpty()).toStrictEqual(false);
        expect(signatures.count).toStrictEqual(1);

        let base64;

        // `get`
        {
            const signature = signatures.get(user);

            expect(signature.has("ed25519:foobar")).toStrictEqual(true);

            const s = signature.get("ed25519:foobar");

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
            const signature = signatures.getSignature(user, new DeviceKeyId("ed25519:foobar"));
            expect(signature.toBase64()).toStrictEqual(base64);
        }

        // Unknown signatures.
        {
            expect(signatures.get(new UserId("@hello:example.org"))).toBeUndefined();
            expect(signatures.getSignature(user, new DeviceKeyId("world:foobar"))).toBeUndefined();
        }
    });

    test("can get a user identities", async () => {
        const m = await machine();
        let _ = m.bootstrapCrossSigning(true);

        const identity = await m.getIdentity(user);

        expect(identity).toBeInstanceOf(OwnUserIdentity);

        const signatureUploadRequest = await identity.verify();
        expect(signatureUploadRequest).toBeInstanceOf(SignatureUploadRequest);

        const [verificationRequest, outgoingVerificationRequest] = await identity.requestVerification();
        expect(verificationRequest).toBeInstanceOf(VerificationRequest);
        expect(outgoingVerificationRequest).toBeInstanceOf(ToDeviceRequest);

        const isTrusted = await identity.trustsOurOwnDevice();

        expect(isTrusted).toStrictEqual(false);
    });

    describe("can export/import room keys", () => {
        let m;
        let exportedRoomKeys;

        test("can export room keys", async () => {
            m = await machine();
            await m.shareRoomKey(room, [new UserId("@bob:example.org")], new EncryptionSettings());

            exportedRoomKeys = await m.exportRoomKeys((session) => {
                expect(session).toBeInstanceOf(InboundGroupSession);
                expect(session.roomId.toString()).toStrictEqual(room.toString());
                expect(session.sessionId).toBeDefined();
                expect(session.hasBeenImported()).toStrictEqual(false);

                return true;
            });

            const roomKeys = JSON.parse(exportedRoomKeys);
            expect(roomKeys).toHaveLength(1);
            expect(roomKeys[0]).toMatchObject({
                algorithm: expect.any(String),
                room_id: room.toString(),
                sender_key: expect.any(String),
                session_id: expect.any(String),
                session_key: expect.any(String),
                sender_claimed_keys: {
                    ed25519: expect.any(String),
                },
                forwarding_curve25519_key_chain: [],
            });
        });

        let encryptedExportedRoomKeys;
        let encryptionPassphrase = "Hello, Matrix!";

        test("can encrypt the exported room keys", () => {
            encryptedExportedRoomKeys = OlmMachine.encryptExportedRoomKeys(
                exportedRoomKeys,
                encryptionPassphrase,
                100_000,
            );

            expect(encryptedExportedRoomKeys).toMatch(/^-----BEGIN MEGOLM SESSION DATA-----/);
        });

        test("can decrypt the exported room keys", () => {
            const decryptedExportedRoomKeys = OlmMachine.decryptExportedRoomKeys(
                encryptedExportedRoomKeys,
                encryptionPassphrase,
            );

            expect(decryptedExportedRoomKeys).toStrictEqual(exportedRoomKeys);
        });

        test("can import room keys", async () => {
            const progressListener = (progress, total) => {
                expect(progress).toBeLessThan(total);

                // Since it's called only once, let's be crazy.
                expect(progress).toStrictEqual(0n);
                expect(total).toStrictEqual(1n);
            };

            const result = JSON.parse(await m.importRoomKeys(exportedRoomKeys, progressListener));

            expect(result).toMatchObject({
                imported_count: expect.any(Number),
                total_count: expect.any(Number),
                keys: expect.any(Object),
            });
        });
    });

    describe("can do in-room verification", () => {
        let m;
        const user = new UserId("@alice:example.org");
        const device = new DeviceId("JLAFKJWSCS");
        const room = new RoomId("!test:localhost");

        beforeAll(async () => {
            m = await machine(user, device);
        });

        test("can inject devices from someone else", async () => {
            {
                const hypothetical_response = JSON.stringify({
                    device_keys: {
                        "@example:morpheus.localhost": {
                            ATRLDCRXAC: {
                                algorithms: ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
                                device_id: "ATRLDCRXAC",
                                keys: {
                                    "curve25519:ATRLDCRXAC": "cAVT5Es3Z3F5pFD+2w3HT7O9+R3PstzYVkzD51X/FWQ",
                                    "ed25519:ATRLDCRXAC": "V2w/T/x7i7AXiCCtS6JldrpbvRliRoef3CqTUNqMRHA",
                                },
                                signatures: {
                                    "@example:morpheus.localhost": {
                                        "ed25519:ATRLDCRXAC":
                                            "ro2BjO5J6089B/JOANHnFmGrogrC2TIdMlgJbJO00DjOOcGxXfvOezCFIORTwZNHvkHU617YIGl/4keTDIWvBQ",
                                    },
                                },
                                user_id: "@example:morpheus.localhost",
                                unsigned: {
                                    device_display_name: "Element Desktop: Linux",
                                },
                            },
                            EYYGYTCTNC: {
                                algorithms: ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
                                device_id: "EYYGYTCTNC",
                                keys: {
                                    "curve25519:EYYGYTCTNC": "Pqu50fo472wgb6NjKkaUxjuqoAIEAmhln2gw/zSQ7Ek",
                                    "ed25519:EYYGYTCTNC": "Pf/2QPvui8lDty6TCTglVPRVM+irNHYavNNkyv5yFpU",
                                },
                                signatures: {
                                    "@example:morpheus.localhost": {
                                        "ed25519:EYYGYTCTNC":
                                            "pnP5BYLEUUaxDgrvdzCznkjNDbvY1/MFBr1JejdnLiXlcmxRULQpIWZUCO7QTbULsCwMsYQNGn50nfmjBQX3CQ",
                                    },
                                },
                                user_id: "@example:morpheus.localhost",
                                unsigned: {
                                    device_display_name: "WeeChat-Matrix-rs",
                                },
                            },
                            SUMODVLSIU: {
                                algorithms: ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
                                device_id: "SUMODVLSIU",
                                keys: {
                                    "curve25519:SUMODVLSIU": "geQXWGWc++gcUHk0JcFmEVSjyzDOnk2mjVsUQwbNqQU",
                                    "ed25519:SUMODVLSIU": "ccktaQ3g+B18E6FwVhTBYie26OlHbvDUzDEtxOQ4Qcs",
                                },
                                signatures: {
                                    "@example:morpheus.localhost": {
                                        "ed25519:SUMODVLSIU":
                                            "Yn+AOxHRt1GQpY2xT2Jcqqn8jh5+Vw23ctA7NXyDiWPsLPLNTpjGWHMjZdpUqflQvpiKfhODPICoIa7Pu0iSAg",
                                        "ed25519:rUiMNDjIu6gqsrhJPbj3phyIzuEtuQGrLOEa9mCbtTM":
                                            "Cio6k/sq289XNTOvTCWre7Q6zg+A3euzMUe7Uy1T3gPqYFzX+kt7EAxrhbPqx1HyXAEz9zD0D/uw9VEXFCvWBQ",
                                    },
                                },
                                user_id: "@example:morpheus.localhost",
                                unsigned: {
                                    device_display_name: "Element Desktop (Linux)",
                                },
                            },
                        },
                    },
                    failures: {},
                    master_keys: {
                        "@example:morpheus.localhost": {
                            user_id: "@example:morpheus.localhost",
                            usage: ["master"],
                            keys: {
                                "ed25519:ZzU4WCyBfOFitdGmfKCq6F39iQCDk/zhNNTsi+tWH7A":
                                    "ZzU4WCyBfOFitdGmfKCq6F39iQCDk/zhNNTsi+tWH7A",
                            },
                            signatures: {
                                "@example:morpheus.localhost": {
                                    "ed25519:SUMODVLSIU":
                                        "RL6WOuuzB/mZ+edfUFG/KeEcmKh+NaWpM6m2bUYmDnJrtTCYyoU+pgHJuL2/6nynemmONo18JEHBuqtNcMq2AQ",
                                },
                            },
                        },
                    },
                    self_signing_keys: {
                        "@example:morpheus.localhost": {
                            user_id: "@example:morpheus.localhost",
                            usage: ["self_signing"],
                            keys: {
                                "ed25519:rUiMNDjIu6gqsrhJPbj3phyIzuEtuQGrLOEa9mCbtTM":
                                    "rUiMNDjIu6gqsrhJPbj3phyIzuEtuQGrLOEa9mCbtTM",
                            },
                            signatures: {
                                "@example:morpheus.localhost": {
                                    "ed25519:ZzU4WCyBfOFitdGmfKCq6F39iQCDk/zhNNTsi+tWH7A":
                                        "uCBn9rpeg6umY8H97ejN26UMp6QDwNL98869t1DoVGL50J8adLN05OZd8lYk9QzwTr2d56ZTGYSYX8kv28SDDA",
                                },
                            },
                        },
                    },
                    user_signing_keys: {
                        "@example:morpheus.localhost": {
                            user_id: "@example:morpheus.localhost",
                            usage: ["user_signing"],
                            keys: {
                                "ed25519:GLhEKLQ50jnF6IMEPsO2ucpHUNIUEnbBXs5gYbHg4Aw":
                                    "GLhEKLQ50jnF6IMEPsO2ucpHUNIUEnbBXs5gYbHg4Aw",
                            },
                            signatures: {
                                "@example:morpheus.localhost": {
                                    "ed25519:ZzU4WCyBfOFitdGmfKCq6F39iQCDk/zhNNTsi+tWH7A":
                                        "4fIyWlVzuz1pgoegNLZASycORXqKycVS0dNq5vmmwsVEudp1yrPhndnaIJ3fjF8LDHvwzXTvohOid7DiU1j0AA",
                                },
                            },
                        },
                    },
                });
                const marked = await m.markRequestAsSent("foo", RequestType.KeysQuery, hypothetical_response);
            }
        });

        test("can start an in-room SAS verification", async () => {
            let _ = m.bootstrapCrossSigning(true);
            const identity = await m.getIdentity(new UserId("@example:morpheus.localhost"));

            expect(identity).toBeInstanceOf(UserIdentity);
            expect(identity.isVerified()).toStrictEqual(false);

            const eventId = new EventId("$Rqnc-F-dvnEYJTyHq_iKxU2bZ1CI92-kuZq3a5lr5Zg");
            const verificationRequest = await identity.requestVerification(room, eventId);
            expect(verificationRequest).toBeInstanceOf(VerificationRequest);

            await m.receiveVerificationEvent(
                JSON.stringify({
                    sender: "@example:morpheus.localhost",
                    type: "m.key.verification.ready",
                    event_id: "$QguWmaeMt6Hao7Ea6XHDInvr8ndknev79t9a2eBxlz0",
                    origin_server_ts: 1674037263075,
                    content: {
                        "methods": ["m.sas.v1", "m.qr_code.show.v1", "m.reciprocate.v1"],
                        "from_device": "SUMODVLSIU",
                        "m.relates_to": {
                            rel_type: "m.reference",
                            event_id: eventId.toString(),
                        },
                    },
                }),
                room,
            );

            expect(verificationRequest.roomId.toString()).toStrictEqual(room.toString());

            const [_sas, outgoingVerificationRequest] = await verificationRequest.startSas();

            expect(outgoingVerificationRequest).toBeInstanceOf(RoomMessageRequest);
            expect(outgoingVerificationRequest.id).toBeDefined();
            expect(outgoingVerificationRequest.room_id).toStrictEqual(room.toString());
            expect(outgoingVerificationRequest.txn_id).toBeDefined();
            expect(outgoingVerificationRequest.event_type).toStrictEqual("m.key.verification.start");
            expect(outgoingVerificationRequest.body).toBeDefined();

            const body = JSON.parse(outgoingVerificationRequest.body);
            expect(body).toMatchObject({
                "from_device": expect.any(String),
                "method": "m.sas.v1",
                "key_agreement_protocols": [expect.any(String)],
                "hashes": [expect.any(String)],
                "message_authentication_codes": [expect.any(String), expect.any(String)],
                "short_authentication_string": ["decimal", "emoji"],
                "m.relates_to": {
                    rel_type: "m.reference",
                    event_id: eventId.toString(),
                },
            });
        });
    });
});
