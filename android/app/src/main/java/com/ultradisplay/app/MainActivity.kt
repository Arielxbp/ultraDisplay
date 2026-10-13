package com.ultradisplay.app

import android.app.Activity
import android.app.AlertDialog
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.Gravity
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.view.WindowManager
import android.text.InputType
import android.widget.Button
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView

class MainActivity : Activity(), SurfaceHolder.Callback {
    private lateinit var surfaceView: SurfaceView
    private lateinit var status: TextView
    private lateinit var stats: TextView
    private lateinit var waitingPanel: LinearLayout
    private var client: StreamClient? = null
    private val ui = Handler(Looper.getMainLooper())
    private var showStats = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        window.attributes = window.attributes.apply {
            layoutInDisplayCutoutMode =
                WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
            preferredDisplayModeId = highestRefreshMode()?.modeId ?: 0
        }

        surfaceView = SurfaceView(this).apply { holder.addCallback(this@MainActivity) }
        status = TextView(this).apply {
            setTextColor(Color.WHITE)
            textSize = 20f
            gravity = Gravity.CENTER
        }
        stats = TextView(this).apply {
            setTextColor(Color.GREEN)
            setBackgroundColor(0x80000000.toInt())
            textSize = 14f
            setPadding(16, 8, 16, 8)
            visibility = View.GONE
        }
        waitingPanel = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER
            addView(status, LinearLayout.LayoutParams(-2, -2))
            addView(LinearLayout(this@MainActivity).apply {
                gravity = Gravity.CENTER
                setPadding(0, 48, 0, 0)
                addView(Button(this@MainActivity).apply {
                    text = "Host address…"
                    setOnClickListener { askHostAddress() }
                })
                addView(Button(this@MainActivity).apply {
                    text = "Forget pairing"
                    setOnClickListener { Prefs(this@MainActivity).pairedHost = null }
                })
            })
        }
        val root = FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            addView(surfaceView, FrameLayout.LayoutParams(-1, -1))
            addView(waitingPanel, FrameLayout.LayoutParams(-1, -1))
            addView(stats, FrameLayout.LayoutParams(-2, -2, Gravity.TOP or Gravity.START))
            // Tap anywhere to show/hide the stats overlay.
            setOnClickListener {
                showStats = !showStats
                stats.visibility = if (showStats) View.VISIBLE else View.GONE
            }
        }
        setContentView(root)
        ui.post(object : Runnable {
            override fun run() {
                if (showStats) stats.text = StreamClient.overlay
                ui.postDelayed(this, 500)
            }
        })
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) hideSystemBars()
    }

    private fun hideSystemBars() {
        window.insetsController?.let {
            it.hide(WindowInsets.Type.systemBars())
            it.systemBarsBehavior = WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
    }

    private fun highestRefreshMode() = display?.supportedModes
        ?.filter { it.physicalWidth == display!!.mode.physicalWidth && it.physicalHeight == display!!.mode.physicalHeight }
        ?.maxByOrNull { it.refreshRate }

    override fun surfaceCreated(holder: SurfaceHolder) {
        val surface = holder.surface
        // Tell the compositor this surface wants the panel at the stream's 120 Hz.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            surface.setFrameRate(
                120f,
                android.view.Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE,
                android.view.Surface.CHANGE_FRAME_RATE_ALWAYS,
            )
        } else {
            surface.setFrameRate(120f, android.view.Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE)
        }

        val mode = display!!.mode
        val device = DeviceInfo(
            model = Build.MODEL,
            // Landscape: long edge is the width.
            width = maxOf(mode.physicalWidth, mode.physicalHeight),
            height = minOf(mode.physicalWidth, mode.physicalHeight),
            refreshRates = display!!.supportedModes
                .filter { it.physicalWidth == mode.physicalWidth }
                .map { it.refreshRate }
                .distinct()
                .sortedDescending(),
            codecs = VideoDecoder.supportedCodecs(),
        )
        client = StreamClient(this, surface, device, onStatus = { msg ->
            ui.post {
                status.text = msg
                waitingPanel.visibility = if (msg.isEmpty()) View.GONE else View.VISIBLE
            }
        }).also { it.start() }
    }

    /** Manual host IP for Wi-Fi networks that block mDNS. Blank clears it. */
    private fun askHostAddress() {
        val prefs = Prefs(this)
        val input = EditText(this).apply {
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI
            hint = "192.168.1.100"
            setText(prefs.manualHostAddress ?: "")
        }
        AlertDialog.Builder(this)
            .setTitle("Host address (Wi-Fi)")
            .setMessage("Only needed if the host isn't found automatically. The tablet must have been paired over USB.")
            .setView(input)
            .setPositiveButton("Save") { _, _ -> prefs.manualHostAddress = input.text.toString().trim() }
            .setNegativeButton("Cancel", null)
            .show()
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        client?.stop()
        client = null
    }
}
