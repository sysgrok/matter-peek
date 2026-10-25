package com.sysgrok.matterpeek

import android.net.wifi.WifiManager
import android.os.Bundle
import android.view.View
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.updatePadding

class MainActivity : TauriActivity() {
  // Wi-Fi drops multicast to save power unless an app holds this: the core's
  // mDNS (discovery, resolving devices) needs it while the app is in front.
  private var multicast: WifiManager.MulticastLock? = null

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)

    val wifi = applicationContext.getSystemService(WIFI_SERVICE) as WifiManager
    multicast = wifi.createMulticastLock("matter-peek").apply { setReferenceCounted(false) }

    // The page stays clear of the status and navigation bars, the cutout and
    // the keyboard; the window's background shows behind the bars.
    val content = findViewById<View>(android.R.id.content)
    ViewCompat.setOnApplyWindowInsetsListener(content) { view, insets ->
      val bars = insets.getInsets(
        WindowInsetsCompat.Type.systemBars() or
          WindowInsetsCompat.Type.displayCutout() or
          WindowInsetsCompat.Type.ime()
      )
      view.updatePadding(left = bars.left, top = bars.top, right = bars.right, bottom = bars.bottom)
      WindowInsetsCompat.CONSUMED
    }
  }

  override fun onResume() {
    super.onResume()
    multicast?.acquire()
  }

  override fun onPause() {
    multicast?.takeIf { it.isHeld }?.release()
    super.onPause()
  }
}
