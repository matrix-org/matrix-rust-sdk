const { EncryptionAlgorithm, EncryptionSettings, HistoryVisibility, VerificationState } = require("../");

describe("EncryptionAlgorithm", () => {
    test("has the correct variant values", () => {
        expect(EncryptionAlgorithm.OlmV1Curve25519AesSha2).toStrictEqual(0);
        expect(EncryptionAlgorithm.MegolmV1AesSha2).toStrictEqual(1);
    });
});

describe(EncryptionSettings.name, () => {
    test("can be instantiated with default values", () => {
        const es = new EncryptionSettings();

        expect(es.algorithm).toStrictEqual(EncryptionAlgorithm.MegolmV1AesSha2);
        expect(es.rotationPeriod).toStrictEqual(604800000000n);
        expect(es.rotationPeriodMessages).toStrictEqual(100n);
        expect(es.historyVisibility).toStrictEqual(HistoryVisibility.Shared);
    });

    test("checks the history visibility values", () => {
        const es = new EncryptionSettings();

        es.historyVisibility = HistoryVisibility.Invited;

        expect(es.historyVisibility).toStrictEqual(HistoryVisibility.Invited);
        expect(() => {
            es.historyVisibility = 42;
        }).toThrow();
    });
});

describe('VerificationState', () => {
    test('has the correct variant values', () => {
        expect(VerificationState.Verified).toStrictEqual(0);
        expect(VerificationState.SignedDeviceOfUnverifiedUser).toStrictEqual(1);
        expect(VerificationState.UnSignedDeviceOfVerifiedUser).toStrictEqual(2);
        expect(VerificationState.UnSignedDeviceOfUnverifiedUser).toStrictEqual(3);
        expect(VerificationState.UnknownDevice).toStrictEqual(4);
        expect(VerificationState.UnsafeSource).toStrictEqual(5);
    });
});
