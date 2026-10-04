package dev.pairly.android.phone

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Matrix
import androidx.core.graphics.createBitmap
import androidx.exifinterface.media.ExifInterface
import dev.pairly.core.ffi.OutgoingAttachmentData
import dev.pairly.core.ffi.PairlyException
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import kotlin.math.max

/**
 * Carriers cap the size of a picture message (often 300 KB). Pictures that would go over are
 * re-encoded as smaller JPEGs, sharing what's left after the text and any other attachments.
 */
object MmsFit {
    /** Headers, the SMIL layout and part headers. */
    private const val OVERHEAD = 6 * 1024
    private const val LARGEST_SIDE = 1280
    private const val SMALLEST_SIDE = 160

    fun fit(attachments: List<OutgoingAttachmentData>, text: String, limit: Int): List<MmsPdu.Part> {
        val shrinkable = attachments.map { it.mime.startsWith("image/") && it.mime != "image/gif" }
        val fixed = OVERHEAD + text.toByteArray().size +
            attachments.filterIndexed { i, _ -> !shrinkable[i] }.sumOf { it.data.size }
        if (fixed > limit) {
            throw PairlyException.Failed("too big for a picture message: this carrier allows ${limit / 1024} KB")
        }
        val pictures = shrinkable.count { it }
        val each = if (pictures == 0) 0 else (limit - fixed) / pictures
        return attachments.mapIndexed { i, a ->
            if (shrinkable[i] && a.data.size > each) {
                val stem = a.name.substringBeforeLast('.').ifEmpty { "picture" }
                MmsPdu.Part("image/jpeg", "$stem.jpg", shrink(a.data, each))
            } else {
                MmsPdu.Part(a.mime, a.name, a.data)
            }
        }
    }

    private fun shrink(data: ByteArray, budget: Int): ByteArray {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeByteArray(data, 0, data.size, bounds)
        val longest = max(bounds.outWidth, bounds.outHeight)
        if (longest <= 0) throw PairlyException.Failed("couldn't read the picture")
        val rotation = runCatching {
            ExifInterface(ByteArrayInputStream(data)).rotationDegrees
        }.getOrDefault(0)
        var side = minOf(LARGEST_SIDE, longest)
        while (side >= SMALLEST_SIDE) {
            var sample = 1
            while (longest / (sample * 2) >= side) sample *= 2
            val decoded = BitmapFactory.decodeByteArray(data, 0, data.size, BitmapFactory.Options().apply { inSampleSize = sample })
                ?: throw PairlyException.Failed("couldn't read the picture")
            val bitmap = prepare(decoded, side, rotation)
            try {
                for (quality in intArrayOf(85, 70, 55, 40)) {
                    val out = ByteArrayOutputStream()
                    bitmap.compress(Bitmap.CompressFormat.JPEG, quality, out)
                    if (out.size() <= budget) return out.toByteArray()
                }
            } finally {
                bitmap.recycle()
            }
            side = side * 3 / 4
        }
        throw PairlyException.Failed("couldn't make the picture small enough for a picture message")
    }

    /** Scaled to `side`, turned upright, and on white (JPEG has no transparency). */
    private fun prepare(decoded: Bitmap, side: Int, rotation: Int): Bitmap {
        val scale = minOf(1f, side.toFloat() / max(decoded.width, decoded.height))
        val matrix = Matrix().apply {
            postScale(scale, scale)
            postRotate(rotation.toFloat())
        }
        val turned = Bitmap.createBitmap(decoded, 0, 0, decoded.width, decoded.height, matrix, true)
        if (turned !== decoded) decoded.recycle()
        if (!turned.hasAlpha()) return turned
        val flat = createBitmap(turned.width, turned.height)
        Canvas(flat).apply {
            drawColor(Color.WHITE)
            drawBitmap(turned, 0f, 0f, null)
        }
        turned.recycle()
        return flat
    }
}
