package dev.pairly.android.device

import android.content.Context
import android.hardware.Sensor
import android.hardware.SensorEvent
import android.hardware.SensorEventListener
import android.hardware.SensorManager
import android.os.SystemClock
import dev.pairly.android.Pairly
import dev.pairly.core.ffi.LaserActionData

/**
 * The presentation laser pointer: while held, the phone's turning (gyroscope) moves a red dot
 * on the PC's screen. Point the top of the phone at the screen; turning left/right moves the
 * dot sideways, tilting up/down moves it up and down.
 */
class LaserPointer(context: Context, private val device: String) : SensorEventListener {
    private val sensors = context.getSystemService(SensorManager::class.java)
    private val gyroscope: Sensor? = sensors?.getDefaultSensor(Sensor.TYPE_GYROSCOPE)
    private var lastEvent = 0L
    private var lastSent = 0L
    private var dx = 0f
    private var dy = 0f
    private var active = false

    val available: Boolean get() = gyroscope != null

    fun start() {
        val gyro = gyroscope ?: return
        if (active) return
        active = true
        lastEvent = 0L
        dx = 0f
        dy = 0f
        Pairly.inputLaser(device, LaserActionData.SHOW, 0f, 0f)
        sensors?.registerListener(this, gyro, SensorManager.SENSOR_DELAY_GAME)
    }

    fun stop() {
        if (!active) return
        active = false
        sensors?.unregisterListener(this)
        Pairly.inputLaser(device, LaserActionData.HIDE, 0f, 0f)
    }

    override fun onSensorChanged(event: SensorEvent) {
        if (!active) return
        if (lastEvent != 0L) {
            val dt = (event.timestamp - lastEvent) / 1_000_000_000f
            // Angular speed (rad/s) around the screen's normal (left/right) and the phone's
            // width axis (up/down), as a fraction of the screen per radian turned.
            dx -= event.values[2] * dt / SWEEP_X
            dy -= event.values[0] * dt / SWEEP_Y
        }
        lastEvent = event.timestamp
        val now = SystemClock.uptimeMillis()
        if (now - lastSent >= SEND_MS && (dx != 0f || dy != 0f)) {
            Pairly.inputLaser(device, LaserActionData.MOVE, dx, dy)
            dx = 0f
            dy = 0f
            lastSent = now
        }
    }

    override fun onAccuracyChanged(sensor: Sensor?, accuracy: Int) {}

    private companion object {
        /** Turning this far (radians, about 35°) crosses the whole screen width. */
        const val SWEEP_X = 0.6f
        /** About 20° from top to bottom (screens are wider than tall). */
        const val SWEEP_Y = 0.35f
        /** Send at most about 60 moves a second. */
        const val SEND_MS = 16L
    }
}
