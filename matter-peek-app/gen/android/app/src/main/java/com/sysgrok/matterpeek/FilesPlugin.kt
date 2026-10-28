package com.sysgrok.matterpeek

import android.app.Activity
import android.content.Intent
import android.util.Base64
import androidx.activity.result.ActivityResult
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.IOException

@InvokeArg
class SaveArgs {
  var name: String = ""
  var data: String = ""
}

/**
 * Files for the user to keep, saved where the user picks on Android's "save
 * as" screen: the page's downloads do not work in the app.
 */
@TauriPlugin
class FilesPlugin(private val activity: Activity) : Plugin(activity) {
  /** The contents of the file being saved, until the user picked where. */
  private var pending: ByteArray? = null

  /**
   * Save a file, its contents base64, named as given unless the user renames
   * it. Answered whether it is saved.
   */
  @Command
  fun save(invoke: Invoke) {
    val args = invoke.parseArgs(SaveArgs::class.java)
    pending = Base64.decode(args.data, Base64.DEFAULT)
    val intent = Intent(Intent.ACTION_CREATE_DOCUMENT)
      .addCategory(Intent.CATEGORY_OPENABLE)
      .setType("application/octet-stream")
      .putExtra(Intent.EXTRA_TITLE, args.name)
    startActivityForResult(invoke, intent, "picked")
  }

  @ActivityCallback
  private fun picked(invoke: Invoke, result: ActivityResult) {
    val data = pending
    pending = null
    val uri = result.data?.data
    if (result.resultCode != Activity.RESULT_OK || uri == null || data == null) {
      invoke.resolve(JSObject().put("saved", false))
      return
    }

    try {
      val out = activity.contentResolver.openOutputStream(uri, "wt")
        ?: throw IOException("cannot open the file")
      out.use { it.write(data) }
      invoke.resolve(JSObject().put("saved", true))
    } catch (e: Exception) {
      invoke.reject(e.message ?: "cannot save the file", e)
    }
  }
}
