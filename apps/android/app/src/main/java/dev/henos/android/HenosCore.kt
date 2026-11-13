package dev.henos.android

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import org.json.JSONArray
import org.json.JSONObject
import uniffi.henos_ffi.ChangeListener
import uniffi.henos_ffi.ClipboardSink
import uniffi.henos_ffi.Henos
import uniffi.henos_ffi.HenosException

/** A paired device, as the Rust core describes it. */
data class PairedDevice(
    val id: String,
    val name: String,
    val platform: String,
    val connected: Boolean,
    val nearby: Boolean,
)

/** A HENOS device on the network that we are not paired with. */
data class NearbyDevice(
    val id: String,
    val name: String,
    val platform: String,
    val pairing: Boolean,
)

data class PairingOffer(val code: String, val secondsLeft: Long)

data class ActivityEntry(val atMs: Long, val text: String)

/** A file being offered, sent or received. */
data class Transfer(
    val id: Long,
    val deviceName: String,
    val name: String,
    val size: Long,
    val bytes: Long,
    val incoming: Boolean,
    /** offered, waiting, active, done or failed. */
    val status: String,
    val detail: String?,
)

/** Everything the screen shows. Parsed from the same JSON the desktop app reads. */
data class UiState(
    val version: String,
    val deviceName: String,
    val paired: List<PairedDevice>,
    val nearby: List<NearbyDevice>,
    val pairing: PairingOffer?,
    val clipboardPaused: Boolean,
    val transfers: List<Transfer>,
    val activity: List<ActivityEntry>,
) {
    companion object {
        fun parse(json: String): UiState {
            val root = JSONObject(json)
            val pairing = root.optJSONObject("pairing")
            return UiState(
                version = root.getString("version"),
                deviceName = root.getJSONObject("device").getString("name"),
                paired = root.getJSONArray("paired").map {
                    PairedDevice(
                        id = it.getString("id"),
                        name = it.getString("name"),
                        platform = it.getString("platform"),
                        connected = it.getBoolean("connected"),
                        nearby = it.getBoolean("nearby"),
                    )
                },
                nearby = root.getJSONArray("nearby").map {
                    NearbyDevice(
                        id = it.getString("id"),
                        name = it.getString("name"),
                        platform = it.getString("platform"),
                        pairing = it.getBoolean("pairing"),
                    )
                },
                pairing = pairing?.let {
                    PairingOffer(it.getString("code"), it.getLong("secondsLeft"))
                },
                clipboardPaused = root.getBoolean("clipboardPaused"),
                transfers = root.getJSONArray("transfers").map {
                    Transfer(
                        id = it.getLong("id"),
                        deviceName = it.getString("deviceName"),
                        name = it.getString("name"),
                        size = it.getLong("size"),
                        bytes = it.getLong("bytes"),
                        incoming = it.getBoolean("incoming"),
                        status = it.getString("status"),
                        detail = if (it.isNull("detail")) null else it.getString("detail"),
                    )
                },
                activity = root.getJSONArray("activity").map {
                    ActivityEntry(it.getLong("atMs"), it.getString("text"))
                },
            )
        }

        private fun <T> JSONArray.map(transform: (JSONObject) -> T): List<T> =
            (0 until length()).map { transform(getJSONObject(it)) }
    }
}

/**
 * The one running HENOS instance of this app.
 *
 * It lives in the process, not in a screen: the background service starts it
 * and keeps the process alive, and screens only observe [state].
 */
object HenosCore {
    private val mainThread = Handler(Looper.getMainLooper())
    private val mutableState = MutableStateFlow<UiState?>(null)
    private val mutableError = MutableStateFlow<String?>(null)

    @Volatile
    private var henos: Henos? = null

    @Volatile
    private var files: FileBridge? = null

    /** The latest snapshot, or null until HENOS has started. */
    val state: StateFlow<UiState?> = mutableState

    /** Why HENOS could not start, if it could not. */
    val startError: StateFlow<String?> = mutableError

    /** Starts HENOS if it is not running yet. Safe to call repeatedly. */
    @Synchronized
    fun start(context: Context) {
        if (henos != null) return
        val app = context.applicationContext
        try {
            val bridge = FileBridge(app)
            files = bridge
            henos = Henos.start(
                dataDir = app.filesDir.resolve("henos").absolutePath,
                downloadsDir = bridge.incomingDir.absolutePath,
                deviceName = deviceName(app),
                clipboard = object : ClipboardSink {
                    override fun writeText(text: String) = putOnClipboard(app, text)
                },
                listener = object : ChangeListener {
                    override fun onChange() = refresh()
                },
            )
            mutableError.value = null
            refresh()
        } catch (e: HenosException) {
            mutableError.value = e.message ?: "HENOS could not start."
        }
    }

    /** Re-reads the state from the core. */
    fun refresh() {
        val running = henos ?: return
        val parsed = UiState.parse(running.stateJson())
        mutableState.value = parsed
        files?.onTransfers(parsed.transfers)
    }

    /**
     * Offers the picked or shared files to a device, or to every connected
     * device when [deviceId] is null. Returns an error sentence or null.
     */
    fun sendFiles(context: Context, uris: List<android.net.Uri>, deviceId: String?): String? {
        val bridge = files ?: return "HENOS is not running."
        val targets = deviceId?.let { listOf(it) }
            ?: state.value?.paired?.filter { it.connected }?.map { it.id }.orEmpty()
        if (targets.isEmpty()) return "None of your devices are connected right now."
        var problem: String? = null
        for (uri in uris) {
            val copy = bridge.copyForSending(context, uri)
            if (copy == null) {
                problem = "One of the files could not be read."
                continue
            }
            for (target in targets) {
                problem = run { bridge.sent(it.sendFile(target, copy.absolutePath).toLong(), copy) } ?: problem
            }
        }
        return problem
    }

    /** Runs [action] against the core; returns an error sentence or null. */
    fun run(action: (Henos) -> Unit): String? {
        val running = henos ?: return "HENOS is not running."
        return try {
            action(running)
            refresh()
            null
        } catch (e: HenosException) {
            refresh()
            e.message ?: "Something went wrong."
        }
    }

    /** Sends text copied or shared on this phone to the paired devices. */
    fun sendText(text: String): String? {
        if (text.isEmpty()) return "There is no text to send."
        if (text.toByteArray().size > MAX_TEXT_BYTES) return "That text is too long to send."
        return run { it.copyText(text) }
    }

    @Synchronized
    fun stop() {
        henos?.shutdown()
        henos = null
        mutableState.value = null
    }

    /** Received text goes on the clipboard, from the main thread as Android requires. */
    private fun putOnClipboard(context: Context, text: String) {
        mainThread.post {
            val clipboard = context.getSystemService(ClipboardManager::class.java)
            clipboard.setPrimaryClip(ClipData.newPlainText("HENOS", text))
        }
    }

    /** The name the person gave this phone, falling back to its model. */
    private fun deviceName(context: Context): String {
        val chosen = runCatching {
            Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME)
        }.getOrNull()
        return chosen?.takeIf { it.isNotBlank() } ?: Build.MODEL ?: "Android phone"
    }

    private const val MAX_TEXT_BYTES = 1024 * 1024
}
