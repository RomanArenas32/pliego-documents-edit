package com.romanarenas.pliego

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.provider.MediaStore
import android.provider.OpenableColumns
import java.io.File
import androidx.core.content.FileProvider
import androidx.activity.result.ActivityResult
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@InvokeArg
class UriArgs {
    lateinit var uri: String
}

@InvokeArg
class WriteArgs {
    lateinit var uri: String
    /** Temporary file with the contents; deleted once copied. */
    lateinit var source: String
}

/** Answers questions about content:// URIs that only Android can resolve (see files.rs). */
@TauriPlugin
class FilesPlugin(private val activity: Activity) : Plugin(activity) {
    /**
     * Picks a PDF the app can also write back to. The dialog plugin uses ACTION_GET_CONTENT,
     * which only grants reading, so "Save" could never overwrite the original.
     */
    @Command
    fun pickPdf(invoke: Invoke) {
        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT)
        intent.addCategory(Intent.CATEGORY_OPENABLE)
        intent.type = "application/pdf"
        startActivityForResult(invoke, intent, "pickPdfResult")
    }

    @ActivityCallback
    fun pickPdfResult(invoke: Invoke, result: ActivityResult) {
        val uri = if (result.resultCode == Activity.RESULT_OK) result.data?.data else null
        if (uri != null) {
            try {
                // Keeps access after the app restarts (recent files, saving later).
                activity.contentResolver.takePersistableUriPermission(
                    uri, Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
                )
            } catch (_: Exception) {
            }
        }
        val response = JSObject()
        response.put("uri", uri?.toString())
        invoke.resolve(response)
    }

    /** File the camera app is writing the photo to (see takePhoto). */
    private var photoFile: File? = null

    /**
     * Opens the camera app to photograph a document. The photo goes to a file in the app's
     * cache, shared through the FileProvider; using the camera app this way needs no camera
     * permission.
     */
    @Command
    fun takePhoto(invoke: Invoke) {
        val dir = File(activity.cacheDir, "scan").apply { mkdirs() }
        val file = File(dir, "foto-${System.currentTimeMillis()}.jpg")
        photoFile = file
        val uri = FileProvider.getUriForFile(activity, "${activity.packageName}.fileprovider", file)
        val intent = Intent(MediaStore.ACTION_IMAGE_CAPTURE)
        intent.putExtra(MediaStore.EXTRA_OUTPUT, uri)
        intent.addFlags(Intent.FLAG_GRANT_WRITE_URI_PERMISSION or Intent.FLAG_GRANT_READ_URI_PERMISSION)
        try {
            startActivityForResult(invoke, intent, "takePhotoResult")
        } catch (e: Exception) {
            invoke.reject("No hay una app de cámara en este teléfono.")
        }
    }

    @ActivityCallback
    fun takePhotoResult(invoke: Invoke, result: ActivityResult) {
        val file = photoFile
        photoFile = null
        val taken = result.resultCode == Activity.RESULT_OK && file != null && file.length() > 0
        if (!taken) file?.delete()
        val response = JSObject()
        response.put("path", if (taken) file!!.absolutePath else null)
        invoke.resolve(response)
    }

    /**
     * Writes a file through the content resolver. Closing the stream is what tells Android the
     * file changed; without it the Files app keeps showing the old size (0 B for new files).
     */
    @Command
    fun writeUri(invoke: Invoke) {
        val args = invoke.parseArgs(WriteArgs::class.java)
        val source = File(args.source)
        try {
            val output = activity.contentResolver.openOutputStream(Uri.parse(args.uri), "wt")
                ?: throw IllegalStateException("no output stream")
            output.use { out -> source.inputStream().use { it.copyTo(out) } }
            invoke.resolve()
        } catch (e: Exception) {
            invoke.reject(e.message ?: "write failed")
        } finally {
            source.delete()
        }
    }

    /** Leaves the app like the home button does; the open document stays as it was. */
    @Command
    fun moveToBack(invoke: Invoke) {
        activity.moveTaskToBack(true)
        invoke.resolve()
    }

    /** The name the file has for the user ("factura.pdf"), not the URI's opaque id. */
    @Command
    fun displayName(invoke: Invoke) {
        val args = invoke.parseArgs(UriArgs::class.java)
        var name: String? = null
        try {
            activity.contentResolver.query(Uri.parse(args.uri), arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { cursor ->
                if (cursor.moveToFirst()) name = cursor.getString(0)
            }
        } catch (_: Exception) {
        }
        val result = JSObject()
        result.put("name", name)
        invoke.resolve(result)
    }
}
