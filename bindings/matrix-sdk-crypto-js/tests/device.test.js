const { OlmMachine, UserId, DeviceId, DeviceKeyId, RoomId, DeviceKeyAlgorithName, Device, LocalTrust, UserDevices, DeviceKey, DeviceKeyName, DeviceKeyAlgorithmName, Ed25519PublicKey, Curve25519PublicKey, Signatures, VerificationMethod, VerificationRequest, ToDeviceRequest, DeviceLists, KeysUploadRequest, RequestType, KeysQueryRequest, Sas } = require('../pkg/matrix_sdk_crypto_js');
const { addMachineToMachine } = require('./helper');

describe('LocalTrust', () => {
    test('has the correct variant values', () => {
        expect(LocalTrust.Verified).toStrictEqual(0);
        expect(LocalTrust.BlackListed).toStrictEqual(1);
        expect(LocalTrust.Ignored).toStrictEqual(2);
        expect(LocalTrust.Unset).toStrictEqual(3);
    });
});

describe('DeviceKeyName', () => {
    test('has the correct variant values', () => {
        expect(DeviceKeyName.Curve25519).toStrictEqual(0);
        expect(DeviceKeyName.Ed25519).toStrictEqual(1);
        expect(DeviceKeyName.Unknown).toStrictEqual(2);
    });
});

describe(OlmMachine.name, () => {
    const user = new UserId('@alice:example.org');
    const device = new DeviceId('foobar');
    const room = new RoomId('!baz:matrix.org');

    function machine(new_user, new_device) {
        return new OlmMachine(new_user || user, new_device || device);
    }

    test('can read user devices', async () => {
        const m = await machine();
        const userDevices = await m.getUserDevices(user);

        expect(userDevices).toBeInstanceOf(UserDevices);
        expect(userDevices.get(device)).toBeInstanceOf(Device);
        expect(userDevices.isAnyVerified()).toStrictEqual(false);
        expect(userDevices.keys().map(device_id => device_id.toString())).toStrictEqual([device.toString()]);
        expect(userDevices.devices().map(device => device.deviceId.toString())).toStrictEqual([device.toString()]);
    });

    test('can read a user device', async () => {
        const m = await machine();
        const dev = await m.getDevice(user, device);

        expect(dev).toBeInstanceOf(Device);
        expect(dev.isVerified()).toStrictEqual(false);
        expect(dev.isCrossSigningTrusted()).toStrictEqual(false);

        expect(dev.localTrustState).toStrictEqual(LocalTrust.Unset);
        expect(dev.isLocallyTrusted()).toStrictEqual(false);
        expect(await dev.setLocalTrust(LocalTrust.Verified)).toBeNull();
        expect(dev.localTrustState).toStrictEqual(LocalTrust.Verified);
        expect(dev.isLocallyTrusted()).toStrictEqual(true);

        expect(dev.userId.toString()).toStrictEqual(user.toString());
        expect(dev.deviceId.toString()).toStrictEqual(device.toString());
        expect(dev.deviceName).toBeUndefined();

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

describe(Device.name, () => {
    const userId1 = new UserId('@alice:example.org');
    const deviceId1 = new DeviceId('alice_device');

    const userId2 = new UserId('@bob:example.org');
    const deviceId2 = new DeviceId('bob_device');

    function machine(new_user, new_device) {
        return new OlmMachine(new_user || userId1, new_device || deviceId1);
    }

    test('can request verification', async () => {
        // First Olm machine.
        const m1 = await machine(userId1, deviceId1);
        // Second Olm machine.
        const m2 = await machine(userId2, deviceId2);

        // Make `m1` and `m2` be aware of each other.
        {
            await addMachineToMachine(m2, m1);
            await addMachineToMachine(m1, m2);
        }

        // Pick the device we want to start the verification with.
        const device2 = await m1.getDevice(userId2, deviceId2);

        expect(device2).toBeInstanceOf(Device);

        // Request a verification from `m1` to `device2`.
        let [verificationRequest1, outgoingVerificationRequest] = await device2.requestVerification();

        {
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
            expect(verificationRequest1.ourSupportedMethods).toStrictEqual([VerificationMethod.SasV1, VerificationMethod.ReciprocateV1]);
            expect(verificationRequest1.flowId).toMatch(/^[a-f0-9]+$/);
            expect(verificationRequest1.isSelfVerification()).toStrictEqual(false);
            expect(verificationRequest1.weStarted()).toStrictEqual(true);
            expect(verificationRequest1.isDone()).toStrictEqual(false);
            expect(verificationRequest1.isCancelled()).toStrictEqual(false);

            expect(outgoingVerificationRequest).toBeInstanceOf(ToDeviceRequest);

            outgoingVerificationRequest = JSON.parse(outgoingVerificationRequest.body);
            expect(outgoingVerificationRequest.event_type).toStrictEqual('m.key.verification.request');
        }

        let flowId;

        // Fetch the verification from `m2`.
        let verificationRequest2;

        {
            const outgoingContent = outgoingVerificationRequest.messages[userId2.toString()][deviceId2.toString()];

            // Let's pretend the message is coming from a server.
            const toDeviceEvents = {
                events: [{
                    sender: userId1.toString(),
                    type: outgoingVerificationRequest.event_type,
                    content: outgoingContent,
                }]
            };

            // Let's send the verification request to `m2`.
            await m2.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());

            // Oh, a new verification request.
            flowId = outgoingContent.transaction_id;
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
            expect(verificationRequest2.theirSupportedMethods).toStrictEqual([VerificationMethod.SasV1, VerificationMethod.ReciprocateV1]);
            expect(verificationRequest2.ourSupportedMethods).toBeUndefined();
            expect(verificationRequest2.flowId).toMatch(/^[a-f0-9]+$/);
            expect(verificationRequest2.isSelfVerification()).toStrictEqual(false);
            expect(verificationRequest2.weStarted()).toStrictEqual(false);
            expect(verificationRequest2.isDone()).toStrictEqual(false);
            expect(verificationRequest2.isCancelled()).toStrictEqual(false);

            const verificationRequests = m2.getVerificationRequests(userId1);
            expect(verificationRequests).toHaveLength(1);
            expect(verificationRequests[0].flowId).toStrictEqual(verificationRequest2.flowId); // there are the same
        }

        // The request verification is ready.
        {
            let outgoingVerificationRequest = verificationRequest2.accept();

            expect(outgoingVerificationRequest).toBeInstanceOf(ToDeviceRequest);

            outgoingVerificationRequest = JSON.parse(outgoingVerificationRequest.body);
            expect(outgoingVerificationRequest.event_type).toStrictEqual('m.key.verification.ready');

            // Let's pretend the message is coming from a server.
            const toDeviceEvents = {
                events: [{
                    sender: userId2.toString(),
                    type: outgoingVerificationRequest.event_type,
                    content: outgoingVerificationRequest.messages[userId1.toString()][deviceId1.toString()],
                }]
            };

            // Let's send the verification ready to `m1`.
            await m1.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());
        }

        // Verification is ready on both side.
        {
            expect(verificationRequest1.isReady()).toStrictEqual(true);
            expect(verificationRequest2.isReady()).toStrictEqual(true);

            expect(verificationRequest1.theirSupportedMethods).toStrictEqual([VerificationMethod.SasV1, VerificationMethod.ReciprocateV1]);
            expect(verificationRequest1.ourSupportedMethods).toStrictEqual([VerificationMethod.SasV1, VerificationMethod.ReciprocateV1]);

            expect(verificationRequest2.theirSupportedMethods).toStrictEqual([VerificationMethod.SasV1, VerificationMethod.ReciprocateV1]);
            expect(verificationRequest2.ourSupportedMethods).toStrictEqual([VerificationMethod.SasV1, VerificationMethod.ReciprocateV1]);
        }

        // Let's start a SAS verification, from `m2` for example.
        let sas2;

        {
            let [sas, outgoingVerificationRequest] = await verificationRequest2.startSas();
            sas2 = sas;
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

            expect(outgoingVerificationRequest).toBeInstanceOf(ToDeviceRequest);

            outgoingVerificationRequest = JSON.parse(outgoingVerificationRequest.body);
            expect(outgoingVerificationRequest.event_type).toStrictEqual('m.key.verification.start');

            const toDeviceEvents = {
                events: [{
                    sender: userId2.toString(),
                    type: outgoingVerificationRequest.event_type,
                    content: outgoingVerificationRequest.messages[userId1.toString()][deviceId1.toString()],
                }]
            };

            // Let's send the SAS start to `m1`.
            await m1.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());
        }

        // Let's accept the SAS start request.
        let sas1;

        {
            const sas = await m1.getVerification(userId2, flowId);
            sas1 = sas;

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


            let outgoingVerificationRequest = sas1.accept();
            expect(outgoingVerificationRequest).toBeInstanceOf(ToDeviceRequest);

            outgoingVerificationRequest = JSON.parse(outgoingVerificationRequest.body);
            expect(outgoingVerificationRequest.event_type).toStrictEqual('m.key.verification.accept');

            const toDeviceEvents = {
                events: [{
                    sender: userId1.toString(),
                    type: outgoingVerificationRequest.event_type,
                    content: outgoingVerificationRequest.messages[userId2.toString()][deviceId2.toString()],
                }]
            };

            // Let's send the SAS accept to `m2`.
            await m2.receiveSyncChanges(JSON.stringify(toDeviceEvents), new DeviceLists(), new Map(), new Set());
        }

        // Let's see if SAS's state on both side.
        {
            expect(sas1.supportsEmoji()).toStrictEqual(true);
            expect(sas2.supportsEmoji()).toStrictEqual(true);
        }

        // Let's send the verification key from `m2` to `m1`.
        {

        }
    });
});

describe('VerificationMethod', () => {
    test('has the correct variant values', () => {
        expect(VerificationMethod.SasV1).toStrictEqual(0);
        expect(VerificationMethod.QrCodeScanV1).toStrictEqual(1);
        expect(VerificationMethod.QrCodeShowV1).toStrictEqual(2);
        expect(VerificationMethod.ReciprocateV1).toStrictEqual(3);
    });
});
