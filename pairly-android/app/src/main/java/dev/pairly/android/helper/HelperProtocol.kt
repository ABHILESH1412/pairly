package dev.pairly.android.helper

import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.IOException
import java.net.Socket

/**
 * How the Pairly app and the screen helper ([Helper]) talk: TCP on the phone's own
 * 127.0.0.1, each message `[kind u8][length u32 BE][payload]`.
 */
object HelperProtocol {
    /** Bumped when the messages change: the app ignores a helper that speaks another one. */
    const val VERSION = 2
    const val PORT = 47188
    private const val MAX_MESSAGE = 4 * 1024 * 1024

    // Helper → app.
    /** `[version u32]`, right after connecting. */
    const val HELLO = 1
    /** `[width u32][height u32]`: the video begins (again after the screen rotates). */
    const val STARTED = 2
    /** `[flags u8: 1 key frame, 2 codec setup][H.264]`. */
    const val FRAME = 3
    /** `[reason UTF-8]`. */
    const val STOPPED = 4
    /** `[rate u32][channels u8][16-bit little-endian samples]`: the phone's sound. */
    const val AUDIO = 5

    // App → helper.
    /** `[longest side u32][bits per second u32][frames per second u32]`. */
    const val START = 10
    const val STOP = 11
    /** The link fell behind: send a key frame now. */
    const val KEY_FRAME = 12
    /** Exit (a newer Pairly found this helper outdated). */
    const val QUIT = 13
    /** `[x f32][y f32]`, fractions of the screen. */
    const val TAP = 20
    const val LONG_PRESS = 21
    /** `[duration ms u32][count u16][x f32][y f32]…`. */
    const val SWIPE = 22
    /** `[Android key code u32]`. */
    const val KEY = 23
    /** `[UTF-8]`. */
    const val TEXT = 24

    /** One end of the connection: sends from any thread, reads from one. */
    class Wire(private val socket: Socket) {
        private val input = DataInputStream(socket.getInputStream().buffered())
        private val output = DataOutputStream(socket.getOutputStream().buffered())

        @Synchronized
        fun send(kind: Int, payload: ByteArray = ByteArray(0)) {
            output.writeByte(kind)
            output.writeInt(payload.size)
            output.write(payload)
            output.flush()
        }

        /** The next message, or null when the other end closed. */
        fun read(): Pair<Int, ByteArray>? = try {
            val kind = input.readUnsignedByte()
            val length = input.readInt()
            if (length !in 0..MAX_MESSAGE) throw IOException("message too large")
            kind to ByteArray(length).also { input.readFully(it) }
        } catch (_: IOException) {
            null
        }

        fun close() = runCatching { socket.close() }
    }
}
