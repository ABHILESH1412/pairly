package dev.pairly.android.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import dev.pairly.android.R

/** Asks for Android 12+'s "Nearby devices" permission so Pairly can use Bluetooth. */
@Composable
fun BluetoothCard(onGrant: () -> Unit) {
    ElevatedCard(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp)) {
            Text(stringResource(R.string.bt_title), style = MaterialTheme.typography.titleMedium)
            Text(
                stringResource(R.string.bt_body),
                style = MaterialTheme.typography.bodyMedium,
                modifier = Modifier.padding(top = 4.dp, bottom = 12.dp),
            )
            FilledTonalButton(onClick = onGrant) { Text(stringResource(R.string.bt_grant)) }
        }
    }
}
