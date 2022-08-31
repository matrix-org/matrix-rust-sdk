const { OlmMachine, UserId, DeviceId, DeviceKeyId, RoomId, DeviceKeyAlgorithName, Device, LocalTrust, UserDevices, DeviceKey, DeviceKeyName, DeviceKeyAlgorithmName, Ed25519PublicKey, Curve25519PublicKey, Signatures, VerificationRequest, ToDeviceRequest } = require('../pkg/matrix_sdk_crypto_js');

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
    const user = new UserId('@alice:example.org');
    const device = new DeviceId('foobar');
    const room = new RoomId('!baz:matrix.org');

    function machine(new_user, new_device) {
        return new OlmMachine(new_user || user, new_device || device);
    }

    test('can request verification', async () => {
        const m = await machine();
        const dev = await m.getDevice(user, device);

        const [verificationRequest, outgoingVerificationRequest] = await dev.requestVerification();

        expect(verificationRequest).toBeInstanceOf(VerificationRequest);
        expect(outgoingVerificationRequest).toBeInstanceOf(ToDeviceRequest);
    });
});
