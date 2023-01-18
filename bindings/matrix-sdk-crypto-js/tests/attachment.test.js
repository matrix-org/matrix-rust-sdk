const { Attachment, EncryptedAttachment } = require('../pkg/matrix_sdk_crypto_js');

describe(Attachment.name, () => {
    const originalData = 'hello';
    const textEncoder = new TextEncoder();
    const textDecoder = new TextDecoder();

    let encryptedAttachment;

    test('can encrypt data', () => {
        encryptedAttachment = Attachment.encrypt(textEncoder.encode(originalData));

        const mediaEncryptionInfo = JSON.parse(encryptedAttachment.mediaEncryptionInfo);

        expect(mediaEncryptionInfo).toMatchObject({
            v: 'v2',
            key: {
                kty: expect.any(String),
                key_ops: expect.arrayContaining(['encrypt', 'decrypt']),
                alg: expect.any(String),
                k: expect.any(String),
                ext: expect.any(Boolean),
            },
            iv: expect.stringMatching(/^[A-Za-z0-9\+/]+$/),
            hashes: {
                sha256: expect.stringMatching(/^[A-Za-z0-9\+/]+$/)
            }
        });

        const encryptedData = encryptedAttachment.encryptedData;
        expect(encryptedData.every((i) => { i != 0 })).toStrictEqual(false);
    });

    test('can decrypt data', () => {
        expect(encryptedAttachment.hasMediaEncryptionInfoBeenConsumed).toStrictEqual(false);

        const decryptedAttachment = Attachment.decrypt(encryptedAttachment);

        expect(textDecoder.decode(decryptedAttachment)).toStrictEqual(originalData);
        expect(encryptedAttachment.hasMediaEncryptionInfoBeenConsumed).toStrictEqual(true);
    });

    test('can only decrypt once', () => {
        expect(encryptedAttachment.hasMediaEncryptionInfoBeenConsumed).toStrictEqual(true);

        expect(() => { textDecoder.decode(decryptedAttachment) }).toThrow()
    });
});

describe(EncryptedAttachment.name, () => {
    const originalData = 'hello';
    const textDecoder = new TextDecoder();

    test('can be created manually', () => {
        const encryptedAttachment = new EncryptedAttachment(
            new Uint8Array([24, 150, 67, 37, 144]),
            JSON.stringify({
                v: 'v2',
                key: {
                    kty: 'oct',
                    key_ops: [ 'encrypt', 'decrypt' ],
                    alg: 'A256CTR',
                    k: 'QbNXUjuukFyEJ8cQZjJuzN6mMokg0HJIjx0wVMLf5BM',
                    ext: true
                },
                iv: 'xk2AcWkomiYAAAAAAAAAAA',
                hashes: {
                    sha256: 'JsRbDXgOja4xvDiF3DwBuLHdxUzIrVYIuj7W/t3aEok'
                }
            })
        );

        expect(encryptedAttachment.hasMediaEncryptionInfoBeenConsumed).toStrictEqual(false);
        expect(textDecoder.decode(Attachment.decrypt(encryptedAttachment))).toStrictEqual(originalData);
        expect(encryptedAttachment.hasMediaEncryptionInfoBeenConsumed).toStrictEqual(true);
    });
});
