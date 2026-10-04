package dev.pairly.android.phone

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Environment
import android.provider.Settings
import androidx.core.content.ContextCompat
import androidx.core.net.toUri
import dev.pairly.core.ffi.FilesHandler
import dev.pairly.core.ffi.PairlyException

/**
 * Lets paired PCs browse the phone's shared storage (`/sdcard`). Android 11+ calls this
 * "All files access"; it's granted in system settings, not with a dialog.
 */
class SharedFiles(context: Context) : FilesHandler {
    private val context = context.applicationContext

    override fun root(): String {
        if (!permitted(context)) {
            throw PairlyException.Failed("Allow “All files access” for Pairly on the phone (Browse files card)")
        }
        return Environment.getExternalStorageDirectory().absolutePath
    }

    companion object {
        fun permitted(context: Context): Boolean =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                Environment.isExternalStorageManager()
            } else {
                ContextCompat.checkSelfPermission(context, Manifest.permission.WRITE_EXTERNAL_STORAGE) ==
                    PackageManager.PERMISSION_GRANTED
            }

        /** The settings screen for All files access (Android 11+). */
        fun settingsIntent(context: Context): Intent =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION, "package:${context.packageName}".toUri())
            } else {
                Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, "package:${context.packageName}".toUri())
            }
    }
}
