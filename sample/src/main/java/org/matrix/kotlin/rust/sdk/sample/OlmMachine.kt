package org.matrix.kotlin.rust.sdk.sample


import java.io.File
class OlmMachine(
    user_id: String,
    device_id: String,
    path: File,
) {
    private val inner: uniffi.olm.OlmMachine = uniffi.olm.OlmMachine(user_id, device_id, path.absolutePath)



}
