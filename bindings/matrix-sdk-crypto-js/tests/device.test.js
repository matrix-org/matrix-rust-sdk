const {
    OlmMachine,
    UserId,
    DeviceId,
    DeviceKeyId,
    RoomId,
    Device,
    LocalTrust,
    UserDevices,
    DeviceKey,
    DeviceKeyName,
    DeviceKeyAlgorithmName,
    Ed25519PublicKey,
    EncryptionAlgorithm,
    Curve25519PublicKey,
    Signatures,
    VerificationMethod,
    VerificationRequest,
    ToDeviceRequest,
    DeviceLists,
    KeysUploadRequest,
    RequestType,
    KeysQueryRequest,
    Sas,
    Emoji,
    SigningKeysUploadRequest,
    SignatureUploadRequest,
    Qr,
    QrCode,
    QrCodeScan,
} = require("../pkg/matrix_sdk_crypto_js");
const { zip, addMachineToMachine } = require("./helper");
const { VerificationRequestPhase } = require("../pkg");

describe("LocalTrust", () => {
    test("has the correct variant values", () => {
        expect(LocalTrust.Verified).toStrictEqual(0);
        expect(LocalTrust.BlackListed).toStrictEqual(1);
        expect(LocalTrust.Ignored).toStrictEqual(2);
        expect(LocalTrust.Unset).toStrictEqual(3);
    });
});

describe("DeviceKeyName", () => {
    test("has the correct variant values", () => {
        expect(DeviceKeyName.Curve25519).toStrictEqual(0);
        expect(DeviceKeyName.Ed25519).toStrictEqual(1);
        expect(DeviceKeyName.Unknown).toStrictEqual(2);
    });
});

describe(OlmMachine.name, () => {
    const user = new UserId("@alice:example.org");
    const device = new DeviceId("foobar");
    const room = new RoomId("!baz:matrix.org");

    function machine(new_user, new_device) {
        return OlmMachine.initialize(new_user || user, new_device || device);
    }

    test("can read user devices", async () => {
        const m = await machine();
        const userDevices = await m.getUserDevices(user);

        expect(userDevices).toBeInstanceOf(UserDevices);
        expect(userDevices.get(device)).toBeInstanceOf(Device);
        expect(userDevices.isAnyVerified()).toStrictEqual(false);
        expect(userDevices.keys().map((device_id) => device_id.toString())).toStrictEqual([device.toString()]);
        expect(userDevices.devices().map((device) => device.deviceId.toString())).toStrictEqual([device.toString()]);
    });

    test("can read a user device", async () => {
        const m = await machine();

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
        // Insert another device into the store
        await m.markRequestAsSent("ID", RequestType.KeysQuery, hypothetical_response);

        const secondDeviceId = new DeviceId("JLAFKJWSCS");
        const dev = await m.getDevice(user, secondDeviceId);

        expect(dev).toBeInstanceOf(Device);
        expect(dev.isVerified()).toStrictEqual(false);
        expect(dev.isCrossSigningTrusted()).toStrictEqual(false);
        expect(dev.isCrossSignedByOwner()).toStrictEqual(false);

        expect(dev.localTrustState).toStrictEqual(LocalTrust.Unset);
        expect(dev.isLocallyTrusted()).toStrictEqual(false);
        expect(await dev.setLocalTrust(LocalTrust.Verified)).toBeNull();
        expect(dev.localTrustState).toStrictEqual(LocalTrust.Verified);
        expect(dev.isLocallyTrusted()).toStrictEqual(true);

        expect(dev.userId.toString()).toStrictEqual(user.toString());
        expect(dev.deviceId.toString()).toStrictEqual(secondDeviceId.toString());
        expect(dev.deviceName).toBeUndefined();
        expect(dev.algorithms).toEqual([
            EncryptionAlgorithm.OlmV1Curve25519AesSha2,
            EncryptionAlgorithm.MegolmV1AesSha2,
        ]);

        const deviceKey = dev.getKey(DeviceKeyAlgorithmName.Ed25519);

        expect(deviceKey).toBeInstanceOf(DeviceKey);
        expect(deviceKey.name).toStrictEqual(DeviceKeyName.Ed25519);
        expect(deviceKey.curve25519).toBeUndefined();
        expect(deviceKey.ed25519).toBeInstanceOf(Ed25519PublicKey);
        expect(deviceKey.unknown).toBeUndefined();
        expect(deviceKey.toBase64()).toMatch(/^[A-Za-z0-9\+/]+$/);

        expect(dev.curve25519Key).toBeInstanceOf(Curve25519PublicKey);
        expect(dev.ed25519Key).toBeInstanceOf(Ed25519PublicKey);

        for (const [deviceKeyId, deviceKey] of dev.keys) {
            expect(deviceKeyId).toBeInstanceOf(DeviceKeyId);
            expect(deviceKey).toBeInstanceOf(DeviceKey);
        }

        expect(dev.signatures).toBeInstanceOf(Signatures);
        expect(dev.isBlacklisted()).toStrictEqual(false);
        expect(dev.isDeleted()).toStrictEqual(false);
    });
});

describe("Key Verification", () => {
    const userId1 = new UserId("@alice:example.org");
    const deviceId1 = new DeviceId("alice_device");

    const userId2 = new UserId("@bob:example.org");
    const deviceId2 = new DeviceId("bob_device");

    function machine(new_user, new_device) {
        return OlmMachine.initialize(new_user || userId1, new_device || deviceId1);
    }

    describe("SAS", () => {
        // First Olm machine.
        let m1;

        // Second Olm machine.
        let m2;

        beforeAll(async () => {
            m1 = await machine(userId1, deviceId1);
            m2 = await machine(userId2, deviceId2);
        });

        // Verification request for `m1`.
        let verificationRequest1;

        // The flow ID.
        let flowId;

        test("can request verification (`m.key.verification.request`)", async () => {
            // Make `m1` and `m2` be aware of each other.
            {
                await addMachineToMachine(m2, m1);
                await addMachineToMachine(m1, m2);
            }

            // Pick the device we want to start the verification with.
            const device2 = await m1.getDevice(userId2, deviceId2);

            expect(device2).toBeInstanceOf(Device);

            let outgoingVerificationRequest;
            // Request a verification from `m1` to `device2`.
            [verificationRequest1, outgoingVerificationRequest] = await device2.requestVerification();

            expect(verificationRequest1).toBeInstanceOf(VerificationRequest);

            expect(verificationRequest1.ownUserId.toString()).toStrictEqual(userId1.toString());
            expect(verificationRequest1.otherUserId.toString()).toStrictEqual(userId2.toString());
            expect(verificationRequest1.otherDeviceId).toBeUndefined();
            expect(verificationRequest1.roomId).toBeUndefined();
            expect(verificationRequest1.cancelInfo).toBeUndefined();
            expect(verificationRequest1.isPassive()).toStrictEqual(false);
            expect(verificationRequest1.isReady()).toStrictEqual(false);
            expect(verificationRequest1.timedOut()).toStrictEqual(false);
            expect(verificationRequest1.theirSupportedMethods).toBeUndefined();
            expect(verificationRequest1.ourSupportedMethods).toEqual(
                expect.arrayContaining([VerificationMethod.SasV1, VerificationMethod.ReciprocateV1]),
            );
            expect(verificationRequest1.flowId).toMatch(/^[a-f0-9]+$/);
            expect(verificationRequest1.isSelfVerification()).toStrictEqual(false);
            expect(verificationRequest1.weStarted()).toStrictEqual(true);
            expect(verificationRequest1.isDone()).toStrictEqual(false);
            expect(verificationRequest1.isCancelled()).toStrictEqual(false);
            expect(verificationRequest1.phase()).toStrictEqual(VerificationRequestPhase.Created);

            expect(outgoingVerificationRequest).toBeInstanceOf(ToDeviceRequest);
            expect(outgoingVerificationRequest.event_type).toStrictEqual("m.key.verification.request");

            const toDeviceEvents = [
                {
                    sender: userId1.toString(),
                    type: outgoingVerificationRequest.event_type,
                    content: JSON.parse(outgoingVerificationRequest.body).messages[userId2.toString()][
                        deviceId2.toString()
                    ],
                },
            ];

            // Let's send the verification request to `m2`.
            await m2.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());

            flowId = verificationRequest1.flowId;
        });

        // Verification request for `m2`.
        let verificationRequest2;

        test("can fetch received request verification", async () => {
            // Oh, a new verification request.
            verificationRequest2 = m2.getVerificationRequest(userId1, flowId);

            expect(verificationRequest2).toBeInstanceOf(VerificationRequest);

            expect(verificationRequest2.ownUserId.toString()).toStrictEqual(userId2.toString());
            expect(verificationRequest2.otherUserId.toString()).toStrictEqual(userId1.toString());
            expect(verificationRequest2.otherDeviceId.toString()).toStrictEqual(deviceId1.toString());
            expect(verificationRequest2.roomId).toBeUndefined();
            expect(verificationRequest2.cancelInfo).toBeUndefined();
            expect(verificationRequest2.isPassive()).toStrictEqual(false);
            expect(verificationRequest2.isReady()).toStrictEqual(false);
            expect(verificationRequest2.timedOut()).toStrictEqual(false);
            expect(verificationRequest2.theirSupportedMethods).toEqual(
                expect.arrayContaining([VerificationMethod.SasV1, VerificationMethod.ReciprocateV1]),
            );
            expect(verificationRequest2.ourSupportedMethods).toBeUndefined();
            expect(verificationRequest2.flowId).toStrictEqual(flowId);
            expect(verificationRequest2.isSelfVerification()).toStrictEqual(false);
            expect(verificationRequest2.weStarted()).toStrictEqual(false);
            expect(verificationRequest2.isDone()).toStrictEqual(false);
            expect(verificationRequest2.isCancelled()).toStrictEqual(false);
            expect(verificationRequest2.phase()).toStrictEqual(VerificationRequestPhase.Requested);

            const verificationRequests = m2.getVerificationRequests(userId1);
            expect(verificationRequests).toHaveLength(1);
            expect(verificationRequests[0].flowId).toStrictEqual(verificationRequest2.flowId); // there are the same
        });

        test("can accept a verification request (`m.key.verification.ready`)", async () => {
            // Accept the verification request.
            let outgoingVerificationRequest = verificationRequest2.accept();

            expect(outgoingVerificationRequest).toBeInstanceOf(ToDeviceRequest);

            // The request verification is ready.
            expect(outgoingVerificationRequest.event_type).toStrictEqual("m.key.verification.ready");

            const toDeviceEvents = [
                {
                    sender: userId2.toString(),
                    type: outgoingVerificationRequest.event_type,
                    content: JSON.parse(outgoingVerificationRequest.body).messages[userId1.toString()][
                        deviceId1.toString()
                    ],
                },
            ];

            // Let's send the verification ready to `m1`.
            await m1.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());
        });

        test("verification requests are synchronized and automatically updated", () => {
            expect(verificationRequest1.isReady()).toStrictEqual(true);
            expect(verificationRequest2.isReady()).toStrictEqual(true);
            expect(verificationRequest1.phase()).toStrictEqual(VerificationRequestPhase.Ready);
            expect(verificationRequest2.phase()).toStrictEqual(VerificationRequestPhase.Ready);

            expect(verificationRequest1.theirSupportedMethods).toEqual(
                expect.arrayContaining([VerificationMethod.SasV1, VerificationMethod.ReciprocateV1]),
            );
            expect(verificationRequest1.ourSupportedMethods).toEqual(
                expect.arrayContaining([VerificationMethod.SasV1, VerificationMethod.ReciprocateV1]),
            );

            expect(verificationRequest2.theirSupportedMethods).toEqual(
                expect.arrayContaining([VerificationMethod.SasV1, VerificationMethod.ReciprocateV1]),
            );
            expect(verificationRequest2.ourSupportedMethods).toEqual(
                expect.arrayContaining([VerificationMethod.SasV1, VerificationMethod.ReciprocateV1]),
            );
        });

        // SAS verification for the second machine.
        let sas2;

        test("can start a SAS verification (`m.key.verification.start`)", async () => {
            // Let's start a SAS verification, from `m2` for example.
            [sas2, outgoingVerificationRequest] = await verificationRequest2.startSas();
            expect(sas2).toBeInstanceOf(Sas);

            expect(sas2.userId.toString()).toStrictEqual(userId2.toString());
            expect(sas2.deviceId.toString()).toStrictEqual(deviceId2.toString());
            expect(sas2.otherUserId.toString()).toStrictEqual(userId1.toString());
            expect(sas2.otherDeviceId.toString()).toStrictEqual(deviceId1.toString());
            expect(sas2.flowId).toStrictEqual(flowId);
            expect(sas2.roomId).toBeUndefined();
            expect(sas2.supportsEmoji()).toStrictEqual(false);
            expect(sas2.startedFromRequest()).toStrictEqual(true);
            expect(sas2.isSelfVerification()).toStrictEqual(false);
            expect(sas2.haveWeConfirmed()).toStrictEqual(false);
            expect(sas2.hasBeenAccepted()).toStrictEqual(false);
            expect(sas2.cancelInfo()).toBeUndefined();
            expect(sas2.weStarted()).toStrictEqual(false);
            expect(sas2.timedOut()).toStrictEqual(false);
            expect(sas2.canBePresented()).toStrictEqual(false);
            expect(sas2.isDone()).toStrictEqual(false);
            expect(sas2.isCancelled()).toStrictEqual(false);
            expect(sas2.emoji()).toBeUndefined();
            expect(sas2.emojiIndex()).toBeUndefined();
            expect(sas2.decimals()).toBeUndefined();
            expect(verificationRequest2.phase()).toStrictEqual(VerificationRequestPhase.Transitioned);

            expect(outgoingVerificationRequest).toBeInstanceOf(ToDeviceRequest);
            expect(outgoingVerificationRequest.event_type).toStrictEqual("m.key.verification.start");

            const toDeviceEvents = [
                {
                    sender: userId2.toString(),
                    type: outgoingVerificationRequest.event_type,
                    content: JSON.parse(outgoingVerificationRequest.body).messages[userId1.toString()][
                        deviceId1.toString()
                    ],
                },
            ];

            // Let's send the SAS start to `m1`.
            await m1.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());
        });

        // SAS verification for the second machine.
        let sas1;

        test("can fetch and accept an ongoing SAS verification (`m.key.verification.accept`)", async () => {
            expect(verificationRequest1.phase()).toStrictEqual(VerificationRequestPhase.Transitioned);

            // Let's fetch the ongoing SAS verification.
            sas1 = await m1.getVerification(userId2, flowId);

            expect(sas1).toBeInstanceOf(Sas);

            expect(sas1.userId.toString()).toStrictEqual(userId1.toString());
            expect(sas1.deviceId.toString()).toStrictEqual(deviceId1.toString());
            expect(sas1.otherUserId.toString()).toStrictEqual(userId2.toString());
            expect(sas1.otherDeviceId.toString()).toStrictEqual(deviceId2.toString());
            expect(sas1.flowId).toStrictEqual(flowId);
            expect(sas1.roomId).toBeUndefined();
            expect(sas1.startedFromRequest()).toStrictEqual(true);
            expect(sas1.isSelfVerification()).toStrictEqual(false);
            expect(sas1.haveWeConfirmed()).toStrictEqual(false);
            expect(sas1.hasBeenAccepted()).toStrictEqual(false);
            expect(sas1.cancelInfo()).toBeUndefined();
            expect(sas1.weStarted()).toStrictEqual(true);
            expect(sas1.timedOut()).toStrictEqual(false);
            expect(sas1.canBePresented()).toStrictEqual(false);
            expect(sas1.isDone()).toStrictEqual(false);
            expect(sas1.isCancelled()).toStrictEqual(false);
            expect(sas1.emoji()).toBeUndefined();
            expect(sas1.emojiIndex()).toBeUndefined();
            expect(sas1.decimals()).toBeUndefined();

            // we should also be able to get the verification via the request
            const sas1Again = await verificationRequest1.getVerification();
            expect(sas1Again).toBeInstanceOf(Sas);
            expect(sas1Again.flowId).toStrictEqual(flowId);

            // Let's accept thet SAS start request.
            let outgoingVerificationRequest = sas1.accept();

            expect(outgoingVerificationRequest).toBeInstanceOf(ToDeviceRequest);
            expect(outgoingVerificationRequest.event_type).toStrictEqual("m.key.verification.accept");

            const toDeviceEvents = [
                {
                    sender: userId1.toString(),
                    type: outgoingVerificationRequest.event_type,
                    content: JSON.parse(outgoingVerificationRequest.body).messages[userId2.toString()][
                        deviceId2.toString()
                    ],
                },
            ];

            // Let's send the SAS accept to `m2`.
            await m2.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());
        });

        test("emojis are supported by both sides", () => {
            expect(sas1.supportsEmoji()).toStrictEqual(true);
            expect(sas2.supportsEmoji()).toStrictEqual(true);
        });

        test("one side sends verification key (`m.key.verification.key`)", async () => {
            // Let's send the verification keys from `m2` to `m1`.
            const outgoingRequests = await m2.outgoingRequests();
            let toDeviceRequest = outgoingRequests.find((request) => request.type == RequestType.ToDevice);

            expect(toDeviceRequest).toBeInstanceOf(ToDeviceRequest);
            expect(toDeviceRequest.event_type).toStrictEqual("m.key.verification.key");

            const toDeviceEvents = [
                {
                    sender: userId2.toString(),
                    type: toDeviceRequest.event_type,
                    content: JSON.parse(toDeviceRequest.body).messages[userId1.toString()][deviceId1.toString()],
                },
            ];

            // Let's send te SAS key to `m1`.
            await m1.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());

            m2.markRequestAsSent(toDeviceRequest.id, toDeviceRequest.type, "{}");
        });

        test("other side sends back verification key (`m.key.verification.key`)", async () => {
            // Let's send the verification keys from `m1` to `m2`.
            const outgoingRequests = await m1.outgoingRequests();
            let toDeviceRequest = outgoingRequests.find((request) => request.type == RequestType.ToDevice);

            expect(toDeviceRequest).toBeInstanceOf(ToDeviceRequest);
            expect(toDeviceRequest.event_type).toStrictEqual("m.key.verification.key");

            const toDeviceEvents = [
                {
                    sender: userId1.toString(),
                    type: toDeviceRequest.event_type,
                    content: JSON.parse(toDeviceRequest.body).messages[userId2.toString()][deviceId2.toString()],
                },
            ];

            // Let's send te SAS key to `m2`.
            await m2.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());

            m1.markRequestAsSent(toDeviceRequest.id, toDeviceRequest.type, "{}");
        });

        test("emojis match from both sides", () => {
            const emojis1 = sas1.emoji();
            const emojiIndexes1 = sas1.emojiIndex();
            const emojis2 = sas2.emoji();
            const emojiIndexes2 = sas2.emojiIndex();

            expect(emojis1).toHaveLength(7);
            expect(emojiIndexes1).toHaveLength(emojis1.length);
            expect(emojis2).toHaveLength(emojis1.length);
            expect(emojiIndexes2).toHaveLength(emojis1.length);

            const isEmoji =
                /(\u00a9|\u00ae|[\u2000-\u3300]|\ud83c[\ud000-\udfff]|\ud83d[\ud000-\udfff]|\ud83e[\ud000-\udfff])/;

            for (const [emoji1, emojiIndex1, emoji2, emojiIndex2] of zip(
                emojis1,
                emojiIndexes1,
                emojis2,
                emojiIndexes2,
            )) {
                expect(emoji1).toBeInstanceOf(Emoji);
                expect(emoji1.symbol).toMatch(isEmoji);
                expect(emoji1.description).toBeTruthy();

                expect(emojiIndex1).toBeGreaterThanOrEqual(0);
                expect(emojiIndex1).toBeLessThanOrEqual(63);

                expect(emoji2).toBeInstanceOf(Emoji);
                expect(emoji2.symbol).toStrictEqual(emoji1.symbol);
                expect(emoji2.description).toStrictEqual(emoji1.description);

                expect(emojiIndex2).toStrictEqual(emojiIndex1);
            }
        });

        test("decimals match from both sides", () => {
            const decimals1 = sas1.decimals();
            const decimals2 = sas2.decimals();

            expect(decimals1).toHaveLength(3);
            expect(decimals2).toHaveLength(decimals1.length);

            const isDecimal = /^[0-9]{4}$/;

            for (const [decimal1, decimal2] of zip(decimals1, decimals2)) {
                expect(decimal1.toString()).toMatch(isDecimal);

                expect(decimal2).toStrictEqual(decimal1);
            }
        });

        test("can confirm keys match (`m.key.verification.mac`)", async () => {
            // `m1` confirms.
            const [outgoingVerificationRequests, signatureUploadRequest] = await sas1.confirm();

            expect(signatureUploadRequest).toBeUndefined();
            expect(outgoingVerificationRequests).toHaveLength(1);

            let outgoingVerificationRequest = outgoingVerificationRequests[0];

            expect(outgoingVerificationRequest).toBeInstanceOf(ToDeviceRequest);
            expect(outgoingVerificationRequest.event_type).toStrictEqual("m.key.verification.mac");

            const toDeviceEvents = [
                {
                    sender: userId1.toString(),
                    type: outgoingVerificationRequest.event_type,
                    content: JSON.parse(outgoingVerificationRequest.body).messages[userId2.toString()][
                        deviceId2.toString()
                    ],
                },
            ];

            // Let's send te SAS confirmation to `m2`.
            await m2.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());
        });

        test("can confirm back keys match (`m.key.verification.done`)", async () => {
            // `m2` confirms.
            const [outgoingVerificationRequests, signatureUploadRequest] = await sas2.confirm();

            expect(signatureUploadRequest).toBeUndefined();
            expect(outgoingVerificationRequests).toHaveLength(2);

            // `.mac`
            {
                let outgoingVerificationRequest = outgoingVerificationRequests[0];

                expect(outgoingVerificationRequest).toBeInstanceOf(ToDeviceRequest);
                expect(outgoingVerificationRequest.event_type).toStrictEqual("m.key.verification.mac");

                const toDeviceEvents = [
                    {
                        sender: userId2.toString(),
                        type: outgoingVerificationRequest.event_type,
                        content: JSON.parse(outgoingVerificationRequest.body).messages[userId1.toString()][
                            deviceId1.toString()
                        ],
                    },
                ];

                // Let's send te SAS confirmation to `m1`.
                await m1.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());
            }

            // `.done`
            {
                let outgoingVerificationRequest = outgoingVerificationRequests[1];

                expect(outgoingVerificationRequest).toBeInstanceOf(ToDeviceRequest);
                expect(outgoingVerificationRequest.event_type).toStrictEqual("m.key.verification.done");

                const toDeviceEvents = [
                    {
                        sender: userId2.toString(),
                        type: outgoingVerificationRequest.event_type,
                        content: JSON.parse(outgoingVerificationRequest.body).messages[userId1.toString()][
                            deviceId1.toString()
                        ],
                    },
                ];

                // Let's send te SAS done to `m1`.
                await m1.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());
            }
        });

        test("can send final done (`m.key.verification.done`)", async () => {
            const outgoingRequests = await m1.outgoingRequests();
            expect(outgoingRequests).toHaveLength(4);

            let toDeviceRequest = outgoingRequests.find((request) => request.type == RequestType.ToDevice);

            expect(toDeviceRequest).toBeInstanceOf(ToDeviceRequest);
            expect(toDeviceRequest.event_type).toStrictEqual("m.key.verification.done");

            const toDeviceEvents = [
                {
                    sender: userId1.toString(),
                    type: toDeviceRequest.event_type,
                    content: JSON.parse(toDeviceRequest.body).messages[userId2.toString()][deviceId2.toString()],
                },
            ];

            // Let's send te SAS key to `m2`.
            await m2.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());

            m1.markRequestAsSent(toDeviceRequest.id, toDeviceRequest.type, "{}");
        });

        test("can see if verification is done", () => {
            expect(verificationRequest1.isDone()).toStrictEqual(true);
            expect(verificationRequest2.isDone()).toStrictEqual(true);

            expect(sas1.isDone()).toStrictEqual(true);
            expect(sas2.isDone()).toStrictEqual(true);

            expect(verificationRequest1.phase()).toStrictEqual(VerificationRequestPhase.Done);
            expect(verificationRequest2.phase()).toStrictEqual(VerificationRequestPhase.Done);
        });
    });

    describe("QR Code", () => {
        if (undefined === Qr) {
            // qrcode supports is not enabled
            console.info("qrcode support is disabled, skip the associated test suite");

            return;
        }

        // First Olm machine.
        let m1;

        // Second Olm machine.
        let m2;

        beforeAll(async () => {
            m1 = await machine(userId1, deviceId1);
            m2 = await machine(userId2, deviceId2);
        });

        // Verification request for `m1`.
        let verificationRequest1;

        // The flow ID.
        let flowId;

        test("can request verification (`m.key.verification.request`)", async () => {
            // Make `m1` and `m2` be aware of each other.
            {
                await addMachineToMachine(m2, m1);
                await addMachineToMachine(m1, m2);
            }

            // Pick the device we want to start the verification with.
            const device2 = await m1.getDevice(userId2, deviceId2);

            expect(device2).toBeInstanceOf(Device);

            let outgoingVerificationRequest;
            // Request a verification from `m1` to `device2`.
            [verificationRequest1, outgoingVerificationRequest] = await device2.requestVerification([
                VerificationMethod.QrCodeScanV1, // by default
                VerificationMethod.QrCodeShowV1, // the one we add
            ]);

            expect(verificationRequest1).toBeInstanceOf(VerificationRequest);

            expect(verificationRequest1.ownUserId.toString()).toStrictEqual(userId1.toString());
            expect(verificationRequest1.otherUserId.toString()).toStrictEqual(userId2.toString());
            expect(verificationRequest1.otherDeviceId).toBeUndefined();
            expect(verificationRequest1.roomId).toBeUndefined();
            expect(verificationRequest1.cancelInfo).toBeUndefined();
            expect(verificationRequest1.isPassive()).toStrictEqual(false);
            expect(verificationRequest1.isReady()).toStrictEqual(false);
            expect(verificationRequest1.timedOut()).toStrictEqual(false);
            expect(verificationRequest1.theirSupportedMethods).toBeUndefined();
            expect(verificationRequest1.ourSupportedMethods).toEqual(
                expect.arrayContaining([VerificationMethod.QrCodeShowV1]),
            );
            expect(verificationRequest1.flowId).toMatch(/^[a-f0-9]+$/);
            expect(verificationRequest1.isSelfVerification()).toStrictEqual(false);
            expect(verificationRequest1.weStarted()).toStrictEqual(true);
            expect(verificationRequest1.isDone()).toStrictEqual(false);
            expect(verificationRequest1.isCancelled()).toStrictEqual(false);

            expect(outgoingVerificationRequest).toBeInstanceOf(ToDeviceRequest);
            expect(outgoingVerificationRequest.event_type).toStrictEqual("m.key.verification.request");

            const toDeviceEvents = [
                {
                    sender: userId1.toString(),
                    type: outgoingVerificationRequest.event_type,
                    content: JSON.parse(outgoingVerificationRequest.body).messages[userId2.toString()][
                        deviceId2.toString()
                    ],
                },
            ];

            // Let's send the verification request to `m2`.
            await m2.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());

            flowId = verificationRequest1.flowId;
        });

        // Verification request for `m2`.
        let verificationRequest2;

        test("can fetch received request verification", async () => {
            // Oh, a new verification request.
            verificationRequest2 = m2.getVerificationRequest(userId1, flowId);

            expect(verificationRequest2).toBeInstanceOf(VerificationRequest);

            expect(verificationRequest2.ownUserId.toString()).toStrictEqual(userId2.toString());
            expect(verificationRequest2.otherUserId.toString()).toStrictEqual(userId1.toString());
            expect(verificationRequest2.otherDeviceId.toString()).toStrictEqual(deviceId1.toString());
            expect(verificationRequest2.roomId).toBeUndefined();
            expect(verificationRequest2.cancelInfo).toBeUndefined();
            expect(verificationRequest2.isPassive()).toStrictEqual(false);
            expect(verificationRequest2.isReady()).toStrictEqual(false);
            expect(verificationRequest2.timedOut()).toStrictEqual(false);
            expect(verificationRequest2.theirSupportedMethods).toEqual(
                expect.arrayContaining([VerificationMethod.QrCodeScanV1, VerificationMethod.QrCodeShowV1]),
            );
            expect(verificationRequest2.ourSupportedMethods).toBeUndefined();
            expect(verificationRequest2.flowId).toStrictEqual(flowId);
            expect(verificationRequest2.isSelfVerification()).toStrictEqual(false);
            expect(verificationRequest2.weStarted()).toStrictEqual(false);
            expect(verificationRequest2.isDone()).toStrictEqual(false);
            expect(verificationRequest2.isCancelled()).toStrictEqual(false);

            const verificationRequests = m2.getVerificationRequests(userId1);
            expect(verificationRequests).toHaveLength(1);
            expect(verificationRequests[0].flowId).toStrictEqual(verificationRequest2.flowId); // there are the same
        });

        test("can accept a verification request with methods (`m.key.verification.ready`)", async () => {
            // Accept the verification request.
            let outgoingVerificationRequest = verificationRequest2.acceptWithMethods([
                VerificationMethod.QrCodeScanV1, // by default
                VerificationMethod.QrCodeShowV1, // the one we add
            ]);

            expect(outgoingVerificationRequest).toBeInstanceOf(ToDeviceRequest);

            // The request verification is ready.
            expect(outgoingVerificationRequest.event_type).toStrictEqual("m.key.verification.ready");

            const toDeviceEvents = [
                {
                    sender: userId2.toString(),
                    type: outgoingVerificationRequest.event_type,
                    content: JSON.parse(outgoingVerificationRequest.body).messages[userId1.toString()][
                        deviceId1.toString()
                    ],
                },
            ];

            // Let's send the verification ready to `m1`.
            await m1.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());
        });

        test("verification requests are synchronized and automatically updated", () => {
            expect(verificationRequest1.isReady()).toStrictEqual(true);
            expect(verificationRequest2.isReady()).toStrictEqual(true);

            expect(verificationRequest1.theirSupportedMethods).toEqual(
                expect.arrayContaining([VerificationMethod.QrCodeScanV1, VerificationMethod.QrCodeShowV1]),
            );
            expect(verificationRequest1.ourSupportedMethods).toEqual(
                expect.arrayContaining([VerificationMethod.QrCodeScanV1, VerificationMethod.QrCodeShowV1]),
            );

            expect(verificationRequest2.theirSupportedMethods).toEqual(
                expect.arrayContaining([VerificationMethod.QrCodeScanV1, VerificationMethod.QrCodeShowV1]),
            );
            expect(verificationRequest2.ourSupportedMethods).toEqual(
                expect.arrayContaining([VerificationMethod.QrCodeScanV1, VerificationMethod.QrCodeShowV1]),
            );
        });

        // QR verification for the second machine.
        let qr2;

        test("can generate a QR code", async () => {
            qr2 = await verificationRequest2.generateQrCode();

            expect(qr2).toBeInstanceOf(Qr);

            expect(qr2.hasBeenScanned()).toStrictEqual(false);
            expect(qr2.hasBeenConfirmed()).toStrictEqual(false);
            expect(qr2.userId.toString()).toStrictEqual(userId2.toString());
            expect(qr2.otherUserId.toString()).toStrictEqual(userId1.toString());
            expect(qr2.otherDeviceId.toString()).toStrictEqual(deviceId1.toString());
            expect(qr2.weStarted()).toStrictEqual(false);
            expect(qr2.cancelInfo()).toBeUndefined();
            expect(qr2.isDone()).toStrictEqual(false);
            expect(qr2.isCancelled()).toStrictEqual(false);
            expect(qr2.isSelfVerification()).toStrictEqual(false);
            expect(qr2.reciprocated()).toStrictEqual(false);
            expect(qr2.flowId).toMatch(/^[a-f0-9]+$/);
            expect(qr2.roomId).toBeUndefined();
        });

        test("can read QR code's bytes", async () => {
            const qrCodeHeader = "MATRIX";
            const qrCodeVersion = "\x02";

            const qrCodeBytes = qr2.toBytes();

            expect(qrCodeBytes).toHaveLength(122);
            expect(Array.from(qrCodeBytes.slice(0, 7))).toEqual(
                [...qrCodeHeader, ...qrCodeVersion].map((char) => char.charCodeAt(0)),
            );
        });

        test("can render QR code", async () => {
            const qrCode = qr2.toQrCode();

            expect(qrCode).toBeInstanceOf(QrCode);

            // Want to get `canvasBuffer` to render the QR code? Install `npm install canvas` and uncomment the following blocks.

            //let canvasBuffer;

            {
                const buffer = qrCode.renderIntoBuffer();

                expect(buffer).toBeInstanceOf(Uint8ClampedArray);
                // 45px ⨉ 45px
                expect(buffer).toHaveLength(45 * 45);
                // 0 for a white pixel, 1 for a black pixel.
                expect(buffer.every((p) => p == 0 || p == 1)).toStrictEqual(true);

                /*
                const { Canvas } = require('canvas');
                const canvas = new Canvas(55, 55);

                const context = canvas.getContext('2d');
                context.fillStyle = 'white';
                context.fillRect(0, 0, canvas.width, canvas.height);

                // New image data, filled with black, transparent pixels.
                const imageData = context.createImageData(45, 45);
                const data = imageData.data;

                const [r, g, b, a] = [0, 1, 2, 3];

                for (
                    let dataNth = 0,
                        bufferNth = 0;
                    dataNth < data.length && bufferNth < buffer.length;
                    dataNth += 4,
                    bufferNth += 1
                ) {
                    data[dataNth + a] = 255;

                    // White pixel
                    if (buffer[bufferNth] == 0) {
                        data[dataNth + r] = 255;
                        data[dataNth + g] = 255;
                        data[dataNth + b] = 255;
                    }
                }

                context.putImageData(imageData, 5, 5);
                canvasBuffer = canvas.toBuffer('image/png');
                */
            }

            // Want to see the QR code? Uncomment the following block.
            /*
            {
                const fs = require('fs/promises');
                const path = require('path');
                const os = require('os');

                const tempDirectory = await fs.mkdtemp(path.join(os.tmpdir(), 'matrix-sdk-crypto--'));
                const qrCodeFile = path.join(tempDirectory, 'qrcode.png');

                console.log(`View the QR code at \`${qrCodeFile}\`.`);

                expect(await fs.writeFile(qrCodeFile, canvasBuffer)).toBeUndefined();
            }
            */
        });

        let qr1;

        test("can scan a QR code from bytes", async () => {
            const scan = QrCodeScan.fromBytes(qr2.toBytes());

            expect(scan).toBeInstanceOf(QrCodeScan);

            qr1 = await verificationRequest1.scanQrCode(scan);

            expect(qr1).toBeInstanceOf(Qr);

            expect(qr1.hasBeenScanned()).toStrictEqual(false);
            expect(qr1.hasBeenConfirmed()).toStrictEqual(false);
            expect(qr1.userId.toString()).toStrictEqual(userId1.toString());
            expect(qr1.otherUserId.toString()).toStrictEqual(userId2.toString());
            expect(qr1.otherDeviceId.toString()).toStrictEqual(deviceId2.toString());
            expect(qr1.weStarted()).toStrictEqual(true);
            expect(qr1.cancelInfo()).toBeUndefined();
            expect(qr1.isDone()).toStrictEqual(false);
            expect(qr1.isCancelled()).toStrictEqual(false);
            expect(qr1.isSelfVerification()).toStrictEqual(false);
            expect(qr1.reciprocated()).toStrictEqual(true);
            expect(qr1.flowId).toMatch(/^[a-f0-9]+$/);
            expect(qr1.roomId).toBeUndefined();
        });

        test("can start a QR verification/reciprocate (`m.key.verification.start`)", async () => {
            let outgoingVerificationRequest = qr1.reciprocate();

            expect(outgoingVerificationRequest).toBeInstanceOf(ToDeviceRequest);
            expect(outgoingVerificationRequest.event_type).toStrictEqual("m.key.verification.start");

            const toDeviceEvents = [
                {
                    sender: userId1.toString(),
                    type: outgoingVerificationRequest.event_type,
                    content: JSON.parse(outgoingVerificationRequest.body).messages[userId2.toString()][
                        deviceId2.toString()
                    ],
                },
            ];

            // Let's send the verification request to `m2`.
            await m2.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());
        });

        test("can confirm QR code has been scanned", () => {
            expect(qr2.hasBeenScanned()).toStrictEqual(true);
        });

        test("can confirm scanning (`m.key.verification.done`)", async () => {
            let outgoingVerificationRequest = qr2.confirmScanning();

            expect(outgoingVerificationRequest).toBeInstanceOf(ToDeviceRequest);
            expect(outgoingVerificationRequest.event_type).toStrictEqual("m.key.verification.done");

            const toDeviceEvents = [
                {
                    sender: userId2.toString(),
                    type: outgoingVerificationRequest.event_type,
                    content: JSON.parse(outgoingVerificationRequest.body).messages[userId1.toString()][
                        deviceId1.toString()
                    ],
                },
            ];

            // Let's send the verification request to `m2`.
            await m2.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());
        });

        test("can confirm QR code has been confirmed", () => {
            expect(qr2.hasBeenConfirmed()).toStrictEqual(true);
        });
    });
});

describe("VerificationMethod", () => {
    test("has the correct variant values", () => {
        expect(VerificationMethod.SasV1).toStrictEqual(0);
        expect(VerificationMethod.QrCodeScanV1).toStrictEqual(1);
        expect(VerificationMethod.QrCodeShowV1).toStrictEqual(2);
        expect(VerificationMethod.ReciprocateV1).toStrictEqual(3);
    });
});
