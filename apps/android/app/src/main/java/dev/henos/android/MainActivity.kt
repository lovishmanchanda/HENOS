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
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.ColorScheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
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
import androidx.compose.ui.unit.Dp
import androidx.compose.foundation.Canvas
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.semantics.Role
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

    override fun onResume() {
        super.onResume()
        // Opening the app is a good moment to look for devices.
        kotlin.concurrent.thread(name = "henos-look") { HenosCore.run { it.lookNow() } }
    }
}

// The same quiet, paper-like look as the desktop app: one accent colour,
// grouped rounded lists, nothing decorative.
private val LightColors = lightColorScheme(
    primary = Color(0xFF2A5FE6),
    onPrimary = Color.White,
    background = Color(0xFFF5F4F0),
    surface = Color.White,
    onBackground = Color(0xFF1B1B1F),
    onSurface = Color(0xFF1B1B1F),
    onSurfaceVariant = Color(0xFF6D6D76),
    outline = Color(0xFFA2A2AB),
    outlineVariant = Color(0x14141E1E),
    error = Color(0xFFCF3A3A),
    surfaceVariant = Color(0xFFF5F4F0),
)
private val DarkColors = darkColorScheme(
    primary = Color(0xFF7EA2FF),
    onPrimary = Color(0xFF0C1326),
    background = Color(0xFF161618),
    surface = Color(0xFF212124),
    onBackground = Color(0xFFF2F2F4),
    onSurface = Color(0xFFF2F2F4),
    onSurfaceVariant = Color(0xFFA0A0AA),
    outline = Color(0xFF6C6C76),
    outlineVariant = Color(0x14FFFFFF),
    error = Color(0xFFFF7A7A),
    surfaceVariant = Color(0xFF161618),
)
private val Ok = Color(0xFF1F9D55)
private val Wait = Color(0xFFC98A1B)

@Composable
private fun HenosTheme(content: @Composable () -> Unit) {
    val colors: ColorScheme = if (isSystemInDarkTheme()) DarkColors else LightColors
    MaterialTheme(colorScheme = colors, content = content)
}

/** Which part of "Add a device" is open. */
private enum class Panel { Closed, Choose, Join }

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
                .padding(horizontal = 18.dp, vertical = 24.dp),
            verticalArrangement = Arrangement.spacedBy(22.dp),
        ) {
            val current = state
            when {
                current != null -> Content(current)
                startError != null -> ErrorText("HENOS could not start: $startError")
                else -> Text("Starting…", color = MaterialTheme.colorScheme.onSurfaceVariant)
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

    Top(state) { paused -> act { it.setClipboardPaused(paused) } }
    Files(state.transfers, act = ::act)
    Devices(state, message, onMessage = { message = it }, act = ::act)
    Recent(state.activity)
    Text(
        "HENOS ${state.version} · One Digital World · end-to-end encrypted",
        Modifier.fillMaxWidth(),
        color = MaterialTheme.colorScheme.outline,
        style = MaterialTheme.typography.bodySmall,
        textAlign = androidx.compose.ui.text.style.TextAlign.Center,
    )
}

@Composable
private fun Eyebrow(text: String, modifier: Modifier = Modifier) {
    Text(
        text.uppercase(),
        modifier,
        color = MaterialTheme.colorScheme.outline,
        fontSize = 12.sp,
        fontWeight = FontWeight.SemiBold,
        letterSpacing = 0.8.sp,
    )
}

private fun summary(state: UiState): String {
    if (state.clipboardPaused) return "Sharing is paused"
    val connected = state.paired.count { it.connected }
    return when {
        state.paired.isEmpty() -> "Add a device to start sharing"
        connected == 0 -> "None of your devices are nearby"
        connected == 1 -> "Sharing with 1 device"
        else -> "Sharing with $connected devices"
    }
}

@Composable
private fun Top(state: UiState, onPause: (Boolean) -> Unit) {
    Row(verticalAlignment = Alignment.Bottom) {
        Column(Modifier.weight(1f)) {
            Eyebrow("This phone")
            Text(
                state.deviceName,
                fontSize = 28.sp,
                fontWeight = FontWeight.Bold,
                letterSpacing = (-0.5).sp,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Text(summary(state), color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        Switch(
            checked = !state.clipboardPaused,
            onCheckedChange = { onPause(!it) },
            colors = SwitchDefaults.colors(checkedTrackColor = Ok, checkedThumbColor = Color.White),
        )
    }
}

/** A titled group whose rows sit on one rounded sheet. */
@Composable
private fun Group(title: String, content: @Composable () -> Unit) {
    Column {
        Eyebrow(title, Modifier.padding(start = 14.dp, bottom = 7.dp))
        Column(
            Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(14.dp))
                .background(MaterialTheme.colorScheme.surface),
        ) { content() }
    }
}

@Composable
private fun Hairline(inset: Dp = 52.dp) {
    Box(
        Modifier
            .padding(start = inset)
            .fillMaxWidth()
            .height(1.dp)
            .background(MaterialTheme.colorScheme.outlineVariant),
    )
}

@Composable
private fun Dot(color: Color) {
    Box(Modifier.size(7.dp).clip(CircleShape).background(color))
}

/** A small line drawing of a phone or a computer. */
@Composable
private fun DeviceGlyph(platform: String, tint: Color) {
    val phone = platform == "android" || platform == "ios"
    Canvas(Modifier.size(24.dp)) {
        val stroke = Stroke(width = 1.6.dp.toPx(), cap = StrokeCap.Round)
        val u = size.width / 24f
        if (phone) {
            drawRoundRect(tint, Offset(7 * u, 3 * u), Size(10 * u, 18 * u), CornerRadius(2.5f * u), stroke)
            drawLine(tint, Offset(11 * u, 18 * u), Offset(13 * u, 18 * u), stroke.width, StrokeCap.Round)
        } else {
            drawRoundRect(tint, Offset(4 * u, 5 * u), Size(16 * u, 11 * u), CornerRadius(2 * u), stroke)
            drawLine(tint, Offset(2.5f * u, 19.5f * u), Offset(21.5f * u, 19.5f * u), stroke.width, StrokeCap.Round)
        }
    }
}

@Composable
private fun DeviceLine(
    name: String,
    platform: String,
    dot: Color,
    status: String,
    modifier: Modifier = Modifier,
    tint: Color = MaterialTheme.colorScheme.onSurfaceVariant,
    trailing: @Composable () -> Unit = {},
) {
    Row(
        modifier.fillMaxWidth().heightIn(min = 56.dp).padding(horizontal = 14.dp, vertical = 9.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        DeviceGlyph(platform, tint)
        Spacer(Modifier.width(14.dp))
        Column(Modifier.weight(1f)) {
            Text(name, fontWeight = FontWeight.SemiBold, maxLines = 1, overflow = TextOverflow.Ellipsis)
            Row(verticalAlignment = Alignment.CenterVertically) {
                Dot(dot)
                Spacer(Modifier.width(6.dp))
                Text(status, color = MaterialTheme.colorScheme.onSurfaceVariant, fontSize = 13.sp)
            }
        }
        trailing()
    }
}

@Composable
private fun Devices(
    state: UiState,
    message: String?,
    onMessage: (String?) -> Unit,
    act: ((uniffi.henos_ffi.Henos) -> Unit) -> Unit,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var panel by remember { mutableStateOf(Panel.Closed) }
    var confirming by remember { mutableStateOf<String?>(null) }
    val faint = MaterialTheme.colorScheme.outline

    // Picking files to send to one device.
    var sendTo by remember { mutableStateOf<String?>(null) }
    val picker = rememberLauncherForActivityResult(
        ActivityResultContracts.OpenMultipleDocuments(),
    ) { picked ->
        val target = sendTo
        sendTo = null
        if (picked.isNotEmpty() && target != null) {
            scope.launch {
                val error = withContext(Dispatchers.IO) {
                    HenosCore.sendFiles(context.applicationContext, picked, target)
                }
                onMessage(error)
            }
        }
    }

    Group("Devices") {
        state.paired.forEach { device ->
            val (dot, label) = when {
                device.connected -> Ok to "Connected"
                device.nearby -> Wait to "Connecting"
                else -> faint to "Not nearby"
            }
            DeviceLine(device.name, device.platform, dot, label) {
                if (confirming == device.id) {
                    TextButton(onClick = { confirming = null; act { it.unpair(device.id) } }) {
                        Text("Unpair", color = MaterialTheme.colorScheme.error)
                    }
                    TextButton(onClick = { confirming = null }) { Text("Keep") }
                } else {
                    if (device.connected) {
                        TextButton(onClick = { sendTo = device.id; picker.launch(arrayOf("*/*")) }) {
                            Text("Send file")
                        }
                    }
                    TextButton(onClick = { confirming = device.id }) { Text("Unpair", color = faint) }
                }
            }
            Hairline()
        }

        val offer = state.pairing
        when {
            offer != null -> Padded { ShowCode(offer) { act { it.stopPairing() } } }
            panel == Panel.Join -> JoinPanel(
                state.nearby,
                message,
                onPair = { id, code -> act { it.pairWithCode(id, code) } },
                onCancel = { panel = Panel.Closed; onMessage(null) },
            )
            panel == Panel.Choose -> Padded {
                Text(
                    "Scan the code a computer shows, or pair with a 6-digit code.",
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
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
                    shape = RoundedCornerShape(10.dp),
                ) { Text("Scan a QR code") }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    QuietButton("Enter a code", Modifier.weight(1f)) { onMessage(null); panel = Panel.Join }
                    QuietButton("Show a code", Modifier.weight(1f)) { onMessage(null); act { it.startPairing() } }
                }
                TextButton(onClick = { panel = Panel.Closed; onMessage(null) }) { Text("Cancel", color = faint) }
                if (message != null) ErrorText(message)
            }
            else -> Row(
                Modifier
                    .fillMaxWidth()
                    .clickable(role = Role.Button) {
                        onMessage(null)
                        panel = Panel.Choose
                        act { it.lookNow() }
                    }
                    .heightIn(min = 56.dp)
                    .padding(horizontal = 14.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                PlusGlyph(MaterialTheme.colorScheme.primary)
                Spacer(Modifier.width(14.dp))
                Text("Add a device", color = MaterialTheme.colorScheme.primary, fontWeight = FontWeight.SemiBold)
            }
        }
        // A pairing that just succeeded closes the panel.
        LaunchedEffect(state.paired.size) { panel = Panel.Closed }
    }

    // The only timer: ticking the countdown while a code is on show.
    LaunchedEffect(state.pairing != null) {
        while (state.pairing != null) {
            delay(1000)
            HenosCore.refresh()
        }
    }
}

private fun formatSize(bytes: Long): String = when {
    bytes < 1024 -> "$bytes bytes"
    bytes < 1024 * 1024 -> "%.1f KB".format(bytes / 1024.0)
    bytes < 1024L * 1024 * 1024 -> "%.1f MB".format(bytes / (1024.0 * 1024))
    else -> "%.1f GB".format(bytes / (1024.0 * 1024 * 1024))
}

/** Files being offered, sent or received. Shown only when there are any. */
@Composable
private fun Files(transfers: List<Transfer>, act: ((uniffi.henos_ffi.Henos) -> Unit) -> Unit) {
    if (transfers.isEmpty()) return
    val faint = MaterialTheme.colorScheme.outline
    Group("Files") {
        transfers.forEachIndexed { index, transfer ->
            if (index > 0) Hairline(inset = 14.dp)
            val id = transfer.id.toULong()
            Column(Modifier.padding(horizontal = 14.dp, vertical = 10.dp)) {
                Text(transfer.name, fontWeight = FontWeight.SemiBold, maxLines = 1, overflow = TextOverflow.Ellipsis)
                val percent = if (transfer.size > 0) (transfer.bytes * 100 / transfer.size).toInt() else 100
                val line = when (transfer.status) {
                    "offered" -> "${transfer.deviceName} wants to send this · ${formatSize(transfer.size)}"
                    "waiting" -> "Waiting for ${transfer.deviceName} to accept · ${formatSize(transfer.size)}"
                    "active" -> (if (transfer.incoming) "Receiving from " else "Sending to ") +
                        "${transfer.deviceName} · $percent%"
                    "done" -> if (transfer.incoming) {
                        "From ${transfer.deviceName} · saved in Downloads, in the HENOS folder"
                    } else {
                        "Sent to ${transfer.deviceName} · ${formatSize(transfer.size)}"
                    }
                    else -> transfer.detail ?: "Did not complete."
                }
                Text(
                    line,
                    color = if (transfer.status == "failed") MaterialTheme.colorScheme.error
                    else MaterialTheme.colorScheme.onSurfaceVariant,
                    fontSize = 13.sp,
                )
                if (transfer.status == "active") {
                    Spacer(Modifier.height(6.dp))
                    Box(
                        Modifier.fillMaxWidth().height(4.dp).clip(RoundedCornerShape(2.dp))
                            .background(MaterialTheme.colorScheme.outlineVariant),
                    ) {
                        Box(
                            Modifier.fillMaxWidth(percent / 100f).height(4.dp)
                                .background(MaterialTheme.colorScheme.primary),
                        )
                    }
                }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    when (transfer.status) {
                        "offered" -> {
                            Button(
                                onClick = { act { it.acceptFile(id) } },
                                shape = RoundedCornerShape(10.dp),
                            ) { Text("Accept") }
                            QuietButton("Decline") { act { it.declineFile(id) } }
                        }
                        "waiting", "active" -> QuietButton("Cancel") { act { it.cancelTransfer(id) } }
                        else -> TextButton(onClick = { act { it.dismissTransfer(id) } }) {
                            Text("Clear", color = faint)
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun PlusGlyph(tint: Color) {
    Canvas(Modifier.size(24.dp)) {
        val u = size.width / 24f
        val w = 1.8.dp.toPx()
        drawCircle(tint, 9 * u, style = Stroke(w))
        drawLine(tint, Offset(12 * u, 8 * u), Offset(12 * u, 16 * u), w, StrokeCap.Round)
        drawLine(tint, Offset(8 * u, 12 * u), Offset(16 * u, 12 * u), w, StrokeCap.Round)
    }
}

@Composable
private fun Padded(content: @Composable () -> Unit) {
    Column(Modifier.padding(14.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) { content() }
}

@Composable
private fun QuietButton(label: String, modifier: Modifier = Modifier, onClick: () -> Unit) {
    Button(
        onClick = onClick,
        modifier = modifier,
        shape = RoundedCornerShape(10.dp),
        colors = ButtonDefaults.buttonColors(
            containerColor = MaterialTheme.colorScheme.background,
            contentColor = MaterialTheme.colorScheme.onSurface,
        ),
    ) { Text(label) }
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
        "Works once · expires in %d:%02d".format(offer.secondsLeft / 60, offer.secondsLeft % 60),
        color = MaterialTheme.colorScheme.outline,
        fontSize = 13.sp,
    )
    QuietButton("Cancel", onClick = onCancel)
}

@Composable
private fun JoinPanel(
    nearby: List<NearbyDevice>,
    message: String?,
    onPair: (id: String, code: String) -> Unit,
    onCancel: () -> Unit,
) {
    var selected by remember { mutableStateOf<String?>(null) }
    var code by remember { mutableStateOf("") }
    val faint = MaterialTheme.colorScheme.outline

    // Keep a sensible choice as devices come and go.
    if (nearby.none { it.id == selected }) {
        val showing = nearby.filter { it.pairing }
        selected = when {
            showing.size == 1 -> showing[0].id
            nearby.size == 1 -> nearby[0].id
            else -> null
        }
    }

    nearby.forEach { device ->
        val chosen = device.id == selected
        DeviceLine(
            device.name,
            device.platform,
            if (device.pairing) Ok else faint,
            if (device.pairing) "Showing a code" else "Not showing a code yet",
            modifier = Modifier
                .background(
                    if (chosen) MaterialTheme.colorScheme.primary.copy(alpha = 0.10f) else Color.Transparent,
                )
                .clickable(role = Role.RadioButton) { selected = device.id },
            tint = if (chosen) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Hairline()
    }
    Padded {
        if (nearby.isEmpty()) {
            Text(
                "No other HENOS device found on this network yet. Open HENOS on the other device and choose Show a code.",
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        OutlinedTextField(
            value = code,
            onValueChange = { typed -> code = typed.filter { it.isDigit() }.take(6) },
            label = { Text("6-digit code") },
            singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.NumberPassword),
            shape = RoundedCornerShape(10.dp),
            modifier = Modifier.fillMaxWidth(),
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(
                onClick = { selected?.let { onPair(it, code) } },
                enabled = selected != null && code.length == 6,
                shape = RoundedCornerShape(10.dp),
            ) { Text("Pair") }
            QuietButton("Cancel", onClick = onCancel)
        }
        if (message != null) ErrorText(message)
    }
}

@Composable
private fun Recent(entries: List<ActivityEntry>) {
    if (entries.isEmpty()) return
    val time = remember { DateFormat.getTimeInstance(DateFormat.SHORT) }
    Group("Recent") {
        entries.take(8).forEachIndexed { index, entry ->
            if (index > 0) Hairline(inset = 14.dp)
            Row(Modifier.padding(horizontal = 14.dp, vertical = 10.dp)) {
                Text(
                    time.format(Date(entry.atMs)),
                    Modifier.width(70.dp),
                    color = MaterialTheme.colorScheme.outline,
                    fontSize = 13.sp,
                )
                Text(entry.text, color = MaterialTheme.colorScheme.onSurfaceVariant, fontSize = 14.sp)
            }
        }
    }
}

@Composable
private fun ErrorText(text: String) {
    Text(
        text,
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(10.dp))
            .background(MaterialTheme.colorScheme.error.copy(alpha = 0.10f))
            .padding(horizontal = 12.dp, vertical = 8.dp),
        color = MaterialTheme.colorScheme.error,
    )
}
