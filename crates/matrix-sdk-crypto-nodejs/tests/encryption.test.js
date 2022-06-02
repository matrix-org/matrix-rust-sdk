const { EncryptionAlgorithm, EncryptionSettings, HistoryVisibility } = require('../');

describe('EncryptionAlgorithm', () => {
    test('has the correct variant values', () => {
        expect(EncryptionAlgorithm.OlmV1Curve25519AesSha2).toStrictEqual(0);
        expect(EncryptionAlgorithm.MegolmV1AesSha2).toStrictEqual(1);
    });
});

describe('EncryptionSettings', () => {
    test('can be instantiated with defalut values', () => {
        const es = new EncryptionSettings();

        expect(es.algorithm).toStrictEqual(EncryptionAlgorithm.MegolmV1AesSha2);
        expect(es.rotationPeriod).toStrictEqual(604800000000n);
        expect(es.rotationPeriodMessages).toStrictEqual(100n);
        expect(es.historyVisibility).toStrictEqual(HistoryVisibility.Shared);
    });
});
