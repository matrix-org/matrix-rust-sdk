package org.matrix.kotlin.rust.sdk.sample

import uniffi.olm.OlmMachine
import java.io.File
class OlmMachine(
    user_id: String,
    device_id: String,
    path: File,
) {
    private val inner: OlmMachine = OlmMachine(user_id, device_id, path.toString())






}
