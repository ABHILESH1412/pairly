package dev.pairly.android.ui

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.os.Build
import android.provider.Settings
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import dev.pairly.android.R
import dev.pairly.android.notifications.PairlyNotificationListener
import dev.pairly.android.notifications.PhoneNotifications

fun openNotificationAccessSettings(context: Context) {
    val intent = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
        Intent(Settings.ACTION_NOTIFICATION_LISTENER_DETAIL_SETTINGS).putExtra(
            Settings.EXTRA_NOTIFICATION_LISTENER_COMPONENT_NAME,
            ComponentName(context, PairlyNotificationListener::class.java).flattenToString(),
        )
    } else {
        Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS)
    }
    context.startActivity(intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
}

/** Asks for notification access, or once granted links to the per-app picker. */
@Composable
fun NotificationAccessCard(granted: Boolean, onChooseApps: () -> Unit) {
    val context = LocalContext.current
    if (!granted) {
        ElevatedCard(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(16.dp)) {
                Text(stringResource(R.string.notif_access_title), style = MaterialTheme.typography.titleMedium)
                Text(
                    stringResource(R.string.notif_access_body),
                    style = MaterialTheme.typography.bodyMedium,
                    modifier = Modifier.padding(top = 4.dp, bottom = 12.dp),
                )
                Button(onClick = { openNotificationAccessSettings(context) }) {
                    Text(stringResource(R.string.notif_access_grant))
                }
            }
        }
    } else {
        OutlinedCard(Modifier.fillMaxWidth()) {
            Row(Modifier.padding(start = 16.dp, end = 8.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(stringResource(R.string.notif_sending), modifier = Modifier.weight(1f))
                TextButton(onClick = onChooseApps) { Text(stringResource(R.string.notif_choose_apps)) }
            }
        }
    }
}

/** Per-app switches for which notifications are sent. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun AppsScreen(onBack: () -> Unit) {
    BackHandler(onBack = onBack)
    val prefs = PhoneNotifications.prefs(LocalContext.current)
    val apps by prefs.apps.collectAsStateWithLifecycle()
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(stringResource(R.string.apps_title)) },
                actions = { TextButton(onClick = onBack) { Text(stringResource(R.string.action_done)) } },
            )
        },
    ) { padding ->
        if (apps.isEmpty()) {
            Column(Modifier.fillMaxSize().padding(padding).padding(24.dp)) {
                Text(stringResource(R.string.apps_empty))
            }
            return@Scaffold
        }
        LazyColumn(Modifier.fillMaxSize().padding(padding)) {
            items(apps, key = { it.packageName }) { app ->
                ListItem(
                    headlineContent = { Text(app.label) },
                    supportingContent = { Text(app.packageName, style = MaterialTheme.typography.bodySmall) },
                    trailingContent = {
                        Switch(checked = !app.muted, onCheckedChange = { send -> prefs.setMuted(app.packageName, !send) })
                    },
                )
            }
            item { Spacer(Modifier.padding(8.dp)) }
        }
    }
}
