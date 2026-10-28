package com.sysgrok.matterpeek

import android.app.Activity
import android.graphics.drawable.ColorDrawable
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin
import androidx.core.view.WindowCompat

// The page's top bar, as in values/colors.xml and values-night/colors.xml.
private const val LIGHT_BAR = 0xFFFFFFFF.toInt()
private const val DARK_BAR = 0xFF14161A.toInt()

@InvokeArg
class ThemeArgs {
  var dark: Boolean = false
}

/**
 * The page's colours, light or dark, for the window around it: the colour
 * behind the status and navigation bars, and their icons to match. The page
 * may pick other colours than the system's.
 */
@TauriPlugin
class ThemePlugin(private val activity: Activity) : Plugin(activity) {
  @Command
  fun setTheme(invoke: Invoke) {
    val dark = invoke.parseArgs(ThemeArgs::class.java).dark

    activity.runOnUiThread {
      val window = activity.window
      window.setBackgroundDrawable(ColorDrawable(if (dark) DARK_BAR else LIGHT_BAR))
      val bars = WindowCompat.getInsetsController(window, window.decorView)
      bars.isAppearanceLightStatusBars = !dark
      bars.isAppearanceLightNavigationBars = !dark
    }
    invoke.resolve()
  }
}
