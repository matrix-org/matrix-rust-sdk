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

// @ts-ignore
import {Device, DeviceLists} from "./napi-module";
import {toArray} from "./utils";

export type Optional<T> = T | null | undefined;

// Manually imported type
enum RequestKind {
    KeysUpload = 1,
    KeysQuery = 2,
    ToDevice = 3,
    SignatureUpload = 4,
    RoomMessage = 5,
    KeysClaim = 6,
    KeysBackup = 7
}

export interface MatrixEvent {
    event_id: string;
    room_id: string;
    type: string;
    sender: string;
    content: Record<string, any>;
    origin_server_ts: number;
    unsigned: Record<string, any>;
}

export interface ToDeviceEvent {
    type: string;
    sender: string;
    content: Record<string, any>;
}

export interface DecryptedMatrixEvent {
    clearEvent: MatrixEvent;
    senderCurve25519Key: string;
    claimedEd25519Key?: Optional<string>;
    forwardingCurve25519Chain: string[];
}

export interface GenericKeys {
    [keyId:string]: string;
}

export interface Signatures {
    [entity: string]: {
        [keyId: string]: string;
    };
}

export interface DeviceKeys {
    algorithms: string[];
    device_id: string;
    keys: GenericKeys;
    signatures: Signatures;
    user_id: string;
}

export interface FallbackKey {
    key: string;
    fallback: true;
    signatures: Signatures;
}

export interface OTKCounts {
    one_time_key_counts: {
        [algorithm: string]: number;
    };
    fallback_keys?: {
        [algorithm: string]: FallbackKey;
    };
    ["org.matrix.msc2732.fallback_keys"]?: {
        [algorithm: string]: FallbackKey;
    };
}

export interface KeyClaim {
    [userId: string]: {
        [deviceId: string]: string;
    };
}

export interface KeyClaimResponse {
    one_time_keys: {
        [userId: string]: {
            [deviceId: string]: {
                [algorithm: string]: string | {
                    key: string;
                    signatures: Signatures;
                };
            };
        };
    };
    failures?: {
        [serverName: string]: Record<string, any>;
    };
}

export interface KeyQueryResults {
    device_keys: {
        [userId: string]: {
            [deviceId: string]: {
                algorithms: string[];
                device_id: string;
                keys: {
                    [keyId: string]: string;
                };
                signatures: Signatures;
                unsigned?: Record<string, any>;
                user_id: string;
            };
        };
    };
    master_keys?: {
        [userId: string]: {
            keys: {
                [keyId: string]: string;
            };
            usage: string[];
            user_id: string;
        };
    };
    self_signing_keys?: {
        [user_id: string]: {
            keys: {
                [keyId: string]: string;
            };
            signatures: Signatures;
            usage: string[];
            user_id: string;
        };
    };
    user_signing_keys?: {
        [user_id: string]: {
            keys: {
                [keyId: string]: string;
            };
            signatures: Signatures;
            usage: string[];
            user_id: string;
        };
    };
    failures?: {
        [serverName: string]: Record<string, any>;
    };
}

export interface ToDeviceMessages {
    [userId: string]: {
        [deviceId: string]: any;
    };
}

export interface OlmEngine {
    uploadOneTimeKeys(body: {device_keys?: DeviceKeys, one_time_keys?: GenericKeys}): Promise<OTKCounts>;
    queryOneTimeKeys(userIds: string[]): Promise<KeyQueryResults>;
    claimOneTimeKeys(claim: KeyClaim): Promise<KeyClaimResponse>;
    sendToDevices(eventType: string, messages: ToDeviceMessages): Promise<void>;
    getEffectiveJoinedUsersInRoom(roomId: string): Promise<string[]>;
}

// Dev note: Due to complexities in the types structure, we can't reliably pull in the types for the
// napi-module. Be careful with your implementation!

export class OlmMachine {
    private machine: any; // SledBackedOlmMachine

    private constructor(private _userId: string, private _deviceId: string, private engine: OlmEngine) {
    }

    private makeSled(dir: string): OlmMachine {
        this.machine = new napi.SledBackedOlmMachine(this._userId, this._deviceId, dir);
        return this;
    }

    public static withSledBackend(userId: string, deviceId: string, engine: OlmEngine, sledPath: string) {
        return (new OlmMachine(userId, deviceId, engine)).makeSled(sledPath);
    }

    public async runEngine(): Promise<void> {
        const requests = toArray(this.machine.outgoingRequests ?? []).map((r: string) => JSON.parse(r));
        for (const request of requests) {
            await this.runEngineRequest(request);
        }
    }

    private async runEngineRequest(request: any) {
        switch(request['request_kind']) {
            case 'KeysUpload': {
                const resp = await this.engine.uploadOneTimeKeys(request['body']);
                this.machine.markRequestAsSent(request['request_id'], RequestKind.KeysUpload, JSON.stringify(resp));
                break;
            }
            case 'KeysQuery': {
                const userIds = Array.isArray(request['users']) ? request['users'] : [request['users']];
                const resp = await this.engine.queryOneTimeKeys(userIds);
                this.machine.markRequestAsSent(request['request_id'], RequestKind.KeysQuery, JSON.stringify(resp));
                break;
            }
            case 'KeysClaim': {
                const resp = await this.engine.claimOneTimeKeys(request['one_time_keys']);
                this.machine.markRequestAsSent(request['request_id'], RequestKind.KeysClaim, JSON.stringify(resp));
                break;
            }
            case 'ToDevice': {
                await this.engine.sendToDevices(request['event_type'], request['body']);
                break;
            }
            default:
                // TODO: Handle properly
                console.error("Unhandled request:", request);
                break;
        }
    }

    // --- proxy interface below here ---

    public get userId(): string {
        return this.machine.userId;
    }

    public get deviceId(): string {
        return this.machine.deviceId;
    }

    public get deviceDisplayName(): Optional<string> {
        return this.machine.deviceDisplayName;
    }

    public get identityKeys(): Record<string, string> {
        return this.machine.identityKeys;
    }

    public getDevice(userId: string, deviceId: string): Optional<Device> {
        return this.machine.getDevice(userId, deviceId);
    }

    public getUserDevices(userId: string): Device[] {
        return this.machine.getUserDevices(userId);
    }

    public async pushSync(events: ToDeviceEvent[], deviceLists: DeviceLists, remainingKeyCounts: Record<string, number>, unusedFallbackKeyTypes?: string[]) {
        const keyCounts = Object.entries(remainingKeyCounts).map(e => [e[0], e[1].toString()]).reduce((c, p) => ({...c, [p[0]]: p[1]}), {});
        this.machine.receiveSyncChanges(JSON.stringify({events}), deviceLists, keyCounts, unusedFallbackKeyTypes);
        await this.runEngine();
    }

    /**
     * Adds users to the tracked user list. `runEngine()` should be
     * called after this, though can be delayed until all updates have
     * been made.
     * @param {string[]} userIds The user IDs to add.
     * @returns {Promise<void>} Resolves when complete.
     */
    public async updateTrackedUsers(userIds: string[]) {
        this.machine.updateTrackedUsers(userIds);
    }

    public isUserTracked(userId: string): boolean {
        return this.machine.isUserTracked(userId);
    }

    public async ensureSessionsFor(userIds: string[]) {
        const request = JSON.parse(this.machine.getMissingSessions(userIds));
        if (request['request_kind']) {
            await this.runEngineRequest(request);
        }
    }

    // TODO: Need to lock based on room ID
    public async encryptRoomEvent(roomId: string, eventType: string, content: Record<string, any>): Promise<Record<string, any>> {
        const userIds = await this.engine.getEffectiveJoinedUsersInRoom(roomId);

        this.machine.updateTrackedUsers(userIds);
        await this.runEngine();

        await this.ensureSessionsFor(userIds); // runs the relevant parts of the engine internally

        const requests = toArray(this.machine.shareRoomKey(roomId, userIds)).map(k => JSON.parse(k));
        for (const request of requests) {
            await this.runEngineRequest(request);
        }
        await this.runEngine();

        return JSON.parse(this.machine.encrypt(roomId, eventType, JSON.stringify(content)));
    }

    public async decryptRoomEvent(roomId: string, event: MatrixEvent): Promise<DecryptedMatrixEvent> {
        const parsed = this.machine.decryptRoomEvent(JSON.stringify(event), roomId);
        parsed.clearEvent = JSON.parse(parsed.clearEvent);
        await this.runEngine();
        return parsed;
    }

    public async sign(message: Record<string, any>): Promise<Signatures> {
        return JSON.parse(this.machine.sign(JSON.stringify(message)));
    }
}