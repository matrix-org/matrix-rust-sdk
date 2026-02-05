# DCGKA FFI Bindings

This document describes the FFI (Foreign Function Interface) bindings for the DCGKA (Decentralized Continuous Group Key Agreement) implementation.

## Overview

The DCGKA implementation is now exposed through the `matrix-sdk-ffi` bindings, making it available to:
- **Kotlin** (Android - Aurora)
- **Swift** (iOS)
- Other languages supported by UniFFI

## API

The DCGKA functionality is exposed through the `Encryption` interface with three main methods:

### 1. Handle DCGKA Update

Process an incoming DCGKA update event from the Matrix room.

**Signature:**
```kotlin
suspend fun handleDcgkaUpdate(roomId: String, updateJson: String): DcgkaUpdateStatus
```

**Parameters:**
- `roomId`: The Matrix room ID (e.g., "!abc123:matrix.org")
- `updateJson`: JSON-serialized DCGKA update from an `m.room.dcgka.update` event

**Returns:**
- `DcgkaUpdateStatus.Accepted`: Update was valid and applied
- `DcgkaUpdateStatus.Pending`: Update is waiting for dependencies
- `DcgkaUpdateStatus.Rejected`: Update was invalid and rejected

**Example (Kotlin):**
```kotlin
val encryption = client.encryption()
val updateJson = """{"update_id":"...", "issuer":"@alice:matrix.org", ...}"""
val status = encryption.handleDcgkaUpdate("!room:matrix.org", updateJson)

when (status) {
    DcgkaUpdateStatus.ACCEPTED -> println("Update applied successfully")
    DcgkaUpdateStatus.PENDING -> println("Update queued, waiting for dependencies")
    DcgkaUpdateStatus.REJECTED -> println("Invalid update")
}
```

### 2. Encrypt with DCGKA

Encrypt data using the room's current DCGKA-derived group key.

**Signature:**
```kotlin
suspend fun dcgkaEncrypt(roomId: String, plaintext: ByteArray): ByteArray
```

**Parameters:**
- `roomId`: The Matrix room ID
- `plaintext`: Raw bytes to encrypt

**Returns:**
- Encrypted ciphertext (includes nonce/IV)

**Example (Kotlin):**
```kotlin
val encryption = client.encryption()
val message = "Hello, DCGKA!".toByteArray()
val ciphertext = encryption.dcgkaEncrypt("!room:matrix.org", message)

// Send ciphertext in a Matrix event
```

### 3. Decrypt with DCGKA

Decrypt data using the room's DCGKA-derived keys (tries current and historical keys).

**Signature:**
```kotlin
suspend fun dcgkaDecrypt(roomId: String, ciphertext: ByteArray): ByteArray
```

**Parameters:**
- `roomId`: The Matrix room ID
- `ciphertext`: Encrypted bytes to decrypt

**Returns:**
- Decrypted plaintext

**Example (Kotlin):**
```kotlin
val encryption = client.encryption()
val plaintext = encryption.dcgkaDecrypt("!room:matrix.org", ciphertext)
val message = String(plaintext)
println("Decrypted: $message")
```

## Types

### DcgkaUpdateStatus (Enum)

```kotlin
enum class DcgkaUpdateStatus {
    ACCEPTED,  // Update was successfully applied
    PENDING,   // Update is queued, waiting for dependencies
    REJECTED   // Update was rejected as invalid
}
```

### DcgkaUpdateType (Enum)

```kotlin
enum class DcgkaUpdateType {
    ADD,     // Adding a new device to the group
    REMOVE,  // Removing a device from the group
    ROTATE   // Rotating the group key
}
```

### DcgkaUpdate (Record)

```kotlin
data class DcgkaUpdate(
    val updateId: String,           // Unique update identifier
    val issuer: String,              // User ID who issued the update
    val issuerDevice: String,        // Device ID that issued the update
    val updateType: DcgkaUpdateType, // Type of update
    val dependencies: List<String>,  // Update IDs this depends on
    val timestamp: Long,             // Unix timestamp (ms)
    val json: String                 // Full JSON serialization (includes payload + signature)
)
```

## Integration with Aurora

### Step 1: Build the FFI bindings

```bash
cd matrix-rust-sdk
cargo build -p matrix-sdk-ffi --features rustls-tls
```

### Step 2: Generate Kotlin bindings

```bash
cd bindings/matrix-sdk-ffi
cargo run --bin uniffi-bindgen generate src/api.udl --language kotlin
```

This generates:
- `MatrixSdk.kt` - Kotlin bindings
- `libmatrix_sdk_ffi.so` - Native library (for Android)

### Step 3: Use in Aurora

```kotlin
// Initialize client
val client = Client(/* ... */)

// Listen for m.room.dcgka.update events
room.timeline.events.collect { event ->
    if (event.type == "m.room.dcgka.update") {
        val updateJson = event.content.toString()
        val status = client.encryption().handleDcgkaUpdate(room.id, updateJson)
        
        when (status) {
            DcgkaUpdateStatus.ACCEPTED -> {
                // Update applied - can now encrypt/decrypt with new keys
            }
            DcgkaUpdateStatus.PENDING -> {
                // Waiting for more updates
            }
            DcgkaUpdateStatus.REJECTED -> {
                // Invalid update - log error
            }
        }
    }
}

// Send encrypted message
val encryption = client.encryption()
val plaintext = "Secret message".toByteArray()
val ciphertext = encryption.dcgkaEncrypt(room.id, plaintext)

// Send in a custom event type
room.send("m.room.encrypted.dcgka", mapOf("ciphertext" to ciphertext.encodeBase64()))
```

## Notes

- **Persistence**: DCGKA state is automatically persisted via the SQLite crypto store
- **Concurrency**: All methods are thread-safe and use async/await
- **Error Handling**: Methods throw `ClientError` on failure
- **JSON Format**: The `updateJson` parameter must be valid JSON matching the `DcgkaUpdate` schema

## Testing

To test the FFI bindings:

```bash
cd bindings/matrix-sdk-ffi
cargo test --features rustls-tls
```

## Further Reading

- [DCGKA Specification](../../element-web-specs/specs/003-dcgka-poc/spec.md)
- [UniFFI Documentation](https://mozilla.github.io/uniffi-rs/)
- [Aurora DCGKA Integration Guide](TBD)
