const {
    DeviceId,
    DeviceKeyAlgorithm,
    DeviceKeyAlgorithmName,
    DeviceKeyId,
    EventId,
    RoomId,
    ServerName,
    UserId,
} = require("../pkg/matrix_sdk_crypto_js");

describe(UserId.name, () => {
    test("cannot be invalid", () => {
        expect(() => {
            new UserId("@foobar");
        }).toThrow();
    });

    const user = new UserId("@foo:bar.org");

    test("localpart is present", () => {
        expect(user.localpart).toStrictEqual("foo");
    });

    test("server name is present", () => {
        expect(user.serverName).toBeInstanceOf(ServerName);
    });

    test("user ID is not historical", () => {
        expect(user.isHistorical()).toStrictEqual(false);
    });

    test("can read the user ID as a string", () => {
        expect(user.toString()).toStrictEqual("@foo:bar.org");
    });
});

describe(DeviceId.name, () => {
    const device = new DeviceId("foo");

    test("can read the device ID as a string", () => {
        expect(device.toString()).toStrictEqual("foo");
    });
});

describe(DeviceKeyId.name, () => {
    for (const deviceKey of [
        {
            name: "ed25519",
            id: "ed25519:foobar",
            algorithmName: DeviceKeyAlgorithmName.Ed25519,
            algorithm: "ed25519",
            deviceId: "foobar",
        },

        {
            name: "curve25519",
            id: "curve25519:foobar",
            algorithmName: DeviceKeyAlgorithmName.Curve25519,
            algorithm: "curve25519",
            deviceId: "foobar",
        },

        {
            name: "signed curve25519",
            id: "signed_curve25519:foobar",
            algorithmName: DeviceKeyAlgorithmName.SignedCurve25519,
            algorithm: "signed_curve25519",
            deviceId: "foobar",
        },

        {
            name: "unknown",
            id: "hello:foobar",
            algorithmName: DeviceKeyAlgorithmName.Unknown,
            algorithm: "hello",
            deviceId: "foobar",
        },
    ]) {
        test(`${deviceKey.name} algorithm`, () => {
            const dk = new DeviceKeyId(deviceKey.id);

            expect(dk.algorithm.name).toStrictEqual(deviceKey.algorithmName);
            expect(dk.algorithm.toString()).toStrictEqual(deviceKey.algorithm);
            expect(dk.deviceId.toString()).toStrictEqual(deviceKey.deviceId);
            expect(dk.toString()).toStrictEqual(deviceKey.id);
        });
    }
});

describe("DeviceKeyAlgorithmName", () => {
    test("has the correct variants", () => {
        expect(DeviceKeyAlgorithmName.Ed25519).toStrictEqual(0);
        expect(DeviceKeyAlgorithmName.Curve25519).toStrictEqual(1);
        expect(DeviceKeyAlgorithmName.SignedCurve25519).toStrictEqual(2);
        expect(DeviceKeyAlgorithmName.Unknown).toStrictEqual(3);
    });
});

describe(RoomId.name, () => {
    test("cannot be invalid", () => {
        expect(() => {
            new RoomId("!foo");
        }).toThrow();
    });

    const room = new RoomId("!foo:bar.org");

    test("localpart is present", () => {
        expect(room.localpart).toStrictEqual("foo");
    });

    test("server name is present", () => {
        expect(room.serverName).toBeInstanceOf(ServerName);
    });

    test("can read the room ID as string", () => {
        expect(room.toString()).toStrictEqual("!foo:bar.org");
    });
});

describe(ServerName.name, () => {
    test("cannot be invalid", () => {
        expect(() => {
            new ServerName("@foobar");
        }).toThrow();
    });

    test("host is present", () => {
        expect(new ServerName("foo.org").host).toStrictEqual("foo.org");
    });

    test("port can be optional", () => {
        expect(new ServerName("foo.org").port).toStrictEqual(undefined);
        expect(new ServerName("foo.org:1234").port).toStrictEqual(1234);
    });

    test("server is not an IP literal", () => {
        expect(new ServerName("foo.org").isIpLiteral()).toStrictEqual(false);
    });
});

describe(EventId.name, () => {
    test("cannot be invalid", () => {
        expect(() => {
            new EventId("%foo");
        }).toThrow();
    });

    describe("Versions 1 & 2", () => {
        const room = new EventId("$h29iv0s8:foo.org");

        test("localpart is present", () => {
            expect(room.localpart).toStrictEqual("h29iv0s8");
        });

        test("server name is present", () => {
            expect(room.serverName).toBeInstanceOf(ServerName);
        });

        test("can read the room ID as string", () => {
            expect(room.toString()).toStrictEqual("$h29iv0s8:foo.org");
        });
    });

    describe("Version 3", () => {
        const room = new EventId("$acR1l0raoZnm60CBwAVgqbZqoO/mYU81xysh1u7XcJk");

        test("localpart is present", () => {
            expect(room.localpart).toStrictEqual("acR1l0raoZnm60CBwAVgqbZqoO/mYU81xysh1u7XcJk");
        });

        test("server name is present", () => {
            expect(room.serverName).toBeUndefined();
        });

        test("can read the room ID as string", () => {
            expect(room.toString()).toStrictEqual("$acR1l0raoZnm60CBwAVgqbZqoO/mYU81xysh1u7XcJk");
        });
    });

    describe("Version 4", () => {
        const room = new EventId("$Rqnc-F-dvnEYJTyHq_iKxU2bZ1CI92-kuZq3a5lr5Zg");

        test("localpart is present", () => {
            expect(room.localpart).toStrictEqual("Rqnc-F-dvnEYJTyHq_iKxU2bZ1CI92-kuZq3a5lr5Zg");
        });

        test("server name is present", () => {
            expect(room.serverName).toBeUndefined();
        });

        test("can read the room ID as string", () => {
            expect(room.toString()).toStrictEqual("$Rqnc-F-dvnEYJTyHq_iKxU2bZ1CI92-kuZq3a5lr5Zg");
        });
    });
});
