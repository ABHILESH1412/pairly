package dev.pairly.android.files

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.text.format.DateUtils
import android.text.format.Formatter
import android.webkit.MimeTypeMap
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.outlined.Description
import androidx.compose.material.icons.outlined.Folder
import androidx.compose.material.icons.outlined.Refresh
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.SnackbarResult
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.pairly.android.Pairly
import dev.pairly.android.R
import dev.pairly.android.share.Downloads
import dev.pairly.android.ui.theme.PairlyTheme
import dev.pairly.core.ffi.FileEntryData
import kotlinx.coroutines.launch

/**
 * Another device's files, from wherever this phone is: the folder a PC shares (over Wi-Fi,
 * Bluetooth or the internet relay). Tap a folder to open it, a file to download it into
 * Downloads/Pairly.
 */
class RemoteFilesActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val device = intent.getStringExtra(EXTRA_DEVICE) ?: run { finish(); return }
        val name = intent.getStringExtra(EXTRA_NAME).orEmpty()
        setContent { PairlyTheme { RemoteFiles(device, name, onClose = ::finish) } }
    }

    companion object {
        private const val EXTRA_DEVICE = "device"
        private const val EXTRA_NAME = "name"

        fun open(context: Context, device: String, name: String) {
            context.startActivity(
                Intent(context, RemoteFilesActivity::class.java)
                    .putExtra(EXTRA_DEVICE, device)
                    .putExtra(EXTRA_NAME, name),
            )
        }
    }
}

private sealed interface Listing {
    data object Loading : Listing
    data class Loaded(val entries: List<FileEntryData>) : Listing
    data class Failed(val reason: String) : Listing
}

/** A download in progress; updated from a background thread. */
private class Download(val name: String, total: Long) {
    var done by mutableLongStateOf(0L)
    var total by mutableLongStateOf(total)
}

private fun parent(path: String) = path.substringBeforeLast('/', "")

private fun child(path: String, name: String) = if (path.isEmpty()) name else "$path/$name"

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun RemoteFiles(device: String, name: String, onClose: () -> Unit) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val snackbar = remember { SnackbarHostState() }
    var path by rememberSaveable { mutableStateOf("") }
    var reload by remember { mutableIntStateOf(0) }
    var listing by remember { mutableStateOf<Listing>(Listing.Loading) }
    val downloads = remember { mutableStateListOf<Download>() }

    LaunchedEffect(path, reload) {
        listing = Listing.Loading
        listing = runCatching { Pairly.filesList(device, path) }
            .fold({ Listing.Loaded(it) }, { Listing.Failed(Pairly.errorText(it)) })
    }
    BackHandler(enabled = path.isNotEmpty()) { path = parent(path) }

    fun download(entry: FileEntryData) {
        val item = Download(entry.name, entry.size.toLong())
        downloads.add(item)
        scope.launch {
            val saved = runCatching {
                Pairly.filesDownload(device, child(path, entry.name), entry.name, entry.size.toLong()) { done, total ->
                    item.done = done
                    item.total = total
                }
            }
            downloads.remove(item)
            saved.onSuccess { uri ->
                val result = snackbar.showSnackbar(
                    context.getString(R.string.files_saved, entry.name),
                    actionLabel = context.getString(R.string.files_open),
                )
                if (result == SnackbarResult.ActionPerformed) {
                    val type = MimeTypeMap.getSingleton()
                        .getMimeTypeFromExtension(entry.name.substringAfterLast('.', "").lowercase()) ?: "*/*"
                    val view = Intent(Intent.ACTION_VIEW)
                        .setDataAndType(Downloads.viewable(context, uri), type)
                        .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                    try {
                        context.startActivity(view)
                    } catch (_: ActivityNotFoundException) {
                        snackbar.showSnackbar(context.getString(R.string.files_no_app))
                    }
                }
            }.onFailure {
                snackbar.showSnackbar(context.getString(R.string.files_download_failed, entry.name, Pairly.errorText(it)))
            }
        }
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Column {
                        Text(name, maxLines = 1, overflow = TextOverflow.Ellipsis)
                        Text(
                            "/$path",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            maxLines = 1,
                            overflow = TextOverflow.StartEllipsis,
                        )
                    }
                },
                navigationIcon = {
                    IconButton(onClick = { if (path.isEmpty()) onClose() else path = parent(path) }) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = stringResource(R.string.action_back))
                    }
                },
                actions = {
                    IconButton(onClick = { reload++ }) {
                        Icon(Icons.Outlined.Refresh, contentDescription = stringResource(R.string.files_refresh))
                    }
                },
            )
        },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.padding(padding).fillMaxSize()) {
            downloads.forEach { d ->
                Column(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 6.dp)) {
                    Text(
                        stringResource(R.string.files_downloading, d.name),
                        style = MaterialTheme.typography.bodySmall,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    if (d.total > 0) {
                        LinearProgressIndicator(
                            progress = { (d.done.toFloat() / d.total).coerceIn(0f, 1f) },
                            modifier = Modifier.fillMaxWidth().padding(top = 4.dp),
                        )
                    } else {
                        LinearProgressIndicator(Modifier.fillMaxWidth().padding(top = 4.dp))
                    }
                }
            }
            when (val l = listing) {
                Listing.Loading -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                    CircularProgressIndicator()
                }
                is Listing.Failed -> Column(
                    Modifier.fillMaxSize().padding(32.dp),
                    verticalArrangement = Arrangement.spacedBy(16.dp, Alignment.CenterVertically),
                    horizontalAlignment = Alignment.CenterHorizontally,
                ) {
                    Text(l.reason, style = MaterialTheme.typography.bodyLarge)
                    Button(onClick = { reload++ }) { Text(stringResource(R.string.files_retry)) }
                }
                is Listing.Loaded -> if (l.entries.isEmpty()) {
                    Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                        Text(stringResource(R.string.files_empty), color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                } else {
                    LazyColumn(Modifier.fillMaxSize()) {
                        items(l.entries, key = { it.name }) { entry ->
                            ListItem(
                                headlineContent = { Text(entry.name, maxLines = 2, overflow = TextOverflow.Ellipsis) },
                                supportingContent = {
                                    val time = entry.modifiedMs.takeIf { it > 0 }
                                        ?.let { DateUtils.formatDateTime(context, it, DateUtils.FORMAT_SHOW_DATE or DateUtils.FORMAT_ABBREV_MONTH) }
                                    val size = if (entry.dir) null else Formatter.formatShortFileSize(context, entry.size.toLong())
                                    listOfNotNull(size, time).joinToString(" · ").takeIf { it.isNotEmpty() }?.let { Text(it) }
                                },
                                leadingContent = {
                                    Icon(if (entry.dir) Icons.Outlined.Folder else Icons.Outlined.Description, contentDescription = null)
                                },
                                modifier = Modifier.clickable {
                                    if (entry.dir) path = child(path, entry.name) else download(entry)
                                },
                            )
                            HorizontalDivider()
                        }
                    }
                }
            }
        }
    }
}
