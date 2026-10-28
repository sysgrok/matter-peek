package com.sysgrok.matterpeek

import android.app.Activity
import android.content.ComponentName
import android.content.Intent
import android.os.SystemClock
import android.util.Log
import android.webkit.WebView
import androidx.activity.result.ActivityResult
import androidx.activity.result.IntentSenderRequest
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import com.google.android.gms.common.ConnectionResult
import com.google.android.gms.common.GoogleApiAvailability
import com.google.android.gms.home.matter.Matter
import com.google.android.gms.home.matter.commissioning.CommissioningRequest
import com.google.android.gms.home.matter.commissioning.CommissioningWindow
import com.google.android.gms.home.matter.commissioning.DeviceInfo
import com.google.android.gms.home.matter.commissioning.ShareDeviceRequest
import com.google.android.gms.home.matter.commissioning.SharedDeviceData
import com.google.android.gms.home.matter.common.DeviceDescriptor
import com.google.android.gms.home.matter.common.Discriminator

private const val TAG = "matter-peek"

@InvokeArg
class WatchSharesArgs {
  lateinit var channel: Channel
}

@InvokeArg
class CommissionArgs {
  // A device shared with us: its pairing code, IDs and name. None for a new
  // device, whose QR code Google Play services scans.
  var code: String? = null
  var vendorId: Int? = null
  var productId: Int? = null
  var name: String? = null
  lateinit var handoff: Channel
}

@InvokeArg
class HandedOverArgs {
  var nodeId: String? = null
  var error: String? = null
}

@InvokeArg
class ShareArgs {
  // The device, and the commissioning window the page opened on it.
  lateinit var name: String
  var vendorId: Int = 0
  var productId: Int = 0
  var discriminator: Int = 0
  var passcode: Long = 0
  var remainingSecs: Long = 0
}

/**
 * Google Play services' Matter commissioning, for the page. Google Play
 * services puts a new device on the network (its QR code, Bluetooth, Wi-Fi or
 * Thread), then hands it over to [HandoffService] for the page to commission
 * onto our fabric. A device another Matter app shares with us, such as Google
 * Home, goes the same way, with its pairing code.
 */
@TauriPlugin
class PlayPlugin(private val activity: Activity) : Plugin(activity) {
  // A device shared with us before the page watches for them, and the page's
  // channel for them once it does.
  private var shared: JSObject? = null
  private var shares: Channel? = null

  override fun load(webView: WebView) {
    receiveShare(activity.intent)
  }

  override fun onNewIntent(intent: Intent) {
    receiveShare(intent)
  }

  /** Whether Google Play services is there. */
  @Command
  fun available(invoke: Invoke) {
    val status = GoogleApiAvailability.getInstance().isGooglePlayServicesAvailable(activity)
    invoke.resolve(JSObject().put("available", status == ConnectionResult.SUCCESS))
  }

  /** Send the devices shared with us on a channel, starting with one waiting. */
  @Command
  fun watchShares(invoke: Invoke) {
    val args = invoke.parseArgs(WatchSharesArgs::class.java)
    shares = args.channel
    shared?.let { args.channel.send(it) }
    shared = null
    invoke.resolve()
  }

  /**
   * Commission a device with Google Play services, which hands it over on the
   * `handoff` channel. Answered when its screens close: whether it is done.
   */
  @Command
  fun commission(invoke: Invoke) {
    val args = invoke.parseArgs(CommissionArgs::class.java)
    val request = CommissioningRequest.builder()
      .setCommissioningService(ComponentName(activity, HandoffService::class.java))
    args.code?.let { request.setOnboardingPayload(it) }
    args.name?.let { request.setDeviceNameHint(it) }
    val vendorId = args.vendorId
    val productId = args.productId
    if (vendorId != null && productId != null) {
      request.setDeviceInfo(DeviceInfo.builder().setVendorId(vendorId).setProductId(productId).build())
    }

    Handoff.begin(args.handoff)
    Matter.getCommissioningClient(activity)
      .commissionDevice(request.build())
      .addOnSuccessListener { sender ->
        startIntentSenderForResult(invoke, IntentSenderRequest.Builder(sender).build(), "commissioned")
      }
      .addOnFailureListener { e ->
        Handoff.end()
        invoke.reject(e.message ?: "Google Play services cannot commission", e)
      }
  }

  @ActivityCallback
  private fun commissioned(invoke: Invoke, result: ActivityResult) {
    Handoff.end()
    invoke.resolve(JSObject().put("done", result.resultCode == Activity.RESULT_OK))
  }

  /**
   * Share a device with another Matter app on the phone, which the user picks
   * from Android's list of them, through the commissioning window the page
   * opened. Answered when Android's screens close: whether an app was picked
   * (not that it commissioned the device).
   */
  @Command
  fun share(invoke: Invoke) {
    val args = invoke.parseArgs(ShareArgs::class.java)
    val request = try {
      val descriptor = DeviceDescriptor.builder()
      if (args.vendorId != 0) {
        descriptor.setVendorId(args.vendorId)
        if (args.productId != 0) descriptor.setProductId(args.productId)
      }
      // Open from now, as far as Android knows, for the time it has left.
      val window = CommissioningWindow.builder()
        .setDiscriminator(Discriminator.forLongValue(args.discriminator))
        .setPasscode(args.passcode)
        .setWindowOpenMillis(SystemClock.elapsedRealtime())
        .setDurationSeconds(args.remainingSecs)
        .build()
      ShareDeviceRequest.builder()
        .setDeviceDescriptor(descriptor.build())
        .setDeviceName(args.name)
        .setCommissioningWindow(window)
        .build()
    } catch (e: IllegalArgumentException) {
      invoke.reject(e.message ?: "Google Play services does not take this window", e)
      return
    } catch (e: IllegalStateException) {
      invoke.reject(e.message ?: "Google Play services does not take this window", e)
      return
    }

    Matter.getCommissioningClient(activity)
      .shareDevice(request)
      .addOnSuccessListener { sender ->
        startIntentSenderForResult(invoke, IntentSenderRequest.Builder(sender).build(), "shared")
      }
      .addOnFailureListener { e ->
        invoke.reject(e.message ?: "Google Play services cannot share the device", e)
      }
  }

  @ActivityCallback
  private fun shared(invoke: Invoke, result: ActivityResult) {
    invoke.resolve(JSObject().put("done", result.resultCode == Activity.RESULT_OK))
  }

  /** How the page's commission of the device handed over went: its node ID, or why not. */
  @Command
  fun handedOver(invoke: Invoke) {
    val args = invoke.parseArgs(HandedOverArgs::class.java)
    Handoff.answer(args.nodeId, args.error)
    invoke.resolve()
  }

  /** The device shared with us by `intent`, if it shares one, to the page. */
  private fun receiveShare(intent: Intent?) {
    if (intent?.action != Matter.ACTION_COMMISSION_DEVICE) return
    // Reopened from the recent apps: shared long ago.
    if (intent.flags and Intent.FLAG_ACTIVITY_LAUNCHED_FROM_HISTORY != 0) return

    val data = try {
      SharedDeviceData.fromIntent(intent)
    } catch (e: SharedDeviceData.InvalidSharedDeviceDataException) {
      Log.w(TAG, "share: a shared device without its data", e)
      return
    }
    // Its commissioning window's end is on the uptime clock.
    val windowSecs = (data.commissioningWindowExpirationMillis - SystemClock.elapsedRealtime()) / 1000
    Log.i(TAG, "share: ${data.deviceName}, window open for $windowSecs s")

    val device = JSObject()
      .put("code", data.manualPairingCode)
      .put("name", data.deviceName)
      .put("room", data.roomName)
      .put("vendorId", data.vendorId)
      .put("productId", data.productId)
      .put("windowSecs", windowSecs)
    val shares = shares
    if (shares != null) {
      shares.send(device)
    } else {
      shared = device
    }
  }
}
