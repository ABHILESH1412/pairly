package dev.pairly.android.phone

import android.Manifest
import android.app.Activity
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.database.ContentObserver
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.provider.Telephony
import android.telephony.SmsManager
import android.util.Log
import androidx.core.content.ContextCompat
import androidx.core.content.FileProvider
import androidx.core.net.toUri
import dev.pairly.android.Pairly
import dev.pairly.android.R
import dev.pairly.core.ffi.AttachmentData
import dev.pairly.core.ffi.ConversationData
import dev.pairly.core.ffi.MessageData
import dev.pairly.core.ffi.OutgoingAttachmentData
import dev.pairly.core.ffi.PairlyException
import dev.pairly.core.ffi.SmsHandler
import java.io.File

/**
 * Text and picture messages for paired PCs: reads conversations (SMS and MMS, including group
 * threads) from the phone's message store, sends texts and picture/group messages, and reports
 * new ones. New messages are found by watching the store, which needs no RECEIVE_SMS.
 */
class Sms(context: Context) : SmsHandler {
    private val context = context.applicationContext
    private val recipientCache = HashMap<Long, String>()

    // ----- conversations -------------------------------------------------------------------

    override fun conversations(): List<ConversationData> {
        check()
        return runCatching { threads() }.getOrElse {
            Log.w(TAG, "thread list unavailable, scanning SMS instead", it)
            smsThreads()
        }
    }

    /** From the combined SMS+MMS thread table, which also lists group participants. */
    private fun threads(): List<ConversationData> {
        val out = ArrayList<ConversationData>()
        val uri = "content://mms-sms/conversations?simple=true".toUri()
        val columns = arrayOf("_id", "date", "recipient_ids", "snippet", "read")
        context.contentResolver.query(uri, columns, null, null, "date DESC")?.use { c ->
            while (c.moveToNext() && out.size < 200) {
                val addresses = c.getString(2).orEmpty().split(' ')
                    .mapNotNull { it.toLongOrNull()?.let(::recipient) }
                    .filter { it.isNotBlank() }
                if (addresses.isEmpty()) continue
                out += ConversationData(
                    threadId = c.getLong(0),
                    addresses = addresses,
                    names = addresses.map { Calls.contactName(context, it).orEmpty() },
                    snippet = c.getString(3)?.takeIf { it.isNotBlank() } ?: context.getString(R.string.sms_picture),
                    dateMs = c.getLong(1),
                    read = c.getInt(4) != 0,
                )
            }
        }
        return out
    }

    private fun recipient(id: Long): String? = recipientCache.getOrPut(id) {
        context.contentResolver.query("content://mms-sms/canonical-address/$id".toUri(), null, null, null, null)
            ?.use { c -> if (c.moveToFirst()) c.getString(c.getColumnIndexOrThrow("address")) else null }
            .orEmpty()
    }.ifEmpty { null }

    /** Fallback: newest SMS per thread. */
    private fun smsThreads(): List<ConversationData> {
        val latest = LinkedHashMap<Long, MessageData>()
        val unread = HashSet<Long>()
        querySms(null, null, "${Telephony.Sms.DATE} DESC LIMIT 3000") { m ->
            latest.putIfAbsent(m.threadId, m)
            if (!m.read && !m.outgoing) unread += m.threadId
        }
        return latest.values.take(200).map { m ->
            ConversationData(
                threadId = m.threadId,
                addresses = listOf(m.address),
                names = listOf(Calls.contactName(context, m.address).orEmpty()),
                snippet = m.body,
                dateMs = m.dateMs,
                read = m.threadId !in unread,
            )
        }
    }

    // ----- messages ------------------------------------------------------------------------

    override fun messages(threadId: Long, beforeMs: Long?, limit: UInt): List<MessageData> {
        check()
        val n = limit.toInt().coerceIn(1, 100)
        val out = ArrayList<MessageData>()
        val selection = buildString {
            append("${Telephony.Sms.THREAD_ID} = ?")
            if (beforeMs != null) append(" AND ${Telephony.Sms.DATE} < ?")
        }
        val args = listOfNotNull(threadId.toString(), beforeMs?.toString()).toTypedArray()
        querySms(selection, args, "${Telephony.Sms.DATE} DESC LIMIT $n") { out += it }
        out += mms(threadId, beforeMs, n)
        return out.sortedByDescending { it.dateMs }.take(n).reversed()
    }

    /** Picture and group messages in a thread (MMS dates are in seconds). */
    private fun mms(threadId: Long, beforeMs: Long?, limit: Int): List<MessageData> {
        val out = ArrayList<MessageData>()
        val selection = buildString {
            append("${Telephony.Mms.THREAD_ID} = ?")
            if (beforeMs != null) append(" AND ${Telephony.Mms.DATE} < ?")
        }
        val args = listOfNotNull(threadId.toString(), beforeMs?.let { (it / 1000).toString() }).toTypedArray()
        val columns = arrayOf(Telephony.Mms._ID, Telephony.Mms.DATE, Telephony.Mms.MESSAGE_BOX, Telephony.Mms.READ)
        runCatching {
            context.contentResolver.query(
                Telephony.Mms.CONTENT_URI,
                columns,
                selection,
                args,
                "${Telephony.Mms.DATE} DESC LIMIT $limit",
            )?.use { c ->
                while (c.moveToNext()) {
                    out += mmsMessage(c.getLong(0), threadId, c.getLong(1) * 1000, c.getInt(2), c.getInt(3) != 0)
                }
            }
        }.onFailure { Log.w(TAG, "reading MMS failed", it) }
        return out
    }

    private fun mmsMessage(id: Long, threadId: Long, dateMs: Long, box: Int, read: Boolean): MessageData {
        val outgoing = box != Telephony.Mms.MESSAGE_BOX_INBOX
        // Addresses: 137 = from, 151 = to, 130 = cc.
        var from = ""
        val everyone = ArrayList<String>()
        context.contentResolver.query("content://mms/$id/addr".toUri(), arrayOf("address", "type"), null, null, null)
            ?.use { c ->
                while (c.moveToNext()) {
                    val address = c.getString(0).orEmpty()
                    if (address.isBlank() || address == "insert-address-token") continue
                    if (c.getInt(1) == 137) from = address
                    if (address !in everyone) everyone += address
                }
            }
        val body = StringBuilder()
        val attachments = ArrayList<AttachmentData>()
        context.contentResolver.query(
            "content://mms/part".toUri(),
            arrayOf("_id", "ct", "text", "name", "cl"),
            "mid = ?",
            arrayOf(id.toString()),
            null,
        )?.use { c ->
            while (c.moveToNext()) {
                val type = c.getString(1).orEmpty()
                when {
                    type == "text/plain" -> {
                        c.getString(2)?.let { if (body.isNotEmpty()) body.append('\n'); body.append(it) }
                    }
                    type == "application/smil" -> {}
                    else -> attachments += AttachmentData(
                        partId = c.getLong(0),
                        mime = type,
                        name = c.getString(3) ?: c.getString(4) ?: "attachment",
                        size = 0u,
                    )
                }
            }
        }
        val address = if (outgoing) everyone.firstOrNull { it != from }.orEmpty() else from
        return MessageData(
            id = -id, // negative: MMS ids live in their own table
            threadId = threadId,
            address = address,
            body = body.toString(),
            dateMs = dateMs,
            outgoing = outgoing,
            read = read,
            participants = if (everyone.size > 2) everyone else emptyList(),
            attachments = attachments,
        )
    }

    override fun attachment(partId: Long, offset: ULong, len: UInt): ByteArray {
        check()
        context.contentResolver.openInputStream("content://mms/part/$partId".toUri())?.use { input ->
            var skip = offset.toLong()
            while (skip > 0) {
                val n = input.skip(skip)
                if (n <= 0) return ByteArray(0)
                skip -= n
            }
            val buf = ByteArray(len.toInt())
            var read = 0
            while (read < buf.size) {
                val n = input.read(buf, read, buf.size - read)
                if (n < 0) break
                read += n
            }
            return buf.copyOf(read)
        }
        throw PairlyException.Failed("attachment not found")
    }

    // ----- sending -------------------------------------------------------------------------

    override fun send(addresses: List<String>, text: String, attachments: List<OutgoingAttachmentData>) {
        if (ContextCompat.checkSelfPermission(context, Manifest.permission.SEND_SMS) != PackageManager.PERMISSION_GRANTED) {
            throw PairlyException.Failed("Pairly isn't allowed to send texts")
        }
        val manager = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            context.getSystemService(SmsManager::class.java)
        } else {
            @Suppress("DEPRECATION")
            SmsManager.getDefault()
        } ?: throw PairlyException.Failed("this phone can't send texts")
        if (attachments.isEmpty() && addresses.size == 1) {
            // As a non-default SMS app, the system records the sent message itself.
            manager.sendMultipartTextMessage(addresses[0], null, manager.divideMessage(text), null, null)
            Pairly.say(context.getString(R.string.sms_sent))
            return
        }
        sendMms(manager, addresses, text, attachments)
    }

    /** A group or picture message: build the PDU and hand it to the system's MMS service. */
    private fun sendMms(manager: SmsManager, to: List<String>, text: String, attachments: List<OutgoingAttachmentData>) {
        val limit = runCatching { manager.carrierConfigValues.getInt(SmsManager.MMS_CONFIG_MAX_MESSAGE_SIZE) }
            .getOrNull()?.takeIf { it > 0 } ?: DEFAULT_MMS_LIMIT
        val parts = MmsFit.fit(attachments, text, limit)
        val pdu = MmsPdu.sendRequest(to, text.ifBlank { null }, parts)
        Log.i(TAG, "sending MMS: ${pdu.size} bytes (carrier limit $limit) to ${to.size} recipient(s)")
        val dir = File(context.cacheDir, "mms").apply { mkdirs() }
        val file = File(dir, "send-${System.nanoTime()}.pdu")
        file.writeBytes(pdu)
        val uri = FileProvider.getUriForFile(context, "${context.packageName}.files", file)
        // The MMS service reads the PDU through our provider.
        for (pkg in listOf("com.android.phone", "com.android.mms.service")) {
            runCatching { context.grantUriPermission(pkg, uri, Intent.FLAG_GRANT_READ_URI_PERMISSION) }
        }
        val sent = PendingIntent.getBroadcast(
            context,
            file.hashCode(),
            Intent(context, MmsSentReceiver::class.java).putExtra(EXTRA_FILE, file.path),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        manager.sendMultimediaMessage(context, uri, null, null, sent)
        Pairly.say(context.getString(R.string.mms_sending))
    }

    private fun check() {
        if (!permitted(context)) throw PairlyException.Failed("Allow Pairly to read texts on the phone first")
    }

    private fun querySms(selection: String?, args: Array<String>?, order: String, row: (MessageData) -> Unit) {
        val columns = arrayOf(
            Telephony.Sms._ID,
            Telephony.Sms.THREAD_ID,
            Telephony.Sms.ADDRESS,
            Telephony.Sms.BODY,
            Telephony.Sms.DATE,
            Telephony.Sms.TYPE,
            Telephony.Sms.READ,
        )
        context.contentResolver.query(Telephony.Sms.CONTENT_URI, columns, selection, args, order)?.use { c ->
            while (c.moveToNext()) {
                row(
                    MessageData(
                        id = c.getLong(0),
                        threadId = c.getLong(1),
                        address = c.getString(2).orEmpty(),
                        body = c.getString(3).orEmpty(),
                        dateMs = c.getLong(4),
                        outgoing = c.getInt(5) != Telephony.Sms.MESSAGE_TYPE_INBOX,
                        read = c.getInt(6) != 0,
                        participants = emptyList(),
                        attachments = emptyList(),
                    ),
                )
            }
        }
    }

    /** Cleans up after an MMS send and reports failures. */
    class MmsSentReceiver : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            intent.getStringExtra(EXTRA_FILE)?.let { File(it).delete() }
            // Even a delivered request can be refused by the carrier: read its answer.
            val answer = intent.getByteArrayExtra(SmsManager.EXTRA_MMS_DATA)?.let(MmsPdu::sendConfStatus)
            val problem = when {
                resultCode != Activity.RESULT_OK -> mmsError(resultCode, intent.getIntExtra(SmsManager.EXTRA_MMS_HTTP_STATUS, 0))
                answer != null && answer.first != MmsPdu.STATUS_OK ->
                    "the carrier refused it (status 0x${answer.first.toString(16)}${answer.second?.let { ": $it" } ?: ""})"
                else -> null
            }
            if (problem == null) {
                Log.i(TAG, "MMS sent")
                Pairly.smsStatus(true, context.getString(R.string.mms_sent))
            } else {
                Log.w(TAG, "MMS send failed: $problem")
                Pairly.say(context.getString(R.string.mms_failed, problem))
                Pairly.smsStatus(false, context.getString(R.string.mms_failed, problem))
            }
        }

        private fun mmsError(code: Int, http: Int): String = when (code) {
            SmsManager.MMS_ERROR_INVALID_APN -> "the phone's MMS settings (APN) are wrong"
            SmsManager.MMS_ERROR_UNABLE_CONNECT_MMS -> "couldn't connect to the carrier's MMS service"
            SmsManager.MMS_ERROR_HTTP_FAILURE -> "the carrier's MMS service answered with an error (HTTP $http)"
            SmsManager.MMS_ERROR_IO_ERROR -> "the connection to the carrier failed"
            SmsManager.MMS_ERROR_RETRY -> "the carrier asked to try again later"
            SmsManager.MMS_ERROR_CONFIGURATION_ERROR -> "the phone isn't set up for MMS"
            SmsManager.MMS_ERROR_NO_DATA_NETWORK -> "no mobile data (MMS needs mobile data, even on Wi-Fi)"
            MMS_ERROR_DATA_DISABLED -> "mobile data is off (MMS needs it, even on Wi-Fi)"
            else -> "error $code"
        }
    }

    companion object {
        private const val TAG = "Sms"
        private const val EXTRA_FILE = "file"
        /** MMS often lands a moment after its row appears (the parts are downloaded first). */
        private const val MMS_SETTLE_MS = 3_000L
        /** When the carrier doesn't say: the smallest limit in common use. */
        private const val DEFAULT_MMS_LIMIT = 300 * 1024
        /** `SmsManager.MMS_ERROR_DATA_DISABLED` (API 30). */
        private const val MMS_ERROR_DATA_DISABLED = 11

        val PERMISSIONS = arrayOf(
            Manifest.permission.READ_SMS,
            Manifest.permission.SEND_SMS,
            Manifest.permission.READ_CONTACTS,
        )

        fun permitted(context: Context): Boolean =
            ContextCompat.checkSelfPermission(context, Manifest.permission.READ_SMS) == PackageManager.PERMISSION_GRANTED

        private var observer: ContentObserver? = null
        private var lastSms = -1L
        private var lastMms = -1L

        /** Message store queries run here, never on the main thread. */
        private val worker: Handler by lazy {
            Handler(HandlerThread("pairly-sms").apply { start() }.looper)
        }

        /** Report new messages (texts and picture messages) to paired PCs as they appear. */
        fun watch(context: Context) {
            val app = context.applicationContext
            if (observer != null || !permitted(app)) return
            val sms = Sms(app)
            worker.post {
                lastSms = newest(app, Telephony.Sms.CONTENT_URI)
                lastMms = newest(app, Telephony.Mms.CONTENT_URI)
            }
            val scan = Runnable {
                val fresh = ArrayList<MessageData>()
                runCatching {
                    sms.querySms("${Telephony.Sms._ID} > ?", arrayOf(lastSms.toString()), "${Telephony.Sms._ID} ASC LIMIT 20") {
                        lastSms = maxOf(lastSms, it.id)
                        // Drafts and failed sends have no place in the conversation view.
                        if (it.body.isNotEmpty()) fresh += it
                    }
                    app.contentResolver.query(
                        Telephony.Mms.CONTENT_URI,
                        arrayOf(Telephony.Mms._ID, Telephony.Mms.THREAD_ID, Telephony.Mms.DATE, Telephony.Mms.MESSAGE_BOX, Telephony.Mms.READ),
                        "${Telephony.Mms._ID} > ?",
                        arrayOf(lastMms.toString()),
                        "${Telephony.Mms._ID} ASC LIMIT 10",
                    )?.use { c ->
                        while (c.moveToNext()) {
                            lastMms = maxOf(lastMms, c.getLong(0))
                            fresh += sms.mmsMessage(c.getLong(0), c.getLong(1), c.getLong(2) * 1000, c.getInt(3), c.getInt(4) != 0)
                        }
                    }
                }.onFailure { Log.w(TAG, "reading new messages failed", it) }
                for (m in fresh) Pairly.smsNew(m, Calls.contactName(app, m.address))
            }
            val obs = object : ContentObserver(worker) {
                override fun onChange(selfChange: Boolean) {
                    worker.removeCallbacks(scan)
                    worker.postDelayed(scan, MMS_SETTLE_MS)
                }
            }
            app.contentResolver.registerContentObserver("content://mms-sms/".toUri(), true, obs)
            observer = obs
        }

        fun stop(context: Context) {
            observer?.let { context.applicationContext.contentResolver.unregisterContentObserver(it) }
            observer = null
        }

        private fun newest(context: Context, uri: android.net.Uri): Long =
            runCatching {
                context.contentResolver.query(uri, arrayOf("_id"), null, null, "_id DESC LIMIT 1")
                    ?.use { c -> if (c.moveToFirst()) c.getLong(0) else 0L } ?: 0L
            }.getOrDefault(0L)
    }
}
