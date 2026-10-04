package dev.pairly.android.bluetooth

import android.Manifest
import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothServerSocket
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.util.Log
import androidx.core.content.ContextCompat
import dev.pairly.core.ffi.BluetoothHandler
import dev.pairly.core.ffi.PairlyException
import dev.pairly.core.ffi.bluetoothServiceUuid
import java.io.IOException
import java.util.UUID
import kotlin.concurrent.thread
import android.bluetooth.BluetoothSocket as AndroidSocket
import dev.pairly.core.ffi.BluetoothSocket as CoreSocket

/**
 * Bluetooth for the Rust core. Only the Java API can open RFCOMM sockets, so Kotlin owns them:
 * an RFCOMM server for devices that call us, outgoing connections on request, and a thin
 * [SocketBridge] Rust reads and writes through. The connection itself (Noise, sessions) is
 * Rust's, exactly as on LAN.
 *
 * Works only with devices already paired in the phone's Bluetooth settings.
 */
object PairlyBluetooth {
    private const val TAG = "PairlyBluetooth"
    val uuid: UUID by lazy { UUID.fromString(bluetoothServiceUuid()) }

    private var server: BluetoothServerSocket? = null

    /** Android 12+ asks for "Nearby devices" before an app may use Bluetooth connections. */
    fun permitted(context: Context): Boolean =
        Build.VERSION.SDK_INT < Build.VERSION_CODES.S ||
            ContextCompat.checkSelfPermission(context, Manifest.permission.BLUETOOTH_CONNECT) ==
            PackageManager.PERMISSION_GRANTED

    fun adapter(context: Context): BluetoothAdapter? =
        context.getSystemService(BluetoothManager::class.java)?.adapter

    /** Listen for paired devices calling us (no-op without permission or with Bluetooth off). */
    @SuppressLint("MissingPermission") // checked by permitted()
    @Synchronized
    fun startServer(context: Context, onIncoming: (CoreSocket, String) -> Unit) {
        if (server != null || !permitted(context)) return
        val adapter = adapter(context)?.takeIf { it.isEnabled } ?: return
        val socket = try {
            adapter.listenUsingRfcommWithServiceRecord("Pairly", uuid)
        } catch (e: IOException) {
            Log.w(TAG, "can't listen on Bluetooth", e)
            return
        } catch (e: SecurityException) {
            Log.w(TAG, "no Bluetooth permission", e)
            return
        }
        server = socket
        thread(name = "pairly-bt-accept", isDaemon = true) {
            try {
                while (true) {
                    val conn = socket.accept()
                    onIncoming(SocketBridge(conn), conn.remoteDevice.address)
                }
            } catch (_: IOException) {
                // Closed by stopServer() or Bluetooth turning off.
            } finally {
                synchronized(this) { if (server === socket) server = null }
            }
        }
        Log.i(TAG, "listening on Bluetooth")
    }

    @Synchronized
    fun stopServer() {
        runCatching { server?.close() }
        server = null
    }
}

/** Opens outgoing connections for Rust (called on a Rust blocking thread). */
class AndroidBluetooth(context: Context) : BluetoothHandler {
    private val context = context.applicationContext

    @SuppressLint("MissingPermission") // checked by permitted()
    override fun connect(address: String): CoreSocket {
        if (!PairlyBluetooth.permitted(context)) throw PairlyException.Failed("Bluetooth permission not granted")
        val adapter = PairlyBluetooth.adapter(context)?.takeIf { it.isEnabled }
            ?: throw PairlyException.Failed("Bluetooth is off")
        val device = try {
            adapter.getRemoteDevice(address)
        } catch (e: IllegalArgumentException) {
            throw PairlyException.Failed("bad Bluetooth address $address")
        }
        if (device.bondState != BluetoothDevice.BOND_BONDED) {
            throw PairlyException.Failed("$address isn't paired in Bluetooth settings")
        }
        val socket = device.createRfcommSocketToServiceRecord(PairlyBluetooth.uuid)
        try {
            socket.connect()
        } catch (e: IOException) {
            runCatching { socket.close() }
            throw PairlyException.Failed(e.message ?: "Bluetooth connection failed")
        }
        return SocketBridge(socket)
    }
}

/** An RFCOMM socket as Rust sees it. Rust calls these from its own threads. */
class SocketBridge(private val socket: AndroidSocket) : CoreSocket {
    private val input = socket.inputStream
    private val output = socket.outputStream

    override fun read(max: UInt): ByteArray {
        val buf = ByteArray(max.toInt())
        val n = try {
            input.read(buf)
        } catch (_: IOException) {
            -1
        }
        return if (n <= 0) ByteArray(0) else buf.copyOf(n)
    }

    override fun write(data: ByteArray) {
        try {
            output.write(data)
            output.flush()
        } catch (e: IOException) {
            throw PairlyException.Failed(e.message ?: "Bluetooth write failed")
        }
    }

    override fun disconnect() {
        runCatching { socket.close() }
    }
}
