const {
    EncryptionAlgorithm,
    EncryptionSettings,
    HistoryVisibility,
    VerificationState,
} = require("../pkg/matrix_sdk_crypto_js");

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
