package org.matrix.kotlin.rust.sdk.sample

import android.os.Bundle
import android.util.Log
import androidx.appcompat.app.AppCompatActivity
import org.matrix.rustcomponents.sdk.*
import java.io.File

class MainActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        val authFolder = File(filesDir, "auth")
        val authService = AuthenticationService(authFolder.absolutePath)
        authService.configureHomeserver("matrix.org")
        val client = authService.login("", "", "MatrixRustSDKSample", null)
        val clientDelegate = object : ClientDelegate {
            override fun didReceiveAuthError(isSoftLogout: Boolean) {
                Log.v("MainActivity", "didReceiveAuthError()")
            }

            override fun didReceiveSyncUpdate() {
                Log.v("MainActivity", "didReceiveSyncUpdate()")
            }

            override fun didUpdateRestoreToken() {
                Log.v("MainActivity", "didUpdateRestoreToken()")
            }
        }

        client.setDelegate(clientDelegate)
        Log.v("MainActivity", "DisplayName = ${client.displayName()}")

        val slidingSyncView = SlidingSyncViewBuilder()
            .timelineLimit(limit = 10u)
            .requiredState(requiredState = listOf(RequiredState(key = "m.room.avatar", value = "")))
            .name(name = "HomeScreenView")
            .syncMode(mode = SlidingSyncMode.FULL_SYNC)
            .build()

        val slidingSync = client
            .slidingSync()
            .homeserver("https://slidingsync.lab.element.dev")
            .addView(slidingSyncView)
            .build()

        slidingSync.setObserver(object : SlidingSyncObserver {
            override fun didReceiveSyncUpdate(summary: UpdateSummary) {
                Log.v("MainActivity", "didReceiveSyncUpdate=$summary")
            }

        })
        val syncObserverToken = slidingSync.sync()
        syncObserverToken.cancel()
        slidingSync.setObserver(null)
        client.logout()
    }
}
