package dev.pairly.android

import android.app.Application

class PairlyApp : Application() {
    override fun onCreate() {
        super.onCreate()
        Notifications.createChannels(this)
    }
}
