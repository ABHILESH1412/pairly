package dev.pairly.android.share

import android.content.ContentValues
import android.content.Context
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.webkit.MimeTypeMap
import androidx.core.content.FileProvider
import java.io.File

/**
 * Where received files go: `Download/Pairly` through MediaStore (Android 10+), so they show up
 * in Files and other apps without any storage permission. A file stays hidden (pending) until
 * its checksum verifies; MediaStore picks a unique name if one is taken.
 *
 * Android 8–9 has no MediaStore downloads; files go to the app's own external folder there.
 */
object Downloads {
    private const val FOLDER = "Pairly"

    /** A new, empty file for [name]: its uri and a writable descriptor to hand to Rust. */
    fun create(context: Context, name: String, mime: String?): Pair<Uri, Int> {
        val type = mime ?: guessMime(name)
        val resolver = context.contentResolver
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            val values = ContentValues().apply {
                put(MediaStore.Downloads.DISPLAY_NAME, name)
                put(MediaStore.Downloads.MIME_TYPE, type)
                put(MediaStore.Downloads.RELATIVE_PATH, "${Environment.DIRECTORY_DOWNLOADS}/$FOLDER")
                put(MediaStore.Downloads.IS_PENDING, 1)
            }
            val uri = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                ?: error("Couldn't create $name in Downloads")
            return try {
                // Rust takes over the descriptor; closing the emptied wrapper is a no-op.
                val fd = resolver.openFileDescriptor(uri, "rw")?.use { it.detachFd() } ?: error("Couldn't open $name")
                uri to fd
            } catch (e: Exception) {
                resolver.delete(uri, null, null)
                throw e
            }
        }
        val dir = File(context.getExternalFilesDir(Environment.DIRECTORY_DOWNLOADS), FOLDER).apply { mkdirs() }
        val file = uniqueFile(dir, name)
        file.createNewFile()
        val uri = Uri.fromFile(file)
        val fd = resolver.openFileDescriptor(uri, "rw")?.use { it.detachFd() } ?: error("Couldn't open $name")
        return uri to fd
    }

    /** The file is complete and verified: make it visible. */
    fun publish(context: Context, uri: Uri) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q && uri.scheme == "content") {
            val values = ContentValues().apply { put(MediaStore.Downloads.IS_PENDING, 0) }
            context.contentResolver.update(uri, values, null, null)
        }
    }

    /** Remove a partial file after a failed or cancelled transfer. */
    fun discard(context: Context, uri: Uri) {
        runCatching {
            if (uri.scheme == "file") uri.path?.let { File(it).delete() } else context.contentResolver.delete(uri, null, null)
        }
    }

    /** A uri other apps can open (file uris need a FileProvider on Android 7+). */
    fun viewable(context: Context, uri: Uri): Uri =
        if (uri.scheme == "file") {
            FileProvider.getUriForFile(context, "${context.packageName}.files", File(requireNotNull(uri.path)))
        } else {
            uri
        }

    fun guessMime(name: String): String =
        MimeTypeMap.getSingleton()
            .getMimeTypeFromExtension(name.substringAfterLast('.', "").lowercase())
            ?: "application/octet-stream"

    private fun uniqueFile(dir: File, name: String): File {
        val stem = name.substringBeforeLast('.', name)
        val ext = name.substringAfterLast('.', "").let { if (it.isEmpty() || it == name) "" else ".$it" }
        return generateSequence(0) { it + 1 }
            .map { n -> File(dir, if (n == 0) name else "$stem ($n)$ext") }
            .first { !it.exists() }
    }
}
