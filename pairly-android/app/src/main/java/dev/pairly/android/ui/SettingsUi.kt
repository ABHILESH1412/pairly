package dev.pairly.android.ui

import android.content.Intent
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.outlined.OpenInNew
import androidx.compose.material.icons.outlined.BrightnessAuto
import androidx.compose.material.icons.outlined.ChevronRight
import androidx.compose.material.icons.outlined.DarkMode
import androidx.compose.material.icons.outlined.Edit
import androidx.compose.material.icons.outlined.LightMode
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LargeTopAppBar
import androidx.compose.material3.ListItem
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.unit.dp
import androidx.core.net.toUri
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import dev.pairly.android.AppSettings
import dev.pairly.android.Pairly
import dev.pairly.android.PairlyService
import dev.pairly.android.R
import dev.pairly.android.device.ClipboardSync
import dev.pairly.android.device.PairlyAccessibility
import dev.pairly.android.share.SharePrefs

private const val SOURCE_URL = "https://github.com/ABHILESH1412/pairly"

/** The app's settings: look, this phone's name, sharing, and what this version is. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SettingsScreen(onBack: () -> Unit, onChooseApps: () -> Unit) {
    BackHandler(onBack = onBack)
    val context = LocalContext.current
    val state by Pairly.state.collectAsStateWithLifecycle()
    val theme by AppSettings.theme.collectAsStateWithLifecycle()
    val enabled by AppSettings.enabled.collectAsStateWithLifecycle()
    var askBeforeReceiving by remember { mutableStateOf(SharePrefs.askBeforeReceiving(context)) }
    var autoClipboard by remember { mutableStateOf(ClipboardSync.auto(context)) }
    var renaming by remember { mutableStateOf(false) }
    val version = remember {
        runCatching { context.packageManager.getPackageInfo(context.packageName, 0).versionName }.getOrNull().orEmpty()
    }
    val scroll = TopAppBarDefaults.exitUntilCollapsedScrollBehavior()
    Scaffold(
        modifier = Modifier.nestedScroll(scroll.nestedScrollConnection),
        topBar = {
            LargeTopAppBar(
                title = { Text(stringResource(R.string.settings_title)) },
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = stringResource(R.string.action_back))
                    }
                },
                scrollBehavior = scroll,
            )
        },
    ) { padding ->
        LazyColumn(
            contentPadding = PaddingValues(
                start = 16.dp,
                end = 16.dp,
                top = padding.calculateTopPadding(),
                bottom = padding.calculateBottomPadding() + 24.dp,
            ),
        ) {
            item {
                SettingsCard {
                    SettingsRow(
                        title = stringResource(if (enabled) R.string.settings_on else R.string.settings_off),
                        body = stringResource(R.string.settings_on_body),
                        trailing = { Switch(enabled, onCheckedChange = { PairlyService.setEnabled(context, it) }) },
                    )
                }
            }
            item { SettingsHeader(stringResource(R.string.settings_appearance)) }
            item {
                SettingsCard {
                    val options = listOf(
                        Triple(AppSettings.Theme.SYSTEM, Icons.Outlined.BrightnessAuto, R.string.theme_system),
                        Triple(AppSettings.Theme.LIGHT, Icons.Outlined.LightMode, R.string.theme_light),
                        Triple(AppSettings.Theme.DARK, Icons.Outlined.DarkMode, R.string.theme_dark),
                    )
                    SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth().padding(16.dp)) {
                        options.forEachIndexed { i, (value, icon, label) ->
                            SegmentedButton(
                                selected = theme == value,
                                onClick = { AppSettings.setTheme(context, value) },
                                shape = SegmentedButtonDefaults.itemShape(i, options.size),
                                icon = { Icon(icon, contentDescription = null) },
                            ) { Text(stringResource(label), maxLines = 1) }
                        }
                    }
                }
            }

            item { SettingsHeader(stringResource(R.string.settings_this_phone)) }
            item {
                SettingsCard {
                    SettingsRow(
                        title = stringResource(R.string.settings_device_name),
                        body = state.self?.name ?: Pairly.systemDeviceName(context),
                        trailing = { Icon(Icons.Outlined.Edit, contentDescription = null) },
                        onClick = { renaming = true },
                    )
                    state.self?.let { self ->
                        SettingsRow(title = stringResource(R.string.settings_device_id), body = self.id)
                    }
                }
            }

            item { SettingsHeader(stringResource(R.string.settings_sharing)) }
            item {
                SettingsCard {
                    SettingsRow(
                        title = stringResource(R.string.share_ask_setting),
                        body = stringResource(R.string.share_ask_setting_body),
                        trailing = {
                            Switch(askBeforeReceiving, onCheckedChange = {
                                askBeforeReceiving = it
                                SharePrefs.setAskBeforeReceiving(context, it)
                            })
                        },
                    )
                    SettingsRow(
                        title = stringResource(R.string.clipboard_auto_setting),
                        body = stringResource(
                            if (PairlyAccessibility.enabled(context)) R.string.clipboard_auto_setting_body else R.string.clipboard_auto_needs_access,
                        ),
                        trailing = {
                            Switch(autoClipboard, onCheckedChange = {
                                autoClipboard = it
                                ClipboardSync.setAuto(context, it)
                            })
                        },
                    )
                    SettingsRow(
                        title = stringResource(R.string.settings_notification_apps),
                        body = stringResource(R.string.settings_notification_apps_body),
                        trailing = { Icon(Icons.Outlined.ChevronRight, contentDescription = null) },
                        onClick = onChooseApps,
                    )
                }
            }

            item { SettingsHeader(stringResource(R.string.settings_about)) }
            item {
                SettingsCard {
                    SettingsRow(title = stringResource(R.string.settings_version), body = version)
                    SettingsRow(title = stringResource(R.string.settings_license), body = "GPL-3.0")
                    SettingsRow(
                        title = stringResource(R.string.settings_source),
                        body = SOURCE_URL.removePrefix("https://"),
                        trailing = { Icon(Icons.AutoMirrored.Outlined.OpenInNew, contentDescription = null) },
                        onClick = {
                            runCatching {
                                context.startActivity(Intent(Intent.ACTION_VIEW, SOURCE_URL.toUri()))
                            }
                        },
                    )
                }
            }
        }
    }
    if (renaming) {
        RenameDialog(
            current = state.self?.name ?: Pairly.systemDeviceName(context),
            systemName = Pairly.systemDeviceName(context),
            onDismiss = { renaming = false },
            onRename = { name ->
                renaming = false
                Pairly.rename(name)
            },
        )
    }
}

@Composable
private fun RenameDialog(current: String, systemName: String, onDismiss: () -> Unit, onRename: (String?) -> Unit) {
    var text by remember { mutableStateOf(current) }
    val valid = text.isNotBlank() && text.trim().length <= 64
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.settings_rename_title)) },
        text = {
            Column {
                Text(stringResource(R.string.settings_rename_body))
                OutlinedTextField(
                    value = text,
                    onValueChange = { text = it.take(64) },
                    singleLine = true,
                    isError = !valid,
                    keyboardOptions = KeyboardOptions(imeAction = ImeAction.Done),
                    modifier = Modifier.fillMaxWidth().padding(top = 16.dp),
                )
                TextButton(onClick = { onRename(null) }, modifier = Modifier.padding(top = 4.dp)) {
                    Text(stringResource(R.string.settings_rename_reset, systemName))
                }
            }
        },
        confirmButton = {
            TextButton(onClick = { onRename(text.trim()) }, enabled = valid) { Text(stringResource(R.string.action_save)) }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.action_cancel)) } },
    )
}

@Composable
private fun SettingsHeader(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.padding(start = 8.dp, top = 20.dp, bottom = 8.dp),
    )
}

/** A rounded group of settings rows. */
@Composable
private fun SettingsCard(content: @Composable () -> Unit) {
    Card(
        shape = RoundedCornerShape(28.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainer),
        modifier = Modifier.fillMaxWidth(),
    ) { Column(Modifier.padding(vertical = 4.dp)) { content() } }
}

@Composable
private fun SettingsRow(
    title: String,
    body: String? = null,
    trailing: (@Composable () -> Unit)? = null,
    icon: ImageVector? = null,
    onClick: (() -> Unit)? = null,
) {
    ListItem(
        headlineContent = { Text(title) },
        supportingContent = body?.let { { Text(it) } },
        leadingContent = icon?.let { { Icon(it, contentDescription = null) } },
        trailingContent = trailing,
        colors = ListItemDefaults.colors(containerColor = androidx.compose.ui.graphics.Color.Transparent),
        modifier = if (onClick != null) Modifier.clickable(onClick = onClick) else Modifier,
    )
}
