package dev.pairly.android

import android.app.Application
import android.content.pm.ApplicationInfo
import android.os.StrictMode

class PairlyApp : Application() {
    override fun onCreate() {
        super.onCreate()
        if (applicationInfo.flags and ApplicationInfo.FLAG_DEBUGGABLE != 0) strictMode()
        // Settings files are read from disk the first time they're used; start that off the main
        // thread so the first screen doesn't wait for it.
        Thread {
            for (name in PREFERENCE_FILES) getSharedPreferences(name, MODE_PRIVATE).all
        }.start()
        Notifications.createChannels(this)
    }

    private companion object {
        val PREFERENCE_FILES = listOf("share", "clipboard", "notifications")
    }

    /**
     * Debug builds log (tag StrictMode) disk or network work on the main thread and leaked
     * resources, so they're caught before they cause jank or leaks in release builds.
     */
    private fun strictMode() {
        StrictMode.setThreadPolicy(
            StrictMode.ThreadPolicy.Builder()
                .detectDiskReads()
                .detectDiskWrites()
                .detectNetwork()
                .detectCustomSlowCalls()
                .penaltyLog()
                .build(),
        )
        StrictMode.setVmPolicy(
            StrictMode.VmPolicy.Builder()
                .detectLeakedClosableObjects()
                .detectLeakedRegistrationObjects()
                .detectLeakedSqlLiteObjects()
                .detectActivityLeaks()
                .detectFileUriExposure()
                .penaltyLog()
                .build(),
        )
    }
}
