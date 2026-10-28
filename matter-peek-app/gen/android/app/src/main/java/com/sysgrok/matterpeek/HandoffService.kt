package com.sysgrok.matterpeek

import android.app.Service
import android.content.Intent
import android.os.IBinder
import android.util.Log
import app.tauri.plugin.Channel
import app.tauri.plugin.JSObject
import com.google.android.gms.home.matter.commissioning.CommissioningCompleteMetadata
import com.google.android.gms.home.matter.commissioning.CommissioningRequestMetadata
import com.google.android.gms.home.matter.commissioning.CommissioningService
import com.google.android.gms.home.matter.commissioning.CommissioningService.CommissioningError

private const val TAG = "matter-peek"

/**
 * Google Play services hands a device over to us here, once it is on the
 * network with a commissioning window open: its address and passcode go to the
 * page, which commissions it onto our fabric (in the app, or on a server), and
 * Google Play services learns how that went.
 */
class HandoffService : Service(), CommissioningService.Callback {
  private lateinit var delegate: CommissioningService

  override fun onCreate() {
    super.onCreate()
    delegate = CommissioningService.Builder(this).setCallback(this).build()
  }

  override fun onBind(intent: Intent): IBinder = delegate.asBinder()

  override fun onCommissioningRequested(metadata: CommissioningRequestMetadata) {
    val location = metadata.networkLocation
    // IPv6 with its scope ID, if it has one: `fe80::1%47`.
    val ip = location.formattedIpAddress
    val addr = if (ip.contains(':')) "[$ip]:${location.port}" else "$ip:${location.port}"
    Log.i(TAG, "handoff: the device is at $addr")

    val device = JSObject().put("addr", addr).put("passcode", metadata.passcode)

    val sent = Handoff.handOver(device) { nodeId, error ->
      if (nodeId != null) {
        val done = CommissioningCompleteMetadata.builder().setToken(nodeId).build()
        delegate.sendCommissioningComplete(done)
      } else {
        Log.w(TAG, "handoff: the commission failed: $error")
        delegate.sendCommissioningError(CommissioningError.OTHER)
      }
    }
    if (!sent) {
      Log.w(TAG, "handoff: no page to commission the device")
      delegate.sendCommissioningError(CommissioningError.OTHER)
    }
  }
}

/**
 * Between [PlayPlugin] and [HandoffService]: the page's channel while a
 * commission with Google Play services runs, and the service waiting for the
 * page's answer.
 */
object Handoff {
  private var page: Channel? = null
  private var answer: ((String?, String?) -> Unit)? = null

  /** The devices handed over go to `channel`, until [end]. */
  @Synchronized
  fun begin(channel: Channel) {
    page = channel
  }

  /** No more devices go to the page; one waiting for it fails. */
  @Synchronized
  fun end() {
    page = null
    answer?.invoke(null, "the page did not answer")
    answer = null
  }

  /** Hand `device` to the page, then `answer` how its commission went; `false` with no page. */
  @Synchronized
  fun handOver(device: JSObject, answer: (nodeId: String?, error: String?) -> Unit): Boolean {
    val page = page ?: return false
    this.answer = answer
    page.send(device)
    return true
  }

  /** The page's answer: the device's node ID, or why it failed. */
  @Synchronized
  fun answer(nodeId: String?, error: String?) {
    answer?.invoke(nodeId, error)
    answer = null
  }
}
