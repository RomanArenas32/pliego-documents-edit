package com.romanarenas.pliego

import android.content.res.Configuration
import android.graphics.Color
import android.os.Bundle
import android.view.View
import android.webkit.WebView
import androidx.activity.OnBackPressedCallback
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  private var webView: WebView? = null

  override fun onWebViewCreate(webView: WebView) {
    this.webView = webView
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // The window is edge to edge: keep the page clear of the status bar, the navigation bar
    // and the keyboard, otherwise they cover the toolbars and swallow their touches.
    val content = findViewById<View>(android.R.id.content)
    // Same color as the app's toolbars (--panel in styles.css), so the bars blend in.
    val night = (resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK) == Configuration.UI_MODE_NIGHT_YES
    content.setBackgroundColor(Color.parseColor(if (night) "#27272a" else "#f7f7f8"))
    ViewCompat.setOnApplyWindowInsetsListener(content) { view, insets ->
      val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.ime())
      view.setPadding(bars.left, bars.top, bars.right, bars.bottom)
      WindowInsetsCompat.CONSUMED
    }
    // Back closes what's open in the app (dialogs, menus…) and asks about unsaved changes
    // before leaving (mobile.ts); by default Android would just close it.
    onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
      override fun handleOnBackPressed() {
        val page = webView
        if (page == null) {
          moveTaskToBack(true)
          return
        }
        page.evaluateJavascript("window.__pliegoBack ? (window.__pliegoBack(), 'handled') : 'none'") { result ->
          if (result != "\"handled\"") moveTaskToBack(true)
        }
      }
    })
  }
}
