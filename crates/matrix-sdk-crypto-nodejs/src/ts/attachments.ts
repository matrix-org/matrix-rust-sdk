// Copyright 2021 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

import napi from "./napi";

export interface EncryptedFile {
    v: "v2";
    web_key: {
        kty: "oct";
        key_ops: ["encrypt", "decrypt", string];
        alg: "A256CTR";
        k: string;
        ext: true;
    };
    iv: string;
    hashes: Record<string, string>;
}

export function encryptFile(buf: Buffer): {data: Buffer, file: EncryptedFile} {
    const res = JSON.parse(napi.encryptFile(JSON.stringify([...buf])));
    return {
        data: Buffer.from([...res.data]),
        file: res.info,
    };
}

export function decryptFile(buf: Buffer, info: EncryptedFile): Buffer {
    const res = JSON.parse(napi.decryptFile(JSON.stringify([
        [...buf],
        info,
    ])));
    return Buffer.from([...res.data]);
}
