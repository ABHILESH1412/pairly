package dev.pairly.android.share

import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.core.content.IntentCompat
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.lifecycleScope
import dev.pairly.android.Pairly
import dev.pairly.android.PairlyService
import dev.pairly.android.R
import dev.pairly.android.ui.theme.PairlyTheme
import dev.pairly.core.ffi.Device
import kotlinx.coroutines.launch

/** What another app shared with Pairly. */
private sealed interface Payload {
    data class Files(val uris: List<Uri>) : Payload
    data class Text(val text: String) : Payload
}

/**
 * The system share sheet's "Pairly" entry: pick a connected device and send the shared files,
 * link or text to it.
 */
class ShareActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        PairlyService.start(this)
        val payload = parse(intent)
        if (payload == null) {
            finish()
            return
        }
        setContent {
            PairlyTheme {
                ShareDialog(payload, onPick = { send(payload, it) }, onDismiss = ::finish)
            }
        }
    }

    private fun send(payload: Payload, device: Device) {
        lifecycleScope.launch {
            when (payload) {
                // Open the files before finishing: the permission to read them ends with us.
                is Payload.Files -> Pairly.sendFiles(device, payload.uris)
                is Payload.Text -> Pairly.sendText(device, payload.text)
            }
            finish()
        }
    }

    private fun parse(intent: Intent): Payload? {
        val streams = when (intent.action) {
            Intent.ACTION_SEND ->
                listOfNotNull(IntentCompat.getParcelableExtra(intent, Intent.EXTRA_STREAM, Uri::class.java))
            Intent.ACTION_SEND_MULTIPLE ->
                IntentCompat.getParcelableArrayListExtra(intent, Intent.EXTRA_STREAM, Uri::class.java).orEmpty()
            else -> return null
        }
        if (streams.isNotEmpty()) return Payload.Files(streams)
        val text = intent.getCharSequenceExtra(Intent.EXTRA_TEXT)?.toString()?.trim()
        return text?.takeIf { it.isNotEmpty() }?.let(Payload::Text)
    }

    override fun finish() {
        super.finish()
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            @Suppress("DEPRECATION")
            overridePendingTransition(0, 0)
        }
    }
}

@Composable
private fun ShareDialog(payload: Payload, onPick: (Device) -> Unit, onDismiss: () -> Unit) {
    val state by Pairly.state.collectAsStateWithLifecycle()
    var sending by remember { mutableStateOf(false) }
    val connected = state.devices.filter { it.paired && it.link != null }
    val title = when (payload) {
        is Payload.Files -> pluralStringResource(R.plurals.share_files_title, payload.uris.size, payload.uris.size)
        is Payload.Text ->
            stringResource(if (SharePrefs.isWebUrl(payload.text)) R.string.share_link_title else R.string.share_text_title)
    }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            Column {
                when {
                    state.self == null || sending -> {
                        Text(stringResource(R.string.starting))
                        LinearProgressIndicator(Modifier.fillMaxWidth().padding(top = 12.dp))
                    }
                    connected.isEmpty() -> Text(stringResource(R.string.share_no_devices))
                    else -> connected.forEach { device ->
                        ListItem(
                            headlineContent = { Text(device.name) },
                            supportingContent = {
                                Text(
                                    stringResource(R.string.share_tap_to_send),
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                            },
                            modifier = Modifier.clickable {
                                sending = true
                                onPick(device)
                            },
                        )
                    }
                }
            }
        },
        confirmButton = {},
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.action_cancel)) } },
    )
}
