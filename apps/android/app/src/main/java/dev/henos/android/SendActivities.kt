package dev.henos.android

import android.app.Activity
import android.content.ClipboardManager
import android.content.Intent
import android.os.Bundle
import android.widget.Toast
import kotlin.concurrent.thread

/** Sends [text] in the background, tells the person how it went, then closes. */
private fun Activity.sendAndFinish(text: String?) {
    if (text.isNullOrEmpty()) {
        Toast.makeText(this, "There is no text to send.", Toast.LENGTH_SHORT).show()
        finish()
        return
    }
    HenosService.start(this)
    thread(name = "henos-send") {
        HenosCore.start(applicationContext)
        val error = HenosCore.sendText(text)
        runOnUiThread {
            val message = error ?: "Sent to your devices."
            Toast.makeText(applicationContext, message, Toast.LENGTH_SHORT).show()
            finish()
        }
    }
}

/**
 * Reads the clipboard and sends it. Opened from the notification.
 *
 * Android lets an app read the clipboard only while it has the screen's
 * focus, so this is an invisible activity that waits for focus, reads once,
 * and closes.
 */
class SendClipboardActivity : Activity() {
    private var handled = false

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (!hasFocus || handled) return
        handled = true
        val clipboard = getSystemService(ClipboardManager::class.java)
        val text = clipboard.primaryClip
            ?.takeIf { it.itemCount > 0 }
            ?.getItemAt(0)
            ?.coerceToText(this)
            ?.toString()
        sendAndFinish(text)
    }
}

/**
 * Receives what another app shares ("Share" then "Send to my devices"):
 * files go to every connected device, plain text goes to their clipboards.
 */
class ShareActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val shared = intent
        val files: List<android.net.Uri> = when (shared?.action) {
            Intent.ACTION_SEND -> listOfNotNull(streamOf(shared))
            Intent.ACTION_SEND_MULTIPLE -> streamsOf(shared)
            else -> emptyList()
        }
        if (files.isEmpty()) {
            sendAndFinish(shared?.takeIf { it.action == Intent.ACTION_SEND }?.getStringExtra(Intent.EXTRA_TEXT))
            return
        }
        HenosService.start(this)
        thread(name = "henos-share") {
            HenosCore.start(applicationContext)
            val error = HenosCore.sendFiles(applicationContext, files, deviceId = null)
            runOnUiThread {
                val message = error ?: "Offered to your devices. They will ask to accept."
                Toast.makeText(applicationContext, message, Toast.LENGTH_LONG).show()
                finish()
            }
        }
    }

    @Suppress("DEPRECATION")
    private fun streamOf(intent: Intent): android.net.Uri? = intent.getParcelableExtra(Intent.EXTRA_STREAM)

    @Suppress("DEPRECATION")
    private fun streamsOf(intent: Intent): List<android.net.Uri> =
        intent.getParcelableArrayListExtra<android.net.Uri>(Intent.EXTRA_STREAM).orEmpty()
}
