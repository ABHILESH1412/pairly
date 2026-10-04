package dev.pairly.android.phone

import android.content.Context
import dev.pairly.android.Pairly
import dev.pairly.android.R
import dev.pairly.core.ffi.CommandData
import dev.pairly.core.ffi.CommandHandler
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update

/** Commands each paired PC lets this phone run, and their results. */
class PcCommands(context: Context) : CommandHandler {
    private val context = context.applicationContext

    override fun peerCommands(fromId: String, fromName: String, commands: List<CommandData>) {
        _byDevice.update { it + (fromId to commands) }
    }

    override fun peerFinished(fromId: String, fromName: String, id: String, success: Boolean, message: String) {
        val name = _byDevice.value[fromId]?.find { it.id == id }?.name ?: id
        val text = if (success) {
            context.getString(R.string.command_done, name, fromName)
        } else {
            context.getString(R.string.command_failed, name, message)
        }
        Pairly.say(text)
    }

    companion object {
        private val _byDevice = MutableStateFlow<Map<String, List<CommandData>>>(emptyMap())
        val byDevice: StateFlow<Map<String, List<CommandData>>> = _byDevice.asStateFlow()
    }
}
