package dev.henos.android

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/**
 * Reconnects after the phone restarts or HENOS is updated, so shared text
 * keeps arriving without opening the app first.
 */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        when (intent.action) {
            Intent.ACTION_BOOT_COMPLETED, Intent.ACTION_MY_PACKAGE_REPLACED ->
                runCatching { HenosService.start(context) }
        }
    }
}
