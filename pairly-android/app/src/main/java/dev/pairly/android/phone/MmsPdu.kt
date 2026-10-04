package dev.pairly.android.phone

import java.io.ByteArrayOutputStream

/**
 * Builds an MMS "m-send-req" PDU (OMA MMS Encapsulation 1.2, WSP-encoded headers and a
 * multipart body), which `SmsManager.sendMultimediaMessage` hands to the carrier, and reads the
 * carrier's "m-send-conf" answer.
 *
 * The body is laid out the way phones' own messaging apps send it: multipart/related, starting
 * with a SMIL slide show that shows each attachment and then the text.
 */
object MmsPdu {
    class Part(val mime: String, val name: String, val data: ByteArray)

    // Header field codes (well-known values with the high bit set).
    private const val MESSAGE_TYPE = 0x8C
    private const val TRANSACTION_ID = 0x98
    private const val MMS_VERSION = 0x8D
    private const val FROM = 0x89
    private const val TO = 0x97
    private const val CONTENT_TYPE = 0x84
    private const val MESSAGE_CLASS = 0x8A
    private const val DELIVERY_REPORT = 0x86
    private const val READ_REPORT = 0x90
    private const val RESPONSE_STATUS = 0x92
    private const val RESPONSE_TEXT = 0x93
    private const val MESSAGE_ID = 0x8B

    // Part header codes.
    private const val PART_CONTENT_LOCATION = 0x8E
    private const val PART_CONTENT_ID = 0xC0

    private const val M_SEND_REQ = 0x80
    private const val M_SEND_CONF = 0x81
    private const val VERSION_1_2 = 0x92
    private const val INSERT_ADDRESS_TOKEN = 0x81
    private const val CLASS_PERSONAL = 0x80
    private const val NO = 0x81
    private const val MULTIPART_RELATED = 0xB3
    private const val PARAM_TYPE = 0x89
    private const val PARAM_START = 0x8A
    private const val TEXT_PLAIN = 0x83
    private const val PARAM_CHARSET = 0x81
    private const val UTF_8 = 0xEA
    private const val PARAM_NAME = 0x85

    /** Carrier answer "Ok". */
    const val STATUS_OK = 0x80

    fun sendRequest(to: List<String>, text: String?, attachments: List<Part>): ByteArray {
        val out = ByteArrayOutputStream()
        out.write(MESSAGE_TYPE)
        out.write(M_SEND_REQ)
        out.write(TRANSACTION_ID)
        textString(out, "T" + System.currentTimeMillis().toString(16))
        out.write(MMS_VERSION)
        out.write(VERSION_1_2)
        // From: let the carrier fill in our number.
        out.write(FROM)
        out.write(1)
        out.write(INSERT_ADDRESS_TOKEN)
        for (address in to) {
            out.write(TO)
            textString(out, address.filter { it.isDigit() || it == '+' } + "/TYPE=PLMN")
        }
        out.write(MESSAGE_CLASS)
        out.write(CLASS_PERSONAL)
        out.write(DELIVERY_REPORT)
        out.write(NO)
        out.write(READ_REPORT)
        out.write(NO)

        // Content-Type must be the last header: multipart/related; start=<smil>; type=application/smil
        val type = ByteArrayOutputStream()
        type.write(MULTIPART_RELATED)
        type.write(PARAM_START)
        textString(type, "<smil>")
        type.write(PARAM_TYPE)
        textString(type, "application/smil")
        out.write(CONTENT_TYPE)
        valueLength(out, type.size())
        out.write(type.toByteArray())

        val parts = ArrayList<Pair<ByteArray, ByteArray>>() // (headers, data)
        val names = HashSet<String>()
        val media = attachments.mapIndexed { i, a ->
            val name = safeName(a.name).let { if (names.add(it)) it else "${i}_$it".also(names::add) }
            a to name
        }
        val textName = "text_0.txt"
        parts += head(textType("application/smil"), "smil.xml", "<smil>", params = false) to
            smil(media.map { (p, name) -> p.mime to name }, if (text.isNullOrEmpty()) null else textName)
        for ((p, name) in media) {
            val t = ByteArrayOutputStream()
            textString(t, p.mime)
            t.write(PARAM_NAME)
            textString(t, name)
            parts += head(t.toByteArray(), name, "<$name>") to p.data
        }
        if (!text.isNullOrEmpty()) {
            val t = ByteArrayOutputStream()
            t.write(TEXT_PLAIN)
            t.write(PARAM_CHARSET)
            t.write(UTF_8)
            parts += head(t.toByteArray(), textName, "<$textName>") to text.toByteArray(Charsets.UTF_8)
        }
        uintvar(out, parts.size)
        for ((head, data) in parts) {
            uintvar(out, head.size)
            uintvar(out, data.size)
            out.write(head)
            out.write(data)
        }
        return out.toByteArray()
    }

    /**
     * The carrier's answer to a send: its response status (and text), or null if `pdu` isn't an
     * m-send-conf. Only the headers a send-conf carries are understood.
     */
    fun sendConfStatus(pdu: ByteArray): Pair<Int, String?>? {
        var i = 0
        var status: Int? = null
        var text: String? = null
        var conf = false
        fun byte() = pdu[i++].toInt() and 0xFF
        fun str(): String {
            if (i < pdu.size && (pdu[i].toInt() and 0xFF) == 0x7F) i++
            val start = i
            while (i < pdu.size && pdu[i].toInt() != 0) i++
            return String(pdu, start, i - start, Charsets.UTF_8).also { i++ }
        }
        try {
            while (i < pdu.size) {
                when (byte()) {
                    MESSAGE_TYPE -> conf = byte() == M_SEND_CONF
                    TRANSACTION_ID, MESSAGE_ID -> str()
                    MMS_VERSION -> byte()
                    RESPONSE_STATUS -> status = byte()
                    RESPONSE_TEXT -> {
                        // Encoded-string: either text, or a length + charset + text.
                        val first = pdu[i].toInt() and 0xFF
                        if (first <= 31) {
                            val len = if (first == 31) {
                                i++
                                readUintvar(pdu, i).also { i = it.second }.first
                            } else {
                                i++
                                first
                            }
                            val end = i + len
                            i++ // charset (short integer)
                            text = String(pdu, i, (end - i).coerceAtLeast(0), Charsets.UTF_8).trimEnd('\u0000')
                            i = end
                        } else {
                            text = str()
                        }
                    }
                    else -> break
                }
            }
        } catch (_: IndexOutOfBoundsException) {
            // Truncated: keep what was read.
        }
        return if (conf && status != null) status to text else null
    }

    private fun readUintvar(b: ByteArray, from: Int): Pair<Int, Int> {
        var i = from
        var v = 0
        while (true) {
            val x = b[i++].toInt() and 0xFF
            v = (v shl 7) or (x and 0x7F)
            if (x and 0x80 == 0) return v to i
        }
    }

    private fun textType(mime: String): ByteArray = ByteArrayOutputStream().also { textString(it, mime) }.toByteArray()

    /** Part headers: content type (length-prefixed if it has parameters), location and id. */
    private fun head(type: ByteArray, location: String, id: String, params: Boolean = true): ByteArray {
        val h = ByteArrayOutputStream()
        if (params) valueLength(h, type.size)
        h.write(type)
        h.write(PART_CONTENT_LOCATION)
        textString(h, location)
        h.write(PART_CONTENT_ID)
        // Quoted-string.
        h.write(0x22)
        h.write(id.toByteArray(Charsets.US_ASCII))
        h.write(0)
        return h.toByteArray()
    }

    /** One slide per attachment, the text on the last. */
    private fun smil(media: List<Pair<String, String>>, text: String?): ByteArray {
        val s = StringBuilder()
        s.append("<smil><head><layout><root-layout width=\"320px\" height=\"480px\"/>")
        s.append("<region id=\"Image\" left=\"0\" top=\"0\" width=\"320px\" height=\"320px\" fit=\"meet\"/>")
        s.append("<region id=\"Text\" left=\"0\" top=\"320\" width=\"320px\" height=\"160px\" fit=\"meet\"/>")
        s.append("</layout></head><body>")
        if (media.isEmpty() && text != null) {
            s.append("<par dur=\"5000ms\"><text src=\"$text\" region=\"Text\"/></par>")
        }
        media.forEachIndexed { i, (mime, name) ->
            val tag = when {
                mime.startsWith("image/") -> "img"
                mime.startsWith("video/") -> "video"
                mime.startsWith("audio/") -> "audio"
                else -> "ref"
            }
            s.append("<par dur=\"5000ms\">")
            s.append(if (tag == "audio") "<audio src=\"$name\"/>" else "<$tag src=\"$name\" region=\"Image\"/>")
            if (text != null && i == media.size - 1) s.append("<text src=\"$text\" region=\"Text\"/>")
            s.append("</par>")
        }
        s.append("</body></smil>")
        return s.toString().toByteArray(Charsets.UTF_8)
    }

    /** ASCII-only names, which every MMS centre and phone accepts. */
    private fun safeName(name: String): String {
        val dot = name.lastIndexOf('.')
        val ext = if (dot > 0) name.substring(dot + 1).filter { it.isLetterOrDigit() && it.code < 128 }.take(5) else ""
        val stem = (if (dot > 0) name.substring(0, dot) else name)
            .map { if (it.code < 128 && (it.isLetterOrDigit() || it == '_' || it == '-')) it else '_' }
            .joinToString("")
            .take(40)
            .ifEmpty { "file" }
        return if (ext.isEmpty()) stem else "$stem.$ext"
    }

    /** Null-terminated; a leading byte ≥ 0x80 is quoted. */
    private fun textString(out: ByteArrayOutputStream, s: String) {
        val bytes = s.toByteArray(Charsets.UTF_8)
        if (bytes.isNotEmpty() && (bytes[0].toInt() and 0xFF) >= 0x80) out.write(0x7F)
        out.write(bytes)
        out.write(0)
    }

    private fun valueLength(out: ByteArrayOutputStream, len: Int) {
        if (len <= 30) {
            out.write(len)
        } else {
            out.write(31)
            uintvar(out, len)
        }
    }

    /** Big-endian base-128 with continuation bits. */
    fun uintvar(out: ByteArrayOutputStream, value: Int) {
        var v = value
        val bytes = ArrayList<Int>()
        bytes += v and 0x7F
        v = v ushr 7
        while (v > 0) {
            bytes += (v and 0x7F) or 0x80
            v = v ushr 7
        }
        for (b in bytes.asReversed()) out.write(b)
    }
}
