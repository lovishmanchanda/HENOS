package dev.henos.android

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.provider.OpenableColumns
import java.io.File
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors

/**
 * The Android side of file sharing.
 *
 * The Rust core reads and writes ordinary files, but Android hands apps
 * content links instead of paths and keeps the shared Downloads folder
 * behind its own interface. This class bridges the two: files to send are
 * copied into the app's private cache first, and received files are moved
 * from the private cache into Downloads once complete.
 */
class FileBridge(private val context: Context) {
    /** Where the core saves accepted files before they are moved out. */
    val incomingDir: File = context.cacheDir.resolve("incoming").apply { mkdirs() }
    private val outgoingDir: File = context.cacheDir.resolve("outgoing").apply { mkdirs() }

    private val background = Executors.newSingleThreadExecutor()
    private val outgoing = ConcurrentHashMap<Long, File>()
    private val moved = ConcurrentHashMap.newKeySet<Long>()
    private val announced = ConcurrentHashMap.newKeySet<Long>()

    init {
        // Anything left from an earlier run was never delivered.
        outgoingDir.listFiles()?.forEach { it.deleteRecursively() }
        incomingDir.listFiles()?.forEach { it.delete() }
        val channel = NotificationChannel(
            CHANNEL_ID, "Incoming files", NotificationManager.IMPORTANCE_HIGH,
        ).apply { description = "Shown when one of your devices wants to send a file." }
        context.getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }

    /** Copies what [uri] points at into the private cache, keeping its name. */
    fun copyForSending(context: Context, uri: Uri): File? = runCatching {
        val name = displayName(context, uri) ?: "file"
        // One folder per file, so two files with the same name do not clash.
        val folder = outgoingDir.resolve(System.nanoTime().toString()).apply { mkdirs() }
        val target = folder.resolve(name.replace('/', '_'))
        context.contentResolver.openInputStream(uri)!!.use { input ->
            target.outputStream().use { input.copyTo(it) }
        }
        target
    }.getOrNull()

    /** Remembers which private copy belongs to an outgoing transfer. */
    fun sent(transferId: Long, copy: File) {
        outgoing[transferId] = copy
    }

    /** Called with every new state: tidies up and moves finished files out. */
    fun onTransfers(transfers: List<Transfer>) {
        for (transfer in transfers) {
            val settled = transfer.status == "done" || transfer.status == "failed"
            if (!transfer.incoming && settled) {
                outgoing.remove(transfer.id)?.parentFile?.deleteRecursively()
            }
            if (transfer.incoming && transfer.status == "offered" && announced.add(transfer.id)) {
                announce(transfer)
            }
            if (transfer.incoming && transfer.status != "offered") {
                context.getSystemService(NotificationManager::class.java).cancel(notificationId(transfer.id))
            }
            val saved = transfer.detail
            if (transfer.incoming && transfer.status == "done" && saved != null && moved.add(transfer.id)) {
                background.execute { moveToDownloads(File(saved)) }
            }
        }
    }

    /** Moves a received file into Downloads/HENOS, where other apps can see it. */
    private fun moveToDownloads(file: File) {
        if (!file.exists()) return
        runCatching {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                val values = ContentValues().apply {
                    put(MediaStore.Downloads.DISPLAY_NAME, file.name)
                    put(MediaStore.Downloads.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS + "/HENOS")
                    put(MediaStore.Downloads.IS_PENDING, 1)
                }
                val resolver = context.contentResolver
                val target = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                    ?: error("Downloads is not available")
                resolver.openOutputStream(target)!!.use { out -> file.inputStream().use { it.copyTo(out) } }
                values.clear()
                values.put(MediaStore.Downloads.IS_PENDING, 0)
                resolver.update(target, values, null, null)
            } else {
                // Older Android: the app's own Downloads folder, which needs
                // no storage permission.
                val folder = context.getExternalFilesDir(Environment.DIRECTORY_DOWNLOADS) ?: return
                file.copyTo(folder.resolve(file.name), overwrite = false)
            }
            file.delete()
        }
    }

    /** Tells the person a file is waiting for their answer. */
    private fun announce(transfer: Transfer) {
        val open = PendingIntent.getActivity(
            context, 0,
            Intent(context, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val notification = android.app.Notification.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle("${transfer.deviceName} wants to send a file")
            .setContentText("${transfer.name} · tap to accept or decline")
            .setContentIntent(open)
            .setAutoCancel(true)
            .build()
        runCatching {
            context.getSystemService(NotificationManager::class.java)
                .notify(notificationId(transfer.id), notification)
        }
    }

    private fun notificationId(transferId: Long) = 1000 + (transferId % 100_000).toInt()

    private fun displayName(context: Context, uri: Uri): String? =
        context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
            ?.use { cursor -> if (cursor.moveToFirst()) cursor.getString(0) else null }
            ?: uri.lastPathSegment

    private companion object {
        const val CHANNEL_ID = "files"
    }
}
