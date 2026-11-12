package dev.henos.android

import android.Manifest
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.codescanner.GmsBarcodeScannerOptions
import com.google.mlkit.vision.codescanner.GmsBarcodeScanning
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.text.DateFormat
import java.util.Date

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        HenosService.start(this)
        setContent { HenosTheme { App() } }
    }
}

private val Blue = Color(0xFF2A5FE6)
private val Green = Color(0xFF14853D)
private val Amber = Color(0xFFA15C00)
private val Grey = Color(0xFF9AA1B0)

@Composable
private fun HenosTheme(content: @Composable () -> Unit) {
    val colors = if (isSystemInDarkTheme()) {
        darkColorScheme(primary = Color(0xFF6F95FF))
    } else {
        lightColorScheme(primary = Blue)
    }
    MaterialTheme(colorScheme = colors, content = content)
}

/** Which panel of "Add a device" is open. */
private enum class Panel { None, Join }

@Composable
private fun App() {
    val state by HenosCore.state.collectAsState()
    val startError by HenosCore.startError.collectAsState()
    AskForPermissions()

    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        Column(
            Modifier
                .safeDrawingPadding()
                .verticalScroll(rememberScrollState())
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            val current = state
            when {
                current != null -> Content(current)
                startError != null -> ErrorText("HENOS could not start: $startError")
                else -> Text("Starting HENOS…", color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
    }
}

/** Asks once for what HENOS needs: its notification and the local network. */
@Composable
private fun AskForPermissions() {
    val launcher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions(),
    ) { HenosCore.refresh() }
    LaunchedEffect(Unit) {
        val wanted = buildList {
            if (Build.VERSION.SDK_INT >= 33) add(Manifest.permission.POST_NOTIFICATIONS)
            if (Build.VERSION.SDK_INT >= 36) add("android.permission.ACCESS_LOCAL_NETWORK")
        }
        if (wanted.isNotEmpty()) launcher.launch(wanted.toTypedArray())
    }
}

@Composable
private fun Content(state: UiState) {
    val scope = rememberCoroutineScope()
    var message by remember { mutableStateOf<String?>(null) }

    /** Runs a core action off the main thread and shows its error, if any. */
    fun act(action: (uniffi.henos_ffi.Henos) -> Unit) {
        scope.launch {
            message = withContext(Dispatchers.IO) { HenosCore.run(action) }
        }
    }

    Header(state) { paused -> act { it.setClipboardPaused(paused) } }
    PairedList(state.paired) { id -> act { it.unpair(id) } }
    AddDevice(state, message, onMessage = { message = it }, act = ::act)
    Activity(state.activity)
    Text(
        "HENOS ${state.version} · One Digital World",
        Modifier.fillMaxWidth(),
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        style = MaterialTheme.typography.bodySmall,
    )
}

@Composable
private fun Header(state: UiState, onPause: (Boolean) -> Unit) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f)) {
            Text(
                state.deviceName,
                style = MaterialTheme.typography.titleLarge,
                fontWeight = FontWeight.SemiBold,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Text(
                if (state.clipboardPaused) "Clipboard sharing paused" else "Clipboard sharing on",
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                style = MaterialTheme.typography.bodyMedium,
            )
        }
        Switch(checked = !state.clipboardPaused, onCheckedChange = { onPause(!it) })
    }
}

@Composable
private fun Section(title: String, content: @Composable () -> Unit) {
    Card(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
            Text(
                title.uppercase(),
                style = MaterialTheme.typography.labelMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            content()
        }
    }
}

@Composable
private fun Dot(color: Color) {
    Box(Modifier.size(9.dp).clip(CircleShape).background(color))
}

private fun platformName(platform: String) = when (platform) {
    "macos" -> "Mac"
    "windows" -> "Windows PC"
    "linux" -> "Linux PC"
    "android" -> "Android"
    "ios" -> "iPhone or iPad"
    else -> "Device"
}

@Composable
private fun PairedList(paired: List<PairedDevice>, onUnpair: (String) -> Unit) {
    var confirming by remember { mutableStateOf<String?>(null) }
    Section("Your devices") {
        if (paired.isEmpty()) {
            Text("No devices paired yet. Add one below.", color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        paired.forEachIndexed { index, device ->
            if (index > 0) HorizontalDivider()
            val (color, label) = when {
                device.connected -> Green to "Connected"
                device.nearby -> Amber to "Nearby, connecting"
                else -> Grey to "Not on this network"
            }
            Row(verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text(device.name, fontWeight = FontWeight.SemiBold, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Dot(color)
                        Spacer(Modifier.width(6.dp))
                        Text(
                            "${platformName(device.platform)} · $label",
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            style = MaterialTheme.typography.bodySmall,
                        )
                    }
                }
                if (confirming == device.id) {
                    TextButton(onClick = { confirming = null; onUnpair(device.id) }) {
                        Text("Unpair", color = MaterialTheme.colorScheme.error)
                    }
                    TextButton(onClick = { confirming = null }) { Text("Keep") }
                } else {
                    TextButton(onClick = { confirming = device.id }) { Text("Unpair") }
                }
            }
        }
    }
}

@Composable
private fun AddDevice(
    state: UiState,
    message: String?,
    onMessage: (String?) -> Unit,
    act: ((uniffi.henos_ffi.Henos) -> Unit) -> Unit,
) {
    val context = LocalContext.current
    var panel by remember { mutableStateOf(Panel.None) }

    Section("Add a device") {
        val offer = state.pairing
        when {
            offer != null -> ShowCode(offer) { act { it.stopPairing() } }
            panel == Panel.Join -> JoinPanel(
                state.nearby,
                onPair = { id, code -> act { it.pairWithCode(id, code) } },
                onCancel = { panel = Panel.None; onMessage(null) },
            )
            else -> {
                Button(
                    onClick = {
                        onMessage(null)
                        val options = GmsBarcodeScannerOptions.Builder()
                            .setBarcodeFormats(Barcode.FORMAT_QR_CODE)
                            .build()
                        GmsBarcodeScanning.getClient(context, options).startScan()
                            .addOnSuccessListener { code ->
                                val ticket = code.rawValue
                                if (ticket == null) {
                                    onMessage("That QR code could not be read.")
                                } else {
                                    act { it.pairWithTicket(ticket) }
                                }
                            }
                            .addOnFailureListener {
                                onMessage("The QR scanner could not start. Use Enter a code instead.")
                            }
                    },
                    modifier = Modifier.fillMaxWidth(),
                ) { Text("Scan a QR code") }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedButton(onClick = { onMessage(null); panel = Panel.Join }, Modifier.weight(1f)) {
                        Text("Enter a code")
                    }
                    OutlinedButton(onClick = { onMessage(null); act { it.startPairing() } }, Modifier.weight(1f)) {
                        Text("Show a code")
                    }
                }
            }
        }
        if (message != null) ErrorText(message)
    }

    // Tick the countdown while a code is on show.
    LaunchedEffect(state.pairing != null) {
        while (state.pairing != null) {
            delay(1000)
            HenosCore.refresh()
        }
    }
}

@Composable
private fun ShowCode(offer: PairingOffer, onCancel: () -> Unit) {
    Text(
        offer.code.take(3) + " " + offer.code.drop(3),
        fontSize = 40.sp,
        fontWeight = FontWeight.Bold,
        letterSpacing = 2.sp,
    )
    Text(
        "On the other device, choose Enter a code and type this.",
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
    Text(
        "Works once. Expires in %d:%02d.".format(offer.secondsLeft / 60, offer.secondsLeft % 60),
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        style = MaterialTheme.typography.bodySmall,
    )
    OutlinedButton(onClick = onCancel) { Text("Cancel") }
}

@Composable
private fun JoinPanel(
    nearby: List<NearbyDevice>,
    onPair: (id: String, code: String) -> Unit,
    onCancel: () -> Unit,
) {
    var selected by remember { mutableStateOf<String?>(null) }
    var code by remember { mutableStateOf("") }

    // Keep a sensible choice as devices come and go.
    if (nearby.none { it.id == selected }) {
        val showing = nearby.filter { it.pairing }
        selected = when {
            showing.size == 1 -> showing[0].id
            nearby.size == 1 -> nearby[0].id
            else -> null
        }
    }

    if (nearby.isEmpty()) {
        Text(
            "No other HENOS device found on this network yet. Open HENOS on the other device, then choose Show a code there.",
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
    nearby.forEach { device ->
        Row(
            Modifier.fillMaxWidth().clickable { selected = device.id },
            verticalAlignment = Alignment.CenterVertically,
        ) {
            RadioButton(selected = device.id == selected, onClick = { selected = device.id })
            Column {
                Text(device.name, maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text(
                    platformName(device.platform) +
                        if (device.pairing) " · showing a code" else " · not showing a code yet",
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    style = MaterialTheme.typography.bodySmall,
                )
            }
        }
    }
    OutlinedTextField(
        value = code,
        onValueChange = { typed -> code = typed.filter { it.isDigit() }.take(6) },
        label = { Text("6-digit code") },
        singleLine = true,
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.NumberPassword),
        modifier = Modifier.fillMaxWidth(),
    )
    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        Button(
            onClick = { selected?.let { onPair(it, code) } },
            enabled = selected != null && code.length == 6,
        ) { Text("Pair") }
        OutlinedButton(onClick = onCancel) { Text("Cancel") }
    }
}

@Composable
private fun Activity(entries: List<ActivityEntry>) {
    if (entries.isEmpty()) return
    val time = remember { DateFormat.getTimeInstance(DateFormat.SHORT) }
    Section("Recent activity") {
        entries.take(10).forEach { entry ->
            Row {
                Text(
                    time.format(Date(entry.atMs)),
                    Modifier.width(72.dp),
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    style = MaterialTheme.typography.bodySmall,
                )
                Text(entry.text, style = MaterialTheme.typography.bodyMedium)
            }
        }
    }
}

@Composable
private fun ErrorText(text: String) {
    Text(text, color = MaterialTheme.colorScheme.error)
}
